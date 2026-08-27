use super::error::MicroVmClientError;
use super::microvm_client::{
    ImageCapability, ImageHooks, ImageState, ImageVersionState, ImageVersionStatus,
    InspectImageRequest, MicroVmClient, ObservedImageRelease, PruneImageVersionsRequest,
    PublishImageRequest, PublishedImage, RunMicroVmRequest, RunMicroVmResponse,
};
use crate::{Arn, Tags};
use aws_config::SdkConfig;
use aws_sdk_lambdamicrovms::operation::get_microvm_image::{
    GetMicrovmImageError, GetMicrovmImageOutput,
};
use aws_sdk_lambdamicrovms::operation::get_microvm_image_version::{
    GetMicrovmImageVersionError, GetMicrovmImageVersionOutput,
};
use aws_sdk_lambdamicrovms::types::{
    CloudWatchLogging, CodeArtifact, HookState, Hooks, Logging, MicrovmHooks, MicrovmImageHooks,
    Resources,
};
use aws_sdk_s3::primitives::ByteStream;

#[derive(Clone)]
pub struct AwsMicroVmClient {
    microvms: aws_sdk_lambdamicrovms::Client,
    s3: aws_sdk_s3::Client,
}

impl AwsMicroVmClient {
    pub fn new(sdk: &SdkConfig) -> Self {
        Self {
            microvms: aws_sdk_lambdamicrovms::Client::new(sdk),
            s3: aws_sdk_s3::Client::new(sdk),
        }
    }

    async fn upload_bundle(
        &self,
        request: &PublishImageRequest,
    ) -> Result<String, MicroVmClientError> {
        let key = format!(
            "clankervm/{}/bundles/{}.zip",
            request.name, request.bundle_digest
        );

        let uri = format!("s3://{}/{key}", request.artifact_bucket);
        let body = ByteStream::from(request.bundle.clone());

        self.s3
            .put_object()
            .bucket(&request.artifact_bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("upload bundle", error))?;

        Ok(uri)
    }

    async fn get_image(
        &self,
        image_identifier: &str,
    ) -> Result<Option<GetMicrovmImageOutput>, MicroVmClientError> {
        match self
            .microvms
            .get_microvm_image()
            .image_identifier(image_identifier)
            .send()
            .await
        {
            Ok(output) => Ok(Some(output)),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(GetMicrovmImageError::is_resource_not_found_exception) =>
            {
                Ok(None)
            }
            Err(error) => Err(MicroVmClientError::service("get image", error)),
        }
    }

    async fn get_image_version(
        &self,
        image_identifier: &str,
        image_version: &str,
    ) -> Result<Option<GetMicrovmImageVersionOutput>, MicroVmClientError> {
        match self
            .microvms
            .get_microvm_image_version()
            .image_identifier(image_identifier)
            .image_version(image_version)
            .send()
            .await
        {
            Ok(output) => Ok(Some(output)),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(GetMicrovmImageVersionError::is_resource_not_found_exception) =>
            {
                Ok(None)
            }
            Err(error) => Err(MicroVmClientError::service("get image version", error)),
        }
    }

    async fn create_image(
        &self,
        request: &PublishImageRequest,
        artifact_uri: &str,
    ) -> Result<String, MicroVmClientError> {
        let configuration = &request.configuration;
        let mut builder = self
            .microvms
            .create_microvm_image()
            .name(&request.name)
            .code_artifact(CodeArtifact::Uri(artifact_uri.into()))
            .base_image_arn(configuration.base_image_arn.as_str())
            .build_role_arn(configuration.build_role_arn.as_str())
            .description(&configuration.description)
            .egress_network_connectors(configuration.egress_network_connector.as_str())
            .hooks(aws_hooks(&configuration.hooks))
            .set_tags(Some(
                request.tags.clone().into_inner().into_iter().collect(),
            ));

        if let Some(memory) = configuration.minimum_memory_mib {
            builder = builder.resources(aws_resources(memory, "create image")?);
        }
        for capability in configuration.capabilities.iter().copied() {
            builder = builder.additional_os_capabilities(aws_capability(capability)?);
        }

        Ok(builder
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("create image", error))?
            .image_version)
    }

    async fn update_image(
        &self,
        request: &PublishImageRequest,
        artifact_uri: &str,
    ) -> Result<String, MicroVmClientError> {
        let configuration = &request.configuration;
        let mut builder = self
            .microvms
            .update_microvm_image()
            .image_identifier(request.image_identifier.as_str())
            .code_artifact(CodeArtifact::Uri(artifact_uri.into()))
            .base_image_arn(configuration.base_image_arn.as_str())
            .build_role_arn(configuration.build_role_arn.as_str())
            .description(&configuration.description)
            .egress_network_connectors(configuration.egress_network_connector.as_str())
            .hooks(aws_hooks(&configuration.hooks));

        if let Some(memory) = configuration.minimum_memory_mib {
            builder = builder.resources(aws_resources(memory, "update image")?);
        }
        for capability in configuration.capabilities.iter().copied() {
            builder = builder.additional_os_capabilities(aws_capability(capability)?);
        }

        Ok(builder
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("update image", error))?
            .image_version)
    }

    async fn tag_image(
        &self,
        image_identifier: &Arn,
        tags: Tags,
    ) -> Result<(), MicroVmClientError> {
        if tags.is_empty() {
            return Ok(());
        }
        self.microvms
            .tag_resource()
            .resource(image_identifier.as_str())
            .set_tags(Some(tags.into_inner().into_iter().collect()))
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("tag image", error))?;
        Ok(())
    }
}

