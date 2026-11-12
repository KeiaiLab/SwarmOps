# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Decides whether a push or pull request builds the site (the freshness design, section 2.1).

    python3 -I scripts/freshness.py scope --event push --ref refs/heads/stable \\
        --before <sha> --sha <sha> --base ''
    python3 -I scripts/freshness.py selftest

    push to stable, workflow_dispatch    -> build
    push of a v* tag                     -> build stable's head, as branch stable
    push to another branch               -> build when a changed file matches `site_paths`
    pull request                         -> the same, over base...head
    a git command fails, unknown event   -> build (never "nothing to do")

`site_paths` is the list in .github/freshness.json. `scope` prints `build`, `sha` and `branch`
as `key=value` lines and appends them to `$GITHUB_OUTPUT` when that is set: `sha` is the commit
to check out, `branch` the name the deploy is made under. A land-queue candidate (`feat/lq-*`)
never reaches this script: the workflow's `scope` job is skipped for it, and the selftest reads
that condition from site.yml. `selftest` needs neither git nor the network. Exit 1 on a failed
selftest or when a tag push cannot name stable's head.
"""

import argparse
import enum
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CONFIG = ROOT / ".github" / "freshness.json"
SITE_WORKFLOW = ROOT / ".github" / "workflows" / "site.yml"

# GitHub's event names and ref prefixes.
EVENT_PUSH = "push"
EVENT_PULL_REQUEST = "pull_request"
EVENT_DISPATCH = "workflow_dispatch"
HEADS = "refs/heads/"
TAGS = "refs/tags/"

STABLE = "stable"
STABLE_REF = HEADS + STABLE
STABLE_REMOTE = "origin/" + STABLE

# A push that creates a branch has no commit before it: forty zeros.
ZERO_SHA = "0" * 40
SHA_RE = re.compile(r"[0-9a-f]{40}")

# The condition that keeps land-queue candidates off the runners, as site.yml must spell it.
SCOPE_JOB = "scope"
LQ_CONDITION = "${{ !(startsWith(github.ref_name, 'feat/lq-')) }}"

# Runs git with these arguments and returns its output; raises when git fails.
Git = Callable[[list[str]], str]


class Build(enum.Enum):
    """Whether the site job runs."""

    YES = "true"
    NO = "false"


@dataclass(frozen=True)
class Push:
    """What the workflow run was started by, as GitHub names it."""

    event: str
    ref: str
    before: str
    sha: str
    base: str


@dataclass(frozen=True)
class Scope:
    """The verdict: build or not, the commit to check out, the deploy's branch, and why."""

    build: Build
    sha: str
    branch: str
    reason: str


class ScopeError(Exception):
    """No verdict can be given: the run must fail, not skip."""


def run_git(args: list[str]) -> str:
    """git in the repository; a non-zero exit raises."""
    done = subprocess.run(
        ["git", *args], cwd=ROOT, check=True, capture_output=True, text=True
    )
    return done.stdout.strip()


def pattern_re(pattern: str) -> re.Pattern[str]:
    """`web/**` as a regular expression: `**` crosses `/`, `*` and `?` stay inside one name."""
    out = []
    index = 0
    while index < len(pattern):
        if pattern.startswith("**", index):
            out.append(".*")
            index += 2
            continue

        char = pattern[index]
        index += 1
        if char == "*":
            out.append("[^/]*")
        elif char == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(char))
    return re.compile("".join(out))


def first_match(changed: list[str], patterns: list[str]) -> str | None:
    """The first changed file one of the patterns matches, or None."""
    compiled = [pattern_re(pattern) for pattern in patterns]
    for name in changed:
        if any(one.fullmatch(name) for one in compiled):
            return name
    return None


def branch_of(ref: str) -> str:
    """`refs/heads/feat/x` -> `feat/x`; any other ref without its `refs/<kind>/` prefix."""
    if ref.startswith(HEADS):
        return ref[len(HEADS) :]
    return re.sub(r"^refs/[^/]+/", "", ref)


def require_sha(name: str, value: str) -> str:
    """`value` when it is 40 hex; else a ValueError naming the argument."""
    if not SHA_RE.fullmatch(value):
        raise ValueError(f"{name} is not a commit: {value!r}")
    return value


