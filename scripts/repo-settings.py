# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Keeps the GitHub repository page in step with .github/repo-settings.json.

    python3 -I scripts/repo-settings.py selftest  # offline: validates the file, proves each rule
    python3 -I scripts/repo-settings.py check     # network: prints every difference from the host
    python3 -I scripts/repo-settings.py apply     # network: sets the page; never deletes a label

The file holds the About box (description, website, topics), the feature switches, the social
preview's path and the labels. `selftest` reads the file, the bug form's area dropdown and the
social card's PNG header, and proves that a wrong copy of each is reported. `check` and `apply`
go through the `gh` CLI and its login; they are not part of `just check`. An empty or unreadable
answer from the host is a failure, never "no difference". The social preview has no API: `check`
names it as a note, and the settings page sets it.

Exit 0 when clean, 1 on a finding or a difference, 2 on a usage or host error. It needs Python
3.11 or newer; `check` and `apply` also need `gh`.
"""

import argparse
import copy
import enum
import json
import re
import struct
import subprocess
import sys
import time
import urllib.parse
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
NAME = "repo-settings"
SETTINGS = Path(".github/repo-settings.json")
BUG_FORM = Path(".github/ISSUE_TEMPLATE/bug_report.yml")

# Exit codes.
CLEAN = 0
FOUND = 1
USAGE = 2

# The file's bounds.
DESCRIPTION_WORDS = 15
DESCRIPTION_CHARS = 120
FORBIDDEN_CLAIM = "git replacement"
TOPICS_MIN = 6
TOPICS_MAX = 12
LABELS_MAX = 30
LABEL_DESCRIPTION_CHARS = 100
LABEL_PREFIXES = ("kind: ", "area: ", "triage: ", "invite: ")
KIND_PREFIX = "kind: "
AREA_PREFIX = "area: "
KINDS = ("kind: bug", "kind: feature", "kind: question", "kind: docs")
FEATURES = ("issues", "discussions", "wiki", "projects", "private_vulnerability_reporting")

# The social card: a PNG of GitHub's preview size, under its upload limit. Width and height sit
# in the IHDR chunk right after the 8-byte signature: length (4), type (4), width (4), height (4).
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
IHDR = b"IHDR"
IHDR_TYPE_AT = slice(12, 16)
IHDR_SIZE_AT = slice(16, 24)
CARD_WIDTH = 1280
CARD_HEIGHT = 640
CARD_MAX_BYTES = 1024 * 1024

# The bug form's area dropdown, read line by line: the standard library has no YAML reader.
FORM_ITEM = re.compile(r"\s*-\s+type:\s*(\S+)")
FORM_ID = re.compile(r"\s*id:\s*(\S+)")
FORM_OPTIONS = re.compile(r"(\s*)options:\s*$")
FORM_OPTION = re.compile(r"(\s*)-\s+[\"']?(.*?)[\"']?\s*$")
AREA_ID = "area"


class Rule(enum.Enum):
    """A selftest finding, named by the id it prints."""

    SHAPE = "shape"
    DESCRIPTION = "description"
    TOPICS = "topics"
    LABELS = "labels"
    AREAS = "areas"
    CARD = "card"


class Body(enum.Enum):
    """What a host call answers with: a JSON document, or nothing (a 204)."""

    JSON = "json"
    EMPTY = "empty"


class UsageError(Exception):
    """A file that cannot be read, or a host that does not answer."""


Finding = tuple[Rule, str]


# ---------------------------------------------------------------------------------------------
# The file and its rules (offline).
# ---------------------------------------------------------------------------------------------


def load(root: Path) -> tuple[dict[str, Any], str, bytes]:
    """Reads the settings, the bug form and the social card the settings name."""
    try:
        settings = json.loads((root / SETTINGS).read_text(encoding="utf-8"))
        form = (root / BUG_FORM).read_text(encoding="utf-8")
        card = (root / settings["social_preview"]).read_bytes()
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise UsageError(f"cannot read the settings: {error}") from error
    return settings, form, card


def area_options(form: str) -> list[str]:
    """The options of the dropdown whose id is `area`, in order; empty when there is none."""
    lines = form.split("\n")
    in_area = False
    for index, line in enumerate(lines):

        # 1. Track which form item the line belongs to.
        if FORM_ITEM.match(line):
            in_area = False
            continue
        found = FORM_ID.match(line)
        if found:
            in_area = found.group(1) == AREA_ID
            continue
        options = FORM_OPTIONS.match(line)
        if not in_area or not options:
            continue

        # 2. The option list: the deeper-indented `- ` lines right after `options:`.
        indent = len(options.group(1))
        picked = []
        for item in lines[index + 1:]:
            option = FORM_OPTION.match(item)
            if not option or len(option.group(1)) < indent:
                break
            picked.append(option.group(2))
        return picked
    return []


def card_size(card: bytes) -> tuple[int, int] | None:
    """The PNG's width and height from its header; None when it is no PNG."""
    if not card.startswith(PNG_SIGNATURE) or card[IHDR_TYPE_AT] != IHDR:
        return None
    width, height = struct.unpack(">II", card[IHDR_SIZE_AT])
    return width, height


