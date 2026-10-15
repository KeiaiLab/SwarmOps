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
# That test exists only without the feature, so it must run: a build that turns the feature on
# all the same fails here.
if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --workspace --all-features --no-tests=warn
  cargo nextest run --release -p swarmops-cli --test test_hooks --no-tests=fail
else
  cargo test --workspace --all-features
  hooks=$(cargo test --release -p swarmops-cli --test test_hooks 2>&1) || { echo "$hooks"; exit 1; }
  echo "$hooks"
  grep -q "test result: ok. 1 passed" <<<"$hooks" || {
    echo "check: test_hooks ran no test: is the test-hooks feature on in a release build?" >&2
    exit 1
  }
fi
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
