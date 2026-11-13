# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Installation acceptance: the server package installed on an empty image, offline, through
every entrance (M28 criterion 2).

    python3 -I scripts/server-acceptance.py fetch [--cache D]          # with the network, once
    python3 -I scripts/server-acceptance.py run [--bin-dir D] [--cache D] [--method M]...
    python3 -I scripts/server-acceptance.py selftest                   # offline, no docker

`fetch` downloads, into the cache, what an offline install needs beside the release's own
packages: for each install method the distribution packages the server package and the run
need, resolved against the same base image pinned by digest that the run starts, and the
previous release's packages, checked against its SHA256SUMS. A part already in the cache is
not fetched again. `run` reaches no network: it packs the build under test (D, default the
debug build under CARGO_TARGET_DIR or target/) as a release archive, makes its packages with
scripts/packages.py, and for each method:

    empty image ──install──▶ guide's setup ──▶ so-kit run accept full ──▶ the cases below
    (docker network --internal: the installation, a stock git server, a bucket stand-in)

    case                           holds when
    offline_install                every container's networks are internal; the packages and
                                   their dependencies install from local files alone
    first_push_in_five_minutes     the acceptance level, which pushes with git and with so,
                                   ends within 300 s of the install's start
    so_through_a_stock_git_server  one store with two remotes, the installation and a stock
                                   git server over ssh: a land on the installation reaches the
                                   git server through the store, a git server commit reaches
                                   the installation (so to GitHub's stand-in)
    backup_wipe_restore            so-backup to a bucket stand-in, the stores wiped, restored by
                                   the drill: every ref equal, served again
    upgrade_from_previous_release  the previous release's swarmops with a FORMAT 1 and a FORMAT
                                   2 store, upgraded to this build's packages: the package moves
                                   no store, `swarmops upgrade` does, every ref equal
    remove_keeps_the_stores        removing the server package keeps /srv/so's stores

The acceptance level's own cases live in packaging/server/accept/, which the package installs;
this script installs and runs them and holds none of their names. Methods: `deb` (Debian 12,
apt-get), `rpm` (AlmaLinux 9, dnf) and `image` (the container image, which waits on the image
slice). Last, the self-check `network_on_fails_the_run` plants a container on a network with a
route out and needs the offline check to find it. Needs docker. Exit 1 on any failure.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import platform
import re
import secrets
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = ROOT / "scripts"
KIT = ROOT / "packaging" / "server"
ACCEPT = KIT / "accept" / "accept"
ACCEPT_MANIFEST = KIT / "accept" / "MANIFEST"
RELEASES = "https://github.com/KeiaiLab/SwarmOps/releases/download"
SUMS = "SHA256SUMS"

# The run's places in the installation, and the acceptance identity's login.
HOST = "so-host"
GIT_HOST = "git-host"
SERVING = "so"
LOGIN = "accept"
STORE = "accept"
IDENTITY = f"person/{LOGIN}"
SECRETS = "/etc/swarmops-server/secrets"
ACCEPT_SECRETS = f"{SECRETS}/accept"
BACKUP_SECRETS = f"{SECRETS}/backup"
HOME = "/var/lib/swarmops-server"
STORES = "/srv/so"
FIRST_PUSH_LIMIT_S = 300
COMMAND_WAIT_S = 900
WAIT_STEP_S = 0.2
READY_WAIT_S = 60

# What an install needs beyond each base image, by method: the server package's dependencies
# and recommendations, and the run's own tools. The bucket stand-in runs python3 on Debian.
DEB_NEEDS = ("ca-certificates", "curl", "fcgiwrap", "git", "nginx", "openssh-client",
             "openssh-server", "openssl")
RPM_NEEDS = ("fcgiwrap", "findutils", "gawk", "git", "nginx", "openssh-clients",
             "openssh-server", "openssl", "shadow-utils", "tar", "util-linux")
BUCKET_NEEDS = ("python3",)


class Failure(Exception):
    """A check that did not hold, with what was seen."""


def check(holds: bool, message: str) -> None:
    """Fails with `message` unless `holds`."""
    if not holds:
        raise Failure(message)


def load(name: str, path: Path):
    """A script of this repository as a module."""
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None, f"cannot load {path}"
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


PACKAGES = load("packages", SCRIPTS / "packages.py")


# ---------------------------------------------------------------------------------------------
# Containers


@dataclass
class Ran:
    """One command's end."""

    code: int
    out: str
    err: str


def docker(*argv: str, stdin: str = "") -> Ran:
    """A docker command; its end."""
    done = subprocess.run(["docker", *argv], input=stdin, capture_output=True, text=True,
                          timeout=COMMAND_WAIT_S, check=False)
    return Ran(done.returncode, done.stdout, done.stderr)


def must(*argv: str) -> str:
    """A docker command that must succeed; its stdout."""
    done = docker(*argv)
    check(done.code == 0, f"docker {' '.join(argv[:2])}: {done.err.strip()}")
    return done.out


@dataclass
class Box:
    """A running container; commands run in it with `docker exec`."""

    name: str

    @staticmethod
    def start(image: str, name: str, options: list[str], argv: list[str]) -> "Box":
        """Starts `image` detached, removed when stopped."""
        must("run", "--detach", "--rm", "--init", "--name", name, *options, image, *argv)
        return Box(name)

    def stop(self) -> None:
        docker("rm", "--force", self.name)

    def run(self, argv: list[str], user: str = "root", stdin: str = "",
            env: dict[str, str] | None = None) -> Ran:
        line = ["exec", "--interactive", "--user", user, "--workdir", "/"]
        for key, value in (env or {}).items():
            line += ["--env", f"{key}={value}"]
        return docker(*line, self.name, *argv, stdin=stdin)

    def sh(self, script: str, user: str = "root", env: dict[str, str] | None = None) -> str:
        """`sh -c script`, which must succeed; its stdout."""
        done = self.run(["sh", "-c", script], user, env=env)
        tail = f"{done.err[-3000:]}{done.out[-1000:]}"
        check(done.code == 0, f"{self.name}: exit {done.code}: {tail}")
        return done.out


def offline_problems(name: str) -> list[str]:
    """The networks of a container that reach beyond the host: each must be `none` or internal."""
    attached = json.loads(must("inspect", "--format", "{{json .NetworkSettings.Networks}}", name))
    problems = []
    for network in attached or {}:
        if network == "none":
            continue
        internal = must("network", "inspect", "--format", "{{.Internal}}", network).strip()
        if internal != "true":
            problems.append(f"{name} is on {network}, which is not internal")
    if not attached:
        problems.append(f"{name} is on no network docker reports")
    return problems


# ---------------------------------------------------------------------------------------------
# Install methods


@dataclass(frozen=True)
class Method:
    """One way to install the server: its base image, what it needs, its package tool."""

    name: str
    image: str
    needs: tuple[str, ...]
    web_user: str
    install: str  # the shell command, `{files}` the local package files
    remove: str

    def deps(self, cache: Path, needs: tuple[str, ...] = ()) -> Path:
        """The cache directory of this method's dependency packages."""
        wanted = needs or self.needs
        key = hashlib.sha256(f"{self.image}\n{' '.join(sorted(wanted))}".encode()).hexdigest()
        return cache / f"{self.name}-{key[:16]}"


DEB = Method(
    "deb", PACKAGES.DEBIAN_IMAGE, DEB_NEEDS, "www-data",
    # No init system in a container: nothing a package installs starts (Debian's convention).
    "printf '#!/bin/sh\\nexit 101\\n' >/usr/sbin/policy-rc.d && chmod 0755 /usr/sbin/policy-rc.d"
    " && DEBIAN_FRONTEND=noninteractive apt-get install --yes --quiet --no-install-recommends"
    " {files}",
    "DEBIAN_FRONTEND=noninteractive apt-get purge --yes --quiet swarmops-server",
)
RPM = Method(
    "rpm", PACKAGES.ALMALINUX_IMAGE, RPM_NEEDS, "nginx",
    "dnf install --assumeyes --quiet --disablerepo='*' --setopt=install_weak_deps=False {files}",
    "dnf remove --assumeyes --quiet --disablerepo='*' swarmops-server",
)
METHODS = {"deb": DEB, "rpm": RPM}
IMAGE_METHOD = "image"


def fetch_deps(method: Method, cache: Path, needs: tuple[str, ...]) -> None:
    """Downloads `needs` and what they need, as the base image lacks them, unless cached."""
    out = method.deps(cache, needs)
    if (out / ".complete").is_file():
        print(f"server-acceptance: {out.name} is cached")
        return
    out.mkdir(parents=True, exist_ok=True)
    owner = f"{os.getuid()}:{os.getgid()}"
    names = " ".join(needs)
    if method is DEB:
        script = ("apt-get update --quiet && DEBIAN_FRONTEND=noninteractive apt-get install"
                  f" --yes --quiet --download-only --no-install-recommends {names}"
                  " && cp /var/cache/apt/archives/*.deb /out/")
    else:
        script = ("dnf install --assumeyes --quiet epel-release && dnf install --assumeyes"
                  " --quiet --downloadonly --downloaddir=/out"
                  f" --setopt=install_weak_deps=False {names}")
    script += f" && chown -R {owner} /out"
    done = docker("run", "--rm", "--volume", f"{out}:/out", method.image, "sh", "-c", script)
    check(done.code == 0, f"fetch {out.name}: {done.err[-3000:]}")
    (out / ".complete").write_text(f"{method.image}\n{names}\n")
    print(f"server-acceptance: fetched {out.name}")


# ---------------------------------------------------------------------------------------------
# The release under test and the previous one


def previous_release() -> str:
    """The newest release CHANGELOG.md names."""
    for line in (ROOT / "CHANGELOG.md").read_text().splitlines():
        found = re.match(r"## \[(\d+\.\d+\.\d+)\]", line)
        if found:
            return found.group(1)
    raise Failure("CHANGELOG.md names no release")


def tested_version(previous: str) -> str:
    """The version the build under test is packaged as: the workspace's when it is newer than
    the previous release, else the next patch as a pre-release, so that it upgrades."""
    text = (ROOT / "Cargo.toml").read_text()
    found = re.search(r'^version = "(\d+\.\d+\.\d+)"', text, re.MULTILINE)
    check(found is not None, "Cargo.toml names no workspace version")
    ours = found.group(1)
    if PACKAGES.version_key(ours) > PACKAGES.version_key(previous):
        return ours
    major, minor, patch = previous.split(".")
    return f"{major}.{minor}.{int(patch) + 1}-acceptance"


def target() -> "PACKAGES.Target":
    """This host's release target."""
    found = [t for t in PACKAGES.TARGETS.values() if t.rpm == platform.machine()]
    check(bool(found), f"no release target for {platform.machine()}")
    return found[0]


def previous_dir(cache: Path, previous: str) -> Path:
    return cache / f"release-{previous}"


def fetch_previous(cache: Path, previous: str) -> None:
    """The previous release's packages for this host, checked against its SHA256SUMS."""
    out = previous_dir(cache, previous)
    if (out / ".complete").is_file():
        print(f"server-acceptance: release {previous} is cached")
        return
    out.mkdir(parents=True, exist_ok=True)
    base = f"{RELEASES}/v{previous}"
    with urllib.request.urlopen(f"{base}/{SUMS}", timeout=60) as answer:
        sums = answer.read().decode()
    digests = {line.split()[1]: line.split()[0] for line in sums.splitlines() if line.strip()}
    wanted = [n for name in (PACKAGES.PACKAGE, PACKAGES.SERVER)
              for n in PACKAGES.package_names(previous, target(), name) if n in digests]
    check(len(wanted) >= 2, f"release {previous} has no packages for {target().triple}")
    for name in wanted:
        with urllib.request.urlopen(f"{base}/{name}", timeout=300) as answer:
            data = answer.read()
        check(hashlib.sha256(data).hexdigest() == digests[name], f"{name}: not its SHA256SUMS")
        (out / name).write_bytes(data)
    (out / ".complete").write_text("\n".join(wanted) + "\n")
    print(f"server-acceptance: fetched release {previous}: {', '.join(wanted)}")


def pack(bin_dir: Path, version: str, out: Path) -> list[Path]:
    """The build as a release archive, as release.yml packs it, then its four packages."""
    top = f"swarmops-{version}-{target().triple}"
    archive = out / f"{top}.tar.gz"
    files = [(n, bin_dir / n, 0o755) for n in PACKAGES.BINARIES]
    files += [(n, ROOT / n, 0o644) for n in PACKAGES.DOCS]
    with tarfile.open(archive, "w:gz") as tar:
        entry = tarfile.TarInfo(top)
        entry.type, entry.mode, entry.mtime = tarfile.DIRTYPE, 0o755, 1
        tar.addfile(entry)
        for name, path, mode in files:
            entry = tarfile.TarInfo(f"{top}/{name}")
            entry.size, entry.mode, entry.mtime = path.stat().st_size, mode, 1
            with path.open("rb") as data:
                tar.addfile(entry, data)
    built = out / "packages"
    PACKAGES.build(archive, built, KIT)
    return sorted(built.iterdir())


# ---------------------------------------------------------------------------------------------
# The installation: install, the guide's setup, the entrances


# The guide's host steps after the install, as root: host keys, the certificate, the web login,
# the kit's settings, the acceptance repository with its key line, the root page.
GUIDE = f"""set -eu
ssh-keygen -A
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \\
    -subj "/CN=$HOST" -addext "subjectAltName=DNS:$HOST" \\
    -keyout /etc/swarmops-server/tls/privkey.pem -out /etc/swarmops-server/tls/fullchain.pem \\
    2>/dev/null
printf '%s:%s\\n' "$LOGIN" "$(openssl passwd -apr1 "$PASSWORD")" >/etc/swarmops-server/web.htpasswd
sed -i "s/^web_user=.*/web_user=$WEB_USER/" /etc/swarmops-server/kit.conf
so-kit setup
runuser -u {SERVING} -- so-kit create-store "$STORE" "person/$LOGIN"
runuser -u {SERVING} -- sh -c 'cat >"{STORES}/$1/access.toml"' access "$STORE" <<EOF
version = 1

[[grant]]
who = ["person/$LOGIN"]
read = true
push = ["main"]
land = ["main"]
EOF
install -d -o {SERVING} -g {SERVING} -m 0700 {ACCEPT_SECRETS}
runuser -u {SERVING} -- ssh-keygen -q -t ed25519 -N '' -C accept -f {ACCEPT_SECRETS}/id
runuser -u {SERVING} -- so-kit add-key "person/$LOGIN" {ACCEPT_SECRETS}/id.pub
runuser -u {SERVING} -- so-kit page
"""

# What a host's init system does once the operator enables the units; a container has none.
ENTRANCES = f"""set -eu
rm -f /etc/nginx/sites-enabled/default
mkdir -p /run/sshd
/usr/sbin/sshd
install -d -o {SERVING} -g swarmops-web -m 0750 /run/swarmops-server
nginx
"""
FCGIWRAP = (f"runuser -u {SERVING} -g swarmops-web -- sh -c"
            " 'umask 007; exec fcgiwrap -s unix:/run/swarmops-server/fcgiwrap.socket'")

# The acceptance program's settings for this installation.
ACCEPT_CONF = f"""accept_host={HOST}
accept_ssh_port=22
accept_https_port=443
accept_store={STORE}
accept_identity={IDENTITY}
accept_key={ACCEPT_SECRETS}/id
accept_password={ACCEPT_SECRETS}/password
accept_ca={ACCEPT_SECRETS}/ca.pem
accept_work=/tmp
"""


@dataclass
class Run:
    """One method's run: its network, volume, containers and the packages."""

    method: Method
    cache: Path
    packages: list[Path]
    previous: str
    tag: str
    network: str = ""
    volume: str = ""

    def mounts(self) -> list[str]:
        """The packages under test, the dependencies and the previous release, read-only."""
        deps = self.method.deps(self.cache)
        check((deps / ".complete").is_file(), f"{deps.name} is not cached: run `fetch` first")
        out = ["--volume", f"{self.packages[0].parent}:/packages:ro"]
        out += ["--volume", f"{deps}:/deps:ro"]
        prior = previous_dir(self.cache, self.previous)
        check((prior / ".complete").is_file(), f"{prior.name} is not cached: run `fetch` first")
        return out + ["--volume", f"{prior}:/previous:ro"]

    def ours(self, name: str) -> str:
        """The local path of this build's package `name` for the method."""
        deb, rpm = PACKAGES.package_names(self.version(), target(), name)
        return f"/packages/{deb if self.method is DEB else rpm}"

    def version(self) -> str:
        return tested_version(self.previous)

    def start(self, name: str, alias: str) -> Box:
        """An empty base image on the run's internal network."""
        options = ["--network", self.network, "--network-alias", alias, "--hostname", alias]
        if alias == HOST:
            options += ["--volume", f"{self.volume}:{STORES}"]
        return Box.start(self.method.image, f"{self.tag}-{name}", options + self.mounts(),
                         ["sleep", "infinity"])


def write(box: Box, path: str, text: str, user: str = SERVING) -> None:
    """A file of the container written whole, as `user`."""
    done = box.run(["sh", "-c", f"cat >{path}"], user, stdin=text)
    check(done.code == 0, f"cannot write {path}: {done.err}")


def install(run: Run, box: Box) -> None:
    """Every dependency and this build's two packages from local files."""
    files = f"/deps/*.{run.method.name} {run.ours(PACKAGES.PACKAGE)} {run.ours(PACKAGES.SERVER)}"
    box.sh(run.method.install.format(files=files))


def set_up(run: Run, box: Box, password: str) -> None:
    """The guide's steps, the entrances started, the acceptance program's settings."""
    env = {"HOST": HOST, "LOGIN": LOGIN, "PASSWORD": password, "STORE": STORE,
           "WEB_USER": run.method.web_user}
    box.sh(GUIDE, env=env)
    box.sh(ENTRANCES)
    must("exec", "--detach", box.name, "sh", "-c", FCGIWRAP)
    deadline = time.monotonic() + READY_WAIT_S
    while box.run(["test", "-S", "/run/swarmops-server/fcgiwrap.socket"]).code != 0:
        check(time.monotonic() < deadline, "fcgiwrap did not start")
        time.sleep(WAIT_STEP_S)
    box.sh(f"printf %s {password} >{ACCEPT_SECRETS}/password && "
           f"cat /etc/swarmops-server/tls/fullchain.pem >{ACCEPT_SECRETS}/ca.pem && "
           f"chown {SERVING}:{SERVING} {ACCEPT_SECRETS}/password {ACCEPT_SECRETS}/ca.pem")
    write(box, "/etc/swarmops-server/accept.conf", ACCEPT_CONF, "root")


# ---------------------------------------------------------------------------------------------
# The cases beyond the acceptance level

# A client environment for the serving account: its own git settings and ssh with the
# acceptance key, trusting a host's key at first sight on the run's own network.
CLIENT = f"""set -eu
work=$1
mkdir -p "$work/home"
export HOME="$work/home" GIT_CONFIG_GLOBAL="$work/gitconfig" GIT_CONFIG_NOSYSTEM=1
git config --global user.name Acceptance
git config --global user.email acceptance@{HOST}.invalid
export GIT_SSH_COMMAND="ssh -F /dev/null -i {ACCEPT_SECRETS}/id -o IdentitiesOnly=yes \\
    -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$work/known"
cd "$work"
"""


def client(box: Box, work: str, script: str) -> str:
    """`script` as the serving account in a client environment under `work`; its stdout."""
    return box.sh(f"set -- {work}\n" + CLIENT + script, user=SERVING).strip()


def ls_main(box: Box, work: str, url: str) -> str:
    """The main a URL serves, read by git."""
    return client(box, work, f"git ls-remote {url} refs/heads/main | cut -f 1")


def so_result(text: str) -> dict:
    """The result of `so`'s envelope, which must be ok."""
    envelope = json.loads(text.strip().splitlines()[-1])
    check(envelope.get("ok") is True, f"so: {text}")
    return envelope["result"]


def git_server(run: Run, box: Box) -> Box:
    """A stock git server over ssh: account `git` with git-shell, an empty bare repository,
    the acceptance key allowed."""
    server = run.start("git", GIT_HOST)
    server.sh(run.method.install.format(files=f"/deps/*.{run.method.name}"))
    key = box.sh(f"cat {ACCEPT_SECRETS}/id.pub")
    server.sh("set -eu\nssh-keygen -A\nmkdir -p /run/sshd\n"
              "useradd --create-home --shell /usr/bin/git-shell git\n"
              "install -d -o git -g git -m 0700 /home/git/.ssh\n"
              f"printf '%s\\n' '{key.strip()}' >/home/git/.ssh/authorized_keys\n"
              "chown git:git /home/git/.ssh/authorized_keys\n"
              "runuser -u git -- git init --quiet --bare --initial-branch main /home/git/gh.git\n"
              "/usr/sbin/sshd\n")
    return server


def so_through_git_server(run: Run, box: Box) -> None:
    """One store, two remotes: the installation and a stock git server."""
    server = git_server(run, box)
    try:
        work = f"{HOME}/two"
        ours = f"ssh://{SERVING}@{HOST}/{STORE}.git"
        theirs = f"ssh://git@{GIT_HOST}/home/git/gh.git"

        # 1. The store: a clone of the installation, the git server a second remote.
        client(box, work, f"so --as {IDENTITY} clone {ours} --into both >/dev/null")
        client(box, work, f"so --repo both remote add gh {theirs} --branch main >/dev/null")
        so_result(client(box, work, "so --repo both push gh"))
        check(ls_main(box, work, theirs) == ls_main(box, work, ours), "the first push to gh")

        # 2. A land on the installation, fetched and synced into the store, pushed on.
        landed = client(box, work, f"git clone -q {ours} land && cd land && echo x >land.txt"
                                   " && git add land.txt && git commit -qm land"
                                   " && git push -q origin HEAD:refs/land/main"
                                   " && git ls-remote origin refs/heads/main | cut -f 1")
        so_result(client(box, work, "so --repo both fetch origin"))
        synced = so_result(client(box, work, f"so --as {IDENTITY} --repo both sync origin"))
        check(synced.get("status") == "fast_forwarded", f"sync origin: {synced}")
        pushed = so_result(client(box, work, "so --repo both push gh"))
        reached = ls_main(box, work, theirs)
        check(reached == landed, f"the land {landed} did not reach the git server: {pushed}")

        # 3. A commit on the git server reaches the installation through the store.
        made = server.sh("runuser -u git -- sh -c 'cd /tmp && git clone -q /home/git/gh.git w"
                         " && cd w && echo y >gh.txt && git add gh.txt"
                         " && git -c user.name=G -c user.email=g@git-host.invalid commit -qm gh"
                         " && git push -q origin HEAD:main && git rev-parse HEAD'").strip()
        so_result(client(box, work, "so --repo both fetch gh"))
        synced = so_result(client(box, work, f"so --as {IDENTITY} --repo both sync gh"))
        check(synced.get("status") == "fast_forwarded", f"sync gh: {synced}")
        so_result(client(box, work, "so --repo both push origin"))
        check(ls_main(box, work, ours) == made, "the git server's commit did not reach us")
    finally:
        server.stop()


BUCKET_DRIVER = """import importlib.util, signal
spec = importlib.util.spec_from_file_location("backup", "/repo/scripts/test-server-backup.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
bucket = module.Bucket()
print(bucket.endpoint, flush=True)
signal.pause()
"""


def bucket(run: Run, box: Box) -> tuple[Box, str]:
    """test-server-backup.py's bucket stand-in, on the installation's loopback address; its
    endpoint."""
    options = ["--network", f"container:{box.name}",
               "--volume", f"{SCRIPTS}:/repo/scripts:ro",
               "--volume", f"{KIT / 'backup' / 'policy'}:/repo/packaging/server/backup/policy:ro"]
    options += ["--volume", f"{DEB.deps(run.cache, BUCKET_NEEDS)}:/deps:ro"]
    script = (f"{DEB.install.format(files='/deps/*.deb')} >/dev/null"
              ' && exec python3 -I -c "$DRIVER"')
    options += ["--env", f"DRIVER={BUCKET_DRIVER}"]
    stand_in = Box.start(DEB.image, f"{run.tag}-bucket", options, ["sh", "-c", script])
    deadline = time.monotonic() + COMMAND_WAIT_S
    while True:
        logs = docker("logs", stand_in.name).out.split()
        if logs and logs[-1].startswith("http://127.0.0.1:"):
            return stand_in, logs[-1]
        check(time.monotonic() < deadline, "the bucket stand-in did not start")
        time.sleep(WAIT_STEP_S)


def backup_settings(endpoint: str, side: str, module) -> str:
    """so-backup's settings for the serving side or the rehearsal side."""
    lines = [f"backup_endpoint={endpoint}", f"backup_region={module.REGION}",
             f"backup_bucket={module.BUCKET}",
             f"backup_credential={BACKUP_SECRETS}/{side}.credential",
             f"backup_recovery_recipient={BACKUP_SECRETS}/recovery.pem",
             f"backup_rehearsal_recipient={BACKUP_SECRETS}/rehearsal.pem",
             f"backup_secrets={HOME}/.ssh/authorized_keys", f"backup_alert={HOME}/alert"]
    if side == "rehearsal":
        lines.append(f"backup_rehearsal_key={BACKUP_SECRETS}/rehearsal.key")
    return "\n".join(lines) + "\n"


def stores_refs(box: Box) -> dict[str, list]:
    """Each store's refs, by name."""
    names = box.sh(f"ls {STORES}", user=SERVING).split()
    return {n: so_result(box.sh(f"swarmops --repo {STORES}/{n} ref list", user=SERVING))["refs"]
            for n in names}


def backup_wipe_restore(run: Run, box: Box) -> None:
    """so-backup to the bucket stand-in; the stores wiped; the drill restores them; equal."""
    module = load("server_backup", SCRIPTS / "test-server-backup.py")
    stand_in, endpoint = bucket(run, box)
    try:
        # 1. Keys, credentials and both sides' settings, as the serving account.
        box.sh(f"set -eu\ninstall -d -o {SERVING} -g {SERVING} -m 0700 {BACKUP_SECRETS}\n"
               f"printf '#!/bin/sh\\nprintf \"%s %s\\\\n\" \"$1\" \"$2\" >>{HOME}/alerts\\n'"
               f" >{HOME}/alert\nchmod 0755 {HOME}/alert\n")
        for name in ("recovery", "rehearsal"):
            box.sh("openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes"
                   f" -subj /CN={name} -days 30 -keyout {BACKUP_SECRETS}/{name}.key"
                   f" -out {BACKUP_SECRETS}/{name}.pem 2>/dev/null", user=SERVING)
        for side, (key_id, secret) in module.CREDENTIALS.items():
            write(box, f"{BACKUP_SECRETS}/{side}.conf", backup_settings(endpoint, side, module))
            write(box, f"{BACKUP_SECRETS}/{side}.credential",
                  f"access_key_id={key_id}\nsecret_access_key={secret}\n")

        # 2. Backup; the stores wiped; the drill restores onto an empty directory; moved back.
        before = stores_refs(box)
        check(bool(before), "no store to back up")
        url = f"ssh://{SERVING}@{HOST}/{STORE}.git"
        served = ls_main(box, f"{HOME}/restored", url)
        box.sh(f"SO_BACKUP_CONF={BACKUP_SECRETS}/serving.conf so-backup backup", user=SERVING)
        box.sh(f"find {STORES} -mindepth 1 -delete", user=SERVING)
        box.sh(f"mkdir {HOME}/restore && SO_BACKUP_CONF={BACKUP_SECRETS}/rehearsal.conf"
               f" so-backup rehearse {HOME}/restore && mv {HOME}/restore/stores/* {STORES}/",
               user=SERVING)

        # 3. Every ref equal, verify --deep clean, served again.
        check(stores_refs(box) == before, "the restored refs differ")
        for name in before:
            box.sh(f"swarmops --repo {STORES}/{name} verify --deep", user=SERVING)
        again = client(box, f"{HOME}/restored", f"git clone -q {url} c && git -C c rev-parse HEAD")
        check(again == served, f"the restored store serves {again}, not {served}")
    finally:
        stand_in.stop()


def remove_keeps_stores(run: Run, box: Box) -> None:
    """The server package's removal removes its files and keeps the stores."""
    box.sh(run.method.remove)
    check(box.run(["test", "-e", "/usr/sbin/so-kit"]).code != 0, "so-kit outlived the removal")
    box.sh(f"swarmops --repo {STORES}/{STORE} verify --deep", user=SERVING)


def targets(box: Box, store: str) -> dict[str, str]:
    """A store's refs: name to target."""
    refs = so_result(box.sh(f"swarmops --repo {store} ref list"))["refs"]
    return {ref["name"]: ref["target"] for ref in refs}


def upgrade_from_previous(run: Run) -> None:
    """The previous release's swarmops with a FORMAT 1 and a FORMAT 2 store, then this build's
    packages: the package moves no store; `swarmops upgrade` does, every ref equal."""
    box = run.start("upgrade", "upgrade")
    try:
        deb, rpm = PACKAGES.package_names(run.previous, target())
        prior = f"/previous/{deb if run.method is DEB else rpm}"
        box.sh(run.method.install.format(files=f"/deps/*.{run.method.name} {prior}"))
        changes = '[{"path":"f.txt","op":"put","content":{"inline":"one"}}]'
        before = {}
        for number in (1, 2):
            store = f"/srv/f{number}"
            box.sh(f"swarmops init {store} --format {number} >/dev/null")
            made = box.run(["swarmops", "--repo", store, "--as", "agent/accept", "commit", "-m",
                            "one", "--changes", "-", "--advance", "main"], stdin=changes)
            check(made.code == 0, f"{store}: commit: {made.out}{made.err}")
            before[store] = (targets(box, store),
                             so_result(box.sh(f"swarmops --repo {store} info"))["store_version"])
            check(before[store][1] == number, f"{store}: format {before[store][1]}")

        # 1. This build's packages over the previous: no store moves.
        install(run, box)
        for store, (refs, version) in before.items():
            info = so_result(box.sh(f"swarmops --repo {store} info"))
            check(info["store_version"] == version, f"{store}: the package upgraded it")

        # 2. `swarmops upgrade`: the latest format, every ref equal, verify --deep clean.
        for store, (refs, _) in before.items():
            moved = so_result(box.sh(f"swarmops --repo {store} upgrade"))
            check(moved.get("upgraded") is True, f"{store}: {moved}")
            after = targets(box, store)
            check(after == refs, f"{store}: refs moved: {refs} became {after}")
            box.sh(f"swarmops --repo {store} verify --deep")
    finally:
        box.stop()


# ---------------------------------------------------------------------------------------------
# The run


def method_run(run: Run, failed: list[str]) -> None:
    """One install method: the installation, the acceptance level, the cases beyond it."""
    name = run.method.name
    run.network = f"{run.tag}-net"
    run.volume = f"{run.tag}-stores"
    must("network", "create", "--internal", run.network)
    must("volume", "create", run.volume)
    box = None

    def case(label: str, body: Callable[[], None]) -> bool:
        started = time.monotonic()
        try:
            body()
        except (Failure, subprocess.TimeoutExpired, json.JSONDecodeError, KeyError) as error:
            print(f"FAILED  {name}: {label}: {error}", flush=True)
            failed.append(f"{name}: {label}")
            return False
        print(f"ok      {name}: {label} ({time.monotonic() - started:.1f} s)", flush=True)
        return True

    try:
        # 1. The installation, offline, then the guide and the acceptance level.
        box = run.start("host", HOST)
        started = time.monotonic()
        password = secrets.token_hex(16)

        def offline() -> None:
            problems = offline_problems(box.name)
            check(not problems, "; ".join(problems))
            install(run, box)
            set_up(run, box, password)

        if not case("offline_install", offline):
            return
        accepted = box.run(["so-kit", "run", "accept", "full"], user=SERVING)
        print(accepted.out + accepted.err, end="", flush=True)
        if accepted.code != 0:
            failed.append(f"{name}: so-kit run accept full")
        took = time.monotonic() - started
        print(f"        {name}: the install's start to the acceptance level's end: {took:.0f} s")
        case("first_push_in_five_minutes",
             lambda: check(took <= FIRST_PUSH_LIMIT_S, f"{took:.0f} s after the install began"))

        # 2. The cases beyond it.
        case("so_through_a_stock_git_server", lambda: so_through_git_server(run, box))
        case("backup_wipe_restore", lambda: backup_wipe_restore(run, box))
        case("remove_keeps_the_stores", lambda: remove_keeps_stores(run, box))
        case("upgrade_from_previous_release", lambda: upgrade_from_previous(run))
    finally:
        if box is not None:
            box.stop()
        docker("volume", "rm", "--force", run.volume)
        docker("network", "rm", run.network)


def network_on_fails_the_run(image: str) -> list[str]:
    """A container on docker's default bridge, which routes out: the offline check must find it."""
    box = Box.start(image, f"so-accept-planted-{secrets.token_hex(4)}", ["--network", "bridge"],
                    ["sleep", "infinity"])
    try:
        found = offline_problems(box.name)
        return [] if found else ["the offline check passed a bridged container"]
    finally:
        box.stop()


def default_bin_dir() -> Path:
    target_dir = os.environ.get("CARGO_TARGET_DIR") or str(ROOT / "target")
    return Path(target_dir) / "debug"


def default_cache() -> Path:
    target_dir = os.environ.get("CARGO_TARGET_DIR") or str(ROOT / "target")
    return Path(target_dir) / "server-acceptance"


def fetch(cache: Path) -> int:
    """Everything `run` needs from the network."""
    for method in METHODS.values():
        fetch_deps(method, cache, method.needs)
    fetch_deps(DEB, cache, BUCKET_NEEDS)
    fetch_previous(cache, previous_release())
    return 0


def run_methods(bin_dir: Path, cache: Path, methods: list[str]) -> int:
    """Every method named, then the self-check."""
    missing = [n for n in PACKAGES.BINARIES if not (bin_dir / n).is_file()]
    if missing:
        print(f"server-acceptance: {bin_dir} lacks {', '.join(missing)}", file=sys.stderr)
        return 1
    previous = previous_release()
    failed: list[str] = []
    with tempfile.TemporaryDirectory(prefix="server-acceptance-") as scratch:
        built = pack(bin_dir.resolve(), tested_version(previous), Path(scratch))
        for name in methods:
            if name == IMAGE_METHOD:
                print("FAILED  image: the container image is not in this tree yet (the image"
                      " slice, packaging/container/)")
                failed.append(IMAGE_METHOD)
                continue
            tag = f"so-accept-{secrets.token_hex(4)}"
            method_run(Run(METHODS[name], cache.resolve(), built, previous, tag), failed)

    problems = network_on_fails_the_run(DEB.image)
    for problem in problems:
        print(f"FAILED  network_on_fails_the_run: {problem}")
    if problems:
        failed.append("network_on_fails_the_run")
    else:
        print("ok      network_on_fails_the_run")
    print(f"server-acceptance: {len(failed)} failed" + (f": {', '.join(failed)}" if failed else ""))
    return 1 if failed else 0


# ---------------------------------------------------------------------------------------------
# selftest: offline, no docker


def accept_case_names(text: str) -> list[str]:
    """The acceptance program's case names: its `case_<name>()` functions."""
    return re.findall(r"^case_([a-z0-9_]+)\(\)", text, re.MULTILINE)


def shared_names(accept: str, harness: str) -> list[str]:
    """The acceptance program's case names this script's text holds."""
    return [n for n in accept_case_names(accept) if n in harness]


def accept_cases_are_one_implementation() -> None:
    """No case of packaging/server/accept/ is a case here; a planted one is found."""
    accept = ACCEPT.read_text()
    names = accept_case_names(accept)
    check(len(names) >= 2, f"the acceptance program has cases {names}")
    found = shared_names(accept, Path(__file__).read_text())
    check(not found, f"this script holds acceptance cases: {found}")
    planted = Path(__file__).read_text() + f"\n# {names[0]}\n"
    check(shared_names(accept, planted) == [names[0]], "a planted case name is not found")


def accept_is_installed_where_so_kit_runs_it() -> None:
    """The MANIFEST installs the program at kit.conf's `accept` path; it parses."""
    wanted = next(line.split("=", 1)[1] for line in (KIT / "kit.conf").read_text().splitlines()
                  if line.startswith("accept="))
    lines = [line.split() for line in ACCEPT_MANIFEST.read_text().splitlines()]
    check(["install", "accept", wanted, "0755"] in lines, f"no install of accept at {wanted}")
    done = subprocess.run(["sh", "-n", str(ACCEPT)], capture_output=True, text=True, check=False)
    check(done.returncode == 0, f"accept does not parse: {done.stderr}")


def accept_refuses(settings: str, level: str = "smoke") -> subprocess.CompletedProcess:
    """The acceptance program with `settings` as accept.conf and an empty kit.conf."""
    with tempfile.TemporaryDirectory(prefix="server-acceptance-") as scratch:
        kit_conf, conf = Path(scratch) / "kit.conf", Path(scratch) / "accept.conf"
        kit_conf.write_text("# none\n")
        conf.write_text(settings)
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "SO_KIT_CONF": str(kit_conf),
               "SO_ACCEPT_CONF": str(conf)}
        return subprocess.run(["sh", str(ACCEPT), level], capture_output=True, text=True,
                              env=env, check=False)