def diff_range(push: Push, git: Git) -> list[str]:
    """The arguments of `git diff --name-only` that name what this run changes."""
    sha = require_sha("sha", push.sha)

    # 1. A pull request: what the head adds to its base.
    if push.event == EVENT_PULL_REQUEST:
        return [f"{require_sha('base', push.base)}...{sha}"]

    # 2. A new branch has no `before`: what it adds to stable.
    if push.before == ZERO_SHA:
        return [git(["merge-base", STABLE_REMOTE, sha]), sha]

    # 3. Any other push: from the commit before it.
    return [require_sha("before", push.before), sha]


def changed_files(push: Push, git: Git) -> list[str]:
    """The files this run changes; raises when a sha is missing or git fails."""
    out = git(["diff", "--name-only", *diff_range(push, git)])
    return [line for line in out.splitlines() if line]


def decide(push: Push, site_paths: list[str], git: Git) -> Scope:
    """The verdict for one run. Every doubt builds."""
    branch = branch_of(push.ref)

    def scope(build: Build, reason: str) -> Scope:
        return Scope(build, push.sha, branch, reason)

    # 1. A release tag: the site names the release, so stable's head is built again. The tag's
    # own commit may be behind stable, and production never goes back.
    if push.event == EVENT_PUSH and push.ref.startswith(TAGS):
        try:
            head = git(["rev-parse", STABLE_REMOTE])
        except (subprocess.CalledProcessError, OSError) as error:
            raise ScopeError(f"tag push: {STABLE_REMOTE} cannot be read: {error}") from error
        if not SHA_RE.fullmatch(head):
            raise ScopeError(f"tag push: {STABLE_REMOTE} is {head!r}, not a commit")
        return Scope(Build.YES, head, STABLE, f"tag push: stable's head {head}")

    # 2. Stable and a run started by hand always build.
    if push.event == EVENT_DISPATCH:
        return scope(Build.YES, "workflow_dispatch")
    if push.event == EVENT_PUSH and push.ref == STABLE_REF:
        return scope(Build.YES, "push to stable")
    if push.event not in (EVENT_PUSH, EVENT_PULL_REQUEST):
        return scope(Build.YES, f"unknown event {push.event!r}")

    # 3. Another branch or a pull request: by the files it changes.
    try:
        changed = changed_files(push, git)
    except (subprocess.CalledProcessError, OSError, ValueError) as error:
        return scope(Build.YES, f"the diff failed: {error}")

    hit = first_match(changed, site_paths)
    if hit is None:
        return scope(Build.NO, f"none of {len(changed)} changed files is a site path")
    return scope(Build.YES, f"{hit} is a site path")


def read_site_paths() -> list[str]:
    """`site_paths` of .github/freshness.json; anything but a list of strings is an error."""
    paths = json.loads(CONFIG.read_text(encoding="utf-8")).get("site_paths")
    if not isinstance(paths, list) or not paths or not all(isinstance(p, str) for p in paths):
        raise ScopeError(f"{CONFIG.name}: site_paths must be a non-empty list of strings")
    return paths


def job_condition(workflow: str, job: str) -> str | None:
    """The `if:` of a workflow job, read as text: the line at job-key depth, or None."""
    inside = False
    for line in workflow.splitlines():
        # A job opens at two spaces; its own keys sit at four.
        if re.fullmatch(r"  [\w-]+:\s*", line):
            inside = line.strip() == f"{job}:"
            continue
        match = re.fullmatch(r"    if:\s*(.+?)\s*", line)
        if inside and match:
            return match.group(1)
    return None


def cmd_scope(args: argparse.Namespace) -> int:
    push = Push(args.event, args.ref, args.before, args.sha, args.base)
    try:
        verdict = decide(push, read_site_paths(), run_git)
    except ScopeError as error:
        print(f"freshness scope: {error}", file=sys.stderr)
        return 1

    # The outputs, for the log and for the jobs that need them.
    lines = [
        f"build={verdict.build.value}",
        f"sha={verdict.sha}",
        f"branch={verdict.branch}",
    ]
    print("\n".join(lines))
    print(f"reason: {verdict.reason}")

    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")
    return 0


# --- selftest: fixed inputs, a git that answers from a table ---------------------------------

SHA_OLD = "1" * 40
SHA_NEW = "2" * 40
SHA_FORK = "3" * 40
SHA_STABLE = "4" * 40

SITE_PATHS = ["web/**", "docs/**", "CHANGELOG.md", ".github/workflows/site.yml"]
CRATE_FILE = "crates/swarmops-core/src/lib.rs"
WEB_FILE = "web/app/layout.tsx"


