use super::error::MicroVmClientError;
use super::microvm_client::{
    InspectImageRequest, MicroVmClient, ObservedImageRelease, PruneImageVersionsRequest,
    PublishImageRequest, PublishedImage, RunMicroVmRequest, RunMicroVmResponse,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicroVmCall {
    PublishImage(PublishImageRequest),
    InspectImage(InspectImageRequest),
    PruneImageVersions(PruneImageVersionsRequest),
    RunMicroVm(RunMicroVmRequest),
}

#[derive(Clone, Default)]
pub struct FakeMicroVmClient {
    state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    publish_responses: VecDeque<Result<PublishedImage, MicroVmClientError>>,
    inspection_responses: VecDeque<Result<Option<ObservedImageRelease>, MicroVmClientError>>,
    prune_responses: VecDeque<Result<(), MicroVmClientError>>,
    run_responses: VecDeque<Result<RunMicroVmResponse, MicroVmClientError>>,
    calls: Vec<MicroVmCall>,
}

impl FakeMicroVmClient {
    pub fn builder() -> FakeMicroVmClientBuilder {
        FakeMicroVmClientBuilder::default()
    }

    pub fn calls(&self) -> Vec<MicroVmCall> {
        self.state
            .lock()
            .expect("fake mutex poisoned")
            .calls
            .clone()
    }
}

#[derive(Default)]
pub struct FakeMicroVmClientBuilder {
    state: FakeState,
}

impl FakeMicroVmClientBuilder {
    pub fn publish_responses(
        mut self,
        responses: impl IntoIterator<Item = Result<PublishedImage, MicroVmClientError>>,
    ) -> Self {
        self.state.publish_responses = responses.into_iter().collect();
        self
    }

    pub fn inspection_responses(
        mut self,
        responses: impl IntoIterator<Item = Result<Option<ObservedImageRelease>, MicroVmClientError>>,
    ) -> Self {
        self.state.inspection_responses = responses.into_iter().collect();
        self
    }

    pub fn prune_responses(
        mut self,
        responses: impl IntoIterator<Item = Result<(), MicroVmClientError>>,
    ) -> Self {
        self.state.prune_responses = responses.into_iter().collect();
        self
    }

    pub fn run_responses(
        mut self,
        responses: impl IntoIterator<Item = Result<RunMicroVmResponse, MicroVmClientError>>,
    ) -> Self {
        self.state.run_responses = responses.into_iter().collect();
        self
    }

    pub fn build(self) -> FakeMicroVmClient {
        FakeMicroVmClient {
            state: Arc::new(Mutex::new(self.state)),
        }
    }
}

impl MicroVmClient for FakeMicroVmClient {
    async fn publish_image(
        &self,
        request: PublishImageRequest,
    ) -> Result<PublishedImage, MicroVmClientError> {
        let default = PublishedImage {
            image_version: "1".into(),
            artifact_uri: format!(
                "s3://{}/clankervm/{}/bundles/{}.zip",
                request.artifact_bucket, request.name, request.bundle_digest
            ),
        };
        let mut state = self.state.lock().expect("fake mutex poisoned");
        state.calls.push(MicroVmCall::PublishImage(request));
        state.publish_responses.pop_front().unwrap_or(Ok(default))
    }

    async fn inspect_image(
        &self,
        request: InspectImageRequest,
    ) -> Result<Option<ObservedImageRelease>, MicroVmClientError> {
        let mut state = self.state.lock().expect("fake mutex poisoned");
        state.calls.push(MicroVmCall::InspectImage(request));
        state.inspection_responses.pop_front().unwrap_or(Ok(None))
    }

    async fn prune_image_versions(
        &self,
        request: PruneImageVersionsRequest,
    ) -> Result<(), MicroVmClientError> {
        let mut state = self.state.lock().expect("fake mutex poisoned");
        state.calls.push(MicroVmCall::PruneImageVersions(request));
        state.prune_responses.pop_front().unwrap_or(Ok(()))
    }

    async fn run_microvm(
        &self,
        request: RunMicroVmRequest,
    ) -> Result<RunMicroVmResponse, MicroVmClientError> {
        let mut state = self.state.lock().expect("fake mutex poisoned");
        state.calls.push(MicroVmCall::RunMicroVm(request));
        state.run_responses.pop_front().unwrap_or_else(|| {
            Ok(RunMicroVmResponse {
                microvm_id: "microvm-fake".into(),
                image_version: "1".into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::InspectImageRequest;
    use crate::test_support::ObservedImageReleaseBuilder;

    #[tokio::test]
    async fn fake_scripts_inspections_and_records_high_level_calls() {
        let observed = ObservedImageReleaseBuilder::active("2");
        let client = FakeMicroVmClient::builder()
            .inspection_responses([Ok(None), Ok(Some(observed.clone()))])
            .build();
        let request = InspectImageRequest {
            image_identifier: crate::Arn::parse(
                "arn:aws:lambda:region:account:microvm-image:image",
            )
            .unwrap(),
            image_version: Some("2".into()),
        };

        assert_eq!(client.inspect_image(request.clone()).await.unwrap(), None);
        assert_eq!(
            client.inspect_image(request.clone()).await.unwrap(),
            Some(observed)
        );
        assert_eq!(
            client.calls(),
            vec![
                MicroVmCall::InspectImage(request.clone()),
                MicroVmCall::InspectImage(request),
            ]
        );
    }
}
