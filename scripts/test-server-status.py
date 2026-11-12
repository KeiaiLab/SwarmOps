# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests the status page and the probe in packaging/server/status/, offline and without root.

    python3 -I scripts/test-server-status.py [--bin-dir D]

`so-status` is the probe (clone, push to refs/land/main, read main back, one record a try) and
the page generator (state, last tries, the recovery numbers). The probe case runs it against a
real `swarmops serve` behind a stand-in for ssh that starts the forced command as sshd would; the
`swarmops` binary is D's (default: the debug build under CARGO_TARGET_DIR, or target/), and a
missing binary fails the case, it is never skipped. The page cases feed it records and read the
page it writes. The self-check `stopped_host_turns_red` probes a host that refuses every
connection and needs the page red. Prints a summary; exit 1 on a failure.
"""

import argparse
import configparser
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STATUS = ROOT / "packaging" / "server" / "status"
SO_STATUS = STATUS / "bin" / "so-status"
SETTINGS = STATUS / "status.conf"
TIMER = STATUS / "systemd" / "swarmops-status.timer"
WORKFLOW = ROOT / ".github" / "workflows" / "status.yml"

PROBE_IDENTITY = "person/probe"
STORE = "probe"
ONE_MINUTE_S = 60
# The page's three states, as its text reads them.
OPERATIONAL, FAILING, STALE = "operational", "failing", "stale"
RECORD = re.compile(r"^(\d+) (ssh|https) (ok|fail) (\d+) (clone|commit|land|read|done)$")
UNIT_SECTIONS = {"Unit", "Service", "Timer", "Install"}

BIN_DIR = Path()


def default_bin_dir() -> Path:
    """The debug build: CARGO_TARGET_DIR's, or target/'s."""
    target = os.environ.get("CARGO_TARGET_DIR") or str(ROOT / "target")
    return Path(target) / "debug"


def check(holds: bool, message: str) -> None:
    """Fails with `message` unless `holds`."""
    if not holds:
        raise AssertionError(message)


def run(argv: list[str], cwd: Path | None = None, env: dict[str, str] | None = None,
        stdin: str = "") -> subprocess.CompletedProcess:
    """`argv`, its output as text, never raising on an exit status."""
    return subprocess.run(argv, cwd=cwd, env=env, input=stdin, text=True, capture_output=True,
                          check=False)


def so_status(scratch: Path, *arguments: str, settings: dict[str, str] | None = None,
              env: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    """`so-status` with a scratch settings file whose paths are under `scratch`."""
    conf = scratch / "status.conf"
    values = {
        "records": str(scratch / "records"),
        "page": str(scratch / "www"),
    }
    values.update(settings or {})
    conf.write_text("".join(f"{key}={value}\n" for key, value in values.items()))
    environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "SO_STATUS_CONF": str(conf),
                   "HOME": str(scratch), "TMPDIR": str(scratch)}
    environment.update(env or {})
    return run(["sh", str(SO_STATUS), *arguments], env=environment)


def records(scratch: Path) -> list[re.Match]:
    """The probe's records, each line parsed; a line of another shape fails."""
    lines = (scratch / "records").read_text().splitlines()
    parsed = [RECORD.match(line) for line in lines]
    check(all(parsed), f"a record of another shape: {lines}")
    return [match for match in parsed if match]


def write_records(scratch: Path, lines: list[tuple[int, str, str, int, str]]) -> None:
    """Records as the probe writes them."""
    (scratch / "records").write_text("".join(f"{t} {e} {r} {s} {step}\n"
                                             for t, e, r, s, step in lines))


def page(scratch: Path, settings: dict[str, str] | None = None) -> str:
    """`so-status page`, which must succeed; the page it wrote."""
    (scratch / "www").mkdir(exist_ok=True)
    done = so_status(scratch, "page", settings=settings)
    check(done.returncode == 0, f"page: exit {done.returncode}: {done.stderr}")
    return (scratch / "www" / "index.html").read_text()


def state(text: str) -> str:
    """The state the page states."""
    found = re.search(r'<p id="state" class="[a-z]+"[^>]*>([a-z]+)', text)
    check(found is not None, "the page states no state")
    return found.group(1) if found else ""


# ---------------------------------------------------------------------------------------------
# A served store, reached through a stand-in for ssh


