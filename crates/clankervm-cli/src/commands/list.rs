use crate::client::{ImageIdentifier, MicroVmClient, MicroVmClientError, MicroVmSummary};
use crate::config::ProjectConfig;
use crate::output::render;
use crate::util::validate_non_empty;
use crate::{ClankerError, OutputFormat};
use aws_sdk_lambdamicrovms::types::MicrovmState;
use clap::Args;
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Args)]
pub struct ListOptions {
    /// Only MicroVMs that ran this image name or ARN.
    #[arg(long, value_name = "NAME_OR_ARN")]
    pub image: Option<String>,
    /// Only MicroVMs that ran this image version; requires --image.
    #[arg(long, value_name = "VERSION", requires = "image")]
    pub image_version: Option<String>,
    /// Only MicroVMs in this state; repeatable, replaces the TERMINATED default.
    #[arg(long = "state", value_name = "STATE", conflicts_with = "all")]
    pub states: Vec<String>,
    /// Include TERMINATED MicroVMs.
    #[arg(long)]
    pub all: bool,
    /// How long listing every page may take.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "1m")]
    pub timeout: Duration,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            image: None,
            image_version: None,
            states: Vec::new(),
            all: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListResult {
    pub microvms: Vec<MicroVmSummary>,
}

pub(super) async fn execute<T: MicroVmClient>(
    options: &ListOptions,
    config: &ProjectConfig,
    format: OutputFormat,
    client: &T,
) -> Result<(), ClankerError> {
    let result = list(options, config, client).await?;
    render(format, &result, || human(&result, config, options))
}

/// Every page `list` asks for must resolve before the invocation deadline; a
/// partial result is never reported as a complete one.
async fn list<T: MicroVmClient>(
    options: &ListOptions,
    config: &ProjectConfig,
    client: &T,
) -> Result<ListResult, ClankerError> {
    validate_non_empty(options.image_version.as_deref(), "image version")?;
    let image = options
        .image
        .as_deref()
        .map(|image| ImageIdentifier::parse(image, &config.aws.region))
        .transpose()?;
    let states = StateFilter::new(&options.states, options.all)?;
    let collected = tokio::time::timeout(
        options.timeout,
        collect(client, image.as_ref(), options.image_version.as_deref()),
    )
    .await
    .map_err(|_| ClankerError::ListTimeout {
        timeout: options.timeout,
    })??;
    Ok(ListResult {
        microvms: states.select(collected),
    })
}

async fn collect<T: MicroVmClient>(
    client: &T,
    image: Option<&ImageIdentifier>,
    version: Option<&str>,
) -> Result<Vec<MicroVmSummary>, MicroVmClientError> {
    let mut microvms = Vec::new();
    let mut next_token = None;
    loop {
        // Empty pages still advance the token, so only a missing token ends it.
        let (page, token) = client
            .list_microvms(image, version, next_token.as_deref())
            .await?;
        microvms.extend(page);
        match token {
            Some(token) => next_token = Some(token),
            None => return Ok(microvms),
        }
    }
}

enum StateFilter {
    Every,
    Live,
    Only(HashSet<MicrovmState>),
}

impl StateFilter {
    fn new(states: &[String], all: bool) -> Result<Self, ClankerError> {
        if states.is_empty() {
            return Ok(if all { Self::Every } else { Self::Live });
        }
        let mut selected = HashSet::new();
        for state in states {
            let state = state.trim().to_ascii_uppercase();
            let parsed = MicrovmState::try_parse(&state).map_err(|_| {
                ClankerError::InvalidConfig(format!(
                    "unsupported MicroVM state `{state}`; expected one of {}",
                    MicrovmState::values().join(", ")
                ))
            })?;
            selected.insert(parsed);
        }
        Ok(Self::Only(selected))
    }

    fn keeps(&self, state: &MicrovmState) -> bool {
        match self {
            Self::Every => true,
            Self::Live => !matches!(state, MicrovmState::Terminated),
            Self::Only(selected) => selected.contains(state),
        }
    }