def accept_refuses_bad_settings() -> None:
    """An unknown key, a bad port, an identity of another family and an unknown level are
    refused with exit 2 before any case runs."""
    for settings, words in (("accept_colour=red\n", "unknown key accept_colour"),
                            ("accept_ssh_port=0\n", "accept_ssh_port is no port"),
                            ("accept_identity=agent/x\n", "no person/<login>"),
                            ("accept_work=tmp\n", "accept_work is no plain absolute path")):
        done = accept_refuses(settings)
        check(done.returncode == 2 and words in done.stderr, f"{settings!r}: {done}")
        check(not done.stdout, f"{settings!r}: a case ran: {done.stdout}")
    done = accept_refuses("", "deep")
    check(done.returncode == 2 and "no acceptance level deep" in done.stderr, f"deep: {done}")


SELFTESTS: dict[str, Callable[[], None]] = {
    "accept_cases_are_one_implementation": accept_cases_are_one_implementation,
    "accept_is_installed_where_so_kit_runs_it": accept_is_installed_where_so_kit_runs_it,
    "accept_refuses_bad_settings": accept_refuses_bad_settings,
}


def selftest() -> int:
    failed = 0
    for label, test in SELFTESTS.items():
        try:
            test()
        except (Failure, OSError) as error:
            print(f"FAILED  {label}: {error}")
            failed += 1
            continue
        print(f"ok      {label}")
    print(f"server-acceptance selftest: {failed} failed")
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    getting = commands.add_parser("fetch", help="download what an offline run needs")
    getting.add_argument("--cache", type=Path, default=default_cache())
    running = commands.add_parser("run", help="install and accept, offline")
    running.add_argument("--bin-dir", type=Path, default=default_bin_dir())
    running.add_argument("--cache", type=Path, default=default_cache())
    running.add_argument("--method", action="append", choices=[*METHODS, IMAGE_METHOD])
    commands.add_parser("selftest", help="the offline checks of this script and accept/")
    args = parser.parse_args()
    try:
        if args.command == "fetch":
            return fetch(args.cache)
        if args.command == "run":
            return run_methods(args.bin_dir, args.cache, args.method or [*METHODS, IMAGE_METHOD])
        return selftest()
    except (Failure, PACKAGES.Failure, OSError, subprocess.TimeoutExpired) as error:
        print(f"server-acceptance: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
