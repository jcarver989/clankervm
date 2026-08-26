set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check:
    cargo check --workspace --all-targets

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check
