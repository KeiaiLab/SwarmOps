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
# bench/compare's lockfile, which the recipe does not build, is resolved `--locked` too
# (`cargo metadata`, no build), so a dependency change that leaves it stale fails every push,
# not the weekly run.
# `cargo deny` checks both lockfiles the recipe builds, with deny.toml: the fuzz crate's too,
# whose build scripts the fuzz clippy step runs (its one license exception is in
# fuzz/deny.exceptions.toml). swarmops-core's `supply_chain` tests fail a recipe, script or
# workflow job that builds a workspace no `cargo deny` checks.
# The last step fails when THIRD-PARTY-LICENSES, the notices of the third-party crates that the
# release archives carry, no longer matches Cargo.lock, the licenses deny.toml allows and the
# crates' packages: `python3 -I scripts/third-party-licenses.py` writes it again.
# CI (.github/workflows/ci.yml) runs this recipe with `NEXTEST_PROFILE=ci`; the release run
# keeps the default profile, so that its one-test report does not replace the JUnit report of
# the workspace run in target/nextest/ci/.
# scripts/check-stamped.py builds swarmops-cli with `SWARMOPS_BUILD_COMMIT` set, which no other
# step does: the `version` tests and each `--version` pin with a fixed commit, every filter
# required to match a test, then a malformed one, whose build must fail; last it links
# target/debug's `swarmops` and `so` again without the fixed commit. Each value rebuilds that
# crate alone, incrementally.
# Then scripts/test-install.py runs the one-line installer against a fake release on 127.0.0.1,
# offline, and checks that web/public/install.sh is the same file; scripts/test-packages.py packs
# fake release archives with scripts/packages.py and reads the .deb and .rpm back, offline.
# Last, scripts/bench-gate.py's selftest shows that the benchmark gate still finds what it is for.
# The definition of done for every session: fmt, clippy, tests, hook proof, stamped build, docs, cargo-deny, license notices, installer, packages, Markdown, bench gate.
check:
    cargo fmt --all --check
    cargo fmt --manifest-path fuzz/Cargo.toml --check
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    cargo clippy --locked --workspace --lib --bins --all-features -- -D warnings -D clippy::unwrap_used -D clippy::missing_assert_message -D clippy::mod_module_files -D clippy::rc_buffer
    cargo clippy --locked -p swarmops-cli --test test_hooks -- -D warnings
    cargo clippy --locked --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
    cargo metadata --locked --format-version 1 --manifest-path bench/compare/Cargo.toml >/dev/null
    cargo nextest run --locked --workspace --all-features --no-tests=warn
    cargo nextest run --locked --profile default --release -p swarmops-cli --test test_hooks --no-tests=fail
    python3 -I scripts/check-stamped.py
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items --all-features
    cargo deny --locked check
    cargo deny --locked --manifest-path fuzz/Cargo.toml --config deny.toml check
    python3 -I scripts/third-party-licenses.py --check
    python3 -I scripts/test-install.py
    python3 -I scripts/test-packages.py
    python3 -I scripts/check-docs.py selftest
    python3 -I scripts/check-docs.py
    python3 -I scripts/bench-gate.py selftest

# The test hooks are on (`--all-features`), as in `just check`. The ignored switches that rewrite
# or rebuild fixtures on purpose (`regenerate_golden`, `regenerate_git_fixtures`,
# `check_git_fixtures`) are left out.
# Slow suite: more proptest cases plus the ignored tests (multi-process, crash, perf-ratio).
check-slow:
    PROPTEST_CASES=2000 cargo nextest run --locked --workspace --all-features --run-ignored all -E 'not test(regenerate_golden) and not test(regenerate_git_fixtures) and not test(check_git_fixtures)'

# `check-slow` without the two 32-process contention measurements (`thirty_two_processes_*`),
# which `just perf` runs: each takes minutes of fsync'd work on this host, and their assertions
# (no `Busy`) assume a disk at most about a third slower than its own, far too heavy a load for a
# 2-vCPU hosted runner. Nor the nine measurement cases on 12,000 or 13,000 commits or 12,000
# changesets (`ci_perf_cases`), which need a longer kill than the `ci-slow` profile's 20 minutes:
# in the Scheduled run of 2026-10-28 on `c7baf861` four ran into it and the other five took 775
# to 912 s, and three take over 20 minutes even optimized; `check-ci-perf` runs them under a
# profile of their own. The crash sweeps and the other ignored tests stay. CI runs it with
# `NEXTEST_PROFILE=ci-slow` (.config/nextest.toml).
# The weekly CI slow suite (scheduled.yml): check-slow less the 32-process and large-history cases.
check-ci-slow:
    PROPTEST_CASES=2000 cargo nextest run --locked --workspace --all-features --run-ignored all -E 'not test(regenerate_golden) and not test(regenerate_git_fixtures) and not test(check_git_fixtures) and not test(thirty_two_processes) and not ({{ ci_perf_cases }})'

