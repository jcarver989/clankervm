use crate::client::MicroVmClient;
use crate::output::render;
use crate::{ClankerError, OutputFormat};
use clap::Args;

#[derive(Debug, Args)]
pub struct CommandOptions {
    pub vm_id: String,
}

pub async fn describe<T: MicroVmClient>(
    options: CommandOptions,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let details = client
        .get_details(&options.vm_id)
        .await?
        .ok_or_else(|| ClankerError::MicroVmNotFound(options.vm_id.clone()))?;

    render(format, &details, || {
        format!(
            "MicroVM {}\nState: {}\nState reason: {}\nEndpoint: {}\nIngress: {}",
            details.microvm_id,
            details.state,
            details.state_reason.as_deref().unwrap_or(""),
            details.endpoint,
            details.ingress_network_connectors.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient};
    use crate::test_support::MicroVmDetailsBuilder;

    #[tokio::test]
    async fn command_describes_the_requested_vm() {
        let client = FakeMicroVmClient::default().described([Ok(Some(
            MicroVmDetailsBuilder::new("vm").application().build(),
        ))]);

        describe(
            CommandOptions { vm_id: "vm".into() },
            OutputFormat::Human,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(client.calls(), [Call::Describe("vm".into())]);
    }
}