def validate(settings: dict[str, Any], form: str, card: bytes) -> list[Finding]:
    """Every rule the file breaks, as (rule, message)."""
    found: list[Finding] = []

    # 1. Shape: the keys the host calls need.
    for key in ("repository", "description", "homepage", "topics", "features", "labels"):
        if key not in settings:
            found.append((Rule.SHAPE, f"no `{key}`"))
    if found:
        return found
    features = settings["features"]
    if sorted(features) != sorted(FEATURES) or any(type(v) is not bool for v in features.values()):
        found.append((Rule.SHAPE, f"`features` must set exactly {', '.join(FEATURES)} to booleans"))

    # 2. Description: short, one claim, not the non-goal.
    description = settings["description"]
    if len(description.split()) > DESCRIPTION_WORDS:
        found.append((Rule.DESCRIPTION, f"more than {DESCRIPTION_WORDS} words"))
    if len(description) > DESCRIPTION_CHARS:
        found.append((Rule.DESCRIPTION, f"more than {DESCRIPTION_CHARS} characters"))
    if FORBIDDEN_CLAIM in description.lower():
        found.append((Rule.DESCRIPTION, f"says `{FORBIDDEN_CLAIM}`"))

    # 3. Topics: a bounded set.
    topics = settings["topics"]
    if not TOPICS_MIN <= len(topics) <= TOPICS_MAX:
        found.append((Rule.TOPICS, f"{len(topics)} topics, not {TOPICS_MIN} to {TOPICS_MAX}"))
    if len(set(topics)) != len(topics):
        found.append((Rule.TOPICS, "a topic appears twice"))

    # 4. Labels: four families, short descriptions, every kind present.
    labels = settings["labels"]
    names = [label.get("name", "") for label in labels]
    if len(labels) > LABELS_MAX:
        found.append((Rule.LABELS, f"{len(labels)} labels, more than {LABELS_MAX}"))
    if len(set(names)) != len(names):
        found.append((Rule.LABELS, "a label appears twice"))
    for label in labels:
        name = label.get("name", "")
        if not name.startswith(LABEL_PREFIXES):
            found.append((Rule.LABELS, f"`{name}` has none of the prefixes {LABEL_PREFIXES}"))
        if len(label.get("description", "")) > LABEL_DESCRIPTION_CHARS:
            found.append((Rule.LABELS, f"`{name}`: description over {LABEL_DESCRIPTION_CHARS}"))
    for kind in KINDS:
        if kind not in names:
            found.append((Rule.LABELS, f"no `{kind}`"))

    # 5. Areas: the labels and the bug form's dropdown name the same set.
    areas = sorted(n.removeprefix(AREA_PREFIX) for n in names if n.startswith(AREA_PREFIX))
    options = sorted(area_options(form))
    if areas != options:
        found.append((Rule.AREAS, f"area labels {areas} differ from the form's {options}"))

    # 6. The social card: a PNG of the preview size, under the limit.
    size = card_size(card)
    if size is None:
        found.append((Rule.CARD, "not a PNG"))
    elif size != (CARD_WIDTH, CARD_HEIGHT):
        found.append((Rule.CARD, f"{size[0]} x {size[1]}, not {CARD_WIDTH} x {CARD_HEIGHT}"))
    if len(card) >= CARD_MAX_BYTES:
        found.append((Rule.CARD, f"{len(card)} bytes, not under {CARD_MAX_BYTES}"))
    return found


