# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Says which builds may install a release automatically, and uploads the release's marks.

    python3 -I scripts/release-marks.py marks X.Y.Z             # print the marks; upload nothing
    python3 -I scripts/release-marks.py marks X.Y.Z --upload    # upload them with gh, and check
    python3 -I scripts/release-marks.py marks X.Y.Z --withdraw  # remove every mark with gh
    python3 -I scripts/release-marks.py selftest                # proves every rule, offline

`just release-marks X.Y.Z` runs the first form, and passes `--upload` or `--withdraw` on.

A build that updates itself installs the latest release only when the release holds the file
`auto-update-from-<major>.<minor>`, named for the minor line the build is of (`so update`'s
reference page, "In the background"). A release without that file is announced to such a build,
not installed. This script computes the files a release gets from CHANGELOG.md's headings:

  1. A line gets a mark when CHANGELOG.md holds a release of that line that is older than
     X.Y.Z, of the same major version, and newer than the installer's UPDATE_AFTER
     (scripts/install.sh): the releases up to that one hold no `so update` and ask for no mark.
  2. A mark is withheld from a line when any release after the line's first (`A.B.0`), up to
     and including X.Y.Z, has a `###` group whose heading says `no automatic update`, such as
     `### Changed (breaking, no automatic update)`. A build of that line would cross that
     release's change on its way. RELEASE.md, step 4, says when a release gets that group: it
     changes what stores older builds can open, or it needs a step by hand.
  3. The words are read wherever the group's heading has them, in any case, and with any
     spacing or punctuation between and around them: `(breaking; no automatic update)`,
     `- no-automatic-update` and `(no automatic updates):` withhold too. A heading of another
     depth that says them, such as `#### Detail (no automatic update)`, or
     `## Changed (no automatic update)` inside the release's section, is a finding when the
     release is on a line's way: it is no group, and a mark given against a heading someone
     wrote to withhold it would be installed unasked.
  4. A group that says `breaking` and not those words withholds nothing; the line is printed
     with it.
  5. A pre-release gets no mark: GitHub's `latest` never names one.

Every line is printed, marked or withheld, with the reason. Only `--upload` and `--withdraw` run
`gh` (the first on PATH), against the release `vX.Y.Z` of KeiaiLab/SwarmOps: `--upload` puts the
marks there, replacing ones of the same name, then lists the release's files and fails unless
its marks are exactly the computed ones; `--withdraw` removes every `auto-update-from-*` file,
which stops the release from being installed automatically, and checks that none is left.

A finding prints as `rule: message`:

  version     X.Y.Z is not three numbers with an optional pre-release.
  no-section  CHANGELOG.md has no `## [X.Y.Z]` section.
  heading     A heading that is no `###` group says `no automatic update`.
  installer   scripts/install.sh names no UPDATE_AFTER this script can read.
  gh          gh is missing, failed, or left the release with other marks than asked.

Exit 0 when done, 1 on a finding, 2 on a usage error. Python 3.11 or newer, stdlib only. No
network without `--upload` or `--withdraw`; `selftest` runs a stand-in for gh, never gh.
"""

import argparse
import contextlib
import io
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

NAME = "release-marks"
DONE = 0
FOUND = 1
USAGE = 2

# The repository whose releases hold the marks, and how a mark's name starts: the product's
# own constants (RELEASES_URL and MARK_PREFIX in crates/swarmops-ops/src/update.rs), which the
# selftest compares these with.
REPO = "KeiaiLab/SwarmOps"
MARK_PREFIX = "auto-update-from-"
PRODUCT_SOURCE = "crates/swarmops-ops/src/update.rs"

# Where the last release without `so update` is named.
INSTALLER = "scripts/install.sh"
UPDATE_AFTER = re.compile(r"^UPDATE_AFTER=(\S+)$", re.MULTILINE)

# The words in a group's heading that withhold a release from automatic update, and the word
# that only flags it: both as `words` gives a heading's.
WITHHOLD = "no automatic update"
BREAKING = "breaking"

# A version as the installer and `so update` read one, a release's section heading, and a
# heading of any depth: its marks and its text. A group's has three marks.
VERSION = re.compile(r"^(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z.-]+)?$")
SECTION = re.compile(r"^## \[([^\]]+)\]")
HEADING = re.compile(r"^ {0,3}(#{1,6})[ \t]+(.*?)\s*$")
GROUP_DEPTH = 3
FENCE = "```"

# How long one run of gh may take.
GH_DEADLINE_S = 120


class Finding(Exception):
    """A reason to stop, printed as `rule: message`."""


@dataclass(frozen=True)
class Line:
    """One minor line's answer for a release."""

    line: str
    marked: bool
    why: str

    @property
    def mark(self) -> str:
        """The name of the line's mark."""
        return MARK_PREFIX + self.line


