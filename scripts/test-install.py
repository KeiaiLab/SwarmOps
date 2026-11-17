# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests scripts/install.sh, the one-line installer, offline.

    python3 -I scripts/test-install.py

A fake release, each target's archive in the real layout and SHA256SUMS, is served on
127.0.0.1 with a `releases/latest` that redirects to its tag page; its "binaries" are sh scripts
that print `swarmops <version>` and their arguments. The installer runs against it in every case
below, under sh, and under bash, dash and busybox sh where they are installed. A case's PATH
holds fake `uname`, `id`, `ldd`, `getconf` and `sysctl`, which play other systems, then links to
the tools the installer runs, so that a case can leave curl or sha256sum out: a Linux host runs
the macOS cases and a Mac the Linux ones, each with its own tools (GNU or BSD). Nothing reaches
the network:
the installer is given the server's URLs, and every proxy variable names that server, which
refuses whatever is not its own. Each install is checked for its receipt too: the file the
installer leaves beside the binaries, which says whether they may update themselves. Also checks
that web/public/install.sh, the copy
get.swarmops.cc serves, is scripts/install.sh byte for byte. Prints a summary; exit 1 on a failure.
"""

import enum
import hashlib
import http.server
import io
import os
import platform
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import threading
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INSTALLER = ROOT / "scripts" / "install.sh"
# The copy the website serves at https://get.swarmops.cc/install.sh: `next build` copies
# web/public/ into the site.
SITE_COPY = ROOT / "web" / "public" / "install.sh"

# The fake release's versions: a good one; one whose SHA256SUMS lines for x86_64 Linux and for
# macOS are wrong; one whose SHA256SUMS has no line for x86_64 Linux; one with the Linux
# archives only, as every release up to 0.11.0 is; one with every archive and an empty
# SHA256SUMS; one whose download the server holds until the installer has been interrupted; the
# last release without `so update`, which the installer's report says nothing of automatic
# update for; and a pre-release of the release after it, the first it speaks of.
GOOD = "9.9.9"
MISMATCH = "9.9.9-mismatch"
NO_LINE = "9.9.9-noline"
LINUX_ONLY = "9.9.8"
EMPTY_SUMS = "9.9.9-emptysums"
HELD = "9.9.9-held"
NO_UPDATE = "0.11.0"
FIRST_UPDATE = "0.12.0-rc.1"
X86 = "x86_64-unknown-linux-gnu"
ARM = "aarch64-unknown-linux-gnu"
MAC = "aarch64-apple-darwin"
BINARIES = ("swarmops", "so")
# The receipt the installer leaves beside the binaries, and what an update leaves there: its
# lock, its note for the next command, and the directory a new release is unpacked in.
RECEIPT = ".swarmops-receipt"
RECEIPT_ON = "receipt = 1\nauto_update = true\n"
RECEIPT_OFF = "receipt = 1\nauto_update = false\n"
UPDATE_FILES = (".swarmops-update.lock", ".swarmops-update.note", ".swarmops-update.d/swarmops")
# A receipt that says no, with a key the installer does not know: `so update --auto` keeps one.
RECEIPT_KEPT = '# kept\nreceipt = 1\nauto_update = false  # by hand\nlater = "kept"\n'
DOCS = ("CHANGELOG.md", "LICENSE", "README.md", "THIRD-PARTY-LICENSES")
MTIME = 1_760_000_000

# The fake server's paths, as github.com/KeiaiLab/SwarmOps has them under /releases.
DOWNLOAD_PATH = "/releases/download"
LATEST_PATH = "/releases/latest"
TAG_PATH = f"/releases/tag/v{GOOD}"

# The installer's pinned strings.
PREFIX = "swarmops-install: "
NO_BUILD_FOR_OS = "SwarmOps runs on Linux and macOS; Windows is next."
INTEL_MAC = "No release build runs on an Intel Mac (x86_64)"
NO_MAC_BUILD = (
    "SwarmOps {version} has no macOS build (its SHA256SUMS lists no"
    " swarmops-{version}-aarch64-apple-darwin.tar.gz): nothing was installed."
    " Releases up to 0.11.0 hold Linux builds only."
)
RELEASES = "https://github.com/KeiaiLab/SwarmOps/releases"
QUICK_START = "https://github.com/KeiaiLab/SwarmOps#quick-start"
WEBSITE = "https://swarmops.cc"
LD_SO_WARNING = "weak version 'GLIBC_2.39' not found"
SH_PATH_LINE = 'export PATH="{dest}:$PATH"'
LINUX_PATH_FILES = "(~/.profile, ~/.bashrc or ~/.zshrc):"
MAC_PATH_FILES = "(~/.zshrc, or ~/.bash_profile under bash):"
FISH_PATH_LINE = 'fish_add_path "{dest}"'
# Why an install does or does not update itself, by the installer's reason: what its receipt
# then says, and the line of its report.
AUTO_UPDATE = {
    "on": (RECEIPT_ON, "Automatic update is on. To turn it off: so update --auto off\n"),
    "flag": (
        RECEIPT_OFF,
        "Automatic update is off (--no-auto-update). To turn it on: so update --auto on\n",
    ),
    "pinned": (
        RECEIPT_OFF,
        "Automatic update is off, because a version was named. To turn it on: so update --auto on\n",
    ),
    "root": (
        RECEIPT_OFF,
        "Automatic update is off, as for every install by root. To update: so update\n",
    ),
    "before": (
        RECEIPT_OFF,
        "Automatic update stays off, as it was. To turn it on: so update --auto on\n",
    ),
}
AUTO_UPDATE_WORDS = "Automatic update"
ENVIRONMENT = (
    "SWARMOPS_VERSION",
    "SWARMOPS_INSTALL_DIR",
    "SWARMOPS_DOWNLOAD_BASE",
    "SWARMOPS_LATEST_URL",
)

# The tools the installer runs, linked into each case's PATH. `uname`, `id`, `ldd`, `getconf`
# and `sysctl` are the case's fakes; the proxy variables all name the fake server.
TOOLS = (
    "awk", "cat", "chmod", "cp", "curl", "grep", "gzip", "head", "mkdir", "mktemp", "mv", "rm",
    "sed", "sha256sum", "shasum", "tail", "tar", "tr", "wget",
)  # fmt: skip
PROXIES = ("http_proxy", "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY")
SHELLS = (("sh",), ("bash",), ("dash",), ("busybox", "sh"))
# busybox sh runs its own uname, id and wget applets, not the PATH fakes a case relies on: a
# case that fakes another uname or id, or leaves a tool out, is skipped under it.
BUSYBOX = "busybox sh"
MUSL = "musl"
TIMEOUT = 30  # seconds, for one run of the installer
WORKERS = 8

# A release "binary": the version line, as `swarmops --version` prints it, then its arguments.
FAKE_BINARY = """#!/bin/sh
# A stand-in for swarmops @VERSION@ on @TARGET@: it prints its version and arguments.
echo 'swarmops @VERSION@'
echo "argv: $*"
"""

FAKE_UNAME = """#!/bin/sh
case $1 in
  -m) echo '@MACHINE@' ;;
  *) echo '@OS@' ;;