def mutants(settings: dict[str, Any], form: str, card: bytes) -> list[tuple[str, Rule, tuple]]:
    """Wrong copies of the tracked inputs, each with the rule that must report it."""
    out: list[tuple[str, Rule, tuple]] = []

    def edit(name: str, rule: Rule, change) -> None:
        wrong = copy.deepcopy(settings)
        change(wrong)
        out.append((name, rule, (wrong, form, card)))

    # 1. The file.
    edit("no description", Rule.SHAPE, lambda s: s.pop("description"))
    edit("a feature not boolean", Rule.SHAPE, lambda s: s["features"].update(wiki="no"))
    edit("16 words", Rule.DESCRIPTION, lambda s: s.update(description="word " * 16))
    edit("121 characters", Rule.DESCRIPTION, lambda s: s.update(description="x" * 121))
    edit("the non-goal", Rule.DESCRIPTION, lambda s: s.update(description="A Git Replacement."))
    edit("5 topics", Rule.TOPICS, lambda s: s.update(topics=s["topics"][:5]))
    edit("13 topics", Rule.TOPICS, lambda s: s.update(topics=[f"t{i}" for i in range(13)]))
    edit("a duplicate topic", Rule.TOPICS, lambda s: s["topics"].__setitem__(1, s["topics"][0]))
    edit("no kind: docs", Rule.LABELS, lambda s: s.update(
        labels=[label for label in s["labels"] if label["name"] != "kind: docs"]))
    edit("31 labels", Rule.LABELS, lambda s: s["labels"].extend(
        {"name": f"triage: t{i}", "color": "ffffff", "description": ""} for i in range(31)))
    edit("no prefix", Rule.LABELS, lambda s: s["labels"].append(
        {"name": "bug", "color": "ffffff", "description": ""}))
    edit("a long label description", Rule.LABELS, lambda s: s["labels"][0].update(
        description="x" * 101))
    edit("an area the form lacks", Rule.AREAS, lambda s: s["labels"].append(
        {"name": "area: perf", "color": "ffffff", "description": ""}))

    # 2. The form and the card.
    out.append(("an option the labels lack", Rule.AREAS,
                (settings, form.replace("        - site\n", "        - site\n        - perf\n"),
                 card)))
    out.append(("not a PNG", Rule.CARD, (settings, form, b"GIF89a" + card[6:])))
    wide = card[:IHDR_SIZE_AT.start] + struct.pack(">II", 1200, 630) + card[IHDR_SIZE_AT.stop:]
    out.append(("1200 x 630", Rule.CARD, (settings, form, wide)))
    out.append(("1 MiB", Rule.CARD, (settings, form, card + bytes(CARD_MAX_BYTES))))
    return out


