use crate::client::{MicroVmClient, MicroVmDetails};
use crate::output::render;
use crate::{ClankerError, OutputFormat};
use clap::Args;
use serde::Serialize;

#[derive(Debug, Args)]
pub struct InspectOptions {
    /// MicroVM to inspect.
    pub microvm_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InspectResult {
    microvm_id: String,
    state: String,
    state_reason: Option<String>,
    endpoint: String,
    ingress_network_connectors: Vec<String>,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &InspectOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let details = client
        .describe(&options.microvm_id)
        .await?
        .ok_or_else(|| ClankerError::MicroVmNotFound(options.microvm_id.clone()))?;
    let result = result(details);
    render(format, &result, || {
        format!(
            "MicroVM {}\n  State:    {}{}\n  Endpoint: {}\n  Ingress:  {}",
            result.microvm_id,
            result.state,
            result
                .state_reason
                .as_ref()
                .map_or(String::new(), |reason| format!(" ({reason})")),
            if result.endpoint.is_empty() {
                "-"
            } else {
                &result.endpoint
            },
            if result.ingress_network_connectors.is_empty() {
                "-".into()
            } else {
                result.ingress_network_connectors.join(", ")
            }
        )
    })
}

fn result(details: MicroVmDetails) -> InspectResult {
    InspectResult {
        microvm_id: details.microvm_id,
        state: details.state.to_string(),
        state_reason: details.state_reason,
        endpoint: details.endpoint,
        ingress_network_connectors: details.ingress_network_connectors,
    }
}
