use super::error::MicroVmClientError;
use super::microvm_client::{
    ImageIdentifier, ImageSpec, Launch, LaunchSpec, MicroVmClient, MicroVmPage, Observation,
    Published, artifact_key,
};
use crate::arn::Arn;
use crate::artifact::Artifact;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// One call recorded by [`FakeMicroVmClient`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Call {
    Publish(Box<ImageSpec>),
    Observe(Arn, Option<String>),
    Prune(Arn, usize),
    ListMicroVms {
        image: Option<ImageIdentifier>,
        version: Option<String>,
        next_token: Option<String>,
    },
    Launch(LaunchSpec),
}

/// An in-memory [`MicroVmClient`] that answers scripted responses in order,
/// falling back to a successful default, and records every call.
#[derive(Clone, Default)]
pub(crate) struct FakeMicroVmClient {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    published: VecDeque<Result<Published, MicroVmClientError>>,
    observed: VecDeque<Result<Option<Observation>, MicroVmClientError>>,
    pruned: VecDeque<Result<(), MicroVmClientError>>,
    listed: VecDeque<Result<MicroVmPage, MicroVmClientError>>,
    launched: VecDeque<Result<Launch, MicroVmClientError>>,
    delay: Option<Duration>,
    calls: Vec<Call>,
}

impl FakeMicroVmClient {
    pub(crate) fn published(
        self,
        responses: impl IntoIterator<Item = Result<Published, MicroVmClientError>>,
    ) -> Self {
        self.lock().published = responses.into_iter().collect();
        self
    }

    pub(crate) fn observed(
        self,
        responses: impl IntoIterator<Item = Result<Option<Observation>, MicroVmClientError>>,
    ) -> Self {
        self.lock().observed = responses.into_iter().collect();
        self
    }

    pub(crate) fn pruned(
        self,
        responses: impl IntoIterator<Item = Result<(), MicroVmClientError>>,
    ) -> Self {
        self.lock().pruned = responses.into_iter().collect();
        self
    }

    pub(crate) fn listed(
        self,
        responses: impl IntoIterator<Item = Result<MicroVmPage, MicroVmClientError>>,
    ) -> Self {
        self.lock().listed = responses.into_iter().collect();
        self
    }

    pub(crate) fn launched(
        self,
        responses: impl IntoIterator<Item = Result<Launch, MicroVmClientError>>,
    ) -> Self {
        self.lock().launched = responses.into_iter().collect();
        self
    }

    /// Makes every call take `delay` to answer, so deadlines can be tested.
    pub(crate) fn with_delay(self, delay: Duration) -> Self {
        self.lock().delay = Some(delay);
        self
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    async fn answer<T>(
        &self,
        call: Call,
        scripted: impl FnOnce(&mut State) -> Option<T>,
        default: impl FnOnce() -> T,
    ) -> T {
        let (answer, delay) = {
            let mut state = self.lock();
            state.calls.push(call);
            let delay = state.delay;
            (scripted(&mut state).unwrap_or_else(default), delay)
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        answer
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("fake mutex poisoned")
    }
}

impl MicroVmClient for FakeMicroVmClient {
    async fn publish(
        &self,
        spec: &ImageSpec,
        bundle: &Artifact,
    ) -> Result<Published, MicroVmClientError> {
        self.answer(
            Call::Publish(Box::new(spec.clone())),
            |state| state.published.pop_front(),
            || {
                Ok(Published {
                    version: "1".into(),
                    artifact_uri: format!(
                        "s3://{}/{}",
                        spec.bucket,
                        artifact_key(&spec.name, &bundle.digest)
                    ),
                })
            },
        )
        .await
    }

    async fn observe(
        &self,
        image: &Arn,
        version: Option<&str>,
    ) -> Result<Option<Observation>, MicroVmClientError> {
        self.answer(
            Call::Observe(image.clone(), version.map(str::to_owned)),
            |state| state.observed.pop_front(),
            || Ok(None),
        )
        .await
    }

    async fn prune(&self, image: &Arn, keep: usize) -> Result<(), MicroVmClientError> {
        self.answer(
            Call::Prune(image.clone(), keep),
            |state| state.pruned.pop_front(),
            || Ok(()),
        )
        .await
    }

    async fn list_microvms(
        &self,
        image: Option<&ImageIdentifier>,
        version: Option<&str>,
        next_token: Option<&str>,
    ) -> Result<MicroVmPage, MicroVmClientError> {
        self.answer(
            Call::ListMicroVms {
                image: image.cloned(),
                version: version.map(str::to_owned),
                next_token: next_token.map(str::to_owned),
            },
            |state| state.listed.pop_front(),
            || Ok((Vec::new(), None)),
        )
        .await
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<Launch, MicroVmClientError> {
        self.answer(
            Call::Launch(spec.clone()),
            |state| state.launched.pop_front(),
            || {
                Ok(Launch {
                    microvm_id: "microvm-fake".into(),
                    image_version: "1".into(),
                })
            },
        )
        .await
    }
}
