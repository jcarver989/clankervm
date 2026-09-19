use super::*;
use crate::client::{Call, FakeMicroVmClient};
use crate::test_support::{MicroVmDetailsBuilder, microvm_details, resuming};
use tokio::time::Instant;

const MICROVM_ID: &str = "vm-1";
const TIMEOUT: Duration = Duration::from_secs(30);

async fn apply(
    client: &FakeMicroVmClient,
    wait: bool,
    operation: Operation,
) -> Result<Confirmation, ClankerError> {
    LifecycleTransition::new(client, MICROVM_ID, operation)
        .apply(wait, TIMEOUT)
        .await
}

#[tokio::test(start_paused = true)]
async fn requests_each_lifecycle_operation_once_without_waiting() {
    for (operation, state, expected) in [
        (
            Operation::Suspend,
            MicrovmState::Running,
            Call::Suspend("vm-1".into()),
        ),
        (
            Operation::Resume,
            MicrovmState::Suspended,
            Call::Resume("vm-1".into()),
        ),
        (
            Operation::Stop,
            MicrovmState::Running,
            Call::Terminate("vm-1".into()),
        ),
    ] {
        let client =
            FakeMicroVmClient::default().described([Ok(Some(microvm_details("vm-1", state)))]);
        assert!(
            !apply(&client, false, operation)
                .await
                .unwrap()
                .is_confirmed()
        );
        assert_eq!(client.calls(), [Call::Describe("vm-1".into()), expected]);
    }
}

#[tokio::test(start_paused = true)]
async fn scripted_lifecycle_failures_are_returned_and_recorded() {
    let failure = MicroVmClientError::Service {
        operation: "scripted",
        message: "failure".into(),
    };
    let suspended = FakeMicroVmClient::default()
        .described([Ok(Some(microvm_details("vm-1", MicrovmState::Running)))])
        .suspended([Err(failure.clone())]);
    assert!(matches!(
        apply(&suspended, false, Operation::Suspend).await,
        Err(ClankerError::MicroVmClient(error)) if error == failure
    ));
    assert_eq!(
        suspended.calls(),
        [Call::Describe("vm-1".into()), Call::Suspend("vm-1".into())]
    );
}

#[tokio::test(start_paused = true)]
async fn target_states_are_idempotent_and_confirmed() {
    for (operation, state) in [
        (Operation::Suspend, MicrovmState::Suspended),
        (Operation::Resume, MicrovmState::Running),
        (Operation::Stop, MicrovmState::Terminated),
    ] {
        let client =
            FakeMicroVmClient::default().described([Ok(Some(microvm_details("vm-1", state)))]);
        assert!(
            apply(&client, false, operation)
                .await
                .unwrap()
                .is_confirmed()
        );
        assert_eq!(client.calls(), [Call::Describe("vm-1".into())]);
    }
}

#[tokio::test(start_paused = true)]
async fn in_progress_states_are_not_requested_twice() {
    for (operation, state) in [
        (Operation::Suspend, MicrovmState::Suspending),
        (Operation::Resume, resuming()),
        (Operation::Stop, MicrovmState::Terminating),
    ] {
        let client =
            FakeMicroVmClient::default().described([Ok(Some(microvm_details("vm-1", state)))]);
        assert!(
            !apply(&client, false, operation)
                .await
                .unwrap()
                .is_confirmed()
        );
        assert_eq!(client.calls(), [Call::Describe("vm-1".into())]);
    }
}