def served_store(scratch: Path) -> tuple[Path, dict[str, str]]:
    """A store holding a one-commit main, granting the probe read and land onto main, and the
    environment that makes git's ssh start its forced `serve` as sshd would."""
    swarmops = BIN_DIR / "swarmops"
    check(swarmops.is_file(), f"no swarmops binary in {BIN_DIR}: build swarmops-cli first")
    stores = scratch / "srv"
    stores.mkdir()
    seed = scratch / "seed"
    for argv in (["git", "init", "-q", "-b", "main", str(seed)],
                 ["git", "-C", str(seed), "commit", "-q", "--allow-empty", "-m", "seed"],
                 [str(swarmops), "init", str(stores / STORE)],
                 [str(swarmops), "--as", "agent/setup", "--repo", str(stores / STORE), "git",
                  "import", str(seed), "--ref", "HEAD", "--into", "main"]):
        done = run(argv, env={**os.environ, "GIT_AUTHOR_NAME": "Seed", "GIT_COMMITTER_NAME": "Seed",
                              "GIT_AUTHOR_EMAIL": "seed@seed.invalid",
                              "GIT_COMMITTER_EMAIL": "seed@seed.invalid"})
        check(done.returncode == 0, f"{argv[:3]}: {done.stderr}{done.stdout}")
    (stores / STORE / "access.toml").write_text(
        f'version = 1\n\n[[grant]]\nwho = ["{PROBE_IDENTITY}"]\nread = true\nland = ["main"]\n')

    # git runs `<ssh> [options] <host> <command>`; sshd would run the key's forced command with
    # the client's command in SSH_ORIGINAL_COMMAND.
    ssh = scratch / "ssh"
    ssh.write_text("#!/bin/sh\n"
                   "for last; do :; done\n"
                   f"SSH_ORIGINAL_COMMAND=$last exec '{swarmops}' serve --root '{stores}' "
                   f"--as {PROBE_IDENTITY}\n")
    ssh.chmod(0o755)
    return stores / STORE, {"GIT_SSH_COMMAND": str(ssh), "GIT_SSH_VARIANT": "ssh"}


def main_tree(store: Path) -> list[str]:
    """The file names in the served main's tree."""
    serve_git = store / "git" / "serve.git"
    done = run(["git", "--git-dir", str(serve_git), "ls-tree", "--name-only", "refs/heads/main"])
    check(done.returncode == 0, f"ls-tree of main: {done.stderr}")
    return done.stdout.split()


# ---------------------------------------------------------------------------------------------
# The cases


def status_files_parse(scratch: Path) -> None:
    """The MANIFEST installs every file of status/; the program passes `sh -n`; the units read as
    INI and are confined; the settings file is refused with a misspelt key."""
    installs = {}
    for line in (STATUS / "MANIFEST").read_text().splitlines():
        words = line.split("#", 1)[0].split()
        if not words:
            continue
        check(words[0] in ("install", "uses"), f"MANIFEST: {line!r}")
        if words[0] == "install":
            check(len(words) == 4 and re.fullmatch(r"0[0-7]{3}", words[3]) is not None,
                  f"MANIFEST: {line!r}")
            installs[words[1]] = words[3]
    files = sorted(str(path.relative_to(STATUS)) for path in STATUS.rglob("*")
                   if path.is_file() and path.name != "MANIFEST")
    check(sorted(installs) == files, f"installed {sorted(installs)}, present {files}")
    check(installs["bin/so-status"] == "0755", "so-status is not installed 0755")
    done = run(["sh", "-n", str(SO_STATUS)])
    check(done.returncode == 0, f"sh -n: {done.stderr}")

    for name in files:
        if not name.startswith("systemd/"):
            continue
        parser = configparser.ConfigParser(interpolation=None, strict=True, delimiters=("=",))
        parser.optionxform = str  # type: ignore[assignment,method-assign]
        parser.read_string((STATUS / name).read_text(), source=name)
        check(set(parser.sections()) <= UNIT_SECTIONS, f"{name}: {parser.sections()}")
        if name.endswith(".service"):
            for key, value in (("DynamicUser", "yes"), ("NoNewPrivileges", "yes"),
                               ("ProtectSystem", "strict"), ("ProtectHome", "yes"),
                               ("PrivateTmp", "yes")):
                check(parser.get("Service", key, fallback="") == value, f"{name}: {key}")

    done = so_status(scratch, "page", settings={"stale_aftr": "900"})
    check(done.returncode == 2 and "stale_aftr" in done.stderr, f"misspelt key: {done}")


