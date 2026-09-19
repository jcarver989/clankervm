#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

mod readiness;
mod transition;

pub(crate) use readiness::wait_until_running;

use crate::client::MicroVmClient;
use crate::output::render;
use crate::util::positive_duration;
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde_json::json;
use std::time::Duration;
use transition::{LifecycleTransition, Operation};

/// How long owned-VM cleanup waits for AWS to confirm termination.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Args)]
pub struct LifecycleOptions {
    pub vm_id: String,
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value = "30s", value_parser = positive_duration)]
    pub timeout: Duration,
}

pub(crate) async fn suspend<T: MicroVmClient>(
    options: LifecycleOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    execute(options, format, client, Operation::Suspend).await
}

pub(crate) async fn resume<T: MicroVmClient>(
    options: LifecycleOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    execute(options, format, client, Operation::Resume).await
}

pub(crate) async fn stop<T: MicroVmClient>(
    options: LifecycleOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    execute(options, format, client, Operation::Stop).await
}

/// Shared owned-VM cleanup, including the termination request in the deadline.
pub(crate) async fn terminate<T: MicroVmClient>(client: &T, id: &str) -> Result<(), ClankerError> {
    LifecycleTransition::new(client, id, Operation::Stop)
        .apply(true, CLEANUP_TIMEOUT)
        .await
        .map(drop)
}

async fn execute<T: MicroVmClient>(
    options: LifecycleOptions,
    format: OutputFormat,
    client: &T,
    operation: Operation,
) -> Result<(), ClankerError> {
    let confirmation = LifecycleTransition::new(client, &options.vm_id, operation)
        .apply(options.wait, options.timeout)
        .await?;
    let confirmed = confirmation.is_confirmed();
    let value = json!({
        "microvmId": options.vm_id,
        operation.confirmed_field(): confirmed,
    });
    render(format, &value, || {
        format!(
            "MicroVM {}: {} {}",
            options.vm_id,
            operation.noun(),
            confirmation.human(),
        )
    })
}
