set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

# The definition of done for every session.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo nextest run --workspace --all-features --no-tests=warn
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    cargo deny check

# Slow suite: more proptest cases plus ignored (multi-process, crash, perf-ratio) tests.
check-slow:
    PROPTEST_CASES=2000 cargo nextest run --workspace --all-features --run-ignored all -E 'not test(regenerate_golden)'

bench:
    cargo bench --workspace

# The M1 measurements in one go, optimized, for a quiet window: the 32-process store-level
# contention test, the 32-process CLI contention test at the spec's 32 x 1,000 commits
# (SWARMOPS_TEST_CLI_CONTENTION_COMMITS sets another count), then every store bench. Prints
# only their summary lines (the store-level test's by their shape, without its per-child JSON
# lines) and the load average between them; target/perf/ keeps the full output. docs/perf.md
# records what they print.
perf:
    #!/usr/bin/env bash
    set -uo pipefail
    mkdir -p target/perf
    status=0
    export SWARMOPS_TEST_CLI_CONTENTION_COMMITS="${SWARMOPS_TEST_CLI_CONTENTION_COMMITS:-1000}"
    load() { echo "perf: load average $1: $(cut -d' ' -f1-3 /proc/loadavg)"; }
    load before
    cargo nextest run --release -p swarmops-store --run-ignored ignored-only --test contention \
        --no-capture 2>&1 | tee target/perf/store-contention.log | grep --line-buffered -E \
        '^[0-9]+ processes x |^  (wall |transaction:|lock wait |child total time:|journal:|uncontended )'
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo nextest run --release -p swarmops-cli --run-ignored ignored-only --test contention \
        --no-capture 2>&1 | tee target/perf/contention.log | grep --line-buffered '^contention: '
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo bench -p swarmops-store --bench '*' -- --output-format bencher 2>&1 \
        | tee target/perf/benches.log | grep --line-buffered -E '^(test |percentiles )'
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load after
    [ "$status" -eq 0 ] || echo "perf: a step failed; target/perf/ has the full output" >&2
    exit "$status"

# Needs a nightly toolchain: `rustup toolchain install nightly`. Targets: codec_roundtrip, tree_decode, frame_decode, journal_recovery.
fuzz target="codec_roundtrip":
    cd fuzz && cargo +nightly fuzz run {{target}} -- -max_total_time=60
