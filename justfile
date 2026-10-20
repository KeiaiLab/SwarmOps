set shell := ["bash", "-euo", "pipefail", "-c"]

# A bare `just` runs `check`, the definition of done.
default: check

# The fuzz crate is a workspace of its own, built with nightly by `just fuzz`; stable formats
# and lints it here. Its clippy step runs libfuzzer-sys's build script, which compiles
# libFuzzer's C++ sources: on a cold cache it needs a C++ compiler (`c++`). `--all-features`
# turns on `test-hooks`, the five `SWARMOPS_TEST_*` hooks (`CRASH_AT`, `FS_CALLS`, `FAIL_AT`,
# `ORDER_CHECK`, `NOW`); the release run of `test_hooks` after it is built without the feature,
# as `cargo build --release` is, and checks that it ignores them.
# That test exists only without the feature, so `--no-tests=fail` fails a build that turns it
# on all the same. The all-features clippy run compiles that test to nothing, so the clippy
# step after it lints it, and the library crates it builds, without the feature. The docs run
# twice: as `cargo doc` builds by default, and with `--all-features`, which renders and
# link-checks the code behind `test-hooks` (the order checker and the fault doubles) that the
# first run leaves out. A doc of the default build cannot link to that code, or the first run
# fails. Every step that resolves dependencies runs `--locked`: a Cargo.lock (either
# workspace's) that the manifests no longer match fails the gate instead of being rewritten.
# CI (.github/workflows/ci.yml) runs this recipe with `NEXTEST_PROFILE=ci`; the release run
# keeps the default profile, so that its one-test report does not replace the JUnit report of
# the workspace run in target/nextest/ci/.
# The definition of done for every session: fmt, clippy, tests, hook proof, docs, cargo-deny.
check:
    cargo fmt --all --check
    cargo fmt --manifest-path fuzz/Cargo.toml --check
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    cargo clippy --locked -p swarmops-cli --test test_hooks -- -D warnings
    cargo clippy --locked --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
    cargo nextest run --locked --workspace --all-features --no-tests=warn
    cargo nextest run --locked --profile default --release -p swarmops-cli --test test_hooks --no-tests=fail
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items --all-features
    cargo deny --locked check

# The test hooks are on (`--all-features`), as in `just check`. The ignored switches that rewrite
# or rebuild fixtures on purpose (`regenerate_golden`, `regenerate_git_fixtures`,
# `check_git_fixtures`) are left out.
# Slow suite: more proptest cases plus the ignored tests (multi-process, crash, perf-ratio).
check-slow:
    PROPTEST_CASES=2000 cargo nextest run --locked --workspace --all-features --run-ignored all -E 'not test(regenerate_golden) and not test(regenerate_git_fixtures) and not test(check_git_fixtures)'

# `check-slow` without the two 32-process contention measurements (`thirty_two_processes_*`),
# which `just perf` runs, and the three landing-model measurements of M4 Task 1
# (`landing_models::landing_models_*`: 32 and 50 landing processes), which
# `scripts/landing-models.sh` runs: each takes minutes of fsync'd work on this host, and their
# assertions (no `Busy`; no land past 30 s) assume a disk at most about a third slower than its
# own, far too heavy a load for a 2-vCPU hosted runner. The crash sweeps and the other ignored
# tests stay. CI runs it with `NEXTEST_PROFILE=ci-slow` (.config/nextest.toml).
# The weekly CI slow suite (scheduled.yml): check-slow without the multi-process measurements.
check-ci-slow:
    PROPTEST_CASES=2000 cargo nextest run --locked --workspace --all-features --run-ignored all -E 'not test(regenerate_golden) and not test(regenerate_git_fixtures) and not test(check_git_fixtures) and not test(thirty_two_processes) and not test(/^landing_models::landing_models_/)'

# The git fixtures (crates/swarmops-testkit/git-fixtures/): every script run again by the git
# that git-fixtures/GIT_VERSION declares (`SWARMOPS_GIT` names the binary; any other version is
# refused unless `SWARMOPS_GIT_FIXTURES_ANY_GIT=1`), and its archive and manifest rewritten,
# byte for byte the same when nothing changed. Review the diff, and commit it with the scripts.
# Rebuilds the git fixtures' archives from their scripts, with the declared git.
git-fixtures:
    SWARMOPS_REGEN_GIT_FIXTURES=1 cargo nextest run --locked -p swarmops-testkit --run-ignored ignored-only -E 'test(regenerate_git_fixtures)' --no-capture

