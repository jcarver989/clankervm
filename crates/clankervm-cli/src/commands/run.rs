use super::render;
use crate::{ClankerError, MicroVmClient, OutputFormat, Project};
use clap::Args;

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long)]
    pub release: Option<String>,
    #[arg(long)]
    pub execution_role_arn: Option<String>,
    #[arg(long)]
    pub ingress: Option<String>,
    #[arg(long)]
    pub egress: Option<String>,
    #[arg(long)]
    pub max_duration: Option<i32>,
    #[arg(long)]
    pub log_group: Option<String>,
    #[arg(long)]
    pub client_token: Option<String>,
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub(super) async fn execute<T: MicroVmClient>(
    args: RunArgs,
    project: &Project,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let result = crate::runtime::run(args, project.config(), client).await?;
    render(format, &result, || {
        format!(
            "✓ Started MicroVM {}\n  Release: {}@{}{}",
            result.microvm_id,
            project.config().app.name,
            result.image_version,
            result
                .log_group
                .as_ref()
                .map_or_else(String::new, |group| format!("\n  Logs:    {group}"))
        )
    })
}
