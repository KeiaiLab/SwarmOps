# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fails a criterion run whose median regresses against bench/thresholds.toml, offline.

    python3 -I scripts/bench-gate.py check <criterion dir> <thresholds>
    python3 -I scripts/bench-gate.py baseline <run 1 dir> <run 2 dir> --measured <date> --run <text>
    python3 -I scripts/bench-gate.py selftest

`check` reads every `<id dir>/new/estimates.json` under the criterion directory, the bench's id
being `full_id` of the `new/benchmark.json` beside it (`BenchmarkId::new(name, param)` ids hold
`/` and nest two directories deep, so the path does not name the bench). A finding prints as
`rule: id: message`. The rules, by the id a finding prints:

  regression    The median is more than REGRESSION times the baseline's `median_ns`.
  missing       A `[bench]` entry has no estimate: the bench did not run.
  unlisted      An estimate's id is in neither `[bench]` nor `[skip]`.
  skip-reason   A `[skip]` entry has no `reason`.
  entry         A `[bench]` entry lacks `median_ns` (a positive number), `measured` or `run`, or
                has a key the file does not define; or an id is in both tables.
  estimate      An estimate file, or the benchmark.json beside it, is unreadable.
  thresholds    The thresholds file is missing or not TOML.
  no-dir        The criterion directory does not exist.
  empty         The directory holds no estimate at all: nothing was measured.
  loaded        A regression measured under load that no quiet rerun decided: LOADED, NO VERDICT.

A regression is measured again only if the run ended loaded. `check` then reads `/proc/loadavg`
once: under the measurement gate (1-minute load < LOAD1_MAX, 5-minute < LOAD5_MAX) the findings
stand, never best-of-two. Over it, `check` waits at most GATE_WAIT_MAX_S for the gate held HELD_S,
runs the flagged benches again (`cargo bench --locked --workspace -- --exact <id>`, from the
working directory) and compares their new medians: still over is REGRESSION, under is CLEARED BY
A QUIET RERUN, a gate that never opens or a rerun that ends loaded is LOADED, NO VERDICT. Each
prints both medians and both loads; only a clearance passes.

`baseline` writes a thresholds file from two quiet runs to stdout: a bench whose medians differ by
less than INCLUSION_SPREAD (of the smaller) enters `[bench]` with their mean, every other one
`[skip]` with its measured spread as the reason. Raising a baseline (allowing slower) takes two
new quiet runs, so a new `measured` and `run`; REGRESSION itself is never raised.

`selftest` crafts a criterion directory and a thresholds file per case in a temporary directory
and checks that each case reports its rules and nothing else, and that the clean case passes.