def selftest(root: Path) -> int:
    """The tracked file passes every rule, and each wrong copy is reported under its rule."""
    start = time.perf_counter()
    settings, form, card = load(root)

    # 1. The tracked inputs.
    problems = [f"{SETTINGS}: {rule.value}: {msg}" for rule, msg in validate(settings, form, card)]

    # 2. The wrong copies.
    cases = mutants(settings, form, card)
    for name, rule, inputs in cases:
        rules = {got for got, _ in validate(*inputs)}
        if rule not in rules:
            problems.append(f"selftest: `{name}` is not reported under {rule.value}")

    for problem in problems:
        print(f"{NAME}: {problem}")
    if problems:
        return FOUND
    print(
        f"{NAME}: selftest: the file is clean, {len(cases)} wrong copies reported under"
        f" {len(Rule)} rules ({time.perf_counter() - start:.2f} s)"
    )
    return CLEAN


# ---------------------------------------------------------------------------------------------
# The host (network, through `gh`).
# ---------------------------------------------------------------------------------------------


def gh(path: str, method: str = "GET", body: Any = None, want: Body = Body.JSON) -> Any:
    """One `gh api` call. A failed call, an empty or non-JSON answer where JSON is due, raises."""
    args = ["gh", "api", "-X", method, "-H", "Accept: application/vnd.github+json", path]
    payload = None
    if body is not None:
        args += ["--input", "-"]
        payload = json.dumps(body)
    try:
        done = subprocess.run(args, input=payload, capture_output=True, text=True, check=False)
    except OSError as error:
        raise UsageError(f"cannot run gh: {error}") from error
    if done.returncode != 0:
        raise UsageError(f"{method} {path}: {done.stderr.strip() or 'gh failed'}")
    if want is Body.EMPTY:
        return None

    # An empty answer is a failure: it must never read as "nothing differs".
    if not done.stdout.strip():
        raise UsageError(f"{method} {path}: empty answer")
    try:
        return json.loads(done.stdout)
    except ValueError as error:
        raise UsageError(f"{method} {path}: unreadable answer: {error}") from error


def live_labels(repo: str) -> dict[str, dict[str, Any]]:
    """The repository's labels by lower-cased name (the host matches names case-insensitively)."""
    labels: dict[str, dict[str, Any]] = {}
    page = 1
    while True:
        answer = gh(f"repos/{repo}/labels?per_page=100&page={page}")
        if not isinstance(answer, list):
            raise UsageError("labels: the answer is not a list")
        if not answer:
            return labels
        for label in answer:
            labels[label["name"].lower()] = label
        page += 1


def live(repo: str) -> dict[str, Any]:
    """What the host holds for every field the file sets, plus the community profile."""
    info = gh(f"repos/{repo}")
    reporting = gh(f"repos/{repo}/private-vulnerability-reporting")
    profile = gh(f"repos/{repo}/community/profile")
    try:
        return {
            "description": info["description"] or "",
            "homepage": info["homepage"] or "",
            "topics": info["topics"],
            "features": {
                "issues": info["has_issues"],
                "discussions": info["has_discussions"],
                "wiki": info["has_wiki"],
                "projects": info["has_projects"],
                "private_vulnerability_reporting": reporting["enabled"],
            },
            "labels": live_labels(repo),
            "health": profile["health_percentage"],
            "missing": sorted(k for k, v in profile["files"].items() if v is None),
        }
    except (KeyError, TypeError) as error:
        raise UsageError(f"unreadable answer, no {error}") from error


def label_differs(want: dict[str, Any], have: dict[str, Any]) -> bool:
    """Whether a live label's name, colour or description differs from the file's."""
    return (
        have["name"] != want["name"]
        or have["color"].lower() != want["color"].lower()
        or (have.get("description") or "") != want["description"]
    )