# The criterion benches, with cargo's full output (`perf` prints only their summary lines).
bench:
    cargo bench --locked --workspace

# Optimized, for a quiet window: the 32-process store-level contention test, the 32-process
# CLI contention test at the spec's 32 x 1,000 commits (SWARMOPS_TEST_CLI_CONTENTION_COMMITS
# sets another count), then every store bench. Prints only their summary lines (the
# store-level test's by their shape, without its per-child JSON lines) and the load average
# between them; target/perf/ keeps the full output. docs/perf.md records what they print.
# The M1 measurements in one go: both 32-process contention tests, then the store benches.
perf:
    #!/usr/bin/env bash
    set -uo pipefail
    mkdir -p target/perf
    status=0
    export SWARMOPS_TEST_CLI_CONTENTION_COMMITS="${SWARMOPS_TEST_CLI_CONTENTION_COMMITS:-1000}"
    load() { echo "perf: load average $1: $(cut -d' ' -f1-3 /proc/loadavg)"; }
    load before
    cargo nextest run --locked --release -p swarmops-store --run-ignored ignored-only --test all \
        -E 'test(/^contention::/)' --no-capture 2>&1 | tee target/perf/store-contention.log \
        | grep --line-buffered -E \
        '^[0-9]+ processes x |^  (wall |transaction:|lock wait |child total time:|journal:|uncontended )'
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo nextest run --locked --release -p swarmops-cli --run-ignored ignored-only --test all \
        -E 'test(/^contention::/)' --no-capture 2>&1 | tee target/perf/contention.log \
        | grep --line-buffered '^contention: '
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo bench --locked -p swarmops-store --bench '*' -- --output-format bencher 2>&1 \
        | tee target/perf/benches.log | grep --line-buffered -E '^(test |percentiles )'
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load after
    [ "$status" -eq 0 ] || echo "perf: a step failed; target/perf/ has the full output" >&2
    exit "$status"

# The native metadata store against SQLite, redb, fjall and sled on SwarmOps' own workloads:
# bench/compare, a cargo workspace of its own that `just check` neither builds nor lints
# (SwarmOps depends on none of these databases). `--quick` is a smoke run at small sizes, under
# 2 minutes once built. The full run (the plan's sizes) waits on the disk: 3 h 18 min measured
# (2026-10-17), with W2 at about 7 ms per fdatasync, so give it a quiet host;
# `--w1-txns N --w2-txns N` scale it down. The report goes to stdout, every raw value to
# bench/compare/target/results/; docs/perf.md records it.
# The native metadata store against SQLite, redb, fjall and sled; `--quick` is a smoke run.
bench-compare *args:
    cargo run --release --locked --manifest-path bench/compare/Cargo.toml -- {{args}}

# cargo-fuzz takes no `--locked`; `just check` lints the fuzz workspace with it, so a
# fuzz/Cargo.lock that has gone stale fails the gate before a fuzz build could rewrite it.
# Two corpus directories: libFuzzer writes the new inputs it finds into the first one it is
# given only, fuzz/corpus-new/<target>/ (git-ignored, kept from one run to the next, and it
# must exist), and reads the tracked fuzz/corpus/<target>/ after it as a seed that it never
# writes to, so a run leaves the tree clean. New inputs join the tracked corpus minimised by
# `cargo fuzz cmin`, as CONTRIBUTING.md says.
# `-timeout=30`: an input that runs for more than 30 s fails the run, and libFuzzer saves it
# under fuzz/artifacts/<target>/ (by default it would run for 20 minutes first).
# Needs a nightly toolchain: `rustup toolchain install nightly`. Targets: codec_roundtrip, tree_decode, frame_decode, journal_recovery.
fuzz target="codec_roundtrip":
    mkdir -p fuzz/corpus-new/{{target}}
    cd fuzz && cargo +nightly fuzz run {{target}} corpus-new/{{target}} corpus/{{target}} -- -max_total_time=60 -timeout=30
