mod aws_microvm_client;
mod error;
#[cfg(test)]
mod fake_microvm_client;
mod microvm_client;

pub(crate) use aws_microvm_client::AwsMicroVmClient;
pub use error::MicroVmClientError;
#[cfg(test)]
pub(crate) use fake_microvm_client::{Call, FakeMicroVmClient};
pub(crate) use microvm_client::*;
