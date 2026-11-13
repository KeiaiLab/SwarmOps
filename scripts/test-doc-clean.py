#!/usr/bin/env python3
"""The gate's doc runs write to a fresh directory of their own, never to a shared one, and refuse
a target directory that is empty, not absolute or the root however written.

The doc line is read from the justfile and from scripts/check.sh, which must carry it unchanged
(a line ending in `\\` joined to the next), and run with stand-ins on PATH: a `cargo` whose
`metadata` fails or prints a given target directory and whose `doc` records its arguments and
environment, and an `rm` that records its arguments instead of removing anything.
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

# The two doc runs, in order, with the flags the gate has always given them.
DOC_RUNS = (
    "doc --locked --workspace --no-deps --document-private-items",
    "doc --locked --workspace --no-deps --document-private-items --all-features",
)
DOC_FLAGS = "-D warnings"

# The `cargo doc` stand-in: one log line per run, with the directories it was given and whether
# its output directory existed.
CARGO_DOC = """if [ "$1" = doc ]; then
  [ -d "$CARGO_TARGET_DIR" ] && made=yes || made=no
  printf '%s|%s|%s|%s|%s\\n' "$*" "$CARGO_TARGET_DIR" "$CARGO_BUILD_BUILD_DIR" "$RUSTDOCFLAGS" \\
    "$made" >> "$DOC_LOG"
  exit "$DOC_EXIT"
fi
"""


def doc_line(path: Path) -> str:
    """The one shell line of `path` that reads the target directory, continuations joined."""
    lines = [line.strip() for line in path.read_text().splitlines()]
    starts = [i for i, line in enumerate(lines) if MARK in line]
    if len(starts) != 1:
        sys.exit(f"test-doc-clean: {path.name}: {len(starts)} doc lines, want 1")

    # 1. Join each line ending in `\` to the next, as the shell does.
    joined, i = "", starts[0]
    while lines[i].endswith("\\"):
        joined += lines[i][:-1] + " "
        i += 1
    return joined + lines[i]


def stand_in(bin_dir: Path, name: str, body: str) -> None:
    """A shell script `name` in `bin_dir`."""
    path = bin_dir / name
    path.write_text("#!/bin/sh\n" + body)
    path.chmod(0o755)


def run(line: str, shell: list[str], meta: str, target: Path, doc_exit: int = 0):
    """Exit code of `line`, what the `rm` stand-in received ("" when not run) and the doc
    runs' log lines, split on `|`."""
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp)
        rm_log, doc_log = bin_dir / "rm.log", bin_dir / "doc.log"
        stand_in(bin_dir, "cargo", CARGO_DOC + meta)
        stand_in(bin_dir, "rm", f'printf "%s " "$@" >> "{rm_log}"\n')

        env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", DOC_LOG=str(doc_log))
        env["DOC_EXIT"] = str(doc_exit)
        for name in ("CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR", "RUSTDOCFLAGS"):
            env.pop(name, None)
        done = subprocess.run([*shell, line], env=env, cwd=target, capture_output=True, text=True)

        called = rm_log.read_text().strip() if rm_log.exists() else ""
        docs = doc_log.read_text().splitlines() if doc_log.exists() else []
        return done.returncode, called, [d.split("|") for d in docs]


def prints(target: str) -> str:
    """A `cargo` body that prints metadata naming `target`."""
    return f"echo '{json.dumps({'target_directory': target})}'\n"


def ran_apart(name: str, shell: str, target: Path, code: int, rm: str, docs, want: int) -> list:
    """What is wrong with a run that should have run `want` doc runs, in a fresh directory below
    `target` that it then removed, leaving `target/doc` alone."""
    if len(docs) != want:
        return [f"{name}, {shell}: {len(docs)} doc runs, want {want}: {docs}"]

    problems = []
    for (args, out, build, flags, made), expected in zip(docs, DOC_RUNS):
        out_dir = Path(out)
        if args != expected or flags != DOC_FLAGS:
            problems.append(f"{name}, {shell}: ran `{args}` with RUSTDOCFLAGS [{flags}]")
        if out_dir.parent != target or out_dir == target / "doc" or made != "yes":
            problems.append(f"{name}, {shell}: doc output in [{out}], made: {made}")
        if build != str(target):
            problems.append(f"{name}, {shell}: build directory [{build}], want {target}")

    if len({d[1] for d in docs}) != 1:
        problems.append(f"{name}, {shell}: the doc runs used different directories")
    if rm != f"-rf {docs[0][1]}":
        problems.append(f"{name}, {shell}: rm got [{rm}], want its own doc directory")
    return problems


def main() -> None:
    paths = (ROOT / "justfile", ROOT / "scripts/check.sh")
    lines = {path.name: doc_line(path) for path in paths}
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
    with tempfile.TemporaryDirectory() as tmp:
        target = Path(tmp).resolve()
        (target / "doc").mkdir()
        used = set()
        for name, line in lines.items():
            for shell in SHELLS:
                for case, body in refused.items():
                    code, called, docs = run(line, shell, body, target)
                    if code == 0 or called or docs:
                        failures.append(f"{name}, {shell[0]}, {case}: exit {code}, rm got "
                                        f"[{called}], doc runs {len(docs)}")

                # 2. Both doc runs write to one directory below the target directory that no
                #    earlier run used, build in the shared one, and the line removes that
                #    directory alone.
                code, called, docs = run(line, shell, prints(str(target)), target)
                failures += ran_apart(name, shell[0], target, code, called, docs, 2)
                if code != 0:
                    failures.append(f"{name}, {shell[0]}: exit {code}")
                if docs and docs[0][1] in used:
                    failures.append(f"{name}, {shell[0]}: doc output in a used [{docs[0][1]}]")
                used.update(d[1] for d in docs)

                # 3. A failing first run fails the line, skips the second and still removes.
                code, called, docs = run(line, shell, prints(str(target)), target, doc_exit=1)
                failures += ran_apart(name, shell[0], target, code, called, docs, 1)
                if code == 0:
                    failures.append(f"{name}, {shell[0]}: a failing doc run exited 0")

    # 4. check.sh carries the justfile's line unchanged.
    if lines["justfile"] != lines["check.sh"]:
        failures.append("check.sh's doc line differs from the justfile's")

    if failures:
        sys.exit("test-doc-clean:\n  " + "\n  ".join(failures))
    print("test-doc-clean: ok")


if __name__ == "__main__":
    main()