Exit 0 when clean, 1 on a finding, 2 on a usage error. Python 3.11 or newer (`tomllib`), stdlib
only, no network.
"""

import argparse
import contextlib
import io
import json
import math
import subprocess
import sys
import tempfile
import time
import tomllib
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

NAME = "bench-gate"

# Exit codes.
CLEAN = 0
FOUND = 1
USAGE = 2

# A median above this many times its baseline fails the gate: a 20 % regression.
REGRESSION = 1.20

# A bench enters `[bench]` when its two baseline medians differ by less than this share.
INCLUSION_SPREAD = 0.10

# Criterion's layout under its output directory, and the keys read from its files.
ESTIMATES = "estimates.json"
BENCHMARK = "benchmark.json"
NEW_DIR = "new"
FULL_ID = "full_id"
MEDIAN = "median"
POINT = "point_estimate"

# The measurement gate (D2 design §11 step 1): 1-minute load under LOAD1_MAX and 5-minute
# under LOAD5_MAX, both held HELD_S seconds. A loaded run's regression waits at most
# GATE_WAIT_MAX_S for it, polling every POLL_S, before its benches run again.
LOAD1_MAX = 2.0
LOAD5_MAX = 6.0
HELD_S = 300
GATE_WAIT_MAX_S = 600
POLL_S = 10
LOADAVG = Path("/proc/loadavg")

# The verdicts a rerun prints.
STILL_OVER = "REGRESSION"
CLEARED = "CLEARED BY A QUIET RERUN"
NO_VERDICT = "LOADED, NO VERDICT"

# The thresholds file's tables and keys.
BENCH_TABLE = "bench"
SKIP_TABLE = "skip"
BENCH_KEYS = frozenset({"median_ns", "measured", "run", "reason"})
BENCH_REQUIRED = ("measured", "run")
SKIP_KEYS = frozenset({"reason", "measured", "run"})


@dataclass(frozen=True, order=True)
class Finding:
    """A reason the gate fails, by rule id and the bench (or path) it is about."""

    rule: str
    subject: str
    message: str

    def __str__(self) -> str:
        return f"{self.rule}: {self.subject}: {self.message}"


@dataclass
class Report:
    """What `check` found: the findings, the lines it prints for the benches compared, and
    their baselines by id."""

    findings: list[Finding]
    lines: list[str]
    estimates: int
    baselines: dict[str, float] = field(default_factory=dict)


# Where `check` prints a line.
Out = Callable[[str], None]


@dataclass(frozen=True)
class Load:
    """The 1- and 5-minute load averages."""

    one: float
    five: float

    def quiet(self) -> bool:
        """Under the measurement gate."""
        return self.one < LOAD1_MAX and self.five < LOAD5_MAX

    def __str__(self) -> str:
        return f"load {self.one:.2f} {self.five:.2f}"


class Host:
    """The machine `check` runs on: its load, its clock and the bench runner."""

    def load(self) -> Load:
        """The load averages `/proc/loadavg` holds now."""
        one, five = LOADAVG.read_text(encoding="ascii").split()[:2]
        return Load(float(one), float(five))

    def now(self) -> float:
        return time.monotonic()

    def sleep(self, seconds: float) -> None:
        time.sleep(seconds)

    def bench(self, bench_id: str) -> int:
        """Runs the bench `bench_id` alone, as `just bench-run` would; its exit code."""
        command = ["cargo", "bench", "--locked", "--workspace", "--", "--exact", bench_id]
        return subprocess.run(command, check=False).returncode


def toml_key(key: str) -> str:
    """`key` as a quoted TOML key."""
    return json.dumps(key)


def read_estimates(root: Path) -> tuple[dict[str, float], list[Finding]]:
    """The median, in nanoseconds, of every estimate under the criterion directory `root`, by
    `full_id`; and a finding for each estimate it cannot read."""
    medians: dict[str, float] = {}
    findings: list[Finding] = []

    # 1. Every `new/estimates.json`, wherever the id's `/` nested it.
    for path in sorted(root.rglob(f"{NEW_DIR}/{ESTIMATES}")):
        place = str(path.relative_to(root))
        try:
            estimates = json.loads(path.read_text(encoding="utf-8"))
            benchmark = json.loads((path.parent / BENCHMARK).read_text(encoding="utf-8"))
            bench_id = benchmark[FULL_ID]
            median = float(estimates[MEDIAN][POINT])
        except (OSError, ValueError, KeyError, TypeError) as error:
            findings.append(Finding("estimate", place, f"unreadable: {error}"))
            continue

        # 2. Two estimates of one id would hide one of them.
        if not isinstance(bench_id, str) or bench_id in medians:
            findings.append(Finding("estimate", place, f"id {bench_id!r} unusable or repeated"))
            continue
        medians[bench_id] = median
    return medians, findings


def read_thresholds(path: Path) -> tuple[dict, list[Finding]]:
    """The thresholds file at `path`, parsed; a finding instead when it cannot be."""
    try:
        return tomllib.loads(path.read_text(encoding="utf-8")), []
    except (OSError, tomllib.TOMLDecodeError) as error:
        return {}, [Finding("thresholds", path.name, f"unreadable: {error}")]


def entry_findings(bench: dict, skip: dict) -> list[Finding]:
    """The findings of the thresholds file's own entries, whatever the run measured."""
    findings: list[Finding] = []

    # 1. `[bench]`: a positive baseline, where and when it was measured, no unknown key.
    for bench_id, entry in sorted(bench.items()):
        if not isinstance(entry, dict):
            findings.append(Finding("entry", bench_id, "is not a table"))
            continue
        median = entry.get("median_ns")
        if isinstance(median, bool) or not isinstance(median, (int, float)) or not median > 0:
            findings.append(Finding("entry", bench_id, "needs a positive `median_ns`"))
        for key in BENCH_REQUIRED:
            if not str(entry.get(key, "")).strip():
                findings.append(Finding("entry", bench_id, f"needs `{key}`"))
        for key in sorted(set(entry) - BENCH_KEYS):
            findings.append(Finding("entry", bench_id, f"unknown key `{key}`"))

    # 2. `[skip]`: a reason, no unknown key, and not listed in `[bench]` as well.
    for bench_id, entry in sorted(skip.items()):
        if not isinstance(entry, dict) or not str(entry.get("reason", "")).strip():
            findings.append(Finding("skip-reason", bench_id, "a skip needs a `reason`"))
            continue
        for key in sorted(set(entry) - SKIP_KEYS):
            findings.append(Finding("entry", bench_id, f"unknown key `{key}`"))
        if bench_id in bench:
            findings.append(Finding("entry", bench_id, "in both `[bench]` and `[skip]`"))
    return findings