class FakeGit:
    """Answers from a table keyed by the joined arguments, and records each call."""

    def __init__(self, answers: dict[str, str]) -> None:
        self.answers = answers
        self.calls: list[str] = []

    def __call__(self, args: list[str]) -> str:
        key = " ".join(args)
        self.calls.append(key)
        if key not in self.answers:
            raise subprocess.CalledProcessError(128, ["git", *args])
        return self.answers[key]


def push_of(ref: str, before: str = SHA_OLD, event: str = EVENT_PUSH, base: str = "") -> Push:
    return Push(event, ref, before, SHA_NEW, base)


def crates_only_stable_push_builds() -> str | None:
    git = FakeGit({f"diff --name-only {SHA_OLD} {SHA_NEW}": CRATE_FILE})
    got = decide(push_of(STABLE_REF), SITE_PATHS, git)
    return expect(got, Scope(Build.YES, SHA_NEW, STABLE, "push to stable"))


def crates_only_feat_push_skips() -> str | None:
    git = FakeGit({f"diff --name-only {SHA_OLD} {SHA_NEW}": CRATE_FILE})
    got = decide(push_of(HEADS + "feat/x"), SITE_PATHS, git)
    return expect(
        got, Scope(Build.NO, SHA_NEW, "feat/x", "none of 1 changed files is a site path")
    )


def web_feat_push_builds() -> str | None:
    git = FakeGit({f"diff --name-only {SHA_OLD} {SHA_NEW}": f"{CRATE_FILE}\n{WEB_FILE}"})
    got = decide(push_of(HEADS + "feat/x"), SITE_PATHS, git)
    return expect(got, Scope(Build.YES, SHA_NEW, "feat/x", f"{WEB_FILE} is a site path"))


def diff_failure_builds() -> str | None:
    # No answer for the diff: the fake git fails as git does for an unknown commit.
    got = decide(push_of(HEADS + "feat/x"), SITE_PATHS, FakeGit({}))
    if got.build is not Build.YES or not got.reason.startswith("the diff failed: "):
        return f"got {got}"
    return None


def dispatch_builds() -> str | None:
    git = FakeGit({})
    got = decide(push_of(HEADS + "feat/x", event=EVENT_DISPATCH), SITE_PATHS, git)
    if git.calls:
        return f"a dispatch asked git: {git.calls}"
    return expect(got, Scope(Build.YES, SHA_NEW, "feat/x", "workflow_dispatch"))


def new_branch_diffs_against_merge_base() -> str | None:
    git = FakeGit(
        {
            f"merge-base {STABLE_REMOTE} {SHA_NEW}": SHA_FORK,
            f"diff --name-only {SHA_FORK} {SHA_NEW}": WEB_FILE,
        }
    )
    got = decide(push_of(HEADS + "feat/x", before=ZERO_SHA), SITE_PATHS, git)
    if f"diff --name-only {SHA_FORK} {SHA_NEW}" not in git.calls:
        return f"the diff did not start at the merge base: {git.calls}"
    return expect(got, Scope(Build.YES, SHA_NEW, "feat/x", f"{WEB_FILE} is a site path"))


def new_branch_crates_only_skips() -> str | None:
    git = FakeGit(
        {
            f"merge-base {STABLE_REMOTE} {SHA_NEW}": SHA_FORK,
            f"diff --name-only {SHA_FORK} {SHA_NEW}": CRATE_FILE,
        }
    )
    got = decide(push_of(HEADS + "feat/x", before=ZERO_SHA), SITE_PATHS, git)
    return expect(
        got, Scope(Build.NO, SHA_NEW, "feat/x", "none of 1 changed files is a site path")
    )


def new_branch_without_merge_base_builds() -> str | None:
    got = decide(push_of(HEADS + "feat/x", before=ZERO_SHA), SITE_PATHS, FakeGit({}))
    if got.build is not Build.YES or not got.reason.startswith("the diff failed: "):
        return f"got {got}"
    return None


def pull_request_diffs_base_to_head() -> str | None:
    git = FakeGit({f"diff --name-only {SHA_OLD}...{SHA_NEW}": CRATE_FILE})
    push = push_of("refs/pull/7/merge", before="", event=EVENT_PULL_REQUEST, base=SHA_OLD)
    got = decide(push, SITE_PATHS, git)
    return expect(
        got, Scope(Build.NO, SHA_NEW, "7/merge", "none of 1 changed files is a site path")
    )


def pull_request_without_base_builds() -> str | None:
    push = push_of("refs/pull/7/merge", before="", event=EVENT_PULL_REQUEST, base="")
    got = decide(push, SITE_PATHS, FakeGit({}))
    if got.build is not Build.YES or "is not a commit" not in got.reason:
        return f"got {got}"
    return None


