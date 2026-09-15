use super::*;
use crate::client::FakeMicroVmClient;
use crate::test_support::MicroVmDetailsBuilder;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn conflict_requires_observed_termination() {
    let conflict = MicroVmClientError::AwsOperationFailed {
        operation: "stop",
        kind: AwsFailure::Conflict,
    };
    let running = Ok(Some(MicroVmDetailsBuilder::new("vm").build()));
    for next in [
        None,
        Some(
            MicroVmDetailsBuilder::new("vm")
                .state(MicrovmState::Terminating)
                .build(),
        ),
    ] {
        let client = FakeMicroVmClient::default()
            .described([running.clone(), Ok(next)])
            .terminated([Err(conflict.clone())]);
        assert!(
            stop_vm(&client, "vm", true, Duration::from_secs(30))
                .await
                .unwrap()
        );
    }
    let client = FakeMicroVmClient::default()
        .described([running.clone(), running])
        .terminated([Err(conflict)]);
    assert!(
        stop_vm(&client, "vm", false, Duration::from_secs(30))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn deadline_bounds_the_entire_operation_and_cleanup() {
    let client = FakeMicroVmClient::default().with_delay(Duration::from_secs(40));
    let start = Instant::now();
    assert!(
        stop_vm(&client, "vm", true, Duration::from_secs(5))
            .await
            .is_err()
    );
    assert_eq!(Instant::now() - start, Duration::from_secs(5));
    let start = Instant::now();
    assert!(terminate(&client, "vm").await.is_err());
    assert_eq!(Instant::now() - start, Duration::from_secs(30));
}
