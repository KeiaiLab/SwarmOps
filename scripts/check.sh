#!/usr/bin/env bash
# Same steps as `just check`, for machines (and CI) without `just`.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo fmt --manifest-path fuzz/Cargo.toml --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
# `--all-features` turns on the test hooks the crash tests drive; the release run of
# `test_hooks` is built without them, as `cargo build --release` is, and checks it ignores them.
if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --workspace --all-features --no-tests=warn
  cargo nextest run --release -p swarmops-cli --test test_hooks
else
  cargo test --workspace --all-features
  cargo test --release -p swarmops-cli --test test_hooks
fi
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