esac
"""

FAKE_LDD_GLIBC = """#!/bin/sh
echo 'ldd (GNU libc) @VERSION@'
echo 'Copyright (C) 2024 Free Software Foundation, Inc.'
"""

# musl's ldd prints its banner on stderr, and fails.
FAKE_LDD_MUSL = """#!/bin/sh
echo 'musl libc (x86_64)' >&2
echo 'Version 1.2.5' >&2
echo 'Dynamic Program Loader' >&2
exit 1
"""

FAKE_ID = """#!/bin/sh
echo '@UID@'
"""

# A program of another name's owner, which --uninstall must leave alone.
FOREIGN = """#!/bin/sh
echo 'so: another program'
"""

FAKE_GETCONF = """#!/bin/sh
if [ "$1" = GNU_LIBC_VERSION ]; then
  echo 'glibc @VERSION@'
  exit 0
fi
exit 1
"""

# macOS's `sysctl -n sysctl.proc_translated`: 1 in a process Rosetta translates, else 0.
FAKE_SYSCTL = """#!/bin/sh
if [ "$1" = -n ] && [ "$2" = sysctl.proc_translated ]; then
  echo '@TRANSLATED@'
  exit 0
fi
exit 1
"""

# Used where the host has no wget: the two forms the installer runs,
# `wget -q -O FILE URL` and `wget -q -S --spider URL`.
FAKE_WGET = '''#!@PYTHON@ -I
"""A stand-in for wget: `-q -O FILE URL`, and `-q -S --spider URL`."""
import sys
import urllib.request


class Redirects(urllib.request.HTTPRedirectHandler):
    """Prints each redirect's Location header, as `wget -S` does."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        print(f"  Location: {newurl}", file=sys.stderr)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


args = sys.argv[1:]
spider = "--spider" in args
request = urllib.request.Request(args[-1], method="HEAD" if spider else "GET")
try:
    with urllib.request.build_opener(Redirects).open(request) as response:
        body = response.read()
except OSError as error:
    print(f"wget: {error}", file=sys.stderr)
    sys.exit(8)
if not spider:
    with open(args[args.index("-O") + 1], "wb") as out:
        out.write(body)