#[tokio::test(start_paused = true)]
async fn wait_polls_until_the_target_state() {
    let client = FakeMicroVmClient::default().described([
        Ok(Some(microvm_details("vm-1", MicrovmState::Running))),
        Ok(Some(microvm_details("vm-1", MicrovmState::Suspending))),
        Ok(Some(microvm_details("vm-1", MicrovmState::Suspended))),
    ]);
    let start = Instant::now();
    assert!(
        apply(&client, true, Operation::Suspend)
            .await
            .unwrap()
            .is_confirmed()
    );
    assert_eq!(start.elapsed(), Duration::from_secs(4));
    assert_eq!(
        client.calls(),
        [
            Call::Describe("vm-1".into()),
            Call::Suspend("vm-1".into()),
            Call::Describe("vm-1".into()),
            Call::Describe("vm-1".into()),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn missing_and_invalid_states_fail_without_a_request() {
    let missing = FakeMicroVmClient::default();
    assert!(matches!(
        apply(&missing, false, Operation::Resume).await,
        Err(ClankerError::MicroVmNotFound(_))
    ));

    let invalid = FakeMicroVmClient::default().described([Ok(Some(
        MicroVmDetailsBuilder::new("vm-1")
            .state(MicrovmState::Terminating)
            .state_reason("stopping")
            .build(),
    ))]);
    let error = apply(&invalid, false, Operation::Resume).await.unwrap_err();
    assert!(matches!(error, ClankerError::UnexpectedMicroVmState { .. }));
    let message = error.to_string();
    assert!(message.contains("TERMINATING") && message.contains("SUSPENDED"));
    assert!(message.contains("stopping"));
}

#[tokio::test(start_paused = true)]
async fn conflict_is_accepted_only_after_an_observed_race() {
    let conflict = MicroVmClientError::AwsOperationFailed {
        operation: "resume MicroVM",
        kind: AwsFailure::Conflict,
    };
    let accepted = FakeMicroVmClient::default()
        .described([
            Ok(Some(microvm_details("vm-1", MicrovmState::Suspended))),
            Ok(Some(microvm_details("vm-1", resuming()))),
        ])
        .resumed([Err(conflict.clone())]);
    assert!(
        !apply(&accepted, false, Operation::Resume)
            .await
            .unwrap()
            .is_confirmed()
    );

    let rejected = FakeMicroVmClient::default()
        .described([
            Ok(Some(microvm_details("vm-1", MicrovmState::Suspended))),
            Ok(Some(microvm_details("vm-1", MicrovmState::Suspended))),
        ])
        .resumed([Err(conflict.clone())]);
    assert!(matches!(
        apply(&rejected, false, Operation::Resume).await,
        Err(ClankerError::MicroVmClient(error)) if error == conflict
    ));
}

#[tokio::test(start_paused = true)]
async fn stop_treats_a_missing_microvm_as_complete_and_requires_an_observed_conflict() {
    let missing = FakeMicroVmClient::default();
    assert!(
        apply(&missing, false, Operation::Stop)
            .await
            .unwrap()
            .is_confirmed()
    );

    let conflict = MicroVmClientError::AwsOperationFailed {
        operation: "stop",
        kind: AwsFailure::Conflict,
    };
    for next in [
        None,
        Some(microvm_details("vm-1", MicrovmState::Terminating)),
    ] {
        let client = FakeMicroVmClient::default()
            .described([
                Ok(Some(microvm_details("vm-1", MicrovmState::Running))),
                Ok(next),
            ])
            .terminated([Err(conflict.clone())]);
        assert!(
            apply(&client, true, Operation::Stop)
                .await
                .unwrap()
                .is_confirmed()
        );
    }

    let client = FakeMicroVmClient::default()
        .described([
            Ok(Some(microvm_details("vm-1", MicrovmState::Running))),
            Ok(Some(microvm_details("vm-1", MicrovmState::Running))),
        ])
        .terminated([Err(conflict)]);
    assert!(matches!(
        apply(&client, false, Operation::Stop).await,
        Err(ClankerError::MicroVmTerminationUnconfirmed(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn timeout_bounds_describe_request_and_polling() {
    for client in [
        FakeMicroVmClient::default().with_delay(Duration::from_secs(60)),
        FakeMicroVmClient::default()
            .described([Ok(Some(microvm_details("vm-1", MicrovmState::Suspended)))])
            .with_delay(Duration::from_secs(60)),
    ] {
        let duration = Duration::from_secs(5);
        let start = Instant::now();
        let error = LifecycleTransition::new(&client, MICROVM_ID, Operation::Resume)
            .apply(true, duration)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClankerError::MicroVmLifecycleUnconfirmed { .. }
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }
}
