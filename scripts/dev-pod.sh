#!/bin/sh
# Runs inside the swarmops-dev pod (devspace.yaml commands, CONTRIBUTING.md) — never on the Mac. The tree is synced
# to /src; CARGO_TARGET_DIR=/cache/target and CARGO_HOME/RUSTUP_HOME under /cache/cargo are PVCs, so builds stay warm
# across pod restarts; sccache (RUSTC_WRAPPER) shares compiled crates through the RGW bucket.
#   build       cargo build --workspace (args go to cargo build)
#   test        cargo nextest run --workspace --all-features (args go to nextest)
#   check       just check
#   fuzz-smoke  each fuzz target (or the ones named) for SECONDS_PER_TARGET (default 60) s, as `just fuzz`: seeded
#               by the tracked fuzz/corpus, new inputs and the log kept on /archive (nas-nfs)
set -eu
cd /src
[ -f Cargo.toml ] || { echo "FATAL: /src is empty — is \`devspace run up\` running (it owns the file sync)?"; exit 1; }
mkdir -p "$HOME" "$TMPDIR" "$CARGO_HOME"

# the Nexus crate proxy, as the CI runner seeds it
cmp -s /opt/so-sbx/cargo-config.toml "$CARGO_HOME/config.toml" || cp /opt/so-sbx/cargo-config.toml "$CARGO_HOME/config.toml"
# the toolchain rust-toolchain.toml pins, from the Nexus rustup mirror (RUSTUP_DIST_SERVER)
rustup toolchain install --no-self-update >/dev/null

tool() { # <binary> <crate> <version> [features]
  if ! "$1" --version 2>/dev/null | grep -qF "$3"; then
    echo "[dev-pod] installing $2 $3 into $CARGO_HOME"
    RUSTC_WRAPPER='' cargo install --locked --quiet ${4:+--features "$4"} "$2@$3"
  fi
}
# the runner image has no libssl-dev: build OpenSSL from source with the crate
tool sccache sccache "$SO_TOOL_SCCACHE" vendored-openssl
tool just just "$SO_TOOL_JUST"
tool cargo-nextest cargo-nextest "$SO_TOOL_NEXTEST"
tool cargo-deny cargo-deny "$SO_TOOL_DENY"
export RUSTC_WRAPPER=sccache
sccache --start-server >/dev/null 2>&1 || true

cmd=${1:-}
[ $# -gt 0 ] && shift
case "$cmd" in
  build) cargo build --locked --workspace "$@" ;;
  test) cargo nextest run --locked --workspace --all-features --no-tests=warn "$@" ;;
  check) just check ;;
  fuzz-smoke)
    secs=${SECONDS_PER_TARGET:-60}
    tool cargo-fuzz cargo-fuzz "${SO_TOOL_FUZZ:-0.13.2}"
    rustup toolchain install nightly --profile minimal --no-self-update >/dev/null
    stamp=$(date -u +%Y%m%dT%H%M%SZ)
    log="$SWARMOPS_ARCHIVE/logs/fuzz-smoke-$stamp.log"
    mkdir -p "$SWARMOPS_ARCHIVE/logs"
    targets=${*:-$(cd fuzz && cargo +nightly fuzz list)}
    for t in $targets; do
      new="$SWARMOPS_ARCHIVE/fuzz/corpus-new/$t"
      mkdir -p "$new"
      (cd fuzz && cargo +nightly fuzz run "$t" "$new" "corpus/$t" -- -max_total_time="$secs" -timeout=30) 2>&1 | tee -a "$log"
    done
    echo "[dev-pod] log: $log (devspace run copy-back -- logs)"
    ;;
  *) echo "usage: dev-pod.sh build|test|check|fuzz-smoke [args]" >&2; exit 2 ;;
esac
sccache --show-stats 2>/dev/null | sed -n '1,6p' || true