impl MicroVmClient for AwsMicroVmClient {
    async fn publish_image(
        &self,
        request: PublishImageRequest,
    ) -> Result<PublishedImage, MicroVmClientError> {
        let artifact_uri = self.upload_bundle(&request).await?;
        let exists = self
            .get_image(request.image_identifier.as_str())
            .await?
            .is_some();
        let image_version = if exists {
            let image_version = self.update_image(&request, &artifact_uri).await?;
            self.tag_image(&request.image_identifier, request.tags.clone())
                .await?;
            image_version
        } else {
            self.create_image(&request, &artifact_uri).await?
        };
        Ok(PublishedImage {
            image_version,
            artifact_uri,
        })
    }

    async fn inspect_image(
        &self,
        request: InspectImageRequest,
    ) -> Result<Option<ObservedImageRelease>, MicroVmClientError> {
        let Some(image) = self.get_image(request.image_identifier.as_str()).await? else {
            return Ok(None);
        };
        let image_state = ImageState::from_aws(image.state().as_str());
        let image_version = request
            .image_version
            .or(image.latest_active_image_version)
            .or(image.latest_failed_image_version);
        let Some(image_version) = image_version else {
            return Ok(None);
        };
        let Some(version) = self
            .get_image_version(request.image_identifier.as_str(), &image_version)
            .await?
        else {
            return Ok(None);
        };
        let version_state = ImageVersionState::from_aws(version.state().as_str());
        let version_status = ImageVersionStatus::from_aws(version.status().as_str());
        Ok(Some(ObservedImageRelease {
            image_version,
            image_state,
            version_state,
            version_status,
            state_reason: version.state_reason,
        }))
    }

    async fn prune_image_versions(
        &self,
        request: PruneImageVersionsRequest,
    ) -> Result<(), MicroVmClientError> {
        if request.versions_to_keep == 0 {
            return Err(MicroVmClientError::InvalidVersionsToKeep);
        }
        let mut versions = Vec::new();
        let mut token = None;
        loop {
            let output = self
                .microvms
                .list_microvm_image_versions()
                .image_identifier(request.image_identifier.as_str())
                .max_results(50)
                .set_next_token(token)
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("list image versions", error))?;

            versions.extend(output.items);
            token = output.next_token;
            if token.is_none() {
                break;
            }
        }

        versions.retain(|version| !matches!(version.state().as_str(), "DELETING" | "DELETED"));
        versions.sort_by_key(|version| std::cmp::Reverse(version.created_at().secs()));
        for version in versions.into_iter().skip(request.versions_to_keep) {
            if version.status().as_str() == "ACTIVE" {
                continue;
            }
            self.microvms
                .delete_microvm_image_version()
                .image_identifier(request.image_identifier.as_str())
                .image_version(version.image_version())
                .send()
                .await
                .map_err(|error| MicroVmClientError::service("delete image version", error))?;
        }

        Ok(())
    }

    async fn run_microvm(
        &self,
        request: RunMicroVmRequest,
    ) -> Result<RunMicroVmResponse, MicroVmClientError> {
        let mut builder = self
            .microvms
            .run_microvm()
            .image_identifier(request.image_identifier.into_string())
            .set_image_version(request.image_version)
            .execution_role_arn(request.execution_role_arn.as_str())
            .ingress_network_connectors(request.ingress_network_connector.into_string())
            .egress_network_connectors(request.egress_network_connector.into_string())
            .run_hook_payload(request.run_hook_payload)
            .maximum_duration_in_seconds(request.maximum_duration_seconds)
            .set_client_token(request.client_token);

        if let Some(log_group) = request.cloudwatch_log_group {
            builder = builder.logging(Logging::CloudWatch(
                CloudWatchLogging::builder().log_group(log_group).build(),
            ));
        }

        let output = builder
            .send()
            .await
            .map_err(|error| MicroVmClientError::service("run MicroVM", error))?;

        Ok(RunMicroVmResponse {
            microvm_id: output.microvm_id().into(),
            image_version: output.image_version,
        })
    }
}

fn aws_resources(memory: i32, operation: &'static str) -> Result<Resources, MicroVmClientError> {
    Resources::builder()
        .minimum_memory_in_mib(memory)
        .build()
        .map_err(|error| MicroVmClientError::service(operation, error))
}

fn aws_hooks(hooks: &ImageHooks) -> Hooks {
    Hooks::builder()
        .port(hooks.port)
        .microvm_image_hooks(
            MicrovmImageHooks::builder()
                .ready(HookState::Enabled)
                .ready_timeout_in_seconds(hooks.ready_timeout_seconds)
                .build(),
        )
        .microvm_hooks(
            MicrovmHooks::builder()
                .run(HookState::Enabled)
                .run_timeout_in_seconds(hooks.run_timeout_seconds)
                .terminate(HookState::Enabled)
                .terminate_timeout_in_seconds(hooks.terminate_timeout_seconds)
                .build(),
        )
        .build()
}

fn aws_capability(
    capability: ImageCapability,
) -> Result<aws_sdk_lambdamicrovms::types::Capability, MicroVmClientError> {
    aws_sdk_lambdamicrovms::types::Capability::try_parse(capability.as_str())
        .map_err(|error| MicroVmClientError::service("configure image capability", error))
}
