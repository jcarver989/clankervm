mod aws_microvm_client;
mod error;
#[cfg(test)]
mod fake_microvm_client;
mod microvm_client;

pub use aws_microvm_client::AwsMicroVmClient;
pub use error::MicroVmClientError;
#[cfg(test)]
pub use fake_microvm_client::{FakeMicroVmClient, MicroVmCall};
pub use microvm_client::*;