def check(root: Path, thresholds: Path) -> Report:
    """Compares the criterion run under `root` with the baselines in `thresholds`."""
    if not root.is_dir():
        return Report([Finding("no-dir", str(root), "no criterion directory")], [], 0)

    # 1. Both inputs, read; an unreadable thresholds file fails before anything is compared.
    medians, findings = read_estimates(root)
    table, unreadable = read_thresholds(thresholds)
    if unreadable:
        return Report(findings + unreadable, [], len(medians))
    bench = table.get(BENCH_TABLE, {})
    skip = table.get(SKIP_TABLE, {})
    findings += entry_findings(bench, skip)
    if not medians and not findings:
        findings.append(Finding("empty", str(root), "no estimate found: nothing was measured"))

    # 2. Every listed bench against its baseline.
    lines: list[str] = []
    baselines: dict[str, float] = {}
    for bench_id, entry in sorted(bench.items()):
        if bench_id not in medians:
            findings.append(Finding("missing", bench_id, "listed, but the run has no estimate"))
            continue
        baseline = entry.get("median_ns") if isinstance(entry, dict) else None
        if isinstance(baseline, bool) or not isinstance(baseline, (int, float)) or baseline <= 0:
            continue
        baselines[bench_id] = baseline
        ratio = medians[bench_id] / baseline
        verdict = "ok" if ratio <= REGRESSION else "REGRESSION"
        lines.append(
            f"{bench_id}: {medians[bench_id]:.1f} ns, baseline {baseline:.1f} ns,"
            f" {100 * (ratio - 1):+.1f} % {verdict}"
        )
        if ratio > REGRESSION:
            findings.append(
                Finding("regression", bench_id, f"{ratio:.3f}x the baseline, over {REGRESSION}x")
            )

    # 3. Every estimate is accounted for: compared, or skipped for a reason.
    for bench_id in sorted(set(medians) - set(bench) - set(skip)):
        findings.append(Finding("unlisted", bench_id, "in neither `[bench]` nor `[skip]`"))
    return Report(sorted(set(findings)), lines, len(medians), baselines)


def spread(first: float, second: float) -> float:
    """How far two medians differ, as a share of the smaller."""
    return abs(first - second) / min(first, second)


def baseline(first: dict[str, float], second: dict[str, float], measured: str, run: str) -> str:
    """The thresholds file two quiet runs give: the inclusion rule applied to every bench."""
    out = [
        "# Written by `scripts/bench-gate.py baseline` from two quiet runs; the inclusion rule:",
        f"# medians less than {100 * INCLUSION_SPREAD:.0f} % apart enter [bench] with their mean.",
        "",
    ]
    skips: list[str] = []

    # 1. A bench both runs measured, by the inclusion rule.
    for bench_id in sorted(set(first) | set(second)):
        if bench_id not in first or bench_id not in second:
            skips += ["", f"[skip.{toml_key(bench_id)}]", 'reason = "measured by one run only"']
            continue
        gap = spread(first[bench_id], second[bench_id])
        if gap >= INCLUSION_SPREAD:
            reason = f"spread {100 * gap:.1f} % between the two baseline runs"
            skips += ["", f"[skip.{toml_key(bench_id)}]", f"reason = {toml_key(reason)}"]
            continue
        mean = (first[bench_id] + second[bench_id]) / 2
        out += [
            f"[bench.{toml_key(bench_id)}]",
            f"median_ns = {mean:.1f}",
            f"measured = {toml_key(measured)}",
            f"run = {toml_key(run)}",
            f"# spread {100 * gap:.1f} %",
            "",
        ]
    return "\n".join(out + skips).rstrip("\n") + "\n"


