#!/usr/bin/env python3
"""The gate's doc-directory removal refuses a target directory that is empty, not absolute
or the root however written.

The line is read from the justfile and from scripts/check.sh, which must carry it unchanged, and
run with stand-ins on PATH: a `cargo` whose `metadata` fails or prints a given target directory,
and an `rm` that records its arguments instead of removing anything.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MARK = '["target_directory"]'

# Each shell runs the line: just's and check.sh's options, and none, so the guard holds alone.
SHELLS = (["bash", "-euo", "pipefail", "-c"], ["sh", "-c"])


def removal_line(path: Path) -> str:
    """The one line of `path` that reads the target directory."""
    lines = [line.strip() for line in path.read_text().splitlines() if MARK in line]
    if len(lines) != 1:
        sys.exit(f"test-doc-clean: {path.name}: {len(lines)} removal lines, want 1")
    return lines[0]


def stand_in(bin_dir: Path, name: str, body: str) -> None:
    """A shell script `name` in `bin_dir`."""
    path = bin_dir / name
    path.write_text("#!/bin/sh\n" + body)
    path.chmod(0o755)


def run(line: str, shell: list[str], cargo_body: str) -> tuple[int, str]:
    """Exit code of `line` and what the `rm` stand-in received ("" when not run)."""
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp)
        log = bin_dir / "rm.log"
        stand_in(bin_dir, "cargo", cargo_body)
        stand_in(bin_dir, "rm", f'printf "%s " "$@" >> "{log}"\n')

        env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}")
        done = subprocess.run([*shell, line], env=env, capture_output=True, text=True)
        called = log.read_text().strip() if log.exists() else ""
        return done.returncode, called


def prints(target: str) -> str:
    """A `cargo` body that prints metadata naming `target`."""
    return f"echo '{json.dumps({'target_directory': target})}'\n"


def main() -> None:
    paths = (ROOT / "justfile", ROOT / "scripts/check.sh")
    lines = {path.name: removal_line(path) for path in paths}
    failures = []

    # 1. Refused: metadata fails, or names an empty or relative directory, or one that
    #    normalises to the root ("//", "/.", "/a/..").
    refused = {
        "metadata fails": "exit 101\n",
        "empty": prints(""),
        "relative": prints("target"),
        "root": prints("/"),
    }
    for root in ("//", "///", "/.", "/./", "/..", "//.", "/a/.."):
        refused[f"root as {root}"] = prints(root)
    for name, line in lines.items():
        for shell in SHELLS:
            for case, body in refused.items():
                code, called = run(line, shell, body)
                if code == 0 or called:
                    failures.append(f"{name}, {shell[0]}, {case}: exit {code}, rm got [{called}]")

            # 2. Removed: an absolute target directory loses its doc directory only.
            code, called = run(line, shell, prints("/data/t"))
            if code != 0 or called != "-rf /data/t/doc":
                failures.append(f"{name}, {shell[0]}, absolute: exit {code}, rm got [{called}]")

    # 3. check.sh carries the justfile's line unchanged.
    if lines["justfile"] != lines["check.sh"]:
        failures.append("check.sh's removal line differs from the justfile's")

    if failures:
        sys.exit("test-doc-clean:\n  " + "\n  ".join(failures))
    print("test-doc-clean: ok")


if __name__ == "__main__":
    main()