@dataclass
class Plan:
    """What a release gets: its lines, marked or withheld, and what is said beside them."""

    version: str
    lines: list[Line] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    @property
    def marks(self) -> list[str]:
        """The names of the marks to upload."""
        return [line.mark for line in self.lines if line.marked]


def numbers(version: str) -> tuple[int, int, int] | None:
    """The three numbers of a release's version; None for a pre-release or anything else."""
    found = VERSION.match(version)
    if found is None or found.group(4):
        return None
    return int(found.group(1)), int(found.group(2)), int(found.group(3))


def show(version: tuple[int, int, int]) -> str:
    """A version's text."""
    return ".".join(str(n) for n in version)


@dataclass(frozen=True)
class Heading:
    """A heading of a release's section: its text, whether it is a `###` group's, its words."""

    text: str
    group: bool
    said: str


def words(text: str) -> str:
    """The words of a heading, in lower case with one space between them and nothing else.

    `(Breaking;  no-automatic-update):` gives `breaking no automatic update`, so that the
    words that withhold a release are found however they were spaced and punctuated.
    """
    return " ".join(re.sub(r"[^0-9a-z]+", " ", text.lower()).split())


def sections(changelog: str) -> dict[tuple[int, int, int], list[Heading]]:
    """Each release's group headings, and its other headings that say the withholding words.

    A section is a `## [X.Y.Z]` heading and what follows it, up to the next `## ` heading;
    `Unreleased` and a pre-release's section are no release's. Lines inside a code fence are
    not headings. A `## ` heading that says the words is no boundary to hide them behind: one
    that ends a release's section is that release's heading, as a release's own is its own.
    """
    found: dict[tuple[int, int, int], list[Heading]] = {}
    current = None
    fenced = False
    for line in changelog.splitlines():
        if line.lstrip().startswith(FENCE):
            fenced = not fenced
            continue
        if fenced:
            continue
        if line.startswith("## "):
            heading = SECTION.match(line)
            release = numbers(heading.group(1)) if heading else None
            if release is not None:
                found.setdefault(release, [])
            # Read for the words before it closes the section: it is no group, so the release
            # it stands in, or the one it opens, stops with a finding and gets no mark.
            said = words(line)
            owner = release if release is not None else current
            if owner is not None and WITHHOLD in said:
                found[owner].append(Heading(line.strip(), False, said))
            current = release
            continue
        heading = HEADING.match(line)
        if heading is None or current is None:
            continue
        group = len(heading.group(1)) == GROUP_DEPTH
        said = words(heading.group(2))
        if group or WITHHOLD in said:
            found[current].append(Heading(line.strip(), group, said))
    return found


def update_after(installer: str) -> tuple[int, int, int]:
    """The last release without `so update`, as the installer names it."""
    found = UPDATE_AFTER.search(installer)
    version = numbers(found.group(1)) if found else None
    if version is None:
        raise Finding(f"installer: {INSTALLER} has no line `UPDATE_AFTER=X.Y.Z`")
    return version