def tag_push_builds_stable_head() -> str | None:
    git = FakeGit({f"rev-parse {STABLE_REMOTE}": SHA_STABLE})
    got = decide(push_of(TAGS + "v0.12.0", before=ZERO_SHA), SITE_PATHS, git)
    return expect(
        got, Scope(Build.YES, SHA_STABLE, STABLE, f"tag push: stable's head {SHA_STABLE}")
    )


def tag_push_without_stable_fails() -> str | None:
    try:
        got = decide(push_of(TAGS + "v0.12.0", before=ZERO_SHA), SITE_PATHS, FakeGit({}))
    except ScopeError as error:
        if f"{STABLE_REMOTE} cannot be read" in str(error):
            return None
        return f"wrong message: {error}"
    return f"no failure, got {got}"


def unknown_event_builds() -> str | None:
    got = decide(push_of(STABLE_REF, event="schedule"), SITE_PATHS, FakeGit({}))
    return expect(got, Scope(Build.YES, SHA_NEW, STABLE, "unknown event 'schedule'"))


def double_star_crosses_directories() -> str | None:
    cases = [
        ("web/**", "web/app/now/page.tsx", True),
        ("web/**", "website/x", False),
        ("docs/status/**", "docs/status/M20/M20-1.txt", True),
        ("CHANGELOG.md", "CHANGELOG.md", True),
        ("CHANGELOG.md", "docs/CHANGELOG.md", False),
        ("scripts/*.sh", "scripts/site/x.sh", False),
        ("scripts/*.sh", "scripts/install.sh", True),
    ]
    for pattern, name, want in cases:
        if (first_match([name], [pattern]) is not None) != want:
            return f"{pattern} against {name}: expected {want}"
    return None


def lq_candidate_is_skipped_by_the_job_condition() -> str | None:
    # 1. The reader finds a job's own `if`, not a step's and not another job's.
    sample = "jobs:\n  other:\n    if: no\n  scope:\n    steps:\n      - if: step\n    if: yes\n"
    if job_condition(sample, SCOPE_JOB) != "yes":
        return f"the reader took {job_condition(sample, SCOPE_JOB)!r} from the sample"

    # 2. site.yml's `scope` job carries the condition, so a candidate takes no runner.
    found = job_condition(SITE_WORKFLOW.read_text(encoding="utf-8"), SCOPE_JOB)
    if found != LQ_CONDITION:
        return f"site.yml job `{SCOPE_JOB}` has `if: {found}`, expected `{LQ_CONDITION}`"
    return None


def site_paths_are_readable() -> str | None:
    paths = read_site_paths()
    for name in ("web/package.json", ".github/workflows/site.yml", "docs/status/M20/M20-1.txt"):
        if first_match([name], paths) is None:
            return f"{name} matches no site path of {CONFIG.name}"
    if first_match([CRATE_FILE], paths) is not None:
        return f"{CRATE_FILE} matches a site path of {CONFIG.name}"
    return None


def expect(got: Scope, want: Scope) -> str | None:
    """None when equal, else both values."""
    if got == want:
        return None
    return f"got {got}, expected {want}"


SELFTESTS: list[Callable[[], str | None]] = [
    crates_only_stable_push_builds,
    crates_only_feat_push_skips,
    web_feat_push_builds,
    diff_failure_builds,
    dispatch_builds,
    new_branch_diffs_against_merge_base,
    new_branch_crates_only_skips,
    new_branch_without_merge_base_builds,
    pull_request_diffs_base_to_head,
    pull_request_without_base_builds,
    tag_push_builds_stable_head,
    tag_push_without_stable_fails,
    unknown_event_builds,
    double_star_crosses_directories,
    lq_candidate_is_skipped_by_the_job_condition,
    site_paths_are_readable,
]


def cmd_selftest(_args: argparse.Namespace) -> int:
    failed = 0
    for test in SELFTESTS:
        problem = test()
        if problem is None:
            print(f"ok   {test.__name__}")
            continue
        failed += 1
        print(f"FAIL {test.__name__}: {problem}")

    print(f"freshness selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} passed")
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)

    scope = commands.add_parser("scope", help="decide whether this run builds the site")
    for flag in ("--event", "--ref", "--before", "--sha", "--base"):
        scope.add_argument(flag, required=True)
    scope.set_defaults(run=cmd_scope)

    selftest = commands.add_parser("selftest", help="run the offline cases")
    selftest.set_defaults(run=cmd_selftest)

    args = parser.parse_args()
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main())
