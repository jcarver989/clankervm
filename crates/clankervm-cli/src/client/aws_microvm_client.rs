use super::error::MicroVmClientError;
use super::microvm_client::{
    ImageIdentifier, ImageSpec, Launch, LaunchSpec, LogEvent, LogPage, LogQuery, MicroVmClient,
    MicroVmDetails, MicroVmSummary, Observation, Published, ShellToken,
};
use crate::arn::Arn;
use crate::artifact::Artifact;
use aws_config::SdkConfig;
use aws_sdk_cloudwatchlogs::types::OutputLogEvent;
use aws_sdk_lambdamicrovms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_lambdamicrovms::operation::get_microvm::GetMicrovmError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageOutput;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionOutput;
use aws_sdk_lambdamicrovms::operation::terminate_microvm::TerminateMicrovmError;
use aws_sdk_lambdamicrovms::types::{
    CloudWatchLogging, CodeArtifact, Logging, MicrovmImageVersionState, MicrovmImageVersionStatus,
};
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types::DateTime;
use std::collections::HashMap;
use std::future::Future;

/// MicroVMs requested per listing page.
const LIST_PAGE_SIZE: i32 = 50;
/// How long a shell token stays valid; it only has to outlive the handshake.
const SHELL_TOKEN_MINUTES: i32 = 5;
/// Log streams requested per page; a page is all the `logs` hint needs.
const STREAMS_PAGE_SIZE: i32 = 50;
/// Events one read may ask for.
const EVENTS_PAGE_SIZE: usize = 10_000;
/// Image versions requested per page while pruning.
const VERSIONS_PAGE_SIZE: i32 = 50;

#[derive(Clone)]
pub(crate) struct AwsMicroVmClient {
    logs: aws_sdk_cloudwatchlogs::Client,
    microvms: aws_sdk_lambdamicrovms::Client,
    s3: aws_sdk_s3::Client,
}

impl AwsMicroVmClient {
    pub(crate) fn new(sdk: &SdkConfig) -> Self {
        Self {
            logs: aws_sdk_cloudwatchlogs::Client::new(sdk),
            microvms: aws_sdk_lambdamicrovms::Client::new(sdk),
            s3: aws_sdk_s3::Client::new(sdk),
        }
    }