def plan(version: str, changelog: str, installer: str) -> Plan:
    """The marks of `version`, from the changelog's headings and the installer's constant."""

    # 1. The version is one a release can have, and the changelog holds its section.
    if VERSION.match(version) is None:
        raise Finding(f"version: {version!r} is not a version: X.Y.Z or X.Y.Z-pre")
    answer = Plan(version)
    release = numbers(version)
    if release is None:
        answer.notes.append(
            f"{version} is a pre-release: GitHub's `latest` never names one, so it gets no mark"
        )
        return answer
    releases = sections(changelog)
    if release not in releases:
        raise Finding(f"no-section: CHANGELOG.md has no `## [{version}]` section")
    first_with_update = update_after(installer)

    # 2. The lines of older releases, oldest first: which could ask for a mark at all.
    older = sorted(v for v in releases if v < release)
    lines = sorted({v[:2] for v in older})
    asking = []
    silent = []
    foreign = []
    for line in lines:
        if line[0] != release[0]:
            foreign.append(line)
        elif any(v[:2] == line and v > first_with_update for v in older):
            asking.append(line)
        else:
            silent.append(line)

    # 3. A line that asks gets its mark unless a release on its way withholds it.
    for line in asking:
        crossed = sorted(v for v in releases if (*line, 0) < v <= release)
        said = [(v, h) for v in crossed for h in releases[v] if WITHHOLD in h.said]
        for v, heading in said:
            if not heading.group:
                raise Finding(
                    f"heading: {show(v)} has the heading `{heading.text}`, which says "
                    f"`{WITHHOLD}` and is no `###` group: say it in a group's heading, such as "
                    f"`### Changed ({WITHHOLD})`, or take the words out"
                )
        withholding = [f'{show(v)} has the group "{heading.text[4:]}"' for v, heading in said]
        breaking = [
            show(v) for v in crossed if any(BREAKING in heading.said for heading in releases[v])
        ]
        name = f"{line[0]}.{line[1]}"
        if withholding:
            answer.lines.append(Line(name, False, "; ".join(withholding)))
        elif breaking:
            why = f"crosses a breaking group of {', '.join(breaking)}, which withholds nothing"
            answer.lines.append(Line(name, True, why))
        else:
            answer.lines.append(Line(name, True, ""))

    # 4. What is said about the lines that get no line of their own.
    if silent:
        last = f"{silent[-1][0]}.{silent[-1][1]}"
        answer.notes.append(
            f"no mark for the {last} line or an older one: no release there holds `so update` "
            f"(UPDATE_AFTER={show(first_with_update)} in {INSTALLER})"
        )
    if foreign:
        last = f"{foreign[-1][0]}.{foreign[-1][1]}"
        answer.notes.append(
            f"no mark for the {last} line or an older one: another major version is never "
            "installed automatically"
        )
    if any(not line.marked for line in answer.lines):
        answer.notes.append(
            "a build of a withheld line is told of the release once and stays as it is until "
            "`so update` is run there: no later release reaches it automatically either, "
            "since each crosses the same change"
        )
    return answer


def report(answer: Plan) -> list[str]:
    """The plan as lines to print."""
    out = [f"SwarmOps {answer.version}, the release v{answer.version} of {REPO}"]
    for line in answer.lines:
        if line.marked:
            out.append(f"mark      {line.mark}" + (f" ({line.why})" if line.why else ""))
        else:
            out.append(f"withheld  {line.mark}: {line.why}")
    if not answer.lines:
        out.append("no mark: no build that updates itself is older than this release")
    out.extend(answer.notes)
    return out


