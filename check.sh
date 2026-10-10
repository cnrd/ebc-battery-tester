#!/usr/bin/env bash
# This scripts runs various CI-like checks in a convenient way.
set -eux

cargo fmt --all -- --check
cargo check --quiet --workspace --all-targets
cargo check --quiet --no-default-features --features server --bin ebc-server
cargo check --quiet --lib --target wasm32-unknown-unknown
cargo clippy --quiet --workspace --all-targets -- -D warnings -W clippy::all
cargo clippy --quiet --no-default-features --features server --all-targets -- -D warnings -W clippy::all
cargo clippy --quiet --lib --target wasm32-unknown-unknown -- -D warnings -W clippy::all
cargo test --quiet --workspace --all-targets
cargo test --quiet --no-default-features --features server --all-targets
cargo test --quiet --workspace --doc
EBC_WASM_DEFAULT_TRANSPORT=webusb trunk build