'''


class Body(enum.Enum):
    """Whether a reply carries its body: not for a HEAD request."""

    SEND = enum.auto()
    OMIT = enum.auto()


@dataclass(frozen=True)
class System:
    """What a case's fakes report: `uname -s`, `uname -m`, ldd's C library (a glibc version,
    MUSL, or None for no ldd), getconf's glibc version (None for no getconf), `id -u`, and
    macOS's `sysctl.proc_translated` (None for no sysctl)."""

    os_name: str = "Linux"
    machine: str = "x86_64"
    ldd: str | None = "2.39"
    getconf: str | None = None
    uid: int = 1000
    translated: int | None = None


@dataclass
class Run:
    """What one run of the installer did, and where it was to install."""

    code: int
    out: str
    err: str
    dest: Path
    tmp: Path
    number: int = 1


Check = Callable[[Run], list[str]]


@dataclass(frozen=True)
class Case:
    """Runs of the installer: arguments and environment (`{dest}`, `{home}` and `{path}` are
    the case's), the system the fakes play, tools left off PATH, how many runs in a row, the
    host tools it needs, and the check each run must pass. Before the runs: `setup`, an install
    with those arguments; then `foreign`, other programs put in DEST by those names; then
    `beside`, files written in DEST, by path and text."""

    name: str
    check: Check
    args: tuple[str, ...] = ("--version", GOOD, "--dir", "{dest}")
    env: tuple[tuple[str, str], ...] = ()
    system: System = System()
    drop: tuple[str, ...] = ()
    dest: str = "{home}/bin"
    runs: int = 1
    needs: tuple[str, ...] = ()
    setup: tuple[str, ...] | None = None
    foreign: tuple[str, ...] = ()
    beside: tuple[tuple[str, str], ...] = ()


@dataclass
class Sandbox:
    """A case's directories and the whole environment its runs get."""

    base: Path
    dest: Path
    tmp: Path
    env: dict[str, str]
    staged: Path


@dataclass
class Outcome:
    """A case's result under one shell: its problems, or why it was skipped."""

    label: str
    problems: list[str]
    skipped: str = ""


class Release(http.server.ThreadingHTTPServer):
    """The fake GitHub: release files under /releases/download, /releases/latest that redirects
    to /releases/tag/v<GOOD>, that page, and a proxy that refuses every request it gets."""

    daemon_threads = True

    def __init__(self, files: Path) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.files = files
        self.url = f"http://127.0.0.1:{self.server_address[1]}"
        self.escapes: list[str] = []
        self.held = threading.Event()
        self.let_go = threading.Event()
        self.answered = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    """Answers the fake GitHub's requests; see Release."""

    server: Release

    def do_GET(self) -> None:
        self.answer(Body.SEND)

    def do_HEAD(self) -> None:
        self.answer(Body.OMIT)

    def do_CONNECT(self) -> None:
        self.refuse()

    def log_message(self, format: str, *args: object) -> None:
        """Quiet: the summary says what failed."""

    def answer(self, body: Body) -> None:
        release = self.server

        # 1. A request for another host, through a proxy variable: refused, and recorded.
        if not self.path.startswith("/"):
            self.refuse()
            return

        # 2. The latest release: a redirect to its tag's page, as github.com answers.
        if self.path == LATEST_PATH:
            self.reply(302, b"", body, release.url + TAG_PATH)
            return
        if self.path == TAG_PATH:
            self.reply(200, f"<p>v{GOOD}</p>\n".encode(), body)
            return

        # 3. The held version: kept until the interrupted installer is gone.
        if self.path.startswith(f"{DOWNLOAD_PATH}/v{HELD}/"):
            release.held.set()
            release.let_go.wait(TIMEOUT)
            self.reply(404, b"", body)
            release.answered.set()
            return

        # 4. A release file, or 404.
        parts = self.path.removeprefix(DOWNLOAD_PATH + "/").split("/")
        file = release.files.joinpath(*parts)
        if not self.path.startswith(DOWNLOAD_PATH + "/") or ".." in parts or not file.is_file():
            self.reply(404, b"", body)
            return
        self.reply(200, file.read_bytes(), body)

    def refuse(self) -> None:
        self.server.escapes.append(f"{self.command} {self.path}")
        self.reply(403, b"", Body.OMIT)

    def reply(self, code: int, data: bytes, body: Body, location: str = "") -> None:
        try:
            self.send_response(code)
            if location:
                self.send_header("Location", location)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if body is Body.SEND:
                self.wfile.write(data)
        except OSError:
            pass  # the client is gone: an interrupted installer


def build_release(files: Path) -> None:
    """Writes the fake release: per version, each target's archive and SHA256SUMS."""
    for version in (GOOD, MISMATCH, NO_LINE, LINUX_ONLY, EMPTY_SUMS, NO_UPDATE, FIRST_UPDATE):
        folder = files / f"v{version}"
        folder.mkdir(parents=True)
        lines = []
        for target in (X86, ARM, MAC):
            # The release from before the macOS build: neither its archive nor its line.
            if target == MAC and version == LINUX_ONLY:
                continue
            name = f"swarmops-{version}-{target}.tar.gz"
            data = pack(version, target)
            (folder / name).write_bytes(data)
            digest = hashlib.sha256(data).hexdigest()

            # The two broken versions: a wrong line for the x86_64 Linux and the macOS archive,
            # or none for the x86_64 Linux one.
            if target in (X86, MAC) and version == MISMATCH:
                digest = hashlib.sha256(b"another archive").hexdigest()
            if target == X86 and version == NO_LINE:
                continue
            # The release whose SHA256SUMS was uploaded empty: every archive, no line.
            if version == EMPTY_SUMS:
                continue
            lines.append(f"{digest}  {name}\n")
        (folder / "SHA256SUMS").write_text("".join(lines))


def pack(version: str, target: str) -> bytes:
    """The archive as release.yml packs it: one directory with both binaries and the documents."""
    top = f"swarmops-{version}-{target}"
    binary = FAKE_BINARY.replace("@VERSION@", version).replace("@TARGET@", target).encode()
    files = {name: (binary, 0o755) for name in BINARIES}
    files |= {name: (f"{name}\n".encode(), 0o644) for name in DOCS}

    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        tar.addfile(tar_entry(top, tarfile.DIRTYPE, 0o755, 0))
        for name in sorted(files):
            data, mode = files[name]
            tar.addfile(tar_entry(f"{top}/{name}", tarfile.REGTYPE, mode, len(data)), io.BytesIO(data))
    return buffer.getvalue()


def tar_entry(name: str, kind: bytes, mode: int, size: int) -> tarfile.TarInfo:
    """An entry owned by root and dated MTIME, as release.yml's tar writes them."""
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.size = size
    info.mtime = MTIME
    return info


def tool_dir(root: Path, drop: tuple[str, ...]) -> Path:
    """A directory of links to the host's TOOLS but those in `drop`; a fake wget where the host
    has none, so that a case without curl still has a downloader."""
    folder = root / "-".join(("tools", *drop))
    folder.mkdir()
    for name in TOOLS:
        if name in drop:
            continue
        found = shutil.which(name)
        if found:
            (folder / name).symlink_to(found)
        elif name == "wget":
            script(folder / name, FAKE_WGET.replace("@PYTHON@", sys.executable))
    return folder


def script(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(0o755)


def write_fakes(folder: Path, system: System) -> None:
    """The fake `uname`, `id`, `ldd`, `getconf` and `sysctl` that play `system`."""
    uname = FAKE_UNAME.replace("@OS@", system.os_name).replace("@MACHINE@", system.machine)
    script(folder / "uname", uname)
    script(folder / "id", FAKE_ID.replace("@UID@", str(system.uid)))
    if system.ldd == MUSL:
        script(folder / "ldd", FAKE_LDD_MUSL)
    elif system.ldd:
        script(folder / "ldd", FAKE_LDD_GLIBC.replace("@VERSION@", system.ldd))
    if system.getconf:
        script(folder / "getconf", FAKE_GETCONF.replace("@VERSION@", system.getconf))
    if system.translated is not None:
        script(folder / "sysctl", FAKE_SYSCTL.replace("@TRANSLATED@", str(system.translated)))


def sandbox(case: Case, release: Release, tools: dict, root: Path) -> Sandbox:
    """A case's own HOME, TMPDIR and fakes, under a path with a space, and its environment:
    nothing inherited, the release's URLs, and proxies that lead only to the fake server."""
    base = Path(tempfile.mkdtemp(prefix="case ", dir=root))
    home, tmp, fakes = base / "home", base / "tmp", base / "fakes"
    for folder in (home, tmp, fakes):
        folder.mkdir()
    write_fakes(fakes, case.system)
    dest = Path(case.dest.format(home=home))

    # Other programs' files, written now and renamed into DEST after the setup install.
    staged = base / "foreign"
    staged.mkdir()
    for name in case.foreign:
        script(staged / name, FOREIGN)

    # wget reads no startup file but this empty one: it refuses /dev/null as one.
    wgetrc = base / "wgetrc"
    wgetrc.touch()
    path = f"{fakes}:{tools[case.drop]}"

    env = {
        "PATH": path,
        "HOME": str(home),
        "TMPDIR": str(tmp),
        "SHELL": "/bin/sh",
        "SWARMOPS_DOWNLOAD_BASE": release.url + DOWNLOAD_PATH,
        "SWARMOPS_LATEST_URL": release.url + LATEST_PATH,
        "WGETRC": str(wgetrc),
        "SYSTEM_WGETRC": str(wgetrc),
        "no_proxy": "127.0.0.1",
        "NO_PROXY": "127.0.0.1",
    }
    env |= {name: release.url for name in PROXIES}
    env |= {key: value.format(home=home, dest=dest, path=path) for key, value in case.env}
    return Sandbox(base, dest, tmp, env, staged)


def run_case(case: Case, shell: tuple[str, list[str]], box: Sandbox) -> Outcome:
    """Runs a case under a shell in its sandbox, `case.runs` times in a row, and checks each
    run."""
    label = f"[{shell[0]}] {case.name}"
    missing = [name for name in case.needs if not shutil.which(name)]
    if missing:
        return Outcome(label, [], f"no {', '.join(missing)} on this host")
    real = (platform.system(), platform.machine(), os.getuid() == 0)
    applets = (case.system.os_name, case.system.machine, case.system.uid == 0) != real
    if shell[0] == BUSYBOX and (applets or case.drop):
        return Outcome(label, [], "busybox sh runs its own applets, not this case's fakes")

    args = [arg.format(dest=box.dest) for arg in case.args]
    problems = []

    # 1. The state the case starts from: an install, then other programs' files in DEST, then
    # the files of an earlier install or update.
    if case.setup is not None:
        setup = [arg.format(dest=box.dest) for arg in case.setup]
        proc = subprocess.run(
            [*shell[1], str(INSTALLER), *setup],
            cwd=box.base,
            env=box.env,
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
        )
        if proc.returncode != 0:
            return Outcome(label, [f"its setup install failed: {proc.stderr.strip()!r}"])
    for name in case.foreign:
        box.dest.mkdir(parents=True, exist_ok=True)
        (box.staged / name).rename(box.dest / name)
    for name, text in case.beside:
        file = box.dest / name
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        file.chmod(0o644)

    # 2. The runs, each checked.
    for number in range(1, case.runs + 1):
        proc = subprocess.run(
            [*shell[1], str(INSTALLER), *args],
            cwd=box.base,
            env=box.env,
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
        )
        run = Run(proc.returncode, proc.stdout, proc.stderr, box.dest, box.tmp, number)
        found = case.check(run)
        if found and case.runs > 1:
            found = [f"run {number}: {problem}" for problem in found]
        if found:
            found.append(f"its stderr: {proc.stderr.strip()!r}")
        problems += found
    return Outcome(label, problems)


def interrupt(
    shell: tuple[str, list[str]],
    sig: signal.Signals,
    system: System,
    release: Release,
    tools: dict,
    root: Path,
) -> Outcome:
    """The installer, stopped by `sig` while the server holds its download, leaves neither its
    temporary directory nor a binary behind. On Linux the held download is the archive's; on
    macOS it is SHA256SUMS's, which the installer reads first and with its stderr closed."""
    name = f"stopped by {sig.name}" if system.os_name == "Linux" else f"macOS: stopped by {sig.name}"
    case = Case(name, refused(code=128 + sig), args=("--version", HELD, "--dir", "{dest}"), system=system)
    label = f"[{shell[0]}] {case.name}"
    box = sandbox(case, release, tools, root)
    args = [arg.format(dest=box.dest) for arg in case.args]

    # 1. Start it in a process group of its own, as a shell starts a job.
    for event in (release.held, release.let_go, release.answered):
        event.clear()
    proc = subprocess.Popen(
        [*shell[1], str(INSTALLER), *args],
        cwd=box.base,
        env=box.env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )

    # 2. Wait for its download to reach the server, which holds it.
    if not release.held.wait(TIMEOUT):
        proc.kill()
        proc.communicate()
        return Outcome(label, ["its download never reached the server"])

    # 3. Signal the whole group, as a terminal does on Ctrl-C, then let the download go.
    os.killpg(proc.pid, sig)
    release.let_go.set()
    out, err = proc.communicate(timeout=TIMEOUT)
    release.answered.wait(TIMEOUT)

    # 4. It failed, said nothing on stdout, and removed what it had written.
    problems = case.check(Run(proc.returncode, out, err, box.dest, box.tmp))
    return Outcome(label, problems)


def leftovers(run: Run) -> list[str]:
    """What the run left in its TMPDIR: nothing, however it ended."""
    names = sorted(path.name for path in run.tmp.iterdir())
    return [f"TMPDIR still holds {names}"] if names else []


def installed(version: str = GOOD, target: str = X86, auto: str = "pinned", receipt: str = "") -> Check:
    """The run succeeded, and DEST holds exactly both binaries of that archive and the receipt:
    the one of AUTO_UPDATE's reason `auto`, with that reason's line in the report, or the text
    `receipt` where the installer is to leave the receipt it found."""
    wanted, line = AUTO_UPDATE[auto]

    def check(run: Run) -> list[str]:
        problems = leftovers(run)

        # 1. Status 0, every stderr line the installer's, and the summary on stdout.
        if run.code != 0:
            problems.append(f"exit {run.code}, not 0")
        strays = [line for line in run.err.splitlines() if not line.startswith(PREFIX)]
        if strays:
            problems.append(f"stderr lines without {PREFIX!r}: {strays}")
        for text in (f"swarmops {version}\nargv: --version\n", QUICK_START, WEBSITE):
            if text not in run.out:
                problems.append(f"stdout lacks {text!r}: {run.out!r}")

        # 2. The report's line on automatic update: this reason's, once, and no other; none
        # for a release that has no `so update`.
        spoken = [text for text in run.out.splitlines(keepends=True) if AUTO_UPDATE_WORDS in text]
        if spoken != ([] if version == NO_UPDATE else [line]):
            problems.append(f"stdout says {spoken} on automatic update, not {line!r}")

        # 3. DEST: the two binaries and the receipt, and nothing half written.
        names = sorted(path.name for path in run.dest.iterdir()) if run.dest.is_dir() else []
        if names != sorted((*BINARIES, RECEIPT)):
            problems.append(f"{run.dest} holds {names}, not {sorted((*BINARIES, RECEIPT))}")
            return problems

        # 4. The receipt: its two lines, or the text it had, readable by every user.
        left = run.dest / RECEIPT
        if left.read_text() != (receipt or wanted):
            problems.append(f"the receipt is {left.read_text()!r}, not {receipt or wanted!r}")
        if left.is_symlink() or left.stat().st_mode & 0o777 != 0o644:
            problems.append(f"the receipt's mode is {left.stat().st_mode & 0o777:o}, not 644")

        # 5. The binaries: executable, from that version's archive for that target.
        for name in BINARIES:
            binary = run.dest / name
            if not os.access(binary, os.X_OK):
                problems.append(f"{name} is not executable")
            if f"swarmops {version} on {target}" not in binary.read_text():
                problems.append(f"{name} is not from the {version} {target} archive")
            printed = subprocess.run([binary, "--version"], capture_output=True, text=True).stdout
            if not printed.startswith(f"swarmops {version}\n"):
                problems.append(f"{name} --version printed {printed!r}")
        return problems

    return check


def refused(*texts: str, code: int = 1) -> Check:
    """The run stopped with status `code`, said each text on stderr, nothing on stdout, and
    installed nothing."""

    def check(run: Run) -> list[str]:
        problems = leftovers(run)
        if run.code != code and (code or run.code == 0):
            problems.append(f"exit {run.code}, not {code or 'non-zero'}")
        if run.out:
            problems.append(f"stdout is not empty: {run.out!r}")
        for text in texts:
            if text not in run.err:
                problems.append(f"stderr lacks {text!r}")
        if run.dest.exists() and any(run.dest.iterdir()):
            problems.append(f"{run.dest} holds {sorted(path.name for path in run.dest.iterdir())}")
        return problems

    return check


def says(stream: str, text: str) -> Check:
    """`text` (in which `{dest}` is the run's) is on `stream`, "out" or "err"."""

    def check(run: Run) -> list[str]:
        wanted = text.format(dest=run.dest)
        printed = run.out if stream == "out" else run.err
        return [] if wanted in printed else [f"std{stream} lacks {wanted!r}"]

    return check


def on_run(number: int, check: Check) -> Check:
    """`check`, on run `number` of a case only."""
    return lambda run: check(run) if run.number == number else []


def uninstalled(*kept: str) -> Check:
    """--uninstall: status 0, the summary on stdout, and DEST holding only `kept`, each the
    other program it was."""

    def check(run: Run) -> list[str]:
        problems = leftovers(run)
        if run.code != 0:
            problems.append(f"exit {run.code}, not 0")
        if "Stores are left as they are." not in run.out:
            problems.append(f"stdout lacks the summary: {run.out!r}")
        names = sorted(path.name for path in run.dest.iterdir())
        if names != sorted(kept):
            problems.append(f"{run.dest} holds {names}, not {sorted(kept)}")
        for name in kept:
            if (run.dest / name).read_text() != FOREIGN:
                problems.append(f"{name} is no longer the other program")
        return problems

    return check


def lacks_out(text: str) -> Check:
    """`text` is not on stdout."""
    return lambda run: [f"stdout has {text!r}"] if text in run.out else []


def lacks_err(text: str) -> Check:
    """`text` is not on stderr."""
    return lambda run: [f"stderr has {text!r}"] if text in run.err else []


def helped(run: Run) -> list[str]:
    """--help: status 0, the options and environment variables on stdout, and nothing else."""
    problems = leftovers(run)
    if run.code != 0:
        problems.append(f"exit {run.code}, not 0")
    if run.err:
        problems.append(f"stderr is not empty: {run.err!r}")
    for text in ("--version", "--dir", "--no-auto-update", "--uninstall", RECEIPT, *ENVIRONMENT):
        if text not in run.out:
            problems.append(f"stdout lacks {text!r}")
    return problems


def all_of(*checks: Check) -> Check:
    return lambda run: [problem for check in checks for problem in check(run)]


def mac(machine: str = "arm64", translated: int | None = None, uid: int = 1000) -> System:
    """A Mac: no ldd and no getconf answer for glibc, which the installer must not ask."""
    return System("Darwin", machine, ldd=None, translated=translated, uid=uid)


def version_args(version: str) -> tuple[str, ...]:
    return ("--version", version, "--dir", "{dest}")


CASES = (
    Case("an explicit version", all_of(installed(), says("out", SH_PATH_LINE), says("out", LINUX_PATH_FILES))),
    Case(
        "SWARMOPS_VERSION (with its v) and SWARMOPS_INSTALL_DIR",
        installed(),
        args=(),
        env=(("SWARMOPS_VERSION", f"v{GOOD}"), ("SWARMOPS_INSTALL_DIR", "{dest}")),
    ),
    Case("the latest release, into ~/.local/bin", installed(auto="on"), args=(), dest="{home}/.local/bin"),
    Case(
        "a reinstall over an install: an update",
        all_of(installed(), on_run(2, says("out", f"Replaced swarmops {GOOD}."))),
        runs=2,
    ),
    Case("root: /usr/local/bin", refused("into /usr/local/bin."), args=version_args("9.9.0")[:2], system=System(uid=0)),
    # The receipt: automatic update is on for the latest release (above), and off for a named
    # version (the cases that name one), --no-auto-update and root; a receipt that says no
    # stays as it is; the report speaks of it only for a release that reads it.
    Case("--no-auto-update", installed(auto="flag"), args=("--no-auto-update", "--dir", "{dest}")),
    Case(
        "--no-auto-update and a version",
        installed(auto="flag"),
        args=("--no-auto-update", *version_args(GOOD)),
    ),
    Case(
        "root, the latest release: no automatic update",
        installed(auto="root"),
        args=("--dir", "{dest}"),
        system=System(uid=0),
    ),
    Case(
        "an update of an install made with --no-auto-update",
        installed(auto="before"),
        args=("--dir", "{dest}"),
        setup=("--no-auto-update", "--dir", "{dest}"),
    ),
    Case(
        "a receipt that says no is left as it was found",
        installed(auto="before", receipt=RECEIPT_KEPT),
        args=("--dir", "{dest}"),
        beside=((RECEIPT, RECEIPT_KEPT),),
    ),
    Case("a version named over an install that updates itself", installed(), setup=("--dir", "{dest}")),
    Case(
        "a receipt that says neither yes nor no is written again",
        installed(auto="on"),
        args=("--dir", "{dest}"),
        beside=((RECEIPT, "auto_update = maybe\n"),),
    ),
    Case("the last release without so update", installed(NO_UPDATE), args=version_args(NO_UPDATE)),
    Case(
        "a pre-release of the first release with so update",
        installed(FIRST_UPDATE),
        args=version_args(FIRST_UPDATE),
    ),
    Case("--uninstall", uninstalled(), args=("--uninstall", "--dir", "{dest}"), setup=version_args(GOOD)),
    Case(
        "--uninstall removes what an update left",
        uninstalled(),
        args=("--uninstall", "--dir", "{dest}"),
        setup=version_args(GOOD),
        beside=tuple((name, "left\n") for name in UPDATE_FILES),
    ),
    Case(
        "--uninstall, a receipt beside other programs",
        all_of(uninstalled("so", "swarmops"), says("out", "Removed an install's receipt from {dest}.")),
        args=("--uninstall", "--dir", "{dest}"),
        setup=version_args(GOOD),
        foreign=("so", "swarmops"),
    ),
    Case(
        "--uninstall leaves another so",
        uninstalled("so"),
        args=("--uninstall", "--dir", "{dest}"),
        setup=version_args(GOOD),
        foreign=("so",),
    ),
    Case("--uninstall, nothing installed", refused("holds no SwarmOps binary"), args=("--uninstall", "--dir", "{dest}")),
    Case("a checksum mismatch", refused("does not match"), args=version_args(MISMATCH)),
    Case("no SHA256SUMS line", refused("has no line"), args=version_args(NO_LINE)),
    Case("a version that is not released", refused("Cannot download"), args=version_args("9.9.0")),
    Case("neither Linux nor macOS", refused(NO_BUILD_FOR_OS, "This system is FreeBSD.", RELEASES), system=System("FreeBSD")),
    Case("an unsupported uname -m", refused("riscv64"), system=System(machine="riscv64")),
    Case("uname -m amd64", installed(target=X86), system=System(machine="amd64")),
    Case("uname -m aarch64", installed(target=ARM), system=System(machine="aarch64")),
    Case("uname -m arm64", installed(target=ARM), system=System(machine="arm64")),
    Case("musl", refused("musl"), system=System(ldd=MUSL)),
    Case("glibc 2.31", refused("glibc 2.31", "2.34"), system=System(ldd="2.31")),
    Case("glibc 2.35", all_of(installed(), says("err", LD_SO_WARNING)), system=System(ldd="2.35")),
    Case("glibc 2.39, no warning", all_of(installed(), lacks_err("ld.so"))),
    Case("no ldd: getconf", installed(), system=System(ldd=None, getconf="2.39")),
    Case("no glibc version at all", refused("Cannot tell"), system=System(ldd=None)),
    Case("no curl: wget, and the latest release", installed(auto="on"), args=("--dir", "{dest}"), drop=("curl",)),
    Case("neither curl nor wget", refused("Neither curl nor wget"), drop=("curl", "wget")),
    Case("no sha256sum: shasum", installed(), drop=("sha256sum",), needs=("shasum",)),
    Case("DIR on PATH", all_of(installed(), lacks_out("not on PATH")), env=(("PATH", "{dest}:{path}"),)),
    Case("fish", all_of(installed(), says("out", FISH_PATH_LINE)), env=(("SHELL", "/usr/bin/fish"),)),
    Case("--help", helped, args=("--help",)),
    Case("a version that is not one", refused("not a release version"), args=version_args("1.2")),
    Case("an http URL off 127.0.0.1", refused("https:// only"), env=(("SWARMOPS_DOWNLOAD_BASE", "http://example.com"),)),
    Case("an unknown option", refused("Unknown option", code=2), args=("--frobnicate",)),
    # macOS: the Apple silicon archive, no glibc check, its own PATH files and messages.
    Case(
        "macOS on arm64",
        all_of(
            installed(target=MAC),
            says("err", f"swarmops-{GOOD}-{MAC}.tar.gz matches SHA256SUMS."),
            says("out", SH_PATH_LINE),
            says("out", MAC_PATH_FILES),
            lacks_err("glibc"),
        ),
        system=mac(),
    ),
    Case(
        "macOS: the latest release, into ~/.local/bin",
        installed(target=MAC, auto="on"),
        args=(),
        dest="{home}/.local/bin",
        system=mac(),
    ),
    Case("macOS, root: /usr/local/bin", refused("into /usr/local/bin."), args=version_args("9.9.0")[:2], system=mac(uid=0)),
    Case(
        "macOS: a reinstall over an install: an update",
        all_of(installed(target=MAC), on_run(2, says("out", f"Replaced swarmops {GOOD}."))),
        runs=2,
        system=mac(),
    ),
    Case(
        "macOS: --uninstall",
        uninstalled(),
        args=("--uninstall", "--dir", "{dest}"),
        setup=version_args(GOOD),
        system=mac(),
    ),
    Case("macOS on an Intel Mac", refused(INTEL_MAC, RELEASES), system=mac("x86_64", translated=0)),
    Case("macOS on an Intel Mac, no sysctl", refused(INTEL_MAC), system=mac("x86_64")),
    Case("macOS on arm64 under Rosetta", installed(target=MAC), system=mac("x86_64", translated=1)),
    Case("macOS on another machine", refused("macOS on i386"), system=mac("i386")),
    Case(
        "macOS: a release without a macOS build",
        all_of(
            refused(NO_MAC_BUILD.format(version=LINUX_ONLY), RELEASES),
            lacks_err("Cannot download"),
            lacks_err("Downloading"),
        ),
        args=version_args(LINUX_ONLY),
        system=mac(),
    ),
    Case(
        "macOS: a release whose SHA256SUMS is empty",
        all_of(refused(NO_MAC_BUILD.format(version=EMPTY_SUMS), RELEASES), lacks_err("Downloading")),
        args=version_args(EMPTY_SUMS),
        system=mac(),
    ),
    Case(
        "macOS: a version that is not released",
        all_of(refused("Cannot download", "is 9.9.0 a release?"), lacks_err("macOS build")),
        args=version_args("9.9.0"),
        system=mac(),
    ),
    Case("macOS: a checksum mismatch", refused("does not match"), args=version_args(MISMATCH), system=mac()),
    Case("macOS: no sha256sum: shasum", installed(target=MAC), drop=("sha256sum",), needs=("shasum",), system=mac()),
    Case(
        "macOS: no curl: wget, and the latest release",
        installed(target=MAC, auto="on"),
        args=("--dir", "{dest}"),
        drop=("curl",),
        system=mac(),
    ),
    Case(
        "macOS: no curl, a release without a macOS build",
        refused(NO_MAC_BUILD.format(version=LINUX_ONLY)),
        args=version_args(LINUX_ONLY),
        drop=("curl",),
        system=mac(),
    ),
    Case(
        "macOS: fish",
        all_of(installed(target=MAC), says("out", FISH_PATH_LINE)),
        env=(("SHELL", "/opt/homebrew/bin/fish"),),
        system=mac(),
    ),
    Case(
        "macOS: an http URL off 127.0.0.1",
        refused("https:// only"),
        env=(("SWARMOPS_DOWNLOAD_BASE", "http://example.com"),),
        system=mac(),
    ),
)


def found_shells() -> list[tuple[str, list[str]]]:
    """The shells to run the installer under, by name and argv: sh, and bash, dash and busybox
    sh where they are installed."""
    shells = []
    for argv in SHELLS:
        found = shutil.which(argv[0])
        if found:
            shells.append((" ".join(argv), [found, *argv[1:]]))
    return shells


def run_all(shells: list, root: Path) -> list[Outcome]:
    """Every case under every shell, in parallel, then the interrupts, one at a time."""
    outcomes = []

    # 1. The fake release, served.
    build_release(root / "release")
    release = Release(root / "release")
    threading.Thread(target=release.serve_forever, daemon=True).start()
    try:
        # 2. The tool directories the cases need.
        tools = {drop: tool_dir(root, drop) for drop in {case.drop for case in CASES}}

        # 3. Every case's sandbox, before any process starts: a fake written while another
        # thread forks stays open in that child until it execs, and running the fake then
        # fails with ETXTBSY ("Text file busy").
        jobs = [(case, shell, sandbox(case, release, tools, root)) for shell in shells for case in CASES]

        # 4. The cases, in parallel; the interrupts share the server's held download.
        with ThreadPoolExecutor(WORKERS) as pool:
            outcomes += pool.map(lambda job: run_case(*job), jobs)
        for shell in shells:
            for system in (System(), mac()):
                for sig in (signal.SIGINT, signal.SIGTERM):
                    outcomes.append(interrupt(shell, sig, system, release, tools, root))

        # 5. No request went anywhere but the fake release.
        if release.escapes:
            outcomes.append(Outcome("the network", [f"requests left 127.0.0.1: {release.escapes}"]))
    finally:
        release.shutdown()
        release.server_close()
    return outcomes


def main() -> int:
    failures = []

    # 1. The website's copy is the installer, byte for byte.
    if SITE_COPY.read_bytes() != INSTALLER.read_bytes():
        failures.append("web/public/install.sh is not scripts/install.sh: copy it again")

    # 2. The shells: sh at least.
    shells = found_shells()
    if not shells or shells[0][0] != "sh":
        print("test-install: no sh on PATH", file=sys.stderr)
        return 1

    # 3. The cases.
    with tempfile.TemporaryDirectory(prefix="test-install-") as scratch:
        outcomes = run_all(shells, Path(scratch))

    # 4. The summary.
    for outcome in outcomes:
        failures += [f"{outcome.label}: {problem}" for problem in outcome.problems]
    skipped = [f"{outcome.label} ({outcome.skipped})" for outcome in outcomes if outcome.skipped]
    passed = sum(1 for outcome in outcomes if not outcome.problems and not outcome.skipped)
    names = ", ".join(name for name, _ in shells)
    absent = [" ".join(argv) for argv in SHELLS if not shutil.which(argv[0])]
    print(f"test-install: {len(CASES)} cases and 4 interrupts under {names}: {passed} runs passed")
    if absent:
        print(f"test-install: not installed, so not run: {', '.join(absent)}")
    if skipped:
        print(f"test-install: skipped: {'; '.join(skipped)}")
    for failure in failures:
        print(f"test-install: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