# The measurement cases on 12,000 or 13,000 commits or 12,000 changesets: `remote_perf`'s five,
# `serve_perf`'s three and the workspace's checked land. `check-ci-slow` leaves them out,
# `check-ci-perf` runs them, `check-slow` keeps them. swarmops-core's `supply_chain` tests fail a
# case `check-ci-slow` leaves out that no scheduled job runs optimized and one at a time.
ci_perf_cases := 'test(=remote_perf::clone_cost_on_12000_commits) | test(=remote_perf::fetch_and_push_scale_with_history) | test(=remote_perf::fetch_phases_scale_with_history) | test(=remote_perf::sync_of_one_land_on_12000_commits) | test(=remote_perf::sync_phases_scale_with_history) | test(/^serve_perf::/) | test(=ws_processes::perf::perf_checked_land_overhead_on_12000_changesets)'

# `ci_perf_cases` optimized and one at a time (`--no-capture`), as `perf` runs them, printing their
# `remote-perf: `, `serve-perf: ` and `ws-perf: ` lines. They assert what they ran, never its time.
# Measured optimized on this host: the clone cost 850.5 s and `serve_perf`'s three 1,599.3 to
# 1,650.0 s (docs/perf.md, "v0.7.0"), the other four `remote_perf` cases 346.0 to 389.5 s in the
# same window, the checked land 313.5 s in v0.5.0's; 7,541.4 s in all. No match fails.
# scheduled.yml runs it weekly under the heavy lock, with `NEXTEST_PROFILE=ci-perf`.
# The weekly CI measurement cases (scheduled.yml): what check-ci-slow leaves out, optimized.
check-ci-perf:
    cargo nextest run --locked --release --workspace --all-features --run-ignored all --no-tests=fail -E '{{ ci_perf_cases }}' --no-capture

# The git fixtures (crates/swarmops-testkit/git-fixtures/): every script run again by the git
# that git-fixtures/GIT_VERSION declares (`SWARMOPS_GIT` names the binary; any other version is
# refused unless `SWARMOPS_GIT_FIXTURES_ANY_GIT=1`), and its archive and manifest rewritten,
# byte for byte the same when nothing changed. Review the diff, and commit it with the scripts.
# Rebuilds the git fixtures' archives from their scripts, with the declared git.
git-fixtures:
    SWARMOPS_REGEN_GIT_FIXTURES=1 cargo nextest run --locked -p swarmops-testkit --run-ignored ignored-only -E 'test(regenerate_git_fixtures)' --no-capture

# The criterion benches, with cargo's full output (`perf` prints only their summary lines).
bench-run:
    cargo bench --locked --workspace

# scripts/bench-gate.py fails on a median over 20 % above its baseline, a listed bench without an
# estimate, an estimate neither listed nor skipped for a reason, or no estimate at all. Criterion
# writes under CARGO_TARGET_DIR, where the CI runners build; the old results go first, so that the
# gate never reads a bench this run did not measure. Run it in a quiet window, under the heavy lock.
# The benchmark gate: a fresh `bench-run`, its medians against bench/thresholds.toml.
bench: _bench-fresh bench-run
    python3 -I scripts/bench-gate.py check "${CARGO_TARGET_DIR:-target}/criterion" bench/thresholds.toml

# Removes the criterion results of earlier runs (`bench`'s first step).
_bench-fresh:
    rm -rf -- "${CARGO_TARGET_DIR:-target}/criterion"

