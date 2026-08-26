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

release-pr-preview:
    release-plz release-pr --dry-run

dist-generate:
    dist generate

dist-generate-check:
    dist generate --check

dist-plan:
    dist plan

dist-plan-tag tag:
    dist plan --tag "{{ tag }}"

dist-build:
    dist build