def differences(settings: dict[str, Any], have: dict[str, Any]) -> list[str]:
    """Every difference between the file and the host, one line each."""
    out = []

    # 1. The About box and the switches.
    for key in ("description", "homepage"):
        if have[key] != settings[key]:
            out.append(f"{key}: host {have[key]!r}, file {settings[key]!r}")
    if sorted(have["topics"]) != sorted(settings["topics"]):
        out.append(f"topics: host {sorted(have['topics'])}, file {sorted(settings['topics'])}")
    for key, value in settings["features"].items():
        if have["features"][key] != value:
            out.append(f"features.{key}: host {have['features'][key]}, file {value}")

    # 2. Labels: missing or changed, then the host's extras (apply never deletes them).
    wanted = {label["name"].lower(): label for label in settings["labels"]}
    for key, want in wanted.items():
        label = have["labels"].get(key)
        if label is None:
            out.append(f"label {want['name']!r}: missing on the host")
        elif label_differs(want, label):
            out.append(f"label {want['name']!r}: host {label['name']!r} #{label['color']}"
                       f" {label.get('description')!r}")
    for key, label in sorted(have["labels"].items()):
        if key not in wanted:
            out.append(f"label {label['name']!r}: on the host, not in the file")

    # 3. The community profile.
    if have["health"] != 100 or have["missing"]:
        out.append(f"community profile: {have['health']}%, missing {have['missing']}")
    return out


def check(root: Path) -> int:
    """Prints every difference between the file and the host; 1 when there is any."""
    settings, _, _ = load(root)
    have = live(settings["repository"])
    found = differences(settings, have)
    for line in found:
        print(f"{NAME}: {line}")
    print(f"{NAME}: note: the social preview ({settings['social_preview']}) has no API; the"
          " settings page sets it", file=sys.stderr)
    print(f"{NAME}: {len(found)} differences", file=sys.stderr)
    return FOUND if found else CLEAN


def apply(root: Path) -> int:
    """Sets the About box, topics, switches and labels; creates or updates, never deletes."""
    settings, form, card = load(root)
    found = validate(settings, form, card)
    if found:
        for rule, msg in found:
            print(f"{NAME}: {rule.value}: {msg}", file=sys.stderr)
        return FOUND
    repo = settings["repository"]
    features = settings["features"]

    # 1. The About box and the switches.
    gh(f"repos/{repo}", "PATCH", {
        "description": settings["description"],
        "homepage": settings["homepage"],
        "has_issues": features["issues"],
        "has_discussions": features["discussions"],
        "has_wiki": features["wiki"],
        "has_projects": features["projects"],
    })
    print(f"{NAME}: set description, homepage, issues, discussions, wiki, projects")
    gh(f"repos/{repo}/topics", "PUT", {"names": settings["topics"]})
    print(f"{NAME}: set topics {settings['topics']}")
    method = "PUT" if features["private_vulnerability_reporting"] else "DELETE"
    gh(f"repos/{repo}/private-vulnerability-reporting", method, want=Body.EMPTY)
    print(f"{NAME}: set private vulnerability reporting {features['private_vulnerability_reporting']}")

    # 2. Labels: create the missing, update the changed (a case change renames).
    have = live_labels(repo)
    for want in settings["labels"]:
        label = have.get(want["name"].lower())
        if label is None:
            gh(f"repos/{repo}/labels", "POST", want)
            print(f"{NAME}: created label {want['name']!r}")
            continue
        if not label_differs(want, label):
            continue
        quoted = urllib.parse.quote(label["name"], safe="")
        gh(f"repos/{repo}/labels/{quoted}", "PATCH", {
            "new_name": want["name"], "color": want["color"], "description": want["description"],
        })
        print(f"{NAME}: updated label {want['name']!r}")
    return CLEAN


def main() -> int:
    parser = argparse.ArgumentParser(prog="repo-settings.py", description=__doc__.splitlines()[0])
    parser.add_argument("command", choices=("selftest", "check", "apply"))
    args = parser.parse_args()
    commands = {"selftest": selftest, "check": check, "apply": apply}
    try:
        return commands[args.command](ROOT)
    except UsageError as error:
        print(f"{NAME}: {error}", file=sys.stderr)
        return USAGE


if __name__ == "__main__":
    sys.exit(main())
