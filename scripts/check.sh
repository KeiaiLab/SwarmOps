#!/usr/bin/env bash
# Same steps as `just check`, for machines without `just`; every step that resolves
# dependencies runs `--locked`, so a stale Cargo.lock fails here instead of being rewritten.
set -euo pipefail
cd "$(dirname "$0")/.."
# The Python launcher: Windows has `python`, and no `python3`.
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) python=python ;;
  *) python=python3 ;;
esac
cargo fmt --all --check
cargo fmt --manifest-path fuzz/Cargo.toml --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
# `test_hooks` exists only without the feature, so the all-features run above compiles it to
# nothing: this step lints it, and the library crates it builds, without the feature.
cargo clippy --locked -p swarmops-cli --test test_hooks -- -D warnings
cargo clippy --locked --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
# `--all-features` turns on the test hooks the crash tests drive; the release run of
# `test_hooks` is built without them, as `cargo build --release` is, and checks it ignores them.
# That test exists only without the feature, so it must run: a build that turns the feature on
# all the same fails here. Its nextest run keeps the default profile, so that under
# `NEXTEST_PROFILE=ci` its report does not replace the workspace run's JUnit report.
if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --locked --workspace --all-features --no-tests=warn
  cargo nextest run --locked --profile default --release -p swarmops-cli --test test_hooks --no-tests=fail
else
  cargo test --locked --workspace --all-features
  hooks=$(cargo test --locked --release -p swarmops-cli --test test_hooks 2>&1) || { echo "$hooks"; exit 1; }
  echo "$hooks"
  grep -q "test result: ok. 1 passed" <<<"$hooks" || {
    echo "check: test_hooks ran no test: is the test-hooks feature on in a release build?" >&2
    exit 1
  }
fi
# Twice: the second run, with every feature on, also renders and link-checks the test-hooks code.
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items --all-features
cargo deny --locked check
# The fuzz crate's lockfile too, which the fuzz clippy step builds; its one license exception is
# in fuzz/deny.exceptions.toml.
cargo deny --locked --manifest-path fuzz/Cargo.toml --config deny.toml check
# THIRD-PARTY-LICENSES, the notices the release archives carry, still matches Cargo.lock.
"$python" -I scripts/third-party-licenses.py --check
# The public Markdown: its links and anchors resolve, and it carries no host path or tool
# attribution; the selftest first shows that each rule still finds what it is for.
"$python" -I scripts/check-docs.py selftest
"$python" -I scripts/check-docs.py
