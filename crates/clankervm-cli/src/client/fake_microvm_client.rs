use super::error::MicroVmClientError;
use super::microvm_client::{
    ImageSpec, Launch, LaunchSpec, MicroVmClient, Observation, Published, artifact_key,
};
use crate::arn::Arn;
use crate::artifact::Artifact;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// One call recorded by [`FakeMicroVmClient`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Publish(ImageSpec),
    Observe(Arn, Option<String>),
    Prune(Arn, usize),
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
    launched: VecDeque<Result<Launch, MicroVmClientError>>,
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

    pub(crate) fn launched(
        self,
        responses: impl IntoIterator<Item = Result<Launch, MicroVmClientError>>,
    ) -> Self {
        self.lock().launched = responses.into_iter().collect();
        self
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    fn answer<T>(
        &self,
        call: Call,
        scripted: impl FnOnce(&mut State) -> Option<T>,
        default: impl FnOnce() -> T,
    ) -> T {
        let mut state = self.lock();
        state.calls.push(call);
        scripted(&mut state).unwrap_or_else(default)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("fake mutex poisoned")
    }
}

// The trait is async; the fake resolves immediately without awaiting.
#[allow(clippy::unused_async_trait_impl)]
impl MicroVmClient for FakeMicroVmClient {
    async fn publish(
        &self,
        spec: &ImageSpec,
        bundle: &Artifact,
    ) -> Result<Published, MicroVmClientError> {
        self.answer(
            Call::Publish(spec.clone()),
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
    }

    async fn prune(&self, image: &Arn, keep: usize) -> Result<(), MicroVmClientError> {
        self.answer(
            Call::Prune(image.clone(), keep),
            |state| state.pruned.pop_front(),
            || Ok(()),
        )
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
    }
}
