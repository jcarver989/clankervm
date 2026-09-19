use super::*;
use crate::client::{Call, FakeMicroVmClient};
use crate::test_support::{MicroVmDetailsBuilder, microvm_details, resuming};
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn resumes_once_and_waits_for_the_readiness_check() {
    let client = FakeMicroVmClient::default().described([
        Ok(Some(microvm_details("vm-1", MicrovmState::Suspending))),
        Ok(Some(microvm_details("vm-1", MicrovmState::Suspended))),
        Ok(Some(microvm_details("vm-1", MicrovmState::Suspended))),
        Ok(Some(microvm_details("vm-1", resuming()))),
        Ok(Some(
            MicroVmDetailsBuilder::new("vm-1").endpoint("").build(),
        )),
        Ok(Some(MicroVmDetailsBuilder::new("vm-1").build())),
    ]);
    let start = Instant::now();

    let endpoint = wait_until_running(&client, "vm-1", false, |details| {
        Ok((!details.endpoint.is_empty()).then_some(details.endpoint))
    })
    .await
    .unwrap();

    assert!(!endpoint.is_empty());
    assert_eq!(start.elapsed(), Duration::from_secs(10));
    assert_eq!(
        client.calls(),
        [
            Call::Describe("vm-1".into()),
            Call::Describe("vm-1".into()),
            Call::Resume("vm-1".into()),
            Call::Describe("vm-1".into()),
            Call::Describe("vm-1".into()),
            Call::Describe("vm-1".into()),
            Call::Describe("vm-1".into()),
        ]
    );
}