def probe_clones_pushes_lands(scratch: Path) -> None:
    """`so-status probe` clones the probe repository, commits a file, pushes it to
    refs/land/main and reads main back; each try is one `ok ... done` record, the land is in the
    served main, and a second try lands again on the first."""
    store, env = served_store(scratch)
    url = f"ssh://so@probe-host/{STORE}"
    for attempt in (1, 2):
        done = so_status(scratch, "probe", settings={"ssh_url": url}, env=env)
        check(done.returncode == 0, f"try {attempt}: exit {done.returncode}: {done.stderr}")
        found = records(scratch)
        check(len(found) == attempt, f"try {attempt}: records {found}")
        check(found[-1].group(2, 3, 5) == ("ssh", "ok", "done"), f"try {attempt}: {found[-1]}")
    check("probe-ssh" in main_tree(store), f"the land is not in main: {main_tree(store)}")
    check(not list(scratch.glob("so-status.*")), "a try left its clone")

    # Records are kept to `keep` lines, the newest.
    done = so_status(scratch, "probe", settings={"ssh_url": url, "keep": "1"}, env=env)
    check(done.returncode == 0, f"keep: {done.stderr}")
    check(len(records(scratch)) == 1, "keep=1 kept more than one record")


def probe_period_is_one_minute(scratch: Path) -> None:
    """The probe's period is one minute wherever it is set: the default, the settings file, the
    unit's timer and the workflow (its schedule divided by its rounds); a planted 5-minute timer
    is found."""

    def problems(timer_text: str) -> list[str]:
        found = []
        settings = dict(line.split("=", 1) for line in SETTINGS.read_text().splitlines()
                        if line and not line.startswith("#"))
        if settings.get("period") != str(ONE_MINUTE_S):
            found.append(f"status.conf: period={settings.get('period')}")
        default = re.search(r"^period=(\d+)$", SO_STATUS.read_text(), re.MULTILINE)
        if not default or default.group(1) != str(ONE_MINUTE_S):
            found.append("so-status: the default period is not 60")
        timer = configparser.ConfigParser(interpolation=None, strict=True, delimiters=("=",))
        timer.optionxform = str  # type: ignore[assignment,method-assign]
        timer.read_string(timer_text)
        if timer.get("Timer", "OnUnitActiveSec", fallback="") != "1min":
            found.append(f"timer: OnUnitActiveSec={timer.get('Timer', 'OnUnitActiveSec')}")
        workflow = WORKFLOW.read_text()
        cron = re.search(r'cron: "\*/(\d+) \* \* \* \*"', workflow)
        rounds = re.search(r"so-status loop (\d+)", workflow)
        if not cron or not rounds or int(cron.group(1)) != int(rounds.group(1)):
            found.append("status.yml: the schedule is not one round a minute")
        return found

    text = TIMER.read_text()
    found = problems(text)
    check(not found, f"{found}")
    planted = text.replace("OnUnitActiveSec=1min", "OnUnitActiveSec=5min")
    check(planted != text and problems(planted), "a planted 5-minute timer is not found")


def stale_probe_reads_stale(scratch: Path) -> None:
    """Records that stopped more than `stale_after` ago read stale, fresh ones operational; the
    page carries its own time so a browser reads it stale when no new page follows."""
    now = int(time.time())
    write_records(scratch, [(now - 30, "ssh", "ok", 2, "done"), (now - 20, "https", "ok", 3, "done")])
    fresh = page(scratch, {"ssh_url": "ssh://so@probe-host/probe",
                           "https_url": "https://probe-host/probe", "stale_after": "900"})
    check(state(fresh) == OPERATIONAL, f"fresh records read {state(fresh)}")

    old = now - 3600
    write_records(scratch, [(old - 60, "ssh", "ok", 2, "done"), (old, "https", "ok", 3, "done")])
    stale = page(scratch, {"ssh_url": "ssh://so@probe-host/probe",
                           "https_url": "https://probe-host/probe", "stale_after": "900"})
    check(state(stale) == STALE, f"hour-old records read {state(stale)}")
    generated = re.search(r'data-generated="(\d+)" data-stale-after="900"', stale)
    check(generated is not None and abs(int(generated.group(1)) - now) < ONE_MINUTE_S,
          "the page does not carry its own time and the stale bound")
    check("<script>" in stale and "stale" in stale.split("<script>", 1)[1],
          "no in-page check turns an old page stale")


