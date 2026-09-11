use crate::command::Command;
use crate::{HookServerError, RunHookPayload};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(crate) struct HookServerState {
    run_available: AtomicBool,
    ready: AtomicBool,
    initializing: Mutex<bool>,
    cancellation: CancellationToken,
    terminate_grace_period: Duration,
    completion: watch::Sender<bool>,
    result: Mutex<Option<Result<(), HookServerError>>>,
}

impl HookServerState {
    pub(crate) fn new(terminate_grace_period: Duration) -> Arc<Self> {
        Arc::new(Self {
            run_available: AtomicBool::new(true),
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

    pub(crate) fn claim_run(&self) -> bool {
        self.run_available.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn terminate_grace_period(&self) -> Duration {
        self.terminate_grace_period
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

    pub(crate) fn track_command<T>(self: &Arc<Self>, command_result: T)
    where
        T: Future<Output = Result<(), HookServerError>> + Send + 'static,
    {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            state.finish(command_result.await);
        });
    }
}
