use super::error::MicroVmClientError;
use super::microvm_client::{
    ImageConfiguration, ImageHooks, ImageSpec, Launch, LaunchSpec, MicroVmClient, Observation,
    Published, VERSION_ACTIVE_STATUS, artifact_key,
};
use crate::arn::Arn;
use crate::artifact::Artifact;
use aws_config::SdkConfig;
use aws_sdk_lambdamicrovms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageOutput;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionOutput;
use aws_sdk_lambdamicrovms::types::{
    CloudWatchLogging, CodeArtifact, HookState, Hooks, Logging, MicrovmHooks, MicrovmImageHooks,
    Resources,
};
use aws_sdk_s3::primitives::ByteStream;
use std::collections::HashMap;
use std::future::Future;

/// AWS image version states that are already gone.
const VERSION_TERMINAL_STATES: [&str; 2] = ["DELETING", "DELETED"];

#[derive(Clone)]
pub(crate) struct AwsMicroVmClient {
    microvms: aws_sdk_lambdamicrovms::Client,
    s3: aws_sdk_s3::Client,
}

impl AwsMicroVmClient {
    pub(crate) fn new(sdk: &SdkConfig) -> Self {
        Self {
            microvms: aws_sdk_lambdamicrovms::Client::new(sdk),
            s3: aws_sdk_s3::Client::new(sdk),
        }
    }

    async fn upload(
        &self,
        spec: &ImageSpec,
        bundle: &Artifact,
    ) -> Result<String, MicroVmClientError> {
        let key = artifact_key(&spec.name, &bundle.digest);
        self.s3
            .put_object()
            .bucket(&spec.bucket)
            .key(&key)
            .body(ByteStream::from(bundle.bytes.clone()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("upload bundle", error))?;
        Ok(format!("s3://{}/{key}", spec.bucket))
    }

    /// Treats AWS `ResourceNotFoundException` as a missing resource.
    async fn optional<T, E, R>(
        &self,
        operation: &'static str,
        result: impl Future<Output = Result<T, SdkError<E, R>>>,
    ) -> Result<Option<T>, MicroVmClientError>
    where
        E: ProvideErrorMetadata + std::error::Error + 'static,
        R: std::fmt::Debug + 'static,
    {
        match result.await {
            Ok(output) => Ok(Some(output)),
            Err(error)
                if error.as_service_error().and_then(|error| error.code())
                    == Some("ResourceNotFoundException") =>
            {
                Ok(None)
            }
            Err(error) => Err(MicroVmClientError::service(operation, error)),
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
            .hooks(configuration.hooks.aws())
            .set_resources(resources(configuration)?)
            .set_additional_os_capabilities(Some(configuration.capabilities.clone()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("create image", error))?
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
            .hooks(configuration.hooks.aws())
            .set_resources(resources(configuration)?)
            .set_additional_os_capabilities(Some(configuration.capabilities.clone()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("update image", error))?
            .image_version;
        if !spec.tags.is_empty() {
            self.microvms
                .tag_resource()
                .resource(spec.arn.as_str())
                .set_tags(Some(tags(spec)))
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("tag image", error))?;
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
        let image_state = image_output.state().as_str().to_owned();
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
            image_state,
            version_state: version_output.state().as_str().to_owned(),
            version_status: version_output.status().as_str().to_owned(),
            state_reason: version_output.state_reason,
        }))
    }

    async fn prune(&self, image: &Arn, keep: usize) -> Result<(), MicroVmClientError> {
        if keep == 0 {
            return Err(MicroVmClientError::InvalidVersionsToKeep);
        }
        let mut versions = Vec::new();
        let mut token = None;
        loop {
            let page = self
                .microvms
                .list_microvm_image_versions()
                .image_identifier(image.as_str())
                .max_results(50)
                .set_next_token(token)
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("list image versions", error))?;
            versions.extend(page.items);
            token = page.next_token;
            if token.is_none() {
                break;
            }
        }

        versions.retain(|version| !VERSION_TERMINAL_STATES.contains(&version.state().as_str()));
        versions.sort_by_key(|version| std::cmp::Reverse(version.created_at().secs()));
        for version in versions.into_iter().skip(keep) {
            if version.status().as_str() == VERSION_ACTIVE_STATUS {
                continue;
            }
            self.microvms
                .delete_microvm_image_version()
                .image_identifier(image.as_str())
                .image_version(version.image_version())
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("delete image version", error))?;
        }
        Ok(())
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
            .map_err(|error| MicroVmClientError::service("run MicroVM", error))?;
        Ok(Launch {
            microvm_id: output.microvm_id().into(),
            image_version: output.image_version,
        })
    }
}

impl ImageHooks {
    /// The hook configuration AWS accepts on both create and update.
    fn aws(&self) -> Hooks {
        Hooks::builder()
            .port(self.port)
            .microvm_image_hooks(
                MicrovmImageHooks::builder()
                    .ready(HookState::Enabled)
                    .ready_timeout_in_seconds(self.ready_timeout_seconds)
                    .build(),
            )
            .microvm_hooks(
                MicrovmHooks::builder()
                    .run(HookState::Enabled)
                    .run_timeout_in_seconds(self.run_timeout_seconds)
                    .terminate(HookState::Enabled)
                    .terminate_timeout_in_seconds(self.terminate_timeout_seconds)
                    .build(),
            )
            .build()
    }
}

fn tags(spec: &ImageSpec) -> HashMap<String, String> {
    spec.tags.clone().into_iter().collect()
}

fn resources(
    configuration: &ImageConfiguration,
) -> Result<Option<Vec<Resources>>, MicroVmClientError> {
    configuration
        .minimum_memory_mib
        .map(|memory| Resources::builder().minimum_memory_in_mib(memory).build())
        .transpose()
        .map(|resources| resources.map(|resources| vec![resources]))
        .map_err(|error| MicroVmClientError::service("configure image resources", error))
}