def write_estimate(root: Path, bench_id: str, median: float) -> None:
    """Writes one estimate as criterion does, under a directory per `/` of the id."""
    new = root.joinpath(*bench_id.split("/"), NEW_DIR)
    new.mkdir(parents=True, exist_ok=True)
    (new / BENCHMARK).write_text(json.dumps({FULL_ID: bench_id}), encoding="utf-8")
    estimates = {MEDIAN: {POINT: median}, "mean": {POINT: median}}
    (new / ESTIMATES).write_text(json.dumps(estimates), encoding="utf-8")


# The selftest's baseline file: two benches compared, one skipped for a reason.
SELFTEST_THRESHOLDS = """
[bench."group/a/10"]
median_ns = 1000.0
measured = "2026-10-28"
run = "selftest"

[bench."group/b"]
median_ns = 2000.0
measured = "2026-10-28"
run = "selftest"

[skip."group/noisy"]
reason = "spread 30.0 % between the two baseline runs"
"""

# The clean run: every listed bench within REGRESSION, the skipped one present.
CLEAN_RUN = {"group/a/10": 1000.0, "group/b": 2000.0, "group/noisy": 50.0}


@dataclass(frozen=True)
class Case:
    """A crafted run: its estimates, its thresholds file and the rules it must report."""

    name: str
    estimates: dict[str, float] | None
    thresholds: str
    rules: frozenset[str]
    # Ids whose benchmark.json the case removes, as a cut-short run leaves them.
    broken: tuple[str, ...] = ()


def selftest_cases() -> list[Case]:
    """Each rule's crafted case, the 19 % pass beside the 21 % fail, and the clean run."""
    t = SELFTEST_THRESHOLDS
    return [
        Case("clean", CLEAN_RUN, t, frozenset()),
        Case("regression 19 %", CLEAN_RUN | {"group/a/10": 1190.0}, t, frozenset()),
        Case("regression 21 %", CLEAN_RUN | {"group/a/10": 1210.0}, t, frozenset({"regression"})),
        Case("missing", {k: v for k, v in CLEAN_RUN.items() if k != "group/b"}, t,
             frozenset({"missing"})),
        Case("unlisted", CLEAN_RUN | {"group/new": 10.0}, t, frozenset({"unlisted"})),
        Case("reasonless skip", CLEAN_RUN, t.replace('reason = "spread 30.0 %', 'measured = "x'),
             frozenset({"skip-reason"})),
        Case("no measured", CLEAN_RUN, t.replace('measured = "2026-10-28"\n', "", 1),
             frozenset({"entry"})),
        Case("no run", CLEAN_RUN, t.replace('run = "selftest"\n', "", 1), frozenset({"entry"})),
        Case("unreadable estimate", CLEAN_RUN, t, frozenset({"estimate", "missing"}), ("group/b",)),
        Case("bad thresholds", CLEAN_RUN, "[bench\n", frozenset({"thresholds"})),
        Case("empty", {}, "", frozenset({"empty"})),
        Case("no dir", None, t, frozenset({"no-dir"})),
    ]


def selftest_baseline() -> list[str]:
    """The inclusion rule: 9 % apart enters `[bench]`, 11 % apart is skipped, and so is a bench
    one run lacks; the file written parses and passes `check` on the first run."""
    first = {"x/in": 1000.0, "x/out": 1000.0, "x/once": 5.0}
    second = {"x/in": 1090.0, "x/out": 1110.0}
    table = tomllib.loads(baseline(first, second, "2026-10-28", "selftest"))
    problems: list[str] = []
    if set(table.get(BENCH_TABLE, {})) != {"x/in"}:
        problems.append(f"baseline: [bench] is {sorted(table.get(BENCH_TABLE, {}))}")
    if set(table.get(SKIP_TABLE, {})) != {"x/out", "x/once"}:
        problems.append(f"baseline: [skip] is {sorted(table.get(SKIP_TABLE, {}))}")
    if not math.isclose(table.get(BENCH_TABLE, {}).get("x/in", {}).get("median_ns", 0), 1045.0):
        problems.append("baseline: the median of x/in is not the two runs' mean")
    return problems


