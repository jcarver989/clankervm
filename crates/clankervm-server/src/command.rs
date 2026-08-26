use crate::HookServerError;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command as TokioCommand};
use tokio::select;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

pub(crate) struct Command {
    child: Child,
    process_group: Pid,
    terminate_grace_period: Duration,
}

impl Command {
    pub(crate) fn spawn(
        executable: String,
        args: Vec<String>,
        environment: BTreeMap<String, String>,
        terminate_grace_period: Duration,
    ) -> Result<Self, HookServerError> {
        let mut command = TokioCommand::new(executable);
        command
            .args(args)
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        command.as_std_mut().process_group(0);
        let child = command.spawn().map_err(HookServerError::CommandSpawn)?;
        let process_group = child
            .id()
            .map(|id| Pid::from_raw(id.cast_signed()))
            .expect("freshly spawned child has a process ID");

        Ok(Self {
            child,
            process_group,
            terminate_grace_period,
        })
    }

    pub(crate) async fn wait(
        mut self,
        cancellation: CancellationToken,
    ) -> Result<(), HookServerError> {
        select! {
            status = self.child.wait() => {
                let status = status.map_err(HookServerError::CommandWait)?;
                if cancellation.is_cancelled() || status.success() {
                    Ok(())
                } else {
                    Err(HookServerError::CommandFailed)
                }
            }

            () = cancellation.cancelled() => {
                signal_process_group(self.process_group, Signal::SIGTERM)?;
                let deadline = Instant::now() + self.terminate_grace_period;
                let _ = timeout_at(deadline, self.child.wait()).await;
                if !wait_for_process_group_exit(self.process_group, deadline).await? {
                    signal_process_group(self.process_group, Signal::SIGKILL)?;
                }

                self.child.wait().await.map_err(HookServerError::CommandWait)?;
                Ok(())
            }
        }
    }
}

fn signal_process_group(process_group: Pid, signal: Signal) -> Result<(), HookServerError> {
    match killpg(process_group, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(HookServerError::CommandSignal(error.into())),
    }
}

async fn wait_for_process_group_exit(
    process_group: Pid,
    deadline: Instant,
) -> Result<bool, HookServerError> {
    loop {
        match killpg(process_group, None) {
            Err(Errno::ESRCH) => return Ok(true),
            Err(error) => return Err(HookServerError::CommandSignal(error.into())),
            Ok(()) if Instant::now() >= deadline => return Ok(false),
            Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}
