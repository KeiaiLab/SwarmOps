# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests scripts/install.sh, the one-line installer, offline.

    python3 -I scripts/test-install.py

A fake release, both targets' archives in the real layout and SHA256SUMS, is served on
127.0.0.1 with a `releases/latest` that redirects to its tag page; its "binaries" are sh scripts
that print `swarmops <version>` and their arguments. The installer runs against it in every case
below, under sh, and under dash and busybox sh where they are installed. A case's PATH holds
fake `uname`, `ldd` and `getconf`, which play other systems, then links to the tools the
installer runs, so that a case can leave curl or sha256sum out. Nothing reaches the network:
the installer is given the server's URLs, and every proxy variable names that server, which
refuses whatever is not its own. Also checks that site/install.sh, the copy get.swarmops.cc serves,
is scripts/install.sh byte for byte. Prints a summary; exit 1 on a failure.
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
# The copy the website serves at https://get.swarmops.cc/install.sh. When the site moves to Zola,
# its sync copies scripts/install.sh into the site instead, and this check moves with the copy.
SITE_COPY = ROOT / "site" / "install.sh"

# The fake release's versions: a good one; one whose SHA256SUMS line for x86_64 is wrong; one
# whose SHA256SUMS has no line for x86_64; and one whose download the server holds until the
# installer has been interrupted.
GOOD = "9.9.9"
MISMATCH = "9.9.9-mismatch"
NO_LINE = "9.9.9-noline"
HELD = "9.9.9-held"
X86 = "x86_64-unknown-linux-gnu"
ARM = "aarch64-unknown-linux-gnu"
BINARIES = ("swarmops", "so")
DOCS = ("CHANGELOG.md", "LICENSE", "README.md", "THIRD-PARTY-LICENSES")
MTIME = 1_760_000_000

# The fake server's paths, as github.com/KeiaiLab/SwarmOps has them under /releases.
DOWNLOAD_PATH = "/releases/download"
LATEST_PATH = "/releases/latest"
TAG_PATH = f"/releases/tag/v{GOOD}"

# The installer's pinned strings.
PREFIX = "swarmops-install: "
NOT_LINUX = "SwarmOps runs on Linux today; macOS and Windows are next."
RELEASES = "https://github.com/KeiaiLab/SwarmOps/releases"
QUICK_START = "https://github.com/KeiaiLab/SwarmOps#quick-start"
WEBSITE = "https://swarmops.cc"
LD_SO_WARNING = "weak version 'GLIBC_2.39' not found"
SH_PATH_LINE = 'export PATH="{dest}:$PATH"'
FISH_PATH_LINE = 'fish_add_path "{dest}"'
ENVIRONMENT = (
    "SWARMOPS_VERSION",
    "SWARMOPS_INSTALL_DIR",
    "SWARMOPS_DOWNLOAD_BASE",
    "SWARMOPS_LATEST_URL",
)

# The tools the installer runs, linked into each case's PATH. `uname`, `ldd` and `getconf` are
# the case's fakes; the proxy variables all name the fake server.
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
    MUSL, or None for no ldd) and getconf's glibc version (None for no getconf)."""

    os_name: str = "Linux"
    machine: str = "x86_64"
    ldd: str | None = "2.39"
    getconf: str | None = None
    uid: int = 1000


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
    host tools it needs, and the check each run must pass."""

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
    """Writes the fake release: per version, both targets' archives and SHA256SUMS."""
    for version in (GOOD, MISMATCH, NO_LINE):
        folder = files / f"v{version}"
        folder.mkdir(parents=True)
        lines = []
        for target in (X86, ARM):
            name = f"swarmops-{version}-{target}.tar.gz"
            data = pack(version, target)
            (folder / name).write_bytes(data)
            digest = hashlib.sha256(data).hexdigest()

            # The two broken versions: a wrong line, or none, for the x86_64 archive.
            if target == X86 and version == MISMATCH:
                digest = hashlib.sha256(b"another archive").hexdigest()
            if target == X86 and version == NO_LINE:
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
    """The fake `uname`, `ldd` and `getconf` that play `system`."""
    uname = FAKE_UNAME.replace("@OS@", system.os_name).replace("@MACHINE@", system.machine)
    script(folder / "uname", uname)
    script(folder / "id", FAKE_ID.replace("@UID@", str(system.uid)))
    if system.ldd == MUSL:
        script(folder / "ldd", FAKE_LDD_MUSL)
    elif system.ldd:
        script(folder / "ldd", FAKE_LDD_GLIBC.replace("@VERSION@", system.ldd))
    if system.getconf:
        script(folder / "getconf", FAKE_GETCONF.replace("@VERSION@", system.getconf))


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

    # 1. The state the case starts from: an install, then other programs' files in DEST.
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


