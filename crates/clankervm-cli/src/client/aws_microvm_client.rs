use super::error::MicroVmClientError;
use super::microvm_client::{
    ImageIdentifier, ImageSpec, Launch, LaunchSpec, MicroVmClient, MicroVmSummary, Observation,
    Published, artifact_key,
};
use crate::arn::Arn;
use crate::artifact::Artifact;
use aws_config::SdkConfig;
use aws_sdk_lambdamicrovms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image::GetMicrovmImageOutput;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionError;
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::GetMicrovmImageVersionOutput;
use aws_sdk_lambdamicrovms::types::{
    CloudWatchLogging, CodeArtifact, Logging, MicrovmImageVersionState, MicrovmImageVersionStatus,
};
use aws_sdk_s3::primitives::ByteStream;
use std::collections::HashMap;
use std::future::Future;

/// MicroVMs requested per listing page.
const LIST_PAGE_SIZE: i32 = 50;
/// Image versions requested per page while pruning.
const VERSIONS_PAGE_SIZE: i32 = 50;

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
}

fn tags(spec: &ImageSpec) -> HashMap<String, String> {
    spec.tags.clone().into_iter().collect()
}
