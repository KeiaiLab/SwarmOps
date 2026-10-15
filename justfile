set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

# The definition of done for every session.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo nextest run --workspace --no-tests=warn
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    cargo deny check

# Slow suite: more proptest cases plus ignored (multi-process, crash, perf-ratio) tests.
check-slow:
    PROPTEST_CASES=2000 cargo nextest run --workspace --run-ignored all

bench:
    cargo bench --workspace

# Needs a nightly toolchain: `rustup toolchain install nightly`.
fuzz target="codec_roundtrip":
    cd fuzz && cargo +nightly fuzz run {{target}} -- -max_total_time=60