class FakeHost(Host):
    """A host on a virtual clock: `loads(t)` is its load at second t, and a bench takes
    FAKE_BENCH_S and rewrites its estimate to `rerun[id]`."""

    def __init__(self, root: Path, loads: Callable[[float], Load], rerun: dict[str, float]):
        self.root = root
        self.loads = loads
        self.rerun = rerun
        self.clock = 0.0
        self.reads = 0
        self.benched: list[str] = []

    def load(self) -> Load:
        self.reads += 1
        return self.loads(self.clock)

    def now(self) -> float:
        return self.clock

    def sleep(self, seconds: float) -> None:
        self.clock += seconds

    def bench(self, bench_id: str) -> int:
        self.benched.append(bench_id)
        self.clock += FAKE_BENCH_S
        write_estimate(self.root, bench_id, self.rerun[bench_id])
        return CLEAN


# How long a fake bench runs.
FAKE_BENCH_S = 60

QUIET = Load(0.5, 1.2)
LOADED = Load(9.0, 12.0)

# A run of `group/a/10` 25 % over its baseline.
SLOW_RUN = CLEAN_RUN | {"group/a/10": 1250.0}


def rerun_case(
    estimates: dict[str, float], loads: Callable[[float], Load], rerun: dict[str, float]
) -> tuple[int, str, FakeHost]:
    """`check` on a crafted run with a fake host: its exit code, its output and the host."""
    with tempfile.TemporaryDirectory(prefix=f"{NAME}-") as name:
        root = Path(name) / "criterion"
        root.mkdir()
        for bench_id, median in estimates.items():
            write_estimate(root, bench_id, median)
        thresholds = Path(name) / "thresholds.toml"
        thresholds.write_text(SELFTEST_THRESHOLDS, encoding="utf-8")
        host = FakeHost(root, loads, rerun)
        lines: list[str] = []
        with contextlib.redirect_stderr(io.StringIO()):
            code = gate(root, thresholds, host, lines.append)
    return code, "\n".join(lines), host


def loaded_until(second: float) -> Callable[[float], Load]:
    """Loaded before `second`, quiet from then on."""
    return lambda t: LOADED if t < second else QUIET


def rerun_keeps_a_regression() -> list[str]:
    """A loaded run's regression that the quiet rerun repeats fails, with both medians."""
    code, out, host = rerun_case(SLOW_RUN, loaded_until(30), {"group/a/10": 1240.0})
    if code != FOUND or STILL_OVER not in out or host.benched != ["group/a/10"]:
        return [f"rerun_keeps_a_regression: exit {code}, reran {host.benched}: {out!r}"]
    if "1250.0" not in out or "1240.0" not in out:
        return [f"rerun_keeps_a_regression: not both medians: {out!r}"]
    return []


def rerun_clears_noise_and_prints_both() -> list[str]:
    """A loaded run's regression that the quiet rerun does not repeat passes, and says so with
    both medians and both loads."""
    code, out, host = rerun_case(SLOW_RUN, loaded_until(30), {"group/a/10": 1010.0})
    if code != CLEAN or CLEARED not in out or host.benched != ["group/a/10"]:
        return [f"rerun_clears_noise_and_prints_both: exit {code}, reran {host.benched}: {out!r}"]
    if not all(text in out for text in ("1250.0", "1010.0", str(LOADED), str(QUIET))):
        return [f"rerun_clears_noise_and_prints_both: not both medians and loads: {out!r}"]
    return []


def loaded_no_verdict_fails() -> list[str]:
    """A gate that never opens fails without a rerun; so does a rerun that ends loaded."""
    problems: list[str] = []
    code, out, host = rerun_case(SLOW_RUN, lambda t: LOADED, {"group/a/10": 1010.0})
    if code != FOUND or NO_VERDICT not in out or host.benched:
        problems.append(f"loaded_no_verdict_fails: exit {code}, reran {host.benched}: {out!r}")

    # The gate opens, but the rerun itself ends loaded: under, yet no pass.
    late = lambda t: QUIET if 30 <= t < 30 + HELD_S + POLL_S else LOADED
    code, out, host = rerun_case(SLOW_RUN, late, {"group/a/10": 1010.0})
    if code != FOUND or NO_VERDICT not in out or CLEARED in out:
        problems.append(f"loaded_no_verdict_fails: loaded rerun: exit {code}: {out!r}")
    return problems


def no_finding_runs_no_rerun() -> list[str]:
    """A clean run reads no load and runs nothing again."""
    code, out, host = rerun_case(CLEAN_RUN, lambda t: LOADED, {})
    if code != CLEAN or host.reads or host.benched:
        return [f"no_finding_runs_no_rerun: exit {code}, {host.reads} reads, {host.benched}"]
    return []


