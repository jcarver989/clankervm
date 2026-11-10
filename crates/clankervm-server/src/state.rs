use crate::command::Command;
use crate::error::ApiError;
use crate::{HookServerError, RunHookPayload};
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::select;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) struct HookServerState {
    run_available: AtomicBool,
    lifecycle: OnceLock<mpsc::Sender<LifecycleMessage>>,
    validation: Mutex<Validation>,
    ready: AtomicBool,
    initializing: Mutex<bool>,
    cancellation: CancellationToken,
    terminate_grace_period: Duration,
    completion: watch::Sender<bool>,
    result: Mutex<Option<Result<(), HookServerError>>>,
}

type LifecycleMessage = (Lifecycle, oneshot::Sender<Result<(), ApiError>>);

pub(crate) enum Lifecycle {
    Suspend,
    Resume,
}

struct Running {
    stop: CancellationToken,
    exited: JoinHandle<Result<(), HookServerError>>,
}

enum Validation {
    Disabled,
    Pending(RunHookPayload),
    Running,
    Passed,
}

impl HookServerState {
    pub(crate) fn new(terminate_grace_period: Duration) -> Arc<Self> {
        Arc::new(Self {
            run_available: AtomicBool::new(true),
            lifecycle: OnceLock::new(),
            validation: Mutex::new(Validation::Disabled),
            ready: AtomicBool::new(true),
            initializing: Mutex::new(false),
            cancellation: CancellationToken::new(),
            terminate_grace_period,
            completion: watch::channel(false).0,
            result: Mutex::new(None),
        })
    }

    pub(crate) fn initialize(
        self: &Arc<Self>,
        payload: RunHookPayload,
    ) -> Result<(), HookServerError> {
        self.ready.store(false, Ordering::Release);
        let (command, args, environment) = payload.into_parts();
        let command = Command::spawn(command, args, environment, self.terminate_grace_period)?;
        *self.initializing.lock().expect("initialization lock") = true;
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let result = command.wait(state.cancellation_token()).await;
            let mut initializing = state.initializing.lock().expect("initialization lock");
            *initializing = false;
            if result.is_err() || state.cancellation.is_cancelled() {
                state.finish(result);
            } else {
                state.ready.store(true, Ordering::Release);
            }
        });
        Ok(())
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.cancellation.is_cancelled()
    }

    pub(crate) fn configure_validation(&self, payload: RunHookPayload) {
        *self.validation.lock().expect("validation lock") = Validation::Pending(payload);
    }

    pub(crate) fn validate(self: &Arc<Self>) -> Result<bool, ApiError> {
        let mut validation = self.validation.lock().expect("validation lock");
        if !self.is_ready() {
            return Err(ApiError::not_ready());
        }
        match &*validation {
            Validation::Disabled => {
                return Err(ApiError::bad_request("validation is not configured"));
            }
            Validation::Running => return Ok(false),
            Validation::Passed => return Ok(true),
            Validation::Pending(_) => {}
        }
        if !self.run_available.load(Ordering::Acquire) {
            return Err(ApiError::conflict("run already started"));
        }
        let Validation::Pending(payload) = mem::replace(&mut *validation, Validation::Running)
        else {
            unreachable!("pending validation checked above");
        };
        let (command, args, environment) = payload.into_parts();
        let command = Command::spawn(command, args, environment, self.terminate_grace_period)
            .map_err(|error| ApiError::initialization(self, error))?;
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let result = command.wait(state.cancellation_token()).await;
            let mut validation = state.validation.lock().expect("validation lock");
            if result.is_err() || state.cancellation.is_cancelled() {
                state.finish(result);
            } else {
                *validation = Validation::Passed;
            }
        });
        Ok(false)
    }

    pub(crate) fn claim_run(&self) -> bool {
        let validation = self.validation.lock().expect("validation lock");
        if matches!(*validation, Validation::Running) {
            return false;
        }
        self.run_available.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn start_run(self: &Arc<Self>, payload: RunHookPayload) -> Result<(), ApiError> {
        let running = self
            .spawn(&payload)
            .map_err(|error| ApiError::initialization(self, error))?;
        let (sender, messages) = mpsc::channel(1);
        self.lifecycle
            .set(sender)
            .expect("run is claimed only once");
        tokio::spawn(Arc::clone(self).supervise(payload, running, messages));
        Ok(())
    }

    pub(crate) async fn lifecycle(&self, event: Lifecycle) -> Result<(), ApiError> {
        let sender = self
            .lifecycle
            .get()
            .ok_or_else(|| ApiError::conflict("run has not started"))?;
        let stopping = || ApiError::conflict("hook server is stopping");
        let (reply, response) = oneshot::channel();
        sender.send((event, reply)).await.map_err(|_| stopping())?;
        response.await.map_err(|_| stopping())?
    }

    async fn supervise(
        self: Arc<Self>,
        payload: RunHookPayload,
        running: Running,
        mut messages: mpsc::Receiver<LifecycleMessage>,
    ) {
        let mut running = Some(running);
        loop {
            select! {
                result = async { (&mut running.as_mut().expect("guarded").exited).await },
                    if running.is_some() =>
                {
                    return self.finish(result.expect("command task panicked"));
                }
                () = self.cancellation.cancelled(), if running.is_none() => {
                    return self.finish(Ok(()));
                }
                Some((event, reply)) = messages.recv() => {
                    let result = match (event, running.take()) {
                        (Lifecycle::Suspend, Some(command)) => {
                            command.stop.cancel();
                            command.exited.await.expect("command task panicked")
                        }
                        (Lifecycle::Resume, None) => self.spawn(&payload).map(|command| {
                            running = Some(command);
                        }),
                        (_, command) => {
                            running = command;
                            Ok(())
                        }
                    };
                    let _ = reply.send(result.map_err(|error| ApiError::initialization(&self, error)));
                }
            }
        }
    }

    fn spawn(&self, payload: &RunHookPayload) -> Result<Running, HookServerError> {
        let (command, args, environment) = payload.clone().into_parts();
        let command = Command::spawn(command, args, environment, self.terminate_grace_period)?;
        let stop = self.cancellation.child_token();
        let exited = tokio::spawn(command.wait(stop.clone()));
        Ok(Running { stop, exited })
    }

    pub(crate) fn begin_shutdown(&self) {
        self.cancellation.cancel();
        if self.claim_run() && !*self.initializing.lock().expect("initialization lock") {
            self.finish(Ok(()));
        }
    }

    pub(crate) async fn wait_for_completion(&self) {
        let mut completion = self.completion.subscribe();
        let _ = completion.wait_for(|complete| *complete).await;
    }

    pub(crate) fn take_result(&self) -> Result<(), HookServerError> {
        self.result
            .lock()
            .expect("completion result lock")
            .take()
            .unwrap_or(Ok(()))
    }

    pub(crate) fn finish(&self, result: Result<(), HookServerError>) {
        let mut stored = self.result.lock().expect("completion result lock");
        if stored.is_none() {
            *stored = Some(result);
            self.completion.send_replace(true);
        }
    }
}