    /// Keeps every matching MicroVM once, newest first with an id tie-breaker.
    fn select(&self, microvms: Vec<MicroVmSummary>) -> Vec<MicroVmSummary> {
        let mut selected: Vec<_> = microvms
            .into_iter()
            .filter(|microvm| self.keeps(&microvm.state))
            .collect();
        selected.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| left.microvm_id.cmp(&right.microvm_id))
        });
        let mut seen = HashSet::new();
        selected.retain(|microvm| seen.insert(microvm.microvm_id.clone()));
        selected
    }
}

fn human(result: &ListResult, config: &ProjectConfig, options: &ListOptions) -> String {
    let mut lines = vec![
        format!("Region:  {}", config.aws.region),
        format!(
            "Profile: {}",
            config
                .aws
                .profile
                .as_deref()
                .unwrap_or("default credential chain")
        ),
    ];
    if result.microvms.is_empty() {
        lines.push(String::new());
        lines.push("No MicroVMs found.".into());
        if !options.all && options.states.is_empty() {
            lines.push("Terminated MicroVMs are hidden; pass --all to include them.".into());
        }
        return lines.join("\n");
    }
    let rows: Vec<[String; 5]> = result.microvms.iter().map(row).collect();
    lines.push(String::new());
    lines.push(table(
        &["VM ID", "STATE", "IMAGE", "VERSION", "STARTED"],
        &rows,
    ));
    lines.join("\n")
}

fn row(microvm: &MicroVmSummary) -> [String; 5] {
    [
        microvm.microvm_id.clone(),
        microvm.state.to_string(),
        microvm.image_arn.clone(),
        microvm.image_version.clone(),
        microvm.started_at.to_string(),
    ]
}

