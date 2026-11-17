# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Checks `--version` in the stamped builds, which the rest of the gate never makes.

    python3 -I scripts/check-stamped.py

`SWARMOPS_BUILD_COMMIT` and `SWARMOPS_BUILD_CHANNEL` are read at compile time, and the gate
builds without them, so a build that drops the commit, the channel or a compile-time guard passes
there. This script runs the `version` unit tests and the two `--version` pins twice: with the
commit set to a fixed 40-hex one, and with the channel set to `release`, as release.yml builds
(the pins then hold that the channel changes nothing `--version` prints). Then it builds the CLI
with a malformed commit, with a channel that is none, and with a commit and the channel together,
and requires each build to fail with its guard's message. Each filter runs on its own and must
match a test, so a renamed pin fails here instead of dropping out. Last, `swarmops` and `so` in
target/debug are linked again with the caller's environment, so the gate leaves no binary naming
the made-up commit or counting as a release's. Only swarmops-cli is rebuilt for each value (a few
seconds, incremental). Exit 1 on a failure.
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

VARIABLE = "SWARMOPS_BUILD_COMMIT"
STAMP = "0123456789abcdef0123456789abcdef01234567"
MALFORMED = "XYZ"
GUARD = "SWARMOPS_BUILD_COMMIT must be 7 to 40 lowercase hex digits"

CHANNEL_VARIABLE = "SWARMOPS_BUILD_CHANNEL"
CHANNEL = "release"

# The builds the tests run in, and the builds that must fail with the guard's message each
# names: the two variables' values, in every case with neither inherited from the caller.
STAMPED = ({VARIABLE: STAMP}, {CHANNEL_VARIABLE: CHANNEL})
REFUSED = (
    ({VARIABLE: MALFORMED}, GUARD),
    ({CHANNEL_VARIABLE: "nightly"}, "SWARMOPS_BUILD_CHANNEL must be `release`, or unset"),
    (
        {VARIABLE: STAMP, CHANNEL_VARIABLE: CHANNEL},
        "SWARMOPS_BUILD_CHANNEL and SWARMOPS_BUILD_COMMIT cannot both be set",
    ),
)

UNIT_TESTS = "version::tests::"
PINS = ("cli::usage_error_exits_2_with_envelope", "so::so_is_the_same_program_as_swarmops")

# `--profile default`: under `NEXTEST_PROFILE=ci` this run's report must not replace the
# workspace run's JUnit report. `--no-tests=fail` with one filter a run: each must match a test.
NEXTEST = [
    "cargo", "nextest", "run", "--locked", "--profile", "default", "-p", "swarmops-cli",
    "--all-features", "--no-tests=fail", "-E",
]
CARGO_TEST = ["cargo", "test", "--locked", "-p", "swarmops-cli", "--all-features"]
BUILD = ["cargo", "build", "--locked", "-p", "swarmops-cli", "--lib", "--all-features"]
RELINK = ["cargo", "build", "--locked", "-p", "swarmops-cli", "--bins", "--all-features"]


def run(argv: list[str], stamps: dict[str, str] | None) -> subprocess.CompletedProcess[str]:
    """Runs `argv` from the root with the two variables set as `stamps` says and to nothing
    else, its output captured; `None` keeps the caller's environment."""
    env = dict(os.environ)
    if stamps is not None:
        env.pop(VARIABLE, None)
        env.pop(CHANNEL_VARIABLE, None)
        env.update(stamps)
    return subprocess.run(argv, cwd=ROOT, env=env, capture_output=True, text=True)


def said(stamps: dict[str, str]) -> str:
    """`stamps` as a command line would set them."""
    return " ".join(f"{name}={value}" for name, value in stamps.items())


def has_nextest() -> bool:
    """Whether `cargo nextest` is installed (scripts/check.sh falls back to `cargo test`)."""
    if shutil.which("cargo") is None:
        return False
    probe = subprocess.run(["cargo", "nextest", "--version"], capture_output=True)
    return probe.returncode == 0


def ran_tests(done: subprocess.CompletedProcess[str]) -> bool:
    """Whether a `cargo test` run passed and ran at least one test."""
    if done.returncode != 0:
        return False
    return "test result: ok. 0 passed" not in done.stdout and "test result: ok." in done.stdout


def ran_one(done: subprocess.CompletedProcess[str]) -> bool:
    """Whether a `cargo test --exact` run passed and ran exactly one test."""
    return done.returncode == 0 and "test result: ok. 1 passed" in done.stdout


def stamped_tests(stamps: dict[str, str]) -> str | None:
    """Runs the tests in a build stamped with `stamps`; a failure's output, else None."""
    # 1. nextest: the unit tests, then each pin, every run required to match a test.
    if has_nextest():
        filters = [f"test(/^{UNIT_TESTS}/)", *(f"test(={pin})" for pin in PINS)]
        for expr in filters:
            done = run(NEXTEST + [expr], stamps)
            if done.returncode != 0:
                return done.stdout + done.stderr
        return None

    # 2. Without nextest: the unit tests, then each pin, every run required to run its tests.
    unit = run(CARGO_TEST + ["--lib", "--", UNIT_TESTS], stamps)
    if not ran_tests(unit):
        return unit.stdout + unit.stderr
    for pin in PINS:
        done = run(CARGO_TEST + ["--test", "all", "--", "--exact", pin], stamps)
        if not ran_one(done):
            return f"{pin} did not run exactly once:\n{done.stdout}{done.stderr}"
    return None


def refused_build(stamps: dict[str, str], guard: str) -> str | None:
    """Builds with `stamps`, which a guard refuses; why that is wrong, else None."""
    done = run(BUILD, stamps)
    if done.returncode == 0:
        return f"a build with {said(stamps)} succeeded: the compile-time guard is off"
    if guard not in done.stderr:
        return f"a build with {said(stamps)} failed without the guard's message:\n{done.stderr}"
    return None


def relink() -> str | None:
    """Links `swarmops` and `so` again with the caller's environment; a failure's output, else
    None. The stamped runs left them naming the made-up commit or counting as a release's."""
    done = run(RELINK, None)
    return None if done.returncode == 0 else done.stdout + done.stderr


def checks() -> int:
    """The tests in each stamped build, then the refused builds; 0 when all hold, else 1."""
    for stamps in STAMPED:
        failure = stamped_tests(stamps)
        if failure is not None:
            print(failure, file=sys.stderr)
            print(f"check-stamped: the tests failed with {said(stamps)}", file=sys.stderr)
            return 1

    for stamps, guard in REFUSED:
        failure = refused_build(stamps, guard)
        if failure is not None:
            print(f"check-stamped: {failure}", file=sys.stderr)
            return 1

    passed = "; ".join(said(stamps) for stamps in STAMPED)
    refused = "; ".join(said(stamps) for stamps, _ in REFUSED)
    print(f"check-stamped: tests pass with {passed}. Builds fail with {refused}")
    return 0


def main() -> int:
    code = checks()

    # Whatever the checks found, target/debug must keep neither the made-up commit nor the channel.
    failure = relink()
    if failure is not None:
        print(failure, file=sys.stderr)
        print("check-stamped: linking swarmops and so again failed", file=sys.stderr)
        return 1
    return code


if __name__ == "__main__":
    sys.exit(main())