def page_shows_copies_and_backup_age(scratch: Path) -> None:
    """The page states copies, work at risk (the backup period), the newest good backup's age
    and the last rehearsal's restore time; a malformed backup record is refused and the page
    left as it was."""
    now = int(time.time())
    write_records(scratch, [(now - 10, "ssh", "ok", 2, "done")])
    backups = scratch / "backups"
    backups.write_text(f"{now - 7200} backup ok\n"
                       f"{now - 1800} backup ok\n"
                       f"{now - 600} backup fail\n"
                       f"{now - 86400} rehearsal ok 1260\n")
    settings = {"ssh_url": "ssh://so@probe-host/probe", "backups": str(backups),
                "backup_period": "3600", "copies": "1"}
    text = page(scratch, settings)
    for shown in ("Copies</th><td>1<", "Work at risk</th><td>1 h 0 min<",
                  "Newest good backup</th><td>30 min ago<", "Restore time</th><td>21 min<"):
        check(shown in text, f"the page lacks {shown!r}")
    check(str(scratch) not in text, "the page names a host path")
    check((scratch / "www" / "records").read_text() == (scratch / "records").read_text(),
          "the page does not carry the records it was made from")

    backups.write_text(f"{now} backup maybe\n")
    before = (scratch / "www" / "index.html").read_bytes()
    done = so_status(scratch, "page", settings=settings)
    check(done.returncode == 2 and "backup record" in done.stderr, f"malformed: {done}")
    check((scratch / "www" / "index.html").read_bytes() == before, "a refused page wrote")

    backups.write_text("")
    text = page(scratch, settings)
    check("Newest good backup</th><td>none<" in text, "no backup does not read none")
    check("Restore time</th><td>no rehearsal yet<" in text, "no rehearsal reads otherwise")


def stopped_host_turns_red(scratch: Path) -> None:
    """Self-check: a host that refuses every connection fails the try at its clone, and the
    page reads failing; one good entrance does not hide a failed one."""
    refuse = scratch / "ssh"
    refuse.write_text("#!/bin/sh\necho 'ssh: connect to host port 22: Connection refused' >&2\n"
                      "exit 255\n")
    refuse.chmod(0o755)
    settings = {"ssh_url": "ssh://so@probe-host/probe"}
    done = so_status(scratch, "probe", settings=settings,
                     env={"GIT_SSH_COMMAND": str(refuse), "GIT_SSH_VARIANT": "ssh"})
    check(done.returncode == 1, f"a refused probe exited {done.returncode}: {done.stderr}")
    found = records(scratch)
    check([m.group(2, 3, 5) for m in found] == [("ssh", "fail", "clone")], f"records: {found}")
    text = page(scratch, settings)
    check(state(text) == FAILING, f"a stopped host reads {state(text)}")

    now = int(time.time())
    write_records(scratch, [(now - 5, "ssh", "ok", 2, "done"), (now - 4, "https", "fail", 9, "land")])
    text = page(scratch, {**settings, "https_url": "https://probe-host/probe"})
    check(state(text) == FAILING, f"a failed https read {state(text)}")
    write_records(scratch, [(now - 5, "ssh", "ok", 2, "done")])
    text = page(scratch, {**settings, "https_url": "https://probe-host/probe"})
    check(state(text) == FAILING, f"an entrance with no record read {state(text)}")
    (scratch / "records").unlink()
    text = page(scratch, settings)
    check(state(text) == FAILING, f"no record at all read {state(text)}")


CASES: tuple[tuple[str, Callable[[Path], None]], ...] = (
    ("status files parse", status_files_parse),
    ("the probe clones, pushes and lands", probe_clones_pushes_lands),
    ("the probe's period is one minute", probe_period_is_one_minute),
    ("a stale probe reads stale", stale_probe_reads_stale),
    ("the page shows copies and the backup age", page_shows_copies_and_backup_age),
    ("self-check: a stopped host turns the page red", stopped_host_turns_red),
)


def main() -> int:
    global BIN_DIR
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin-dir", type=Path, default=default_bin_dir())
    BIN_DIR = parser.parse_args().bin_dir.resolve()
    if not shutil.which("git"):
        print("test-server-status: git is not on PATH", file=sys.stderr)
        return 1

    failures = []
    for label, case in CASES:
        with tempfile.TemporaryDirectory(prefix="test-server-status-") as scratch:
            try:
                case(Path(scratch))
            except Exception as error:  # a case's crash is its failure, not the run's
                failures.append(f"{label}: {type(error).__name__}: {error}")

    print(f"test-server-status: {len(CASES) - len(failures)} of {len(CASES)} cases passed")
    for failure in failures:
        print(f"test-server-status: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
