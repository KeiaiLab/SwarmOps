# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fails a release whose confirmed golden files differ from the previous release's, offline.

    python3 -I scripts/golden-frozen.py check [<tag>]  # against <tag>, or the previous release
    python3 -I scripts/golden-frozen.py selftest       # proves every rule on a crafted repository

The FORMAT 3 design's "개정 2026-11-11" (G8, as judged): a byte layout is a contract from the
first release that writes it, so every golden file that existed at the previous release tag is
still present at HEAD, byte for byte. `git diff --no-renames --name-status <tag> HEAD -- <paths>`
may print only `A` lines (files added since). The paths are FROZEN: the hash contract's golden
vectors and FORMAT 3's confirmed ones; the provisional fact bodies (`golden3/provisional/`) may
change until their feature is released, so they are left out.

Without a tag, the previous release is the newest `v*` tag merged into HEAD that does not name
HEAD itself. A tag that does not resolve fails (fail-closed); a resolved tag without a path passes
(the first release that lists the format). A finding prints as `rule: message`:

  changed   A golden file of the tag is modified or deleted at HEAD.
  no-tag    The tag does not resolve to a commit, or no previous release tag exists.
  git       git failed otherwise.

Exit 0 when clean, 1 on a finding, 2 on a usage error. Python 3.11 or newer, stdlib only, git,
no network.
"""

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

NAME = "golden-frozen"
CLEAN = 0
FOUND = 1

# The golden paths that never change once released (the hash contract, FORMAT 3's confirmed set).
FROZEN = (
    "crates/swarmops-core/tests/golden",
    "crates/swarmops-store/tests/golden3",
)

# Provisional fact bodies: in place until their feature is released (G2, G9).
PROVISIONAL = "crates/swarmops-store/tests/golden3/provisional"

# The tag pattern of a release (RELEASE.md: an annotated `vX.Y.Z` tag).
RELEASE_TAGS = "v[0-9]*"

# The one status `git diff --name-status` may print: a file added since the tag.
ADDED = "A"


def git(repo: Path, *args: str) -> subprocess.CompletedProcess:
    """Runs git in `repo` with a fixed environment, capturing text."""
    env = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "LC_ALL": "C"}
    return subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, env=env, check=False
    )


def previous_tag(repo: Path) -> str | None:
    """The newest release tag merged into HEAD that does not name HEAD itself."""

    # 1. Every release tag HEAD holds, newest version first.
    listed = git(repo, "tag", "--merged", "HEAD", "--list", RELEASE_TAGS, "--sort=-v:refname")
    if listed.returncode != 0:
        return None
    head = git(repo, "rev-parse", "HEAD").stdout.strip()

    # 2. The first one that is not the release being cut.
    for tag in listed.stdout.split():
        if git(repo, "rev-parse", f"{tag}^{{commit}}").stdout.strip() != head:
            return tag
    return None


def check(repo: Path, tag: str | None) -> list[str]:
    """The findings of HEAD against `tag` (or the previous release tag)."""

    # 1. The tag resolves to a commit, or nothing is compared.
    if tag is None:
        tag = previous_tag(repo)
    if tag is None:
        return ["no-tag: no previous release tag (v*) is merged into HEAD"]
    if git(repo, "rev-parse", "--verify", "--quiet", f"{tag}^{{commit}}").returncode != 0:
        return [f"no-tag: {tag} does not resolve to a commit"]

    # 2. Every line but an addition is a released golden file changed in place.
    paths = [*FROZEN, f":(exclude){PROVISIONAL}"]
    diff = git(repo, "diff", "--no-renames", "--name-status", tag, "HEAD", "--", *paths)
    if diff.returncode != 0:
        return [f"git: git diff against {tag} failed: {diff.stderr.strip()}"]
    findings = []
    for line in diff.stdout.splitlines():
        status, _, path = line.partition("\t")
        if status == ADDED:
            continue
        findings.append(f"changed: {path} ({status} since {tag}): a released golden file is frozen")
    return findings


def run_check(tag: str | None) -> int:
    """Checks the working repository; prints each finding."""
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").stdout.strip() or ".")
    findings = check(root, tag)
    for finding in findings:
        print(f"{NAME}: {finding}", file=sys.stderr)
    if findings:
        return FOUND
    print(f"{NAME}: every released golden file is unchanged")
    return CLEAN


def commit(repo: Path, message: str) -> None:
    """Commits everything in `repo` under a fixed identity and date."""
    git(repo, "add", "-A")
    git(
        repo,
        "-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid",
        "commit", "-q", "--no-gpg-sign", "--allow-empty", "-m", message,
    )


def write(repo: Path, rel: str, text: str) -> None:
    """Writes one file of the crafted repository."""
    path = repo / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def crafted(base: Path, name: str) -> Path:
    """A repository whose release v1 holds a core golden, a confirmed one and a provisional one."""
    repo = base / name
    repo.mkdir()
    git(repo, "init", "-q")
    write(repo, f"{FROZEN[0]}/blob.hex", "00\n")
    write(repo, f"{FROZEN[1]}/marker.hex", "01\n")
    write(repo, f"{PROVISIONAL}/fact_3_0.hex", "02\n")
    commit(repo, "release 1")
    git(repo, "tag", "v1.0.0")
    return repo


def selftest() -> int:
    """Each planted change is caught by its rule; the clean cases pass."""
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        base = Path(tmp)

        # 1. Clean: an added golden and a changed provisional body pass.
        repo = crafted(base, "clean")
        write(repo, f"{FROZEN[1]}/run_c.hex", "03\n")
        write(repo, f"{PROVISIONAL}/fact_3_0.hex", "04\n")
        commit(repo, "next")
        if found := check(repo, None):
            failures.append(f"clean: {found}")

        # 2. A modified file, then a deleted one: each is `changed`.
        for case, plant in (
            ("modified", lambda r: write(r, f"{FROZEN[0]}/blob.hex", "ff\n")),
            ("deleted", lambda r: (r / FROZEN[1] / "marker.hex").unlink()),
        ):
            repo = crafted(base, case)
            plant(repo)
            commit(repo, case)
            found = check(repo, None)
            if len(found) != 1 or not found[0].startswith("changed: "):
                failures.append(f"{case}: {found}")

        # 3. An unresolvable tag, and no previous tag at all: `no-tag`.
        repo = crafted(base, "untagged")
        commit(repo, "next")
        found = check(repo, "v9.9.9")
        if len(found) != 1 or not found[0].startswith("no-tag: "):
            failures.append(f"unresolvable tag: {found}")
        git(repo, "tag", "-d", "v1.0.0")
        found = check(repo, None)
        if len(found) != 1 or not found[0].startswith("no-tag: "):
            failures.append(f"no previous tag: {found}")

        # 4. A resolved tag without the paths passes (the first release of a format).
        repo = base / "empty"
        repo.mkdir()
        git(repo, "init", "-q")
        write(repo, "README", "x\n")
        commit(repo, "release 0")
        git(repo, "tag", "v0.1.0")
        write(repo, f"{FROZEN[1]}/marker.hex", "01\n")
        commit(repo, "format 3")
        if found := check(repo, None):
            failures.append(f"tag without the paths: {found}")

        # 5. The release being cut is not its own previous release.
        git(repo, "tag", "v0.2.0")
        write(repo, f"{FROZEN[1]}/marker.hex", "ff\n")
        commit(repo, "cut")
        git(repo, "tag", "v0.3.0")
        found = check(repo, None)
        if len(found) != 1 or "since v0.2.0" not in found[0]:
            failures.append(f"previous of a tagged HEAD: {found}")

    for failure in failures:
        print(f"{NAME}: selftest: {failure}", file=sys.stderr)
    if failures:
        return FOUND
    print(f"{NAME}: selftest: every planted change is caught, the clean cases pass")
    return CLEAN


def main() -> int:
    parser = argparse.ArgumentParser(prog="golden-frozen.py", description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    gate = commands.add_parser("check", help="compare the golden files with a release tag")
    gate.add_argument("tag", nargs="?", default=None)
    commands.add_parser("selftest", help="prove every rule on crafted repositories")
    args = parser.parse_args()

    if args.command == "check":
        return run_check(args.tag)
    return selftest()


if __name__ == "__main__":
    sys.exit(main())