def a_quiet_first_run_gets_no_rerun() -> list[str]:
    """A regression measured under the gate stands: one load read, no wait, no rerun."""
    code, out, host = rerun_case(SLOW_RUN, lambda t: QUIET, {"group/a/10": 1010.0})
    if code != FOUND or STILL_OVER not in out or host.benched or host.clock or host.reads != 1:
        return [
            f"a_quiet_first_run_gets_no_rerun: exit {code}, {host.reads} reads,"
            f" {host.clock} s, reran {host.benched}: {out!r}"
        ]
    return []


def the_wait_is_bounded() -> list[str]:
    """A gate that never opens is given up after GATE_WAIT_MAX_S."""
    code, out, host = rerun_case(SLOW_RUN, lambda t: LOADED, {"group/a/10": 1010.0})
    if code != FOUND or not GATE_WAIT_MAX_S <= host.clock <= GATE_WAIT_MAX_S + POLL_S:
        return [f"the_wait_is_bounded: exit {code} after {host.clock} s"]
    return []


def threshold_is_still_1_20() -> list[str]:
    """The threshold and the measurement gate are the ones the plan fixed."""
    fixed = (REGRESSION, LOAD1_MAX, LOAD5_MAX, HELD_S, GATE_WAIT_MAX_S)
    if fixed != (1.20, 2.0, 6.0, 300, 600):
        return [f"threshold_is_still_1_20: {fixed}"]
    return []


RERUN_TESTS = (
    rerun_keeps_a_regression,
    rerun_clears_noise_and_prints_both,
    loaded_no_verdict_fails,
    no_finding_runs_no_rerun,
    a_quiet_first_run_gets_no_rerun,
    the_wait_is_bounded,
    threshold_is_still_1_20,
)


def selftest() -> int:
    """Every case reports exactly its rules, and the clean one nothing."""
    start = time.perf_counter()
    problems: list[str] = []
    with tempfile.TemporaryDirectory(prefix=f"{NAME}-") as name:
        for number, case in enumerate(selftest_cases()):
            # 1. The case's criterion directory (none for "no dir") and thresholds file.
            root = Path(name) / f"case-{number}" / "criterion"
            if case.estimates is not None:
                root.mkdir(parents=True)
                for bench_id, median in case.estimates.items():
                    write_estimate(root, bench_id, median)
                for bench_id in case.broken:
                    root.joinpath(*bench_id.split("/"), NEW_DIR, BENCHMARK).unlink()
            thresholds = Path(name) / f"case-{number}.toml"
            thresholds.write_text(case.thresholds, encoding="utf-8")

            # 2. Exactly its rules.
            got = frozenset(finding.rule for finding in check(root, thresholds).findings)
            if got != case.rules:
                problems.append(f"{case.name}: reported {sorted(got)}, want {sorted(case.rules)}")
    problems += selftest_baseline()
    for test in RERUN_TESTS:
        problems += test()
    for problem in problems:
        print(f"{NAME}: selftest: {problem}")
    if problems:
        return FOUND

    print(
        f"{NAME}: selftest: {len(selftest_cases())} crafted runs, each its rules only, the clean"
        f" one clean; the inclusion rule; {len(RERUN_TESTS)} rerun cases"
        f" ({time.perf_counter() - start:.2f} s)"
    )
    return CLEAN


def wait_for_gate(host: Host) -> bool:
    """Whether the measurement gate held HELD_S within GATE_WAIT_MAX_S."""
    start = host.now()
    held_since: float | None = None
    while True:
        # 1. Quiet now: the hold goes on (or starts); loaded: it starts again.
        now = host.now()
        if host.load().quiet():
            held_since = now if held_since is None else held_since
            if now - held_since >= HELD_S:
                return True
        else:
            held_since = None

        # 2. Bounded: the wait never outlasts GATE_WAIT_MAX_S.
        if now - start >= GATE_WAIT_MAX_S:
            return False
        host.sleep(POLL_S)


