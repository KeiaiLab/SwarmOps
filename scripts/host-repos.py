# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Refreshes bare mirror clones of this host's repositories, for the host-repository tests.

    uv run --script scripts/host-repos.py <list file> <mirror dir>

The list file names one source repository per line, by its path (blank lines and `#` comments
are passed over). Each becomes `<mirror dir>/<basename>.git`: `git clone --mirror --no-hardlinks`
the first time, `git remote update --prune` after. The mirror's HEAD is then the source's: the
branch it names, or the commit a detached HEAD names, fetched. Only the mirrors are written; a
source is only read (no hard links into its objects). Exit 1 if any source failed, after trying
every one.

A partial clone (`remote.<name>.promisor` or `extensions.partialclone` set) is passed over with a
warning, and a mirror made of it before is removed: it lacks objects its source fetches only on
demand, from the network, into the source. Every git runs with `GIT_NO_LAZY_FETCH=1` for the same
reason.
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path

MIRROR_SUFFIX = ".git"

USAGE = "usage: host-repos.py <list file> <mirror dir>"


# Without system or global configuration, and never fetching a missing object on demand.
GIT_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_NO_LAZY_FETCH": "1",
    "LC_ALL": "C",
}

# What marks a partial clone in its configuration.
PARTIAL_KEYS = r"^(remote\..*\.promisor|extensions\.partialclone)$"


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Runs git in GIT_ENV's environment, its output captured."""
    env = dict(os.environ, **GIT_ENV)
    return subprocess.run(["git", *args], env=env, check=check, capture_output=True, text=True)


def git(*args: str) -> str:
    """Runs git, which must succeed; its stdout, stripped."""
    return run(*args).stdout.strip()


def partial(source: Path) -> bool:
    """Whether `source` is a partial clone, by its configuration alone."""
    found = run("-C", str(source), "config", "--get-regexp", PARTIAL_KEYS, check=False)
    return any(line.split()[-1] != "false" for line in found.stdout.splitlines())


def sources(list_file: Path) -> list[Path]:
    """The source repositories the list file names, in its order."""
    lines = list_file.read_text().splitlines()
    return [Path(line.strip()) for line in lines if line.strip() and not line.startswith("#")]


def refresh(source: Path, mirror: Path) -> None:
    """Clones `source` into `mirror`, or updates it; then sets the mirror's HEAD to the source's."""
    # 1. Clone, or fetch what is new.
    if mirror.exists():
        git("-C", str(mirror), "remote", "update", "--prune")
    else:
        git("clone", "--quiet", "--mirror", "--no-hardlinks", str(source), str(mirror))

    # 2. HEAD as the source has it: a branch, or a detached commit fetched by name.
    branch = run("-C", str(source), "symbolic-ref", "-q", "HEAD", check=False).stdout.strip()
    if branch:
        git("-C", str(mirror), "symbolic-ref", "HEAD", branch)
        return
    head = git("-C", str(source), "rev-parse", "HEAD")
    held = run("-C", str(mirror), "cat-file", "-e", head + "^{commit}", check=False)
    if held.returncode != 0:
        # The source's upload-pack reads this setting from the environment `-c` sets.
        git("-c", "uploadpack.allowAnySHA1InWant=true", "-C", str(mirror), "fetch", "--quiet", "origin", head)
    git("-C", str(mirror), "update-ref", "--no-deref", "HEAD", head)


def main() -> int:
    if len(sys.argv) != 3:
        print(USAGE, file=sys.stderr)
        return 2
    list_file, mirrors = Path(sys.argv[1]), Path(sys.argv[2])
    mirrors.mkdir(parents=True, exist_ok=True)

    # Every source is tried; a failure is reported and counted.
    failed = []
    for source in sources(list_file):
        mirror = mirrors / (source.name + MIRROR_SUFFIX)
        if partial(source):
            print(f"::warning::{source}: a partial clone, passed over")
            shutil.rmtree(mirror, ignore_errors=True)
            continue
        try:
            refresh(source, mirror)
        except subprocess.CalledProcessError as error:
            print(f"{source}: {error}\n{error.stderr}", file=sys.stderr)
            failed.append(source)
            continue
        count = git("--git-dir", str(mirror), "rev-list", "--count", "HEAD")
        print(f"{mirror.name}: HEAD {git('--git-dir', str(mirror), 'rev-parse', 'HEAD')}, {count} commits")

    if failed:
        print(f"failed: {', '.join(map(str, failed))}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
