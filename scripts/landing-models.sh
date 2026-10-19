#!/usr/bin/env bash
# M4 Task 1's measurement matrix (the M4 plan's Task 1 Step 6; umbrella decision 10): the four
# landing-model prototypes in every cell, from the ignored measurements of
# crates/swarmops-repo/tests/landing_models.rs, one optimized `cargo nextest run` per cell, model
# and run. Every run of a cell lands the same changes in each model (the seed is the run's
# number), and the models' order rotates from one run to the next. The cells:
#
#   closed-disjoint  32 landers x 100 lands of W-disjoint, closed loop
#   closed-shared    32 landers x 100 lands of W-shared, closed loop
#   closed-conflict  32 landers x 100 lands of W-conflict, closed loop
#   large            32 landers x 50 lands of W-large, closed loop
#   scored           50 landers, one land each every 2 s +/- 20 % for 120 s, W-shared, open
#                    loop: the cell Decision 1 scores
#
# Three runs each: 60 runs, about 2.5 hours on this host. The script holds the host's heavy-run
# lock (/tmp/swarmops-heavy.lock, or SWARMOPS_HEAVY_LOCK) for the whole matrix, so run it in a
# quiet window. Each run's output goes to target/perf/landing-models/<cell>-<model>-run<k>.log;
# matrix.log there and the console get the tree measured, the load before each run, and each
# run's `landing-model: ` line (with the load and the raw fdatasync probe at its start and end),
# and its `landing-gate: ` line if its gate failed. A run that fails is recorded and the matrix
# goes on; the script exits non-zero at the end if any failed.
#
# Options: --runs N (3 by default); --cell NAME, repeatable (every cell by default), to run again
# the cells a review names; --smoke, the matrix at tiny sizes (4 landers, 5 lands, 2 of W-large,
# one land every 300 ms for 3 s, one run) into target/perf/landing-models-smoke/, without the
# lock, to check the script itself.
set -euo pipefail
cd "$(dirname "$0")/.."

args=("$@")
runs=3
cells=()
smoke=0
while [ $# -gt 0 ]; do
  case "$1" in
    --runs) runs="$2"; shift 2 ;;
    --cell) cells+=("$2"); shift 2 ;;
    --smoke) smoke=1; shift ;;
    *) echo "landing-models: unknown argument $1" >&2; exit 2 ;;
  esac
done
[ ${#cells[@]} -gt 0 ] || cells=(closed-disjoint closed-shared closed-conflict large scored)

if [ "$smoke" -eq 0 ] && [ -z "${SWARMOPS_LANDING_MODELS_LOCKED:-}" ]; then
  lock="${SWARMOPS_HEAVY_LOCK:-/tmp/swarmops-heavy.lock}"
  echo "landing-models: waiting for the heavy-run lock $lock"
  exec env SWARMOPS_LANDING_MODELS_LOCKED=1 flock "$lock" "$0" "${args[@]}"
fi

if [ "$smoke" -eq 1 ]; then
  out=target/perf/landing-models-smoke
  runs=1
  closed_landers=4 closed_lands=5 large_lands=2 open_landers=4 interval_ms=300 duration_s=3
else
  out=target/perf/landing-models
  closed_landers=32 closed_lands=100 large_lands=50 open_landers=50 interval_ms=2000 duration_s=120
fi
mkdir -p "$out"
models=(cas_loop pushrebase metalog op_heads)
nextest=(cargo nextest run --locked --profile default --release -p swarmops-repo --all-features)

{
  echo "landing-models: $(date -u +%FT%TZ), tree $(git rev-parse HEAD) \"$(git log -1 --format=%s)\"," \
    "$(git status --porcelain | wc -l) uncommitted paths"
  echo "landing-models: cells ${cells[*]}, $runs runs each"
} | tee "$out/matrix.log"
"${nextest[@]}" --no-run

failed=0
for run in $(seq 1 "$runs"); do
  for cell in "${cells[@]}"; do
    case "$cell" in
      closed-disjoint | closed-shared | closed-conflict)
        test=landing_models_closed_loop
        cell_env=(SWARMOPS_TEST_LANDING_WORKLOAD="${cell#closed-}"
          SWARMOPS_TEST_LANDING_LANDERS="$closed_landers" SWARMOPS_TEST_LANDING_LANDS="$closed_lands") ;;
      large)
        test=landing_models_large_change
        cell_env=(SWARMOPS_TEST_LANDING_LANDERS="$closed_landers" SWARMOPS_TEST_LANDING_LANDS="$large_lands") ;;
      scored)
        test=landing_models_open_loop
        cell_env=(SWARMOPS_TEST_LANDING_WORKLOAD=shared SWARMOPS_TEST_LANDING_LANDERS="$open_landers"
          SWARMOPS_TEST_LANDING_INTERVAL_MS="$interval_ms" SWARMOPS_TEST_LANDING_DURATION_S="$duration_s") ;;
      *) echo "landing-models: unknown cell $cell" >&2; exit 2 ;;
    esac
    for i in 0 1 2 3; do
      model="${models[$(((i + run - 1) % 4))]}"
      log="$out/$cell-$model-run$run.log"
      echo "landing-models: $cell $model run $run, load $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$out/matrix.log"
      if ! env "${cell_env[@]}" SWARMOPS_TEST_LANDING_MODEL="$model" SWARMOPS_TEST_LANDING_SEED="$run" \
        "${nextest[@]}" --run-ignored ignored-only -E "test(=landing_models::$test)" --no-capture \
        >"$log" 2>&1; then
        failed=$((failed + 1))
        echo "landing-models: $cell $model run $run FAILED, see $log" | tee -a "$out/matrix.log"
      fi
      grep -E '^landing-(model|gate): ' "$log" | tee -a "$out/matrix.log" || true
    done
  done
done

echo "landing-models: done, $failed failed runs; the logs are in $out/" | tee -a "$out/matrix.log"
[ "$failed" -eq 0 ]