def run_gh(gh: str, *args: str) -> str:
    """Runs gh with `args`; its stdout, or a finding with what it said."""
    try:
        done = subprocess.run(
            [gh, *args], capture_output=True, text=True, check=False, timeout=GH_DEADLINE_S
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Finding(f"gh: {gh} {' '.join(args[:2])} did not run: {error}") from error
    if done.returncode != 0:
        said = done.stderr.strip() or f"exit {done.returncode}"
        raise Finding(f"gh: {gh} {' '.join(args[:2])} failed: {said}")
    return done.stdout


def marks_of(gh: str, tag: str) -> list[str]:
    """The marks the release holds now."""
    listed = run_gh(
        gh, "release", "view", tag, "--repo", REPO, "--json", "assets", "--jq", ".assets[].name"
    )
    return sorted(name for name in listed.split() if name.startswith(MARK_PREFIX))


def upload(gh: str, answer: Plan) -> list[str]:
    """Puts the plan's marks on the release, and checks that it holds exactly those."""
    tag = f"v{answer.version}"

    # 1. The release exists, or nothing is written.
    before = marks_of(gh, tag)

    # 2. Each mark is a file of one line, since a release takes no empty file.
    if answer.marks:
        with tempfile.TemporaryDirectory() as tmp:
            paths = []
            for line in answer.lines:
                if not line.marked:
                    continue
                path = Path(tmp) / line.mark
                path.write_text(
                    f"SwarmOps {answer.version} may be installed automatically by a build of "
                    f"the {line.line} line.\n",
                    encoding="utf-8",
                )
                paths.append(str(path))
            run_gh(gh, "release", "upload", tag, *paths, "--repo", REPO, "--clobber")

    # 3. The release holds the computed marks and no other.
    after = marks_of(gh, tag)
    if after != sorted(answer.marks):
        raise Finding(
            f"gh: {tag} holds the marks {after or 'none'}, not {sorted(answer.marks) or 'none'} "
            f"(it held {before or 'none'} before): `--withdraw` removes every mark"
        )
    return after


def withdraw(gh: str, version: str) -> list[str]:
    """Removes every mark from the release, and checks that none is left."""
    tag = f"v{version}"
    removed = marks_of(gh, tag)
    for name in removed:
        run_gh(gh, "release", "delete-asset", tag, name, "--repo", REPO, "--yes")
    left = marks_of(gh, tag)
    if left:
        raise Finding(f"gh: {tag} still holds {left}")
    return removed


def say(text: str, *, error: bool = False) -> None:
    """Prints one line under the script's name."""
    print(f"{NAME}: {text}", file=sys.stderr if error else sys.stdout)


def run_marks(version: str, do_upload: bool, do_withdraw: bool) -> int:
    """Prints the release's marks, and uploads or withdraws them when asked."""
    root = Path(__file__).resolve().parent.parent
    try:
        # 1. The plan, printed whatever follows.
        answer = plan(
            version,
            (root / "CHANGELOG.md").read_text(encoding="utf-8"),
            (root / INSTALLER).read_text(encoding="utf-8"),
        )
        for line in report(answer):
            say(line)

        # 2. Nothing asked: nothing done.
        if not (do_upload or do_withdraw):
            say(f"dry run: gh was not run. To upload: just release-marks {version} --upload")
            return DONE

        # 3. gh, for the one thing asked.
        gh = shutil.which("gh")
        if gh is None:
            raise Finding("gh: no gh on PATH")
        if do_withdraw:
            removed = withdraw(gh, version)
            say(f"withdrawn from v{version}: {', '.join(removed) or 'nothing, it held no mark'}")
        else:
            held = upload(gh, answer)
            say(f"v{version} holds: {', '.join(held) or 'no mark'}")
    except Finding as finding:
        say(str(finding), error=True)
        return FOUND
    return DONE


# A changelog for the selftest: `{g13}` and `{g121}` are the groups a case plants in 0.13.0 and
# in 0.12.1. The groups under Unreleased, under the pre-release and under the heading that is
# no release's must count for nothing.
CHANGELOG = """\
# Changelog

## [Unreleased]

### Changed (breaking, no automatic update)

## [1.0.1] - 2027-02-02

### Fixed

## [1.0.0] - 2027-02-01

### Changed (breaking)

## [0.14.0] - 2027-01-03

### Added

## A heading that is no release's

### Changed (no automatic update)

## [0.13.0] - 2027-01-02

{g13}

## [0.13.0-rc.1] - 2027-01-01

### Changed (breaking, no automatic update)

## [0.12.1] - 2026-12-02

{g121}

## [0.12.0] - 2026-12-01

### Changed (breaking, no automatic update)

- `init` makes a store 0.11 does not open.

## [0.11.0] - 2026-11-09

### Changed (breaking)

## [0.10.0] - 2026-11-07

### Added
"""

INSTALLER_TEXT = "#!/bin/sh\nMACOS_AFTER=0.11.0\nUPDATE_AFTER=0.11.0\n"

# A stand-in for gh: it keeps the release's files as lines of `$GH_STATE/assets`, appends its
# arguments to `$GH_STATE/calls`, and fails where `$GH_STATE/fail` names its subcommand. An
# upload replaces a file of the same name, as `--clobber` does, and refuses an empty one; with
# `$GH_STATE/drop` it takes the files and keeps none.
FAKE_GH = """\
#!/bin/sh
state=${GH_STATE:?}
echo "$*" >>"$state/calls"
[ "$1" = release ] || exit 64
if [ -f "$state/fail" ] && [ "$(cat "$state/fail")" = "$2" ]; then
  echo "stand-in: $2 refused" >&2
  exit 1
fi
case $2 in
  view)
    cat "$state/assets"
    ;;
  upload)
    shift 3
    for arg in "$@"; do
      case $arg in
        --repo) break ;;
        *)
          [ -s "$arg" ] || exit 65
          name=$(basename "$arg")
          grep -vx "$name" "$state/assets" >"$state/assets.new" || true
          [ -f "$state/drop" ] || echo "$name" >>"$state/assets.new"
          mv "$state/assets.new" "$state/assets"
          ;;
      esac
    done
    ;;
  delete-asset)
    grep -vx "$4" "$state/assets" >"$state/assets.new" || true
    mv "$state/assets.new" "$state/assets"
    ;;
  *)
    exit 64
    ;;
esac
"""


def selftest_plans(failures: list[str]) -> None:
    """Each rule of the computation, on crafted changelogs."""

    def of(version: str, g13: str = "### Added", g121: str = "### Fixed") -> Plan:
        return plan(version, CHANGELOG.format(g13=g13, g121=g121), INSTALLER_TEXT)

    def expect(case: str, answer: Plan, marked: list[str], withheld: list[str]) -> None:
        got = (
            [line.line for line in answer.lines if line.marked],
            [line.line for line in answer.lines if not line.marked],
        )
        if got != (marked, withheld):
            failures.append(f"{case}: marked and withheld are {got}")

    # 1. The first release with `so update` has no build to mark for, and says why.
    answer = of("0.12.0")
    expect("first release", answer, [], [])
    if not any("UPDATE_AFTER=0.11.0" in note for note in answer.notes):
        failures.append(f"first release: {answer.notes}")
    if "no mark:" not in "\n".join(report(answer)):
        failures.append(f"first release's report: {report(answer)}")

    # 2. A minor release is marked for the line before it; a breaking group withholds nothing.
    expect("minor", of("0.13.0"), ["0.12"], [])
    answer = of("0.13.0", g13="### Changed (breaking)")
    expect("breaking", answer, ["0.12"], [])
    if "0.13.0" not in answer.lines[0].why:
        failures.append(f"breaking: the line does not name the release: {answer.lines}")

    # 3. The group withholds the release, in any case, beside other words, and however the
    # words are spaced and punctuated: a heading written to withhold never gives a mark.
    for heading in (
        "### Changed (breaking, no automatic update)",
        "### Changed (No Automatic Update)",
        "### Security (no automatic update, breaking)",
        "### Changed (breaking) (no automatic update)",
        "### Changed (breaking; no automatic update)",
        "### Changed (breaking and no automatic update)",
        "### Changed (no automatic updates)",
        "### Changed (breaking, no automatic update):",
        "### Changed - no automatic update",
        "### Changed (breaking, no  automatic\tupdate)",
        "### Changed (breaking, no-automatic-update)",
        "###   Changed (no automatic update) ###",
    ):
        answer = of("0.13.0", g13=heading)
        expect(heading, answer, [], ["0.12"])
        if heading[4:] not in answer.lines[0].why or "0.13.0" not in answer.lines[0].why:
            failures.append(f"{heading}: the reason is {answer.lines[0].why!r}")
        if not any("so update" in note for note in answer.notes):
            failures.append(f"{heading}: nothing says what a withheld build does")

    # 4. The words outside a heading withhold nothing: body text, a fence, a line that is no
    # heading. Nor does a group that says other words.
    for body in (
        "### Changed\n\n- Says (no automatic update) in a line.",
        "### Changed\n\n```text\n### Changed (no automatic update)\n```",
        "### Changed\n\n###Changed (no automatic update)",
        "### Changed\n\n    ### Changed (no automatic update)",
        "### Changed (automatic update)",
    ):
        expect(f"not a group: {body!r}", of("0.13.0", g13=body), ["0.12"], [])

    # A heading of another depth that says them is no group, and stops the release with its
    # line: on a line's way only, so a release no build crosses is not read for it.
    for body in (
        "### Changed\n\n#### Detail (no automatic update)",
        "### Changed\n\n##### Stores: No automatic update",
        "# Changed (no automatic update)",
        "## Changed (no automatic update)",
        "### Changed\n\n## Stores: no-automatic-update",
    ):
        try:
            of("0.13.0", g13=body)
            failures.append(f"a heading that is no group: {body!r}: no finding")
        except Finding as finding:
            if not str(finding).startswith("heading: 0.13.0 ") or body.splitlines()[-1] not in str(
                finding
            ):
                failures.append(f"a heading that is no group: {body!r}: {finding}")
    deeper = "### Changed\n\n#### Detail (no automatic update)"
    try:
        of("0.14.0", g13=deeper)
        failures.append("a heading that is no group, crossed: no finding")
    except Finding:
        pass
    expect("a heading that is no group, not crossed", of("0.12.1", g13=deeper), ["0.12"], [])

    # A release's own `## ` heading that says them is no group either; and a `## ` heading
    # that says them where no release's section is open is no release's.
    own = "## [0.13.0] - 2027-01-02"
    said = CHANGELOG.format(g13="### Added", g121="### Fixed").replace(own, f"{own} ({WITHHOLD})")
    try:
        plan("0.13.0", said, INSTALLER_TEXT)
        failures.append("a release's heading that says the words: no finding")
    except Finding as finding:
        if not str(finding).startswith(f"heading: 0.13.0 has the heading `{own} ({WITHHOLD})`"):
            failures.append(f"a release's heading that says the words: {finding}")
    outside = "### Added\n\n## Notes\n\n## Changed (no automatic update)"
    expect("a heading in no release's section", of("0.14.0", g13=outside), ["0.12", "0.13"], [])

    # 5. A later release is withheld from the line that would cross the change, not from the
    # line after it; the first release's own group and a pre-release's count for nothing.
    answer = of("0.14.0", g13="### Changed (breaking, no automatic update)")
    expect("crossing", answer, ["0.13"], ["0.12"])
    expect("no crossing", of("0.14.0"), ["0.12", "0.13"], [])

    # 6. A patch release is marked for its own line, and withheld from it by its own group or
    # by an earlier patch's.
    expect("patch", of("0.12.1"), ["0.12"], [])
    withheld = "### Fixed (no automatic update)"
    expect("patch withheld", of("0.12.1", g121=withheld), [], ["0.12"])
    expect("after a withheld patch", of("0.13.0", g121=withheld), [], ["0.12"])

    # 7. Another major version's lines get no mark; a patch of the new major gets its own.
    answer = of("1.0.0")
    expect("major", answer, [], [])
    if not any("another major" in note for note in answer.notes):
        failures.append(f"major: {answer.notes}")
    expect("major's patch", of("1.0.1"), ["1.0"], [])

    # 8. A pre-release gets none, without a section of its own being asked for.
    answer = of("0.15.0-rc.1")
    expect("pre-release", answer, [], [])
    if not any("pre-release" in note for note in answer.notes):
        failures.append(f"pre-release: {answer.notes}")

    # 9. What is no release stops with its rule.
    for case, version, installer, rule in (
        ("no section", "0.15.0", INSTALLER_TEXT, "no-section: "),
        ("unreleased", "Unreleased", INSTALLER_TEXT, "version: "),
        ("a tag", "v0.13.0", INSTALLER_TEXT, "version: "),
        ("two numbers", "0.13", INSTALLER_TEXT, "version: "),
        ("no constant", "0.13.0", "#!/bin/sh\n", "installer: "),
        ("a constant that is no version", "0.13.0", "UPDATE_AFTER=none\n", "installer: "),
    ):
        try:
            plan(version, CHANGELOG.format(g13="### Added", g121="### Fixed"), installer)
            failures.append(f"{case}: no finding")
        except Finding as finding:
            if not str(finding).startswith(rule):
                failures.append(f"{case}: {finding}")

    # 10. A later UPDATE_AFTER leaves the lines up to it without a mark.
    late = plan(
        "0.14.0",
        CHANGELOG.format(g13="### Added", g121="### Fixed"),
        "UPDATE_AFTER=0.12.1\n",
    )
    expect("a later first release", late, ["0.13"], [])


def selftest_gh(failures: list[str], base: Path) -> None:
    """Upload and withdrawal through a stand-in for gh, never gh itself."""
    gh = base / "gh"
    gh.write_text(FAKE_GH, encoding="utf-8")
    gh.chmod(gh.stat().st_mode | stat.S_IXUSR)
    answer = plan(
        "0.14.0",
        CHANGELOG.format(g13="### Added", g121="### Fixed"),
        INSTALLER_TEXT,
    )

    def state(name: str, assets: str) -> Path:
        directory = base / name
        directory.mkdir()
        (directory / "assets").write_text(assets, encoding="utf-8")
        (directory / "calls").write_text("", encoding="utf-8")
        os.environ["GH_STATE"] = str(directory)
        return directory

    def calls(directory: Path) -> list[str]:
        return (directory / "calls").read_text(encoding="utf-8").splitlines()

    def refused(case: str, action) -> None:
        try:
            action()
            failures.append(f"{case}: no finding")
        except Finding as finding:
            if not str(finding).startswith("gh: "):
                failures.append(f"{case}: {finding}")

    kept = os.environ.get("GH_STATE")
    try:
        # 1. An upload puts both marks beside the release's other files, names the release
        # and the repository, and may be run again.
        directory = state("upload", "SHA256SUMS\nswarmops-0.14.0-x86_64-unknown-linux-gnu.tar.gz\n")
        marks = ["auto-update-from-0.12", "auto-update-from-0.13"]
        for run in (1, 2):
            try:
                if (held := upload(str(gh), answer)) != marks:
                    failures.append(f"upload {run}: the release holds {held}")
            except Finding as finding:
                failures.append(f"upload {run}: {finding}")
        uploads = [call for call in calls(directory) if call.startswith("release upload ")]
        if len(uploads) != 2 or not all(
            call.startswith("release upload v0.14.0 ")
            and call.endswith(f"--repo {REPO} --clobber")
            and all(mark in call for mark in marks)
            for call in uploads
        ):
            failures.append(f"upload: gh was run as {uploads}")

        # 2. A release that holds another mark, or one that took no file, is a finding.
        state("stale", "auto-update-from-0.11\n")
        refused("a stale mark", lambda: upload(str(gh), answer))
        (state("dropped", "SHA256SUMS\n") / "drop").write_text("", encoding="utf-8")
        refused("an upload that left nothing", lambda: upload(str(gh), answer))

        # 3. A release that is not there, or an upload gh refuses, is a finding.
        (state("absent", "") / "fail").write_text("view", encoding="utf-8")
        refused("no release", lambda: upload(str(gh), answer))
        (state("refused", "") / "fail").write_text("upload", encoding="utf-8")
        refused("a refused upload", lambda: upload(str(gh), answer))

        # 4. A release with nothing to mark uploads nothing, and must hold no mark.
        directory = state("nothing", "SHA256SUMS\n")
        nothing = plan(
            "0.12.0",
            CHANGELOG.format(g13="### Added", g121="### Fixed"),
            INSTALLER_TEXT,
        )
        if upload(str(gh), nothing) or any("upload" in call for call in calls(directory)):
            failures.append(f"nothing to mark: gh was run as {calls(directory)}")

        # 5. A withdrawal removes every mark and no other file.
        directory = state("withdraw", "SHA256SUMS\nauto-update-from-0.12\nauto-update-from-0.9\n")
        removed = withdraw(str(gh), "0.14.0")
        left = (directory / "assets").read_text(encoding="utf-8")
        if removed != ["auto-update-from-0.12", "auto-update-from-0.9"] or left != "SHA256SUMS\n":
            failures.append(f"withdraw: removed {removed}, left {left!r}")
        deletes = [call for call in calls(directory) if "delete-asset" in call]
        if deletes != [
            f"release delete-asset v0.14.0 {name} --repo {REPO} --yes" for name in removed
        ]:
            failures.append(f"withdraw: gh was run as {deletes}")
        (state("kept", "auto-update-from-0.12\n") / "fail").write_text(
            "delete-asset", encoding="utf-8"
        )
        refused("a refused removal", lambda: withdraw(str(gh), "0.14.0"))

        # 6. No gh at all is a finding, not a trace.
        refused("no gh", lambda: marks_of(str(base / "no-gh"), "v0.14.0"))
    finally:
        if kept is None:
            os.environ.pop("GH_STATE", None)
        else:
            os.environ["GH_STATE"] = kept


def selftest_sources(failures: list[str]) -> None:
    """The script's constants are the product's, and the repository's own files are read."""
    root = Path(__file__).resolve().parent.parent

    # 1. The mark's name and the repository, as `so update` asks for them.
    product = (root / PRODUCT_SOURCE).read_text(encoding="utf-8")
    for constant in (
        f'pub const MARK_PREFIX: &str = "{MARK_PREFIX}";',
        f'pub const RELEASES_URL: &str = "https://github.com/{REPO}/releases";',
    ):
        if constant not in product:
            failures.append(f"{PRODUCT_SOURCE} does not hold `{constant}`")

    # 2. The repository's changelog and installer give a plan for the newest release there.
    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
    installer = (root / INSTALLER).read_text(encoding="utf-8")
    try:
        newest = max(sections(changelog))
        plan(show(newest), changelog, installer)
    except (Finding, ValueError) as error:
        failures.append(f"the repository's own files: {error}")


def selftest_arguments(failures: list[str]) -> None:
    """A flag is taken whole or not at all: nothing short of `--upload` runs gh."""
    parser = arguments()
    whole = parser.parse_args(["marks", "0.13.0", "--upload"])
    if not whole.upload or whole.withdraw:
        failures.append(f"--upload: read as {whole}")
    for flag in ("--up", "--with", "--w"):
        try:
            with contextlib.redirect_stderr(io.StringIO()):
                parser.parse_args(["marks", "0.13.0", flag])
            failures.append(f"{flag}: taken for a whole flag")
        except SystemExit as stopped:
            if stopped.code != USAGE:
                failures.append(f"{flag}: exit {stopped.code}")


def selftest() -> int:
    """Every rule on crafted changelogs, gh's part against a stand-in, and the constants."""
    failures: list[str] = []
    selftest_plans(failures)
    selftest_arguments(failures)
    with tempfile.TemporaryDirectory() as tmp:
        selftest_gh(failures, Path(tmp))
    selftest_sources(failures)

    for failure in failures:
        say(f"selftest: {failure}", error=True)
    if failures:
        return FOUND
    say("selftest: every rule gives its marks, and gh's stand-in was run as gh would be")
    return DONE


def arguments() -> argparse.ArgumentParser:
    """The command line. No flag may be shortened: `--up` is not `--upload`."""
    parser = argparse.ArgumentParser(
        prog="release-marks.py", description=__doc__.splitlines()[0], allow_abbrev=False
    )
    commands = parser.add_subparsers(dest="command", required=True)
    marks = commands.add_parser(
        "marks", help="print a release's marks; upload or withdraw them", allow_abbrev=False
    )
    marks.add_argument("version", help="the release, X.Y.Z, without a leading v")
    action = marks.add_mutually_exclusive_group()
    action.add_argument("--upload", action="store_true", help="upload the marks with gh")
    action.add_argument("--withdraw", action="store_true", help="remove every mark with gh")
    commands.add_parser("selftest", help="prove every rule, offline", allow_abbrev=False)
    return parser


def main() -> int:
    args = arguments().parse_args()

    if args.command == "marks":
        return run_marks(args.version, args.upload, args.withdraw)
    return selftest()


if __name__ == "__main__":
    sys.exit(main())
