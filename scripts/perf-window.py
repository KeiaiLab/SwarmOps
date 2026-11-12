# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Runs the release measurements one step at a time, each only on a quiet host.

    flock /tmp/swarmops-heavy.lock uv run --script scripts/perf-window.py [options] [STEP ...]

The driver of the v0.5.0 measurements (docs/perf.md, "v0.5.0", "Method and host state"). Before
each step it waits until the 1-minute load is under `--quiet-load`, no CI job runs on this host's
runners (a `Runner.Worker` process) and the paused unit's service, if any, is idle; then it runs
the step alone. With `--pause TIMER` it stops that systemd user timer for the window and starts
it again however the window ends (the pilot's soak timer was `swarmops-pilot-soak.timer`).

Every step runs from the repository's root with its stores under the bench directory
(`$SWARMOPS_BENCH_DIR`, else `~/.local/state/swarmops-bench`, as `just perf`). Its output goes
whole to `<out>/<step>.log`; `<out>/window.log` gets each wait, start and end with the load, so
that every figure copied into docs/perf.md keeps its raw log. No step argument runs them all, in
the order `--list` prints. Exit 1 if any step failed, 2 on a usage error.

Options:
  --out DIR          the logs' directory (default target/perf/window)
  --quiet-load N     the 1-minute load a step waits to be under (default 6.0)
  --pause TIMER      a systemd user timer stopped for the window
  --list             print the steps and their commands, run nothing
"""

import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUT = ROOT / "target" / "perf" / "window"
DEFAULT_QUIET_LOAD = 6.0
POLL_S = 30
CI_PROCESS = "Runner.Worker"
TIMER_SUFFIX = ".timer"
SERVICE_SUFFIX = ".service"
BUSY_STATES = ("active", "activating")

USAGE = "usage: perf-window.py [--out DIR] [--quiet-load N] [--pause TIMER] [--list] [STEP ...]"

# The commands the steps share: optimized, the ignored measurements only, output uncaptured.
NEXTEST = ["cargo", "nextest", "run", "--locked", "--release", "-p", "swarmops-cli"]
IGNORED = ["--run-ignored", "ignored-only", "--no-capture"]
CONTENTION = NEXTEST + ["--test", "all"] + IGNORED
CONTENTION += ["-E", "test(=contention::thirty_two_processes_thousand_commits_each)"]
LANDING = NEXTEST + ["--test", "all"] + IGNORED + ["-E", "test(=landing::perf::landing_under_load)"]

# Each step: the environment it adds, then its command. G1 and G4 at 32 and 50 processes, G4's
# closed loop, G9's workspace cases, G17's history ratios and G2's commit without its syncs.
STEPS = {
    "g1-32": (
        {"SWARMOPS_TEST_CLI_CONTENTION_CHILDREN": "32", "SWARMOPS_TEST_CLI_CONTENTION_COMMITS": "1000"},
        CONTENTION,
    ),
    "g1-50": (
        {"SWARMOPS_TEST_CLI_CONTENTION_CHILDREN": "50", "SWARMOPS_TEST_CLI_CONTENTION_COMMITS": "1000"},
        CONTENTION,
    ),
    "g4-32": ({"SWARMOPS_TEST_CLI_LANDING_LANDERS": "32"}, LANDING),
    "g4-50": ({"SWARMOPS_TEST_CLI_LANDING_LANDERS": "50"}, LANDING),
    "g4-50-closed": (
        {
            "SWARMOPS_TEST_CLI_LANDING_LANDERS": "50",
            "SWARMOPS_TEST_CLI_LANDING_INTERVAL_MS": "0",
            "SWARMOPS_TEST_CLI_LANDING_LANDS": "10",
        },
        LANDING,
    ),
    "g9": ({}, NEXTEST + ["--all-features"] + IGNORED + ["-E", "test(/^ws_processes::perf::/)"]),
    "g17": (
        {},
        NEXTEST + ["--all-features"] + IGNORED + ["-E", "test(=remote_perf::fetch_and_push_scale_with_history)"],
    ),
    "g2": (
        {},
        ["cargo", "bench", "--locked", "-p", "swarmops-repo", "--bench", "commit_latency", "--"]
        + ["--output-format", "bencher"],
    ),
}


def load() -> str:
    """The 1-, 5- and 15-minute load, as /proc/loadavg gives them."""
    return " ".join(Path("/proc/loadavg").read_text().split()[:3])


def ci_busy() -> bool:
    """Whether a CI job runs on this host's runners."""
    return subprocess.run(["pgrep", "-f", CI_PROCESS], capture_output=True).returncode == 0


def unit_busy(timer: str | None) -> bool:
    """Whether the paused timer's service is running a cycle."""
    if timer is None:
        return False
    service = timer.removesuffix(TIMER_SUFFIX) + SERVICE_SUFFIX
    run = subprocess.run(["systemctl", "--user", "is-active", service], capture_output=True, text=True)
    return run.stdout.strip() in BUSY_STATES


def stamp() -> str:
    return time.strftime("%F %T %z")


def note(log, line: str) -> None:
    print(line, file=log, flush=True)


def wait_quiet(log, quiet_load: float, timer: str | None) -> None:
    """Polls until the host is quiet: low load, no CI job, no cycle of the paused unit."""
    while True:
        load1 = float(load().split()[0])
        ci = ci_busy()
        unit = unit_busy(timer)
        if load1 < quiet_load and not ci and not unit:
            return
        note(log, f"{stamp()} waiting: load {load()} ci {ci} unit {unit}")
        time.sleep(POLL_S)


def bench_dir() -> Path:
    """The stores' directory, the one `just perf` uses."""
    named = os.environ.get("SWARMOPS_BENCH_DIR")
    return Path(named) if named else Path.home() / ".local" / "state" / "swarmops-bench"


def run_step(name: str, out: Path, log) -> int:
    """Runs one step from the root, its whole output to `<out>/<name>.log`; returns its rc."""
    env, cmd = STEPS[name]
    env = {**os.environ, "TMPDIR": str(bench_dir()), **env}
    started = time.time()
    note(log, f"{stamp()} {name} start load {load()}")
    with open(out / f"{name}.log", "w") as step_log:
        rc = subprocess.run(cmd, cwd=ROOT, env=env, stdout=step_log, stderr=subprocess.STDOUT).returncode
    note(log, f"{stamp()} {name} end rc {rc} wall {time.time() - started:.1f} s load {load()} ci {ci_busy()}")
    return rc


def parse(argv: list[str]) -> dict | None:
    """The options and steps, or None on a usage error."""
    opts = {"out": DEFAULT_OUT, "quiet_load": DEFAULT_QUIET_LOAD, "pause": None, "list": False, "steps": []}
    args = iter(argv)
    for arg in args:
        if arg == "--list":
            opts["list"] = True
            continue
        if arg in ("--out", "--quiet-load", "--pause"):
            value = next(args, None)
            if value is None:
                return None
            if arg == "--out":
                opts["out"] = Path(value)
            elif arg == "--pause":
                opts["pause"] = value
            elif value.replace(".", "", 1).isdigit():
                opts["quiet_load"] = float(value)
            else:
                return None
            continue
        if arg not in STEPS:
            return None
        opts["steps"].append(arg)
    opts["steps"] = opts["steps"] or list(STEPS)
    return opts


def main() -> int:
    opts = parse(sys.argv[1:])
    if opts is None:
        print(USAGE, file=sys.stderr)
        print(f"steps: {' '.join(STEPS)}", file=sys.stderr)
        return 2

    # 1. --list: what each step would run, nothing else.
    if opts["list"]:
        for name in opts["steps"]:
            env, cmd = STEPS[name]
            words = [f"{key}={value}" for key, value in env.items()] + cmd
            print(f"{name}: {' '.join(words)}")
        return 0

    out = opts["out"]
    out.mkdir(parents=True, exist_ok=True)
    bench_dir().mkdir(parents=True, exist_ok=True)
    timer = opts["pause"]
    failed = 0

    # 2. Wait for a quiet host, then pause the timer for the window; start it again whatever
    #    happens. Each step waits for a quiet host again before it starts.
    with open(out / "window.log", "a") as log:
        wait_quiet(log, opts["quiet_load"], timer)
        if timer is not None:
            subprocess.run(["systemctl", "--user", "stop", timer], check=True)
        note(log, f"{stamp()} window opens; paused {timer}; steps {opts['steps']}")
        try:
            for name in opts["steps"]:
                wait_quiet(log, opts["quiet_load"], timer)
                failed += run_step(name, out, log) != 0
        finally:
            if timer is not None:
                subprocess.run(["systemctl", "--user", "start", timer], check=False)
            note(log, f"{stamp()} window closes; {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
