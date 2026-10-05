use super::error::MicroVmClientError;
use super::microvm_client::{
    AuthToken, AuthTokenExpiration, ImageIdentifier, ImageSpec, Launch, LaunchSpec, LogEvent,
    LogPage, LogQuery, MicroVmClient, MicroVmDetails, MicroVmSummary, Observation, PruneReport,
    Published, ShellToken,
};
use crate::arn::Arn;
use crate::artifact::Artifact;
use crate::util::POLL_INTERVAL;
use aws_config::SdkConfig;
use aws_sdk_cloudwatchlogs::types::OutputLogEvent;
use aws_sdk_lambdamicrovms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_lambdamicrovms::operation::get_microvm::GetMicrovmError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageOutput;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionOutput;
use aws_sdk_lambdamicrovms::operation::list_microvm_image_versions::ListMicrovmImageVersionsError;
use aws_sdk_lambdamicrovms::operation::terminate_microvm::TerminateMicrovmError;
use aws_sdk_lambdamicrovms::types::{
    BuildState, CloudWatchLogging, CodeArtifact, Logging, MicrovmImageVersionState,
    MicrovmImageVersionStatus, MicrovmState, PortSpecification,
};
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types::DateTime;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// MicroVMs requested per listing page.
const LIST_PAGE_SIZE: i32 = 50;
/// How long a shell token stays valid; it only has to outlive the handshake.
const SHELL_TOKEN_MINUTES: i32 = 5;
/// Log streams requested per discovery page.
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
    /// How long an image change keeps retrying while AWS reports the image busy.
    image_busy_timeout: Duration,
}

