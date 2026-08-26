use crate::HookServerError;
use crate::handlers::CommandResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(crate) struct HookServerState {
    run_available: AtomicBool,
    cancellation: CancellationToken,
    completion: watch::Sender<bool>,
    result: Mutex<Option<Result<(), HookServerError>>>,
}

impl HookServerState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            run_available: AtomicBool::new(true),
            cancellation: CancellationToken::new(),
            completion: watch::channel(false).0,
            result: Mutex::new(None),
        })
    }

    pub(crate) fn claim_run(&self) -> bool {
        self.run_available.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
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

    pub(crate) fn track_command(self: &Arc<Self>, command_result: CommandResult) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            state.finish(command_result.await);
        });
    }
}
