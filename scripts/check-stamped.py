# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Checks `--version` in a stamped build, which the rest of the gate never makes.

    python3 -I scripts/check-stamped.py

`SWARMOPS_BUILD_COMMIT` is read at compile time, and the gate builds without it, so a build
that drops the commit or its compile-time guard passes there. This script runs the `version`
unit tests and the two `--version` pins with the variable set to a fixed 40-hex commit, then
builds the CLI with a malformed one and requires that build to fail with the guard's message.
Each filter runs on its own and must match a test, so a renamed pin fails here instead of
dropping out. Last, `swarmops` and `so` in target/debug are linked again with the caller's
environment, so the gate leaves no binary naming the made-up commit. Only swarmops-cli is rebuilt
for each value (a few seconds, incremental). Exit 1 on a failure.
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


def run(argv: list[str], commit: str | None) -> subprocess.CompletedProcess[str]:
    """Runs `argv` from the root with the variable set to `commit`, its output captured;
    `None` keeps the caller's environment."""
    env = dict(os.environ) if commit is None else {**os.environ, VARIABLE: commit}
    return subprocess.run(argv, cwd=ROOT, env=env, capture_output=True, text=True)


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


def stamped_tests() -> str | None:
    """Runs the tests in a stamped build; a failure's output, else None."""
    # 1. nextest: the unit tests, then each pin, every run required to match a test.
    if has_nextest():
        filters = [f"test(/^{UNIT_TESTS}/)", *(f"test(={pin})" for pin in PINS)]
        for expr in filters:
            done = run(NEXTEST + [expr], STAMP)
            if done.returncode != 0:
                return done.stdout + done.stderr
        return None

    # 2. Without nextest: the unit tests, then each pin, every run required to run its tests.
    unit = run(CARGO_TEST + ["--lib", "--", UNIT_TESTS], STAMP)
    if not ran_tests(unit):
        return unit.stdout + unit.stderr
    for pin in PINS:
        done = run(CARGO_TEST + ["--test", "all", "--", "--exact", pin], STAMP)
        if not ran_one(done):
            return f"{pin} did not run exactly once:\n{done.stdout}{done.stderr}"
    return None


def malformed_build() -> str | None:
    """Builds with a malformed commit; why that is wrong, else None."""
    done = run(BUILD, MALFORMED)
    if done.returncode == 0:
        return f"a build with {VARIABLE}={MALFORMED} succeeded: the compile-time guard is off"
    if GUARD not in done.stderr:
        return f"a build with {VARIABLE}={MALFORMED} failed without the guard's message:\n{done.stderr}"
    return None


def relink() -> str | None:
    """Links `swarmops` and `so` again with the caller's environment; a failure's output, else
    None. The stamped run left them naming the made-up commit."""
    done = run(RELINK, None)
    return None if done.returncode == 0 else done.stdout + done.stderr


def checks() -> int:
    """The stamped tests, then the malformed build; 0 when both hold, else 1."""
    failure = stamped_tests()
    if failure is not None:
        print(failure, file=sys.stderr)
        print(f"check-stamped: the tests failed with {VARIABLE}={STAMP}", file=sys.stderr)
        return 1

    failure = malformed_build()
    if failure is not None:
        print(f"check-stamped: {failure}", file=sys.stderr)
        return 1

    print(f"check-stamped: tests pass with {VARIABLE}={STAMP}; {MALFORMED} fails the build")
    return 0


def main() -> int:
    code = checks()

    # Whatever the checks found, target/debug must not keep the made-up commit.
    failure = relink()
    if failure is not None:
        print(failure, file=sys.stderr)
        print("check-stamped: linking swarmops and so again failed", file=sys.stderr)
        return 1
    return code


if __name__ == "__main__":
    sys.exit(main())