/// A left-aligned table that never pads its last column.
fn table(headers: &[&str], rows: &[[String; 5]]) -> String {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let header: Vec<String> = headers.iter().map(|header| (*header).to_owned()).collect();
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format_row(&header, &widths));
    for row in rows {
        lines.push(format_row(row, &widths));
    }
    lines.join("\n")
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    let padded: Vec<String> = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!("{cell:<width$}", width = *width))
        .collect();
    padded.join("  ").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Call, FakeMicroVmClient, MicroVmPage};
    use crate::test_support::{MicroVmSummaryBuilder, project};
    use tempfile::TempDir;

    fn config() -> (TempDir, ProjectConfig) {
        let directory = TempDir::new().unwrap();
        let config = project(directory.path(), "");
        (directory, config)
    }

    fn page(
        microvms: impl IntoIterator<Item = MicroVmSummary>,
        next_token: Option<&str>,
    ) -> MicroVmPage {
        (
            microvms.into_iter().collect(),
            next_token.map(str::to_owned),
        )
    }

    fn listed(calls: &[Call]) -> Vec<(Option<ImageIdentifier>, Option<String>, Option<String>)> {
        calls
            .iter()
            .map(|call| {
                let Call::ListMicroVms {
                    image,
                    version,
                    next_token,
                } = call
                else {
                    panic!("expected a listing call, got {call:?}");
                };
                (image.clone(), version.clone(), next_token.clone())
            })
            .collect()
    }

    #[tokio::test]
    async fn every_page_is_traversed_including_empty_ones() {
        let client = FakeMicroVmClient::default().listed([
            Ok(page(
                [MicroVmSummaryBuilder::new("microvm-1").build()],
                Some("token-1"),
            )),
            Ok(page([], Some("token-2"))),
            Ok(page(
                [MicroVmSummaryBuilder::new("microvm-2")
                    .started_at(100)
                    .build()],
                None,
            )),
        ]);
        let (_directory, config) = config();

        let result = list(&ListOptions::default(), &config, &client)
            .await
            .unwrap();

        let ids: Vec<&str> = result
            .microvms
            .iter()
            .map(|microvm| microvm.microvm_id.as_str())
            .collect();
        assert_eq!(ids, ["microvm-2", "microvm-1"]);
        let expected = vec![
            (None, None, None),
            (None, None, Some("token-1".to_owned())),
            (None, None, Some("token-2".to_owned())),
        ];
        assert_eq!(listed(&client.calls()), expected);
    }

    #[tokio::test]
    async fn terminated_microvms_are_hidden_until_requested() {
        let fake = || {
            FakeMicroVmClient::default().listed([Ok(page(
                [
                    MicroVmSummaryBuilder::new("microvm-live").build(),
                    MicroVmSummaryBuilder::new("microvm-dead")
                        .state(MicrovmState::Terminated)
                        .build(),
                ],
                None,
            ))])
        };
        let (_directory, config) = config();

        let client = fake();
        let default = list(&ListOptions::default(), &config, &client)
            .await
            .unwrap();
        assert_eq!(default.microvms.len(), 1);
        assert_eq!(default.microvms[0].microvm_id, "microvm-live");

        let client = fake();
        let all = list(
            &ListOptions {
                all: true,
                ..ListOptions::default()
            },
            &config,
            &client,
        )
        .await
        .unwrap();
        assert_eq!(all.microvms.len(), 2);

        let client = fake();
        let terminated = list(
            &ListOptions {
                states: vec!["TERMINATED".to_owned()],
                ..ListOptions::default()
            },
            &config,
            &client,
        )
        .await
        .unwrap();
        assert_eq!(terminated.microvms.len(), 1);
        assert_eq!(terminated.microvms[0].microvm_id, "microvm-dead");
    }

    #[tokio::test]
    async fn unknown_reported_states_stay_visible() {
        let client = FakeMicroVmClient::default().listed([Ok(page(
            [MicroVmSummaryBuilder::new("microvm-new")
                .state(MicrovmState::from("HIBERNATING"))
                .build()],
            None,
        ))]);
        let (_directory, config) = config();

        let result = list(&ListOptions::default(), &config, &client)
            .await
            .unwrap();

        assert_eq!(result.microvms[0].state.as_str(), "HIBERNATING");
    }

    #[tokio::test]
    async fn microvms_are_sorted_by_start_time_then_id() {
        let client = FakeMicroVmClient::default().listed([Ok(page(
            [
                MicroVmSummaryBuilder::new("microvm-b")
                    .started_at(100)
                    .build(),
                MicroVmSummaryBuilder::new("microvm-c")
                    .started_at(200)
                    .build(),
                MicroVmSummaryBuilder::new("microvm-a")
                    .started_at(100)
                    .build(),
            ],
            None,
        ))]);
        let (_directory, config) = config();

        let result = list(&ListOptions::default(), &config, &client)
            .await
            .unwrap();

        let ids: Vec<&str> = result
            .microvms
            .iter()
            .map(|microvm| microvm.microvm_id.as_str())
            .collect();
        assert_eq!(ids, ["microvm-c", "microvm-a", "microvm-b"]);
    }

    #[tokio::test]
    async fn repeated_microvm_ids_are_reported_once() {
        let client = FakeMicroVmClient::default().listed([Ok(page(
            [
                MicroVmSummaryBuilder::new("microvm-1").build(),
                MicroVmSummaryBuilder::new("microvm-1")
                    .started_at(100)
                    .build(),
            ],
            None,
        ))]);
        let (_directory, config) = config();

        let result = list(&ListOptions::default(), &config, &client)
            .await
            .unwrap();

        assert_eq!(result.microvms.len(), 1);
        assert_eq!(result.microvms[0].started_at.secs(), 100);
    }

    #[tokio::test]
    async fn the_image_filters_reach_the_client_and_pages_are_reported_verbatim() {
        let client = FakeMicroVmClient::default().listed([Ok(page(
            [MicroVmSummaryBuilder::new("microvm-1")
                .image_arn("arn:aws:lambda:us-east-1:123456789012:microvm-image:team-image")
                .image_version("7")
                .build()],
            None,
        ))]);
        let (_directory, config) = config();
        let options = ListOptions {
            image: Some("team-image".into()),
            image_version: Some("7".into()),
            ..ListOptions::default()
        };

        let result = list(&options, &config, &client).await.unwrap();

        assert_eq!(
            listed(&client.calls()),
            [(
                Some(ImageIdentifier::Name("team-image".into())),
                Some("7".into()),
                None
            )]
        );
        assert_eq!(result.microvms[0].image_version, "7");
        assert_eq!(
            result.microvms[0].image_arn,
            "arn:aws:lambda:us-east-1:123456789012:microvm-image:team-image"
        );
    }

    #[tokio::test]
    async fn an_image_arn_from_another_region_is_rejected_before_listing() {
        let client = FakeMicroVmClient::default();
        let (_directory, config) = config();
        let options = ListOptions {
            image: Some("arn:aws:lambda:eu-west-1:123456789012:microvm-image:demo".into()),
            ..ListOptions::default()
        };

        let error = list(&options, &config, &client).await.unwrap_err();

        assert!(
            error.to_string().contains("is not in region `us-east-1`"),
            "{error}"
        );
        assert!(client.calls().is_empty());
    }

    #[tokio::test]
    async fn unsupported_state_filters_are_rejected_before_listing() {
        for state in ["FROZEN", ""] {
            let client = FakeMicroVmClient::default();
            let (_directory, config) = config();
            let options = ListOptions {
                states: vec![state.into()],
                ..ListOptions::default()
            };

            let error = list(&options, &config, &client).await.unwrap_err();

            assert!(
                matches!(error, ClankerError::InvalidConfig(_)),
                "accepted {state:?}"
            );
            assert!(client.calls().is_empty());
        }
    }

    #[tokio::test]
    async fn a_later_page_failure_fails_the_listing() {
        let client = FakeMicroVmClient::default().listed([
            Ok(page(
                [MicroVmSummaryBuilder::new("microvm-1").build()],
                Some("token-1"),
            )),
            Err(MicroVmClientError::Service {
                operation: "list MicroVMs",
                message: "denied".into(),
            }),
        ]);
        let (_directory, config) = config();

        let error = list(&ListOptions::default(), &config, &client)
            .await
            .unwrap_err();

        assert!(matches!(error, ClankerError::MicroVmClient(_)), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn listing_stops_at_the_deadline() {
        let client = FakeMicroVmClient::default()
            .listed([
                Ok(page([], Some("token-1"))),
                Ok(page([], Some("token-2"))),
                Ok(page([], Some("token-3"))),
            ])
            .with_delay(Duration::from_secs(10));
        let (_directory, config) = config();
        let options = ListOptions {
            timeout: Duration::from_secs(25),
            ..ListOptions::default()
        };

        let error = list(&options, &config, &client).await.unwrap_err();

        assert!(matches!(error, ClankerError::ListTimeout { .. }), "{error}");
    }

    #[test]
    fn human_output_names_the_region_and_the_credential_scope() {
        let directory = TempDir::new().unwrap();
        let mut config = project(directory.path(), "");
        config.aws.profile = Some("Production-PowerUser".into());
        let empty = ListResult {
            microvms: Vec::new(),
        };

        let rendered = human(&empty, &config, &ListOptions::default());
        assert!(rendered.contains("Region:  us-east-1"), "{rendered}");
        assert!(
            rendered.contains("Profile: Production-PowerUser"),
            "{rendered}"
        );
        assert!(rendered.contains("No MicroVMs found."), "{rendered}");
        assert!(
            rendered.contains("pass --all to include them"),
            "{rendered}"
        );

        let result = ListResult {
            microvms: vec![MicroVmSummaryBuilder::new("microvm-1").build()],
        };
        let rendered = human(&result, &config, &ListOptions::default());
        let last = rendered.lines().last().unwrap();
        assert!(last.starts_with("microvm-1"), "{rendered}");
        assert!(last.ends_with("1970-01-01T00:00:00Z"), "{rendered}");
    }

    #[test]
    fn the_table_pads_columns_without_trailing_space() {
        let rows = [
            row(&MicroVmSummaryBuilder::new("microvm-1").build()),
            row(&MicroVmSummaryBuilder::new("a-longer-microvm-id").build()),
        ];

        let rendered = table(&["VM ID", "STATE", "IMAGE", "VERSION", "STARTED"], &rows);

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        let column = |line: &str, value: &str| line.find(value).expect("column value");
        assert_eq!(column(lines[1], "RUNNING"), column(lines[2], "RUNNING"));
        assert_eq!(
            column(lines[0], "STARTED"),
            column(lines[1], "1970-01-01T00:00:00Z")
        );
        assert!(lines[2].ends_with("1970-01-01T00:00:00Z"));
    }
}