def interrupt(shell: tuple[str, list[str]], sig: signal.Signals, release: Release, tools: dict, root: Path) -> Outcome:
    """The installer, stopped by `sig` while the server holds its download, leaves neither its
    temporary directory nor a binary behind."""
    case = Case(f"stopped by {sig.name}", refused(code=128 + sig), args=("--version", HELD, "--dir", "{dest}"))
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


def installed(version: str = GOOD, target: str = X86) -> Check:
    """The run succeeded, and DEST holds exactly both binaries of that archive."""

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

        # 2. DEST: the two binaries, executable, from that version's archive for that target.
        names = sorted(path.name for path in run.dest.iterdir()) if run.dest.is_dir() else []
        if names != sorted(BINARIES):
            problems.append(f"{run.dest} holds {names}, not {sorted(BINARIES)}")
            return problems
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
    for text in ("--version", "--dir", "--uninstall", *ENVIRONMENT):
        if text not in run.out:
            problems.append(f"stdout lacks {text!r}")
    return problems


def all_of(*checks: Check) -> Check:
    return lambda run: [problem for check in checks for problem in check(run)]


def version_args(version: str) -> tuple[str, ...]:
    return ("--version", version, "--dir", "{dest}")


CASES = (
    Case("an explicit version", all_of(installed(), says("out", SH_PATH_LINE))),
    Case(
        "SWARMOPS_VERSION (with its v) and SWARMOPS_INSTALL_DIR",
        installed(),
        args=(),
        env=(("SWARMOPS_VERSION", f"v{GOOD}"), ("SWARMOPS_INSTALL_DIR", "{dest}")),
    ),
    Case("the latest release, into ~/.local/bin", installed(), args=(), dest="{home}/.local/bin"),
    Case(
        "a reinstall over an install: an update",
        all_of(installed(), on_run(2, says("out", f"Replaced swarmops {GOOD}."))),
        runs=2,
    ),
    Case("root: /usr/local/bin", refused("into /usr/local/bin."), args=version_args("9.9.0")[:2], system=System(uid=0)),
    Case("--uninstall", uninstalled(), args=("--uninstall", "--dir", "{dest}"), setup=version_args(GOOD)),
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
    Case("not Linux", refused(NOT_LINUX, RELEASES), system=System("Darwin", "arm64")),
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
    Case("no curl: wget, and the latest release", installed(), args=("--dir", "{dest}"), drop=("curl",)),
    Case("neither curl nor wget", refused("Neither curl nor wget"), drop=("curl", "wget")),
    Case("no sha256sum: shasum", installed(), drop=("sha256sum",), needs=("shasum",)),
    Case("DIR on PATH", all_of(installed(), lacks_out("not on PATH")), env=(("PATH", "{dest}:{path}"),)),
    Case("fish", all_of(installed(), says("out", FISH_PATH_LINE)), env=(("SHELL", "/usr/bin/fish"),)),
    Case("--help", helped, args=("--help",)),
    Case("a version that is not one", refused("not a release version"), args=version_args("1.2")),
    Case("an http URL off 127.0.0.1", refused("https:// only"), env=(("SWARMOPS_DOWNLOAD_BASE", "http://example.com"),)),
    Case("an unknown option", refused("Unknown option", code=2), args=("--frobnicate",)),
)


def found_shells() -> list[tuple[str, list[str]]]:
    """The shells to run the installer under, by name and argv: sh, and dash and busybox sh
    where they are installed."""
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
            for sig in (signal.SIGINT, signal.SIGTERM):
                outcomes.append(interrupt(shell, sig, release, tools, root))

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
        failures.append("site/install.sh is not scripts/install.sh: copy it again")

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
    print(f"test-install: {len(CASES)} cases and 2 interrupts under {names}: {passed} runs passed")
    if absent:
        print(f"test-install: not installed, so not run: {', '.join(absent)}")
    if skipped:
        print(f"test-install: skipped: {'; '.join(skipped)}")
    for failure in failures:
        print(f"test-install: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
