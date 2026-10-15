#!/usr/bin/env bash
# Same steps as `just check`, for machines (and CI) without `just`.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --workspace --all-features --no-tests=warn
else
  cargo test --workspace --all-features
fi
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