    async fn upload(
        &self,
        spec: &ImageSpec,
        bundle: &Artifact,
    ) -> Result<String, MicroVmClientError> {
        let key = spec.artifact_key(&bundle.digest);
        self.s3
            .put_object()
            .bucket(&spec.bucket)
            .key(&key)
            .body(ByteStream::from(bundle.bytes.clone()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("upload bundle", &error))?;
        Ok(format!("s3://{}/{key}", spec.bucket))
    }

    /// Treats the errors `is_missing` accepts as a missing resource.
    async fn optional<T, E, R>(
        &self,
        operation: &'static str,
        result: impl Future<Output = Result<T, SdkError<E, R>>>,
        is_missing: impl Fn(&E) -> bool,
    ) -> Result<Option<T>, MicroVmClientError>
    where
        E: ProvideErrorMetadata + std::error::Error + 'static,
        R: std::fmt::Debug + 'static,
    {
        match result.await {
            Ok(output) => Ok(Some(output)),
            Err(error) if error.as_service_error().is_some_and(&is_missing) => Ok(None),
            Err(error) => Err(MicroVmClientError::service(operation, &error)),
        }
    }

    async fn get_image(
        &self,
        image: &Arn,
    ) -> Result<Option<GetMicrovmImageOutput>, MicroVmClientError> {
        self.optional(
            "get image",
            self.microvms
                .get_microvm_image()
                .image_identifier(image.as_str())
                .send(),
            GetMicrovmImageError::is_resource_not_found_exception,
        )
        .await
    }

    async fn get_image_version(
        &self,
        image: &Arn,
        version: &str,
    ) -> Result<Option<GetMicrovmImageVersionOutput>, MicroVmClientError> {
        self.optional(
            "get image version",
            self.microvms
                .get_microvm_image_version()
                .image_identifier(image.as_str())
                .image_version(version)
                .send(),
            GetMicrovmImageVersionError::is_resource_not_found_exception,
        )
        .await
    }

    async fn create_image(
        &self,
        spec: &ImageSpec,
        artifact_uri: &str,
    ) -> Result<String, MicroVmClientError> {
        let configuration = &spec.configuration;
        Ok(self
            .microvms
            .create_microvm_image()
            .name(&spec.name)
            .set_tags(Some(tags(spec)))
            .code_artifact(CodeArtifact::Uri(artifact_uri.into()))
            .base_image_arn(configuration.base_image_arn.as_str())
            .build_role_arn(configuration.build_role_arn.as_str())
            .description(&configuration.description)
            .egress_network_connectors(configuration.egress_network_connector.as_str())
            .hooks(configuration.hooks.clone())
            .set_environment_variables(Some(configuration.environment_variables.clone()))
            .set_resources(configuration.resources.clone())
            .set_additional_os_capabilities(Some(configuration.capabilities.clone()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("create image", &error))?
            .image_version)
    }

    async fn update_image(
        &self,
        spec: &ImageSpec,
        artifact_uri: &str,
    ) -> Result<String, MicroVmClientError> {
        let configuration = &spec.configuration;
        let version = self
            .microvms
            .update_microvm_image()
            .image_identifier(spec.arn.as_str())
            .code_artifact(CodeArtifact::Uri(artifact_uri.into()))
            .base_image_arn(configuration.base_image_arn.as_str())
            .build_role_arn(configuration.build_role_arn.as_str())
            .description(&configuration.description)
            .egress_network_connectors(configuration.egress_network_connector.as_str())
            .hooks(configuration.hooks.clone())
            .set_environment_variables(Some(configuration.environment_variables.clone()))
            .set_resources(configuration.resources.clone())
            .set_additional_os_capabilities(Some(configuration.capabilities.clone()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("update image", &error))?
            .image_version;
        if !spec.tags.is_empty() {
            self.microvms
                .tag_resource()
                .resource(spec.arn.as_str())
                .set_tags(Some(tags(spec)))
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("tag image", &error))?;
        }
        Ok(version)
    }
}

impl MicroVmClient for AwsMicroVmClient {
    async fn publish(
        &self,
        spec: &ImageSpec,
        bundle: &Artifact,
    ) -> Result<Published, MicroVmClientError> {
        let artifact_uri = self.upload(spec, bundle).await?;
        let version = if self.get_image(&spec.arn).await?.is_some() {
            self.update_image(spec, &artifact_uri).await?
        } else {
            self.create_image(spec, &artifact_uri).await?
        };
        Ok(Published {
            version,
            artifact_uri,
        })
    }

    async fn observe(
        &self,
        image: &Arn,
        version: Option<&str>,
    ) -> Result<Option<Observation>, MicroVmClientError> {
        let Some(image_output) = self.get_image(image).await? else {
            return Ok(None);
        };
        let image_state = image_output.state().clone();
        let image_version = version
            .map(str::to_owned)
            .or(image_output.latest_active_image_version)
            .or(image_output.latest_failed_image_version);
        let Some(image_version) = image_version else {
            return Ok(None);
        };
        let Some(version_output) = self.get_image_version(image, &image_version).await? else {
            return Ok(None);
        };
        Ok(Some(Observation {
            image_version,
            image_state: Some(image_state),
            version_state: version_output.state().clone(),
            version_status: version_output.status().clone(),
            state_reason: version_output.state_reason,
        }))
    }

    async fn prune(&self, image: &Arn, keep: usize) -> Result<(), MicroVmClientError> {
        if keep == 0 {
            return Err(MicroVmClientError::InvalidVersionsToKeep);
        }
        let mut versions = self
            .microvms
            .list_microvm_image_versions()
            .image_identifier(image.as_str())
            .into_paginator()
            .page_size(VERSIONS_PAGE_SIZE)
            .items()
            .send()
            .try_collect()
            .await
            .map_err(|error| MicroVmClientError::service("list image versions", &error))?;

        versions.retain(|version| {
            !matches!(
                version.state(),
                MicrovmImageVersionState::Deleting | MicrovmImageVersionState::Deleted
            )
        });
        versions.sort_by_key(|version| std::cmp::Reverse(version.created_at().secs()));
        for version in versions.into_iter().skip(keep) {
            if *version.status() == MicrovmImageVersionStatus::Active {
                continue;
            }
            self.microvms
                .delete_microvm_image_version()
                .image_identifier(image.as_str())
                .image_version(version.image_version())
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("delete image version", &error))?;
        }
        Ok(())
    }

    async fn list_microvms(
        &self,
        image: Option<&ImageIdentifier>,
        version: Option<&str>,
        next_token: Option<&str>,
    ) -> Result<(Vec<MicroVmSummary>, Option<String>), MicroVmClientError> {
        let mut builder = self
            .microvms
            .list_microvms()
            .max_results(LIST_PAGE_SIZE)
            .set_next_token(next_token.map(str::to_owned));
        if let Some(image) = image {
            builder = builder.image_identifier(image.as_str());
        }
        if let Some(version) = version {
            builder = builder.image_version(version);
        }
        let page = builder
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("list MicroVMs", &error))?;
        let microvms = page
            .items
            .into_iter()
            .map(|item| MicroVmSummary {
                microvm_id: item.microvm_id,
                state: item.state,
                image_arn: item.image_arn,
                image_version: item.image_version,
                started_at: item.started_at,
            })
            .collect();
        Ok((microvms, page.next_token))
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<Launch, MicroVmClientError> {
        let mut builder = self
            .microvms
            .run_microvm()
            .image_identifier(spec.image_arn.as_str())
            .set_image_version(spec.image_version.clone())
            .execution_role_arn(spec.execution_role_arn.as_str())
            .ingress_network_connectors(spec.ingress_network_connector.as_str())
            .egress_network_connectors(spec.egress_network_connector.as_str())
            .run_hook_payload(&spec.run_hook_payload)
            .maximum_duration_in_seconds(spec.maximum_duration_seconds)
            .set_client_token(spec.client_token.clone());

        if let Some(log_group) = &spec.cloudwatch_log_group {
            builder = builder.logging(Logging::CloudWatch(
                CloudWatchLogging::builder().log_group(log_group).build(),
            ));
        }

        let output = builder
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("run MicroVM", &error))?;
        Ok(Launch {
            microvm_id: output.microvm_id().into(),
            image_version: output.image_version,
        })
    }