# Optimized, for a quiet window: the 32-process store-level contention test, the 32-process
# CLI contention test at the spec's 32 x 1,000 commits (SWARMOPS_TEST_CLI_CONTENTION_COMMITS
# sets another count), the git remotes' cases (`remote_perf`, against a loopback remote), then
# every store bench. Prints only their summary lines (the
# store-level test's by their shape, without its per-child JSON lines) and the load average
# between them; target/perf/ keeps the full output. docs/perf.md records what they print.
# The M5 workspace cases (ws_processes::perf::*: the 50,000-file tree's status, edits, commits
# and update, the 10,000-file create, the checked land on 12,000 changesets) print `ws-perf: `
# lines.
# The CLI contention test and G4's landing measurement (`landing::perf::landing_under_load`,
# open loop: one land per agent every 2 s for 120 s) run at 32 and at 50 processes
# (`SWARMOPS_PERF_PROCESSES` lists others; the landing store under the bench directory), and
# G2's commit bench (`swarmops-repo`'s
# `commit_latency`: with and without syncs) runs last.
# The M1, M13 and M5 measurements in one go: both 32-process contention tests, the remote
# cases, the workspace cases, then the store benches.
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
    for n in ${SWARMOPS_PERF_PROCESSES:-32 50}; do
        SWARMOPS_TEST_CLI_CONTENTION_CHILDREN="$n" cargo nextest run --locked --release \
            -p swarmops-cli --run-ignored ignored-only --test all -E 'test(/^contention::/)' \
            --no-capture 2>&1 | tee "target/perf/contention-$n.log" \
            | grep --line-buffered '^contention: '
        [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
        load between
    done
    bench_dir="${SWARMOPS_BENCH_DIR:-$HOME/.local/state/swarmops-bench}"
    mkdir -p "$bench_dir"
    for n in ${SWARMOPS_PERF_PROCESSES:-32 50}; do
        TMPDIR="$bench_dir" SWARMOPS_TEST_CLI_LANDING_LANDERS="$n" cargo nextest run --locked --release \
            -p swarmops-cli --run-ignored ignored-only --test all \
            -E 'test(=landing::perf::landing_under_load)' --no-capture 2>&1 \
            | tee "target/perf/landing-$n.log" | grep --line-buffered '^landing-perf: '
        [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
        load between
    done
    cargo nextest run --locked --release -p swarmops-cli --all-features --run-ignored ignored-only \
        -E 'test(/^remote_perf::/)' --no-capture 2>&1 | tee target/perf/remote.log \
        | grep --line-buffered '^remote-perf: '
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo nextest run --locked --release -p swarmops-cli --all-features --run-ignored ignored-only \
        -E 'test(/^serve_perf::/)' --no-capture 2>&1 | tee target/perf/serve.log \
        | grep --line-buffered '^serve-perf: '
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo nextest run --locked --release -p swarmops-cli --all-features --run-ignored ignored-only \
        -E 'test(/^ws_processes::perf::/)' --no-capture 2>&1 | tee target/perf/ws.log \
        | grep --line-buffered '^ws-perf: '
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo bench --locked -p swarmops-store --bench '*' -- --output-format bencher 2>&1 \
        | tee target/perf/benches.log | grep --line-buffered -E '^(test |percentiles )'
    [ "${PIPESTATUS[0]}" -eq 0 ] || status=1
    load between
    cargo bench --locked -p swarmops-repo --bench commit_latency -- --output-format bencher 2>&1 \
        | tee target/perf/commit-latency.log | grep --line-buffered -E '^(test |percentiles )'
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
# bench/compare/target/results/; docs/perf.md records it. Before it builds anything, the recipe
# checks the harness's lockfile with `cargo deny` and the harness's own policy
# (bench/compare/deny.toml), which fetches the advisory database; the run itself needs no
# network.
# The native metadata store against SQLite, redb, fjall and sled; `--quick` is a smoke run.
bench-compare *args:
    cargo deny --locked --manifest-path bench/compare/Cargo.toml --config bench/compare/deny.toml check
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

# The website, web/: a Next.js static export (Fumadocs for /docs) that renders docs/**/*.md and
# CHANGELOG.md, written to web/out/. `npm ci` needs the npm registry, so these recipes stay out of
# `just check`; the build itself is offline once installed. .github/workflows/site.yml runs all three.
# Installs the website's dependencies, exactly as web/package-lock.json pins them.
web-install:
    cd web && npm ci --no-audit --no-fund

# Builds the website into web/out/: pages, search index, per-page CSP, the copied installer.
web-build:
    cd web && npm run build

# Checks web/out/: every docs page, internal links and anchors, no third-party origin, the CSP
# hashes, the first-load JS budget and the files served beside the pages.
web-check:
    cd web && npm run check