def rerun(root: Path, report: Report, host: Host, out: Out) -> list[Finding]:
    """The regressions of a loaded run, measured again under the gate: the findings that stand.
    A regression measured under the gate stands as it is; a cleared one is printed, not kept."""
    flagged = [finding for finding in report.findings if finding.rule == "regression"]
    first_load = host.load()

    # 1. Measured under the gate: never best-of-two.
    if first_load.quiet():
        out(f"{NAME}: {STILL_OVER}: the run ended under the gate ({first_load}), no rerun")
        return flagged

    # 2. Loaded: wait for the gate, bounded; a gate that never opens gives no verdict.
    out(f"{NAME}: the run ended loaded ({first_load}): waiting for the gate")
    if not wait_for_gate(host):
        return [
            Finding("loaded", finding.subject, f"{NO_VERDICT}: the gate stayed shut"
                    f" {GATE_WAIT_MAX_S} s after a run at {first_load}")
            for finding in flagged
        ]

    # 3. Only the flagged benches again, into the same criterion directory.
    first, _ = read_estimates(root)
    failed = {finding.subject for finding in flagged if host.bench(finding.subject) != CLEAN}
    second, _ = read_estimates(root)
    second_load = host.load()

    # 4. Each one compared again: still over, cleared, or measured under load after all.
    standing: list[Finding] = []
    for finding in flagged:
        bench_id = finding.subject
        ratio = second[bench_id] / report.baselines[bench_id] if bench_id in second else None
        both = (
            f"first {first[bench_id]:.1f} ns ({first_load}),"
            f" rerun {second.get(bench_id, math.nan):.1f} ns ({second_load})"
        )
        if bench_id in failed:
            standing.append(Finding("regression", bench_id, f"{STILL_OVER}: {both}, rerun failed"))
        elif ratio is None or ratio > REGRESSION:
            standing.append(Finding("regression", bench_id, f"{STILL_OVER}: {both}"))
        elif not second_load.quiet():
            standing.append(Finding("loaded", bench_id, f"{NO_VERDICT}: {both}"))
        else:
            out(f"{NAME}: {CLEARED}: {bench_id}: {both}, {100 * (ratio - 1):+.1f} %")
    return standing


def gate(root: Path, thresholds: Path, host: Host, out: Out) -> int:
    """`check`: prints the comparison and the findings; exit 1 on any finding. A loaded run's
    regressions are measured again under the gate first (`rerun`)."""
    report = check(root, thresholds)
    for line in report.lines:
        out(line)

    # 1. Regressions: the load decides whether they stand or run again.
    findings = report.findings
    if any(finding.rule == "regression" for finding in findings):
        others = [finding for finding in findings if finding.rule != "regression"]
        findings = sorted(set(others + rerun(root, report, host, out)))

    for finding in findings:
        out(f"{NAME}: {finding}")
    print(
        f"{NAME}: {report.estimates} estimates, {len(report.lines)} compared,"
        f" {len(findings)} findings",
        file=sys.stderr,
    )
    return FOUND if findings else CLEAN


def run_check(root: Path, thresholds: Path) -> int:
    """`check` on this machine, printing to stdout."""
    return gate(root, thresholds, Host(), print)


def run_baseline(first: Path, second: Path, measured: str, run: str) -> int:
    """`baseline`: the thresholds file of two runs, to stdout; exit 1 if either is unreadable."""
    runs = []
    for root in (first, second):
        medians, findings = read_estimates(root) if root.is_dir() else ({}, [])
        if findings or not medians:
            for finding in findings or [Finding("empty", str(root), "no estimate found")]:
                print(f"{NAME}: {finding}", file=sys.stderr)
            return FOUND
        runs.append(medians)
    sys.stdout.write(baseline(runs[0], runs[1], measured, run))
    return CLEAN


def main() -> int:
    parser = argparse.ArgumentParser(prog="bench-gate.py", description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    gate = commands.add_parser("check", help="compare a criterion run with the baselines")
    gate.add_argument("criterion", type=Path)
    gate.add_argument("thresholds", type=Path)
    base = commands.add_parser("baseline", help="write a thresholds file from two quiet runs")
    base.add_argument("first", type=Path)
    base.add_argument("second", type=Path)
    base.add_argument("--measured", required=True)
    base.add_argument("--run", required=True)
    commands.add_parser("selftest", help="prove every rule on crafted runs")
    args = parser.parse_args()

    if args.command == "check":
        return run_check(args.criterion, args.thresholds)
    if args.command == "baseline":
        return run_baseline(args.first, args.second, args.measured, args.run)
    return selftest()


if __name__ == "__main__":
    sys.exit(main())