    async fn log_streams(&self, group: &str) -> Result<Vec<String>, MicroVmClientError> {
        let page = self
            .logs
            .describe_log_streams()
            .log_group_name(group)
            .limit(STREAMS_PAGE_SIZE)
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("describe log streams", &error))?;
        Ok(page
            .log_streams()
            .iter()
            .filter_map(|stream| stream.log_stream_name().map(str::to_owned))
            .collect())
    }

    async fn log_events(&self, query: &LogQuery) -> Result<LogPage, MicroVmClientError> {
        let mut builder = self
            .logs
            .get_log_events()
            .log_group_name(&query.group)
            .log_stream_name(&query.stream)
            .start_from_head(query.window.is_forward())
            .limit(events_per_page(query.limit))
            .set_next_token(query.next_token.clone());
        if let Some(start_time) = query.window.start_time() {
            builder = builder.start_time(start_time);
        }
        let output = builder
            .send()
            .await
            .map_err(|error| match error.as_service_error() {
                Some(error) if error.is_resource_not_found_exception() => {
                    MicroVmClientError::NoLogStream {
                        group: query.group.clone(),
                        stream: query.stream.clone(),
                    }
                }
                _ => MicroVmClientError::service("get log events", &error),
            })?;
        Ok(LogPage {
            events: output.events().iter().map(log_event).collect(),
            // A backward read ends at the newest event, so only a forward read
            // has a token that continues from it.
            next_token: query
                .window
                .is_forward()
                .then(|| output.next_forward_token().map(str::to_owned))
                .flatten(),
        })
    }

    async fn describe(
        &self,
        microvm_id: &str,
    ) -> Result<Option<MicroVmDetails>, MicroVmClientError> {
        let output = self
            .optional(
                "get MicroVM",
                self.microvms
                    .get_microvm()
                    .microvm_identifier(microvm_id)
                    .send(),
                GetMicrovmError::is_resource_not_found_exception,
            )
            .await?;
        Ok(output.map(|output| MicroVmDetails {
            microvm_id: output.microvm_id,
            state: output.state,
            state_reason: output.state_reason,
            endpoint: output.endpoint,
            ingress_network_connectors: output.ingress_network_connectors.unwrap_or_default(),
        }))
    }

    async fn shell_token(&self, microvm_id: &str) -> Result<ShellToken, MicroVmClientError> {
        let output = self
            .microvms
            .create_microvm_shell_auth_token()
            .microvm_identifier(microvm_id)
            .expiration_in_minutes(SHELL_TOKEN_MINUTES)
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("create shell auth token", &error))?;
        if output.auth_token.is_empty() {
            return Err(MicroVmClientError::Service {
                operation: "create shell auth token",
                message: "AWS returned no token".into(),
            });
        }
        Ok(ShellToken {
            headers: output.auth_token,
        })
    }

    async fn terminate(&self, microvm_id: &str) -> Result<(), MicroVmClientError> {
        self.optional(
            "terminate MicroVM",
            self.microvms
                .terminate_microvm()
                .microvm_identifier(microvm_id)
                .send(),
            TerminateMicrovmError::is_resource_not_found_exception,
        )
        .await
        .map(|_| ())
    }
}

fn tags(spec: &ImageSpec) -> HashMap<String, String> {
    spec.tags.clone().into_iter().collect()
}

/// One event as AWS reports it, with the millisecond timestamp it stores.
fn log_event(event: &OutputLogEvent) -> LogEvent {
    LogEvent {
        timestamp: DateTime::from_millis(event.timestamp().unwrap_or_default()),
        message: event.message().unwrap_or_default().to_owned(),
    }
}

/// Clamps a request size to the maximum the logs API accepts.
fn events_per_page(limit: usize) -> i32 {
    i32::try_from(limit.min(EVENTS_PAGE_SIZE)).unwrap_or(i32::MAX)
}