impl AwsMicroVmClient {
    pub(crate) fn new(sdk: &SdkConfig, image_busy_timeout: Duration) -> Self {
        Self {
            logs: aws_sdk_cloudwatchlogs::Client::new(sdk),
            microvms: aws_sdk_lambdamicrovms::Client::new(sdk),
            s3: aws_sdk_s3::Client::new(sdk),
            image_busy_timeout,
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
            Err(error) => Err(MicroVmClientError::from_aws_error(operation, &error)),
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

    /// Why a version's builds failed, for failures that report no reason on
    /// the version itself. Best effort: an unreadable build record must not
    /// hide that the build failed.
    async fn build_failure_reason(&self, image: &Arn, version: &str) -> Option<String> {
        let builds: Vec<_> = self
            .microvms
            .list_microvm_image_builds()
            .image_identifier(image.as_str())
            .image_version(version)
            .into_paginator()
            .items()
            .send()
            .try_collect()
            .await
            .ok()?;
        let mut reasons: Vec<String> = builds
            .into_iter()
            .filter(|build| build.build_state == BuildState::Failed)
            .filter_map(|build| build.state_reason)
            .collect();
        reasons.sort();
        reasons.dedup();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }

    async fn is_image_version_in_use(
        &self,
        image: &Arn,
        version: &str,
    ) -> Result<bool, MicroVmClientError> {
        let mut microvms = self
            .microvms
            .list_microvms()
            .image_identifier(image.as_str())
            .image_version(version)
            .into_paginator()
            .page_size(LIST_PAGE_SIZE)
            .items()
            .send();

        while let Some(vm) = microvms
            .try_next()
            .await
            .map_err(|error| MicroVmClientError::service("list MicroVMs before pruning", &error))?
        {
            if *vm.state() != MicrovmState::Terminated {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn deactivate_image_version(
        &self,
        image: &Arn,
        version: &str,
    ) -> Result<(), MicroVmClientError> {
        let output = retry_while_busy("deactivate image version", self.image_busy_timeout, || {
            self.microvms
                .update_microvm_image_version()
                .image_identifier(image.as_str())
                .image_version(version)
                .status(MicrovmImageVersionStatus::Inactive)
                .send()
        })
        .await?;
        if *output.status() != MicrovmImageVersionStatus::Inactive {
            return Err(MicroVmClientError::Service {
                operation: "deactivate image version",
                message: format!("version {version} is not INACTIVE; refusing to delete it"),
            });
        }
        Ok(())
    }

    async fn delete_image_version(
        &self,
        image: &Arn,
        version: &str,
    ) -> Result<(), MicroVmClientError> {
        retry_while_busy("delete image version", self.image_busy_timeout, || {
            self.microvms
                .delete_microvm_image_version()
                .image_identifier(image.as_str())
                .image_version(version)
                .send()
        })
        .await
        .map(drop)
    }

    async fn update_image(
        &self,
        spec: &ImageSpec,
        artifact_uri: &str,
    ) -> Result<String, MicroVmClientError> {
        let configuration = &spec.configuration;
        let version = retry_while_busy("update image", self.image_busy_timeout, || {
            self.microvms
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
        })
        .await?
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
        let version_state = version_output.state().clone();
        let version_status = version_output.status().clone();
        let state_reason = match version_output.state_reason {
            None if version_state == MicrovmImageVersionState::Failed => {
                self.build_failure_reason(image, &image_version).await
            }
            reason => reason,
        };
        Ok(Some(Observation {
            image_version,
            image_state: Some(image_state),
            version_state,
            version_status,
            state_reason,
        }))
    }

    async fn prune(
        &self,
        image: &Arn,
        keep: usize,
    ) -> Result<Option<PruneReport>, MicroVmClientError> {
        let listed = self
            .microvms
            .list_microvm_image_versions()
            .image_identifier(image.as_str())
            .into_paginator()
            .page_size(VERSIONS_PAGE_SIZE)
            .items()
            .send()
            .try_collect()
            .await;
        let mut versions = match listed {
            Ok(versions) => versions,
            // A first push prunes before its image exists; callers decide
            // whether a missing image is an error.
            Err(error)
                if error.as_service_error().is_some_and(
                    ListMicrovmImageVersionsError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(MicroVmClientError::service("list image versions", &error));
            }
        };

        versions.retain(|version| {
            matches!(
                (version.status(), version.state()),
                (
                    MicrovmImageVersionStatus::Active | MicrovmImageVersionStatus::Inactive,
                    MicrovmImageVersionState::Successful
                        | MicrovmImageVersionState::Failed
                        | MicrovmImageVersionState::DeleteFailed
                )
            )
        });
        versions.sort_by_key(|version| Reverse(*version.created_at()));
        let (launchable, rest): (Vec<_>, Vec<_>) = versions.into_iter().partition(|version| {
            *version.state() == MicrovmImageVersionState::Successful
                && *version.status() == MicrovmImageVersionStatus::Active
        });

        let mut report = PruneReport {
            kept: launchable
                .iter()
                .take(keep)
                .map(|version| version.image_version().to_owned())
                .collect(),
            ..PruneReport::default()
        };
        for version in launchable.into_iter().skip(keep).chain(rest) {
            let name = version.image_version();
            let mut in_use = self.is_image_version_in_use(image, name).await?;

            if !in_use && *version.status() == MicrovmImageVersionStatus::Active {
                self.deactivate_image_version(image, name).await?;
                sleep(POLL_INTERVAL).await;
                in_use = self.is_image_version_in_use(image, name).await?;
                if in_use {
                    report.deactivated.push(name.to_owned());
                }
            }

            if in_use {
                report.in_use.push(name.to_owned());
                continue;
            }

            self.delete_image_version(image, name).await?;
            report.deleted.push(name.to_owned());
        }
        Ok(Some(report))
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
            .map_err(|error| MicroVmClientError::from_aws_error("run MicroVM", &error))?;
        Ok(Launch {
            microvm_id: output.microvm_id().into(),
            image_version: output.image_version,
        })
    }

    async fn log_streams(&self, group: &str) -> Result<Vec<String>, MicroVmClientError> {
        let mut streams = Vec::new();
        let mut next_token = None;
        loop {
            let page = self
                .logs
                .describe_log_streams()
                .log_group_name(group)
                .limit(STREAMS_PAGE_SIZE)
                .set_next_token(next_token)
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("describe log streams", &error))?;

            streams.extend(
                page.log_streams()
                    .iter()
                    .filter_map(|stream| stream.log_stream_name().map(str::to_owned)),
            );

            next_token = page.next_token().map(str::to_owned);

            if next_token.is_none() {
                return Ok(streams);
            }
        }
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

    async fn get_details(
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

    async fn create_auth_token(
        &self,
        microvm_id: &str,
        port: u16,
        expiration: AuthTokenExpiration,
    ) -> Result<AuthToken, MicroVmClientError> {
        let output = self
            .microvms
            .create_microvm_auth_token()
            .microvm_identifier(microvm_id)
            .allowed_ports(PortSpecification::Port(i32::from(port)))
            .expiration_in_minutes(expiration.minutes())
            .send()
            .await
            .map_err(|error| {
                MicroVmClientError::from_aws_error("create application auth token", &error)
            })?;

        let mut values = output
            .auth_token
            .into_iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("X-aws-proxy-auth"));

        let (_, value) = values.next().ok_or(MicroVmClientError::ApplicationToken)?;

        if values.next().is_some() {
            return Err(MicroVmClientError::ApplicationToken);
        }

        AuthToken::new(value)
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

    async fn suspend(&self, microvm_id: &str) -> Result<(), MicroVmClientError> {
        self.microvms
            .suspend_microvm()
            .microvm_identifier(microvm_id)
            .send()
            .await
            .map_err(|error| MicroVmClientError::from_aws_error("suspend MicroVM", &error))?;
        Ok(())
    }

    async fn resume(&self, microvm_id: &str) -> Result<(), MicroVmClientError> {
        self.microvms
            .resume_microvm()
            .microvm_identifier(microvm_id)
            .send()
            .await
            .map_err(|error| MicroVmClientError::from_aws_error("resume MicroVM", &error))?;
        Ok(())
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

/// Retries `send` while AWS rejects it because the image is still settling
/// from an earlier change, such as a version deletion.
async fn retry_while_busy<T, E, R, F, Fut>(
    operation: &'static str,
    timeout: Duration,
    mut send: F,
) -> Result<T, MicroVmClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SdkError<E, R>>>,
    E: ProvideErrorMetadata + std::error::Error,
{
    let deadline = Instant::now() + timeout;
    loop {
        match send().await {
            Ok(output) => return Ok(output),
            Err(error) if is_image_busy(&error) && Instant::now() < deadline => {
                sleep(POLL_INTERVAL).await;
            }
            Err(error) => return Err(MicroVmClientError::service(operation, &error)),
        }
    }
}

/// AWS words a busy image two ways: a conflict such as `MicroVM Image is
/// already in state: UPDATING`, and a validation error `Cannot update MicroVM
/// Image in its current state`.
fn is_image_busy<E: ProvideErrorMetadata, R>(error: &SdkError<E, R>) -> bool {
    let message = error.message().unwrap_or_default().to_ascii_lowercase();
    match error.code() {
        Some("ConflictException") => {
            message.contains("updating") || message.contains("already in state")
        }
        Some("ValidationException") => message.contains("in its current state"),
        _ => false,
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
