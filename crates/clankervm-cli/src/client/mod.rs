mod aws_microvm_client;
mod error;
#[cfg(test)]
mod fake_microvm_client;
mod microvm_client;

pub(crate) use aws_microvm_client::AwsMicroVmClient;
pub(crate) use error::AwsFailure;
pub use error::MicroVmClientError;
#[cfg(test)]
pub(crate) use fake_microvm_client::{Call, FakeMicroVmClient};
pub(crate) use microvm_client::*;

use aws_sdk_lambdamicrovms::types::MicrovmState;

const RESUMING: &str = "RESUMING";

pub(crate) fn is_resuming(state: &MicrovmState) -> bool {
    state.as_str() == RESUMING
}

#[cfg(test)]
pub(crate) fn resuming_state() -> MicrovmState {
    MicrovmState::from(RESUMING)
}
