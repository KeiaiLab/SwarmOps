# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Builds the server's container image from the release's packages, and tests the image.

    python3 -I scripts/server-image.py build <version> <packages dir> <out archive> [--arch A]...
                                             [--fresh]
    python3 -I scripts/server-image.py test <version> <packages dir> [--archive A] [--only NAME]

`build` makes one image index of `swarmops-server:<version>` for each architecture named (default
amd64 and arm64; one that is not this host's needs QEMU registered with binfmt_misc) and writes
it as an OCI archive. The build context holds packaging/container/Dockerfile and the
`swarmops_<version>_<arch>.deb` and `swarmops-server_<version>_<arch>.deb` of the packages
directory, nothing else; the Dockerfile copies no file in. Its dates are the packages' own, so
the same packages give the same archive, byte for byte. The fetch stage downloads Debian's
packages the two need: a cold build needs the network. The install stage always runs again;
`--fresh` runs the fetch stage again too, so no layer of an earlier build is reused.

`test` checks an archive (default: one built for this host into a scratch directory) against
the packages it came from, in containers without a network, the root filesystem read-only, the
contract's volumes mounted (stores and the account's home: volumes; settings: read-only; the run
directory and /tmp: empty tmpfs):

    test                                 fails when
    image_is_built_from_the_packages_only  the Dockerfile copies a file, downloads during the
                                           install, or uses a base not pinned by digest (a
                                           planted one of each must be found); an installed
                                           file of the two packages differs from the package's
                                           payload; a SwarmOps file comes from neither package
    image_runs_unprivileged_serve        the image's user is not 10022, or git cannot clone,
                                         push and land over ssh, and clone over HTTPS
    entry_roles_exist                    `so-kit run <role>` does not start or answer a role
    no_root_process_and_no_setuid_file   a process runs as root, or a file is setuid, setgid
                                         or carries capabilities
    root_filesystem_is_never_written     serving writes to the root filesystem (docker diff)
    changed_certificate_is_served        a replaced certificate is not served within a
                                         minute, or the web server restarted for it
    two_builds_same_digest               a second build from the same packages differs

Exit 1 on any failure. Needs docker with BuildKit and the containerd image store.
"""

import argparse
import enum
import hashlib
import io
import json
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCKERFILE = ROOT / "packaging" / "container" / "Dockerfile"
IMAGE = "swarmops-server"
PACKAGES = ("swarmops", "swarmops-server")
# dpkg's architecture, which is BuildKit's TARGETARCH too, and the binfmt_misc entry that runs
# its binaries on another host.
ARCHES = {"amd64": "qemu-x86_64", "arm64": "qemu-aarch64"}
HOST_ARCH = {"x86_64": "amd64", "aarch64": "arm64"}.get(platform.machine(), "")
BINFMT = Path("/proc/sys/fs/binfmt_misc")
# The Dockerfile's stage that installs; a rebuild of the same packages runs it again.
INSTALL_STAGE = "image"

# The image's contract (the chart relies on it).
SERVING_ID = 10022
SSH_PORT = 2222
HTTPS_PORT = 8443
ETC = "/etc/swarmops-server"
RUN_DIR = "/run/swarmops-server"
WEB_ROOT = f"{RUN_DIR}/www"
DATA = ("/srv/so", "/var/lib/swarmops-server")
ROLES_SERVING = ("sshd", "web", "cgi", "timers")
ACCEPT = "/usr/lib/swarmops-server/accept/accept"

BUILD_WAIT_S = 1800
COMMAND_WAIT_S = 300
READY_WAIT_S = 60
# The contract: a changed certificate is served within one minute.
CERT_WAIT_S = 60
STORE = "demo"
WHO = "person/ada"
WEB_LOGIN = "ada"
WEB_PASSWORD = "c4-test-only"


class Failure(Exception):
    """A check that did not hold, with what was seen."""


@dataclass
class Ran:
    """One command's end: its exit status and its output, as text."""

    code: int
    out: str
    err: str


def check(holds: bool, message: str) -> None:
    """Fails with `message` unless `holds`."""
    if not holds:
        raise Failure(message)


def docker(argv: list[str], stdin: str = "", timeout: int = COMMAND_WAIT_S) -> Ran:
    """`docker argv`, never raising on its exit status."""
    done = subprocess.run(
        ["docker", *argv],
        input=stdin,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    return Ran(done.returncode, done.stdout, done.stderr)


# ---------------------------------------------------------------------------------------------
# The packages


def ar_members(path: Path) -> dict[str, tuple[int, bytes]]:
    """A .deb's ar members: name -> (date, bytes)."""
    data = path.read_bytes()
    check(data.startswith(b"!<arch>\n"), f"{path.name} is not an ar archive")
    members = {}
    at = 8
    while at < len(data):
        header = data[at : at + 60]
        name = header[:16].decode().strip().rstrip("/")
        date = int(header[16:28].decode().strip())
        size = int(header[48:58].decode().strip())
        members[name] = (date, data[at + 60 : at + 60 + size])
        at += 60 + size + (size & 1)
    return members


def deb_tar(path: Path, prefix: str) -> tarfile.TarFile:
    """The .deb's `control.tar.*` or `data.tar.*`."""
    for name, (_, data) in ar_members(path).items():
        if name.startswith(prefix):
            return tarfile.open(fileobj=io.BytesIO(data))
    raise Failure(f"{path.name} has no {prefix}*")


@dataclass
class Deb:
    """What a package installs: regular files and their SHA-256, its configuration files, and
    the files its post-install script makes (its tmpfiles.d `f` lines)."""

    files: dict[str, str] = field(default_factory=dict)
    conffiles: set[str] = field(default_factory=set)
    made: set[str] = field(default_factory=set)


def read_deb(path: Path) -> Deb:
    """The package's payload, read on this host."""
    deb = Deb()
    with deb_tar(path, "control.tar") as control:
        for member in control.getmembers():
            if member.name.lstrip("./") == "conffiles":
                text = control.extractfile(member).read().decode()
                deb.conffiles = {line.strip() for line in text.splitlines() if line.strip()}
    with deb_tar(path, "data.tar") as data:
        for member in data.getmembers():
            if not member.isfile():
                continue
            name = "/" + member.name.lstrip("./")
            body = data.extractfile(member).read()
            deb.files[name] = hashlib.sha256(body).hexdigest()
            if "/tmpfiles.d/" not in name:
                continue
            for line in body.decode().splitlines():
                words = line.split()
                if words and words[0] == "f":
                    deb.made.add(words[1])
    return deb


def package_paths(directory: Path, version: str, arch: str) -> list[Path]:
    """The two release packages of `arch`; a missing one fails."""
    paths = [directory / f"{name}_{version}_{arch}.deb" for name in PACKAGES]
    for path in paths:
        check(path.is_file(), f"no {path.name} in {directory}")
    return paths


def source_date(server_deb: Path) -> int:
    """The packages' date (the tagged commit's): the image's SOURCE_DATE_EPOCH."""
    return ar_members(server_deb)["debian-binary"][0]


# ---------------------------------------------------------------------------------------------
# The image definition and its build


def dockerfile_problems(text: str) -> list[str]:
    """What breaks "built from the packages only": a file copied in, a base not pinned by
    digest, an install that may download."""
    problems = []
    joined = re.sub(r"\\\n", " ", text)
    for line in joined.splitlines():
        words = line.split()
        if not words or words[0].startswith("#"):
            continue
        instruction = words[0].upper()
        if instruction in ("COPY", "ADD"):
            problems.append(f"{instruction} copies a file in: {line.strip()}")
        if instruction == "FROM" and "@sha256:" not in line:
            problems.append(f"a base not pinned by digest: {line.strip()}")
        if instruction == "RUN" and "/packages/" in line and "--download-only" not in line:
            if "--network=none" not in line:
                problems.append(f"the install may download: {line.strip()[:80]}")
    if not any(line.split()[:1] == ["FROM"] for line in joined.splitlines()):
        problems.append("no FROM")
    return problems


def context_dir(scratch: Path, packages: Path, version: str, arches: list[str]) -> Path:
    """The build context: the Dockerfile and the release packages of `arches`, nothing else."""
    check(DOCKERFILE.is_file(), f"no image definition: {DOCKERFILE.relative_to(ROOT)}")
    problems = dockerfile_problems(DOCKERFILE.read_text())
    check(not problems, "; ".join(problems))
    context = scratch / "context"
    context.mkdir()
    shutil.copyfile(DOCKERFILE, context / "Dockerfile")
    for arch in arches:
        for path in package_paths(packages, version, arch):
            shutil.copyfile(path, context / path.name)
    return context


class Cache(enum.Enum):
    """What a build may take from BuildKit's cache: the fetched Debian packages, or nothing."""

    FETCHED = "fetched"
    NONE = "none"


def build_archive(
    packages: Path, version: str, arches: list[str], out: Path, cache: Cache = Cache.FETCHED
) -> str:
    """Builds the image index of `arches` into the OCI archive `out`; its digest."""
    for arch in arches:
        check(arch in ARCHES, f"no architecture {arch}; one of {', '.join(ARCHES)}")
        if arch != HOST_ARCH:
            check(
                (BINFMT / ARCHES[arch]).is_file(),
                f"{arch} needs QEMU registered with binfmt_misc ({BINFMT / ARCHES[arch]})",
            )
    epoch = source_date(package_paths(packages, version, arches[0])[1])

    with tempfile.TemporaryDirectory(prefix="server-image-") as scratch:
        context = context_dir(Path(scratch), packages, version, arches)
        argv = ["buildx", "build", "--builder", "default", "--progress", "plain"]
        argv += ["--platform", ",".join(f"linux/{arch}" for arch in arches)]
        argv += ["--provenance=false", "--sbom=false"]
        argv += ["--no-cache"] if cache == Cache.NONE else ["--no-cache-filter", INSTALL_STAGE]
        argv += ["--build-arg", f"SOURCE_DATE_EPOCH={epoch}"]
        argv += ["--output", f"type=oci,dest={out},name={IMAGE}:{version},rewrite-timestamp=true"]
        built = docker([*argv, str(context)], timeout=BUILD_WAIT_S)
    check(built.code == 0, f"the build failed:\n{built.err[-4000:]}")
    return archive_digest(out)


def archive_digest(archive: Path) -> str:
    """The digest of the archive's one image (an index, or one platform's manifest)."""
    with tarfile.open(archive) as tar:
        index = json.load(tar.extractfile("index.json"))
    manifests = index.get("manifests", [])
    check(len(manifests) == 1, f"{archive.name} holds {len(manifests)} images, not one")
    return manifests[0]["digest"]


def archive_arches(archive: Path) -> list[str]:
    """The architectures the archive's image holds."""
    with tarfile.open(archive) as tar:
        top = json.load(tar.extractfile("index.json"))["manifests"][0]
        if "platform" in top:
            return [top["platform"]["architecture"]]
        name = "blobs/sha256/" + top["digest"].split(":", 1)[1]
        nested = json.load(tar.extractfile(name))
    return sorted(m["platform"]["architecture"] for m in nested["manifests"] if "platform" in m)


def load(archive: Path) -> str:
    """Loads the archive into docker; the image's reference, its digest, which a tag another
    run loads cannot move."""
    loaded = docker(["load", "--input", str(archive)], timeout=BUILD_WAIT_S)
    check(loaded.code == 0, f"docker load: {loaded.err}")
    return archive_digest(archive)


# ---------------------------------------------------------------------------------------------
# Containers of the image


@dataclass
class Site:
    """The loaded image, its settings volume and the containers and volumes a run made."""

    image: str
    run_id: str = field(default_factory=lambda: uuid.uuid4().hex[:12])
    containers: list[str] = field(default_factory=list)
    volumes: list[str] = field(default_factory=list)

    def name(self, what: str) -> str:
        """A name of this run."""
        return f"so-image-{self.run_id}-{what}"

    def volume(self, what: str) -> str:
        """A new volume of this run."""
        name = self.name(what)
        made = docker(["volume", "create", name])
        check(made.code == 0, f"volume {name}: {made.err}")
        self.volumes.append(name)
        return name

    def as_root(self, script: str, mounts: list[str]) -> Ran:
        """`script` as root in a throwaway container of the image: test fixtures only."""
        argv = ["run", "--rm", "--network", "none", "--user", "0:0", "--entrypoint", "sh"]
        for mount in mounts:
            argv += ["--volume", mount]
        return docker([*argv, self.image, "-c", script])

    def settings(self) -> None:
        """The settings volume: kit.conf, host key, key line, certificate and web login."""
        self.etc = self.volume("etc")
        script = f"""set -eu
cd {ETC}
mkdir -p host-keys access tls client
printf '%s\\n' root=/srv/so host_keys={ETC}/host-keys web_root={WEB_ROOT} \\
    authorized_keys={ETC}/access/authorized_keys 'store={STORE} {WHO}' >kit.conf
ssh-keygen -q -t ed25519 -N '' -C '' -f host-keys/ssh_host_ed25519_key
ssh-keygen -q -t ed25519 -N '' -C '' -f client/id
SO_KIT_CONF={ETC}/kit.conf so-kit key-line {WHO} client/id.pub >access/authorized_keys
printf '%s:%s\\n' {WEB_LOGIN} "$(openssl passwd -apr1 {WEB_PASSWORD})" >web.htpasswd
chown -R {SERVING_ID}:{SERVING_ID} host-keys client tls web.htpasswd
chmod 0400 host-keys/ssh_host_ed25519_key
chmod 0600 client/id
"""
        made = self.as_root(script, [f"{self.etc}:{ETC}"])
        check(made.code == 0, f"settings: {made.err}")
        self.certificate("localhost")

    def certificate(self, subject: str) -> str:
        """A new self-signed certificate in the settings, swapped in by rename; its SHA-256
        fingerprint."""
        script = f"""set -eu
cd {ETC}/tls
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 \\
    -subj /CN={subject} -addext subjectAltName=DNS:localhost \\
    -keyout .privkey.pem -out .fullchain.pem 2>/dev/null
chown {SERVING_ID}:{SERVING_ID} .privkey.pem .fullchain.pem
chmod 0400 .privkey.pem
mv .privkey.pem privkey.pem
mv .fullchain.pem fullchain.pem
openssl x509 -in fullchain.pem -noout -fingerprint -sha256
"""
        made = self.as_root(script, [f"{self.etc}:{ETC}"])
        check(made.code == 0, f"certificate: {made.err}")
        return made.out.strip()

    def start(self, what: str, argv: list[str], read_only: bool = True) -> str:
        """A container of the image with the contract's mounts, running `argv`."""
        name = self.name(what)
        line = ["run", "--detach", "--network", "none", "--name", name]
        if read_only:
            line.append("--read-only")
        line += ["--tmpfs", f"{RUN_DIR}:uid={SERVING_ID},gid={SERVING_ID},mode=0700"]
        line += ["--tmpfs", f"{WEB_ROOT}:uid={SERVING_ID},gid={SERVING_ID},mode=0755"]
        line += ["--tmpfs", "/tmp:mode=1777"]
        for path in DATA:
            line += ["--volume", f"{self.volume(what + path.replace('/', '-'))}:{path}"]
        line += ["--volume", f"{self.etc}:{ETC}:ro"]
        started = docker([*line, self.image, *argv])
        check(started.code == 0, f"start {what}: {started.err}")
        self.containers.append(name)
        return name

    def ready(self, name: str, role: str) -> None:
        """Waits until `so-kit ready <role>` answers 0 in `name`."""
        deadline = time.monotonic() + READY_WAIT_S
        while self.exec(name, ["so-kit", "ready", role]).code != 0:
            if time.monotonic() > deadline:
                logs = docker(["logs", name])
                raise Failure(f"{role} did not get ready: {logs.out}{logs.err}")
            time.sleep(0.5)

    def exec(self, name: str, argv: list[str], env: dict[str, str] | None = None) -> Ran:
        """`argv` in container `name` as the image's user."""
        line = ["exec"]
        for key, value in {"GIT_TERMINAL_PROMPT": "0", **(env or {})}.items():
            line += ["--env", f"{key}={value}"]
        return docker([*line, name, *argv])

    def clean(self) -> None:
        """Removes what the run made."""
        for name in self.containers:
            docker(["rm", "--force", name])
        for name in self.volumes:
            docker(["volume", "rm", "--force", name])


# What a git client in the serving container runs: clone over ssh, push main, land a second
# commit through refs/land/main, then clone over HTTPS and print its history.
CLIENT = f"""set -eu
mkdir -p /tmp/client
cd /tmp/client
printf '[localhost]:{SSH_PORT} %s\\n' "$(cut -d' ' -f1,2 {ETC}/host-keys/ssh_host_ed25519_key.pub)" \\
    >known_hosts
git -c init.defaultBranch=main clone -q so@localhost:{STORE} w
cd w
git config user.name Ada
git config user.email ada@example.org
echo one >a.txt
git add a.txt
git commit -qm one
git push -q origin HEAD:main
echo two >b.txt
git add b.txt
git commit -qm two
git push -q origin HEAD:refs/land/main
cd ..
git clone -q https://{WEB_LOGIN}:{WEB_PASSWORD}@localhost:{HTTPS_PORT}/{STORE} h
git -C h log --format=%s
"""
CLIENT_ENV = {
    "HOME": "/tmp/client",
    "GIT_SSH_COMMAND": (
        f"ssh -i {ETC}/client/id -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes"
        f" -o UserKnownHostsFile=/tmp/client/known_hosts -p {SSH_PORT}"
    ),
    "GIT_SSL_CAINFO": f"{ETC}/tls/fullchain.pem",
}
# The store `so-kit run setup` made grants reading and landing; the first push to main needs a
# grant to push, which the store's owner writes.
GRANT = f"""version = 1

[[grant]]
who = ["{WHO}"]
read = true
push = ["main"]
land = ["main"]
"""


def serve_traffic(site: Site, name: str) -> None:
    """git through both entrances of container `name`: the land is on main over HTTPS."""
    wrote = docker(
        ["exec", "--interactive", name, "sh", "-c", f"cat >/srv/so/{STORE}/access.toml"],
        stdin=GRANT,
    )
    check(wrote.code == 0, f"grant: {wrote.err}")
    ran = site.exec(name, ["sh", "-c", CLIENT], env=CLIENT_ENV)
    check(ran.code == 0, f"git over ssh and HTTPS: {ran.err}{ran.out}")
    check(ran.out.split() == ["two", "one"], f"HTTPS clone's history: {ran.out!r}")


def served_fingerprint(site: Site, name: str) -> str:
    """The SHA-256 fingerprint of the certificate the web role serves."""
    script = (
        f"openssl s_client -connect 127.0.0.1:{HTTPS_PORT} </dev/null 2>/dev/null"
        " | openssl x509 -noout -fingerprint -sha256"
    )
    return site.exec(name, ["sh", "-c", script]).out.strip()


def processes(site: Site, name: str) -> list[str]:
    """`<pid> <real> <effective> <saved> <fs uid> <command>` of every process in `name`."""
    script = (
        'for p in /proc/[0-9]*; do [ -r "$p/status" ] || continue; '
        'printf "%s %s %s\\n" "${p#/proc/}" "$(sed -n "s/^Uid:\\t//p" "$p/status" | tr "\\t" " ")"'
        ' "$(tr "\\0" " " <"$p/cmdline")"; done'
    )
    return [line for line in site.exec(name, ["sh", "-c", script]).out.splitlines() if line]


# ---------------------------------------------------------------------------------------------
# The cases


@dataclass
class Run:
    """One test run: the packages, the archive under test and the loaded image."""

    version: str
    packages: Path
    archive: Path
    site: Site
    serving: str = ""


def image_is_built_from_the_packages_only(run: Run) -> None:
    """The definition copies nothing in; the installed files are the packages' payload."""
    # 1. The definition, and a planted break of each kind it must find.
    check(not dockerfile_problems(DOCKERFILE.read_text()), "the Dockerfile breaks its rule")
    text = DOCKERFILE.read_text()
    planted = {
        "copy": text + "\nCOPY kit.conf /etc/swarmops-server/kit.conf\n",
        "url": text + "\nADD https://example.org/x /x\n",
        "tag": text.replace("debian:12@sha256:", "debian:12#", 1),
        "network": text.replace("RUN --network=none", "RUN", 1),
    }
    for kind, body in planted.items():
        check(dockerfile_problems(body), f"a planted {kind} break was not found")

    # 2. Every payload file of both packages, by digest, as so-kit reports the installed ones.
    expected = {}
    made: set[str] = set()
    for path in package_paths(run.packages, run.version, HOST_ARCH):
        deb = read_deb(path)
        made |= deb.made
        expected |= {p: d for p, d in deb.files.items() if p not in deb.conffiles}
    report = docker(["run", "--rm", "--network", "none", run.site.image, "digests"])
    check(report.code == 0, f"so-kit digests: {report.err}")
    installed = {}
    for line in report.out.splitlines():
        digest, path = line.split("  ", 1)
        installed[path] = digest
    check(installed == expected, f"installed files differ from the payload: {diff(expected, installed)}")

    # 3. No SwarmOps file but the packages' and their post-install script's.
    scan = (
        "find / -xdev -type f \\( -path '*swarmops*' -o -name so-kit -o -path /usr/bin/so \\)"
        " ! -path '/var/lib/dpkg/*' | sort | while read -r f; do"
        ' dpkg -S "$f" >/dev/null 2>&1 || echo "$f"; done'
    )
    found = docker(["run", "--rm", "--network", "none", "--entrypoint", "sh", run.site.image, "-c", scan])
    check(found.code == 0, f"scan: {found.err}")
    foreign = sorted(set(found.out.split()) - made)
    check(not foreign, f"files of no package: {foreign}")


def diff(expected: dict[str, str], seen: dict[str, str]) -> str:
    """The paths two digest maps disagree on."""
    paths = sorted(p for p in expected.keys() | seen.keys() if expected.get(p) != seen.get(p))
    return ", ".join(paths[:10])


def image_runs_unprivileged_serve(run: Run) -> None:
    """The serving account's ids are the contract's; git serves through both entrances."""
    user = docker(["image", "inspect", "--format", "{{.Config.User}}", run.site.image])
    check(user.out.strip() == f"{SERVING_ID}:{SERVING_ID}", f"the image's user: {user.out!r}")
    account = docker(["run", "--rm", "--network", "none", "--entrypoint", "id", run.site.image])
    check(
        account.out.startswith(f"uid={SERVING_ID}(so) gid={SERVING_ID}(so)"),
        f"the image runs as {account.out!r}",
    )

    serving(run)


def entry_roles_exist(run: Run) -> None:
    """Each role starts alone and answers; one-shot roles are known; an unknown one is not."""
    # 1. setup makes the settings' store on an empty volume, then exits.
    setup = run.site.start("setup", ["run", "setup"])
    waited = docker(["wait", setup])
    logs = docker(["logs", setup])
    check(waited.out.strip() == "0", f"run setup: {waited.out} {logs.err}")

    # 2. Each serving role in a container of its own, as the chart starts them.
    for role in ROLES_SERVING:
        name = run.site.start(role, ["run", role])
        run.site.ready(name, role)
        docker(["rm", "--force", name])

    # 3. The one-shot roles reach their programs; acceptance's comes with its package files.
    rehearse = docker(["run", "--rm", "--network", "none", run.site.image, "run", "rehearse", "/x", "/y"])
    check("rehearse: usage" in rehearse.err, f"run rehearse: {rehearse.err!r}")
    accept = docker(["run", "--rm", "--network", "none", run.site.image, "run", "accept", "smoke"])
    has_accept = docker(
        ["run", "--rm", "--network", "none", "--entrypoint", "test", run.site.image, "-x", ACCEPT]
    )
    if has_accept.code == 0:
        check("no acceptance program" not in accept.err, f"run accept: {accept.err!r}")
    else:
        check(accept.err.strip() == "so-kit: no acceptance program", f"run accept: {accept.err!r}")
    unknown = docker(["run", "--rm", "--network", "none", run.site.image, "run", "nosuch"])
    check(unknown.code == 2 and "unknown role" in unknown.err, f"run nosuch: {unknown.err!r}")


def serving(run: Run) -> str:
    """The container serving `run all` with traffic served, started once."""
    if run.serving:
        return run.serving
    run.serving = run.site.start("serving", ["run", "all"])
    run.site.ready(run.serving, "all")
    serve_traffic(run.site, run.serving)
    return run.serving


def no_root_process_and_no_setuid_file(run: Run) -> None:
    """No process of the serving container runs as root; no file grants more."""
    serving(run)
    seen = processes(run.site, run.serving)
    check(len(seen) >= len(ROLES_SERVING), f"processes: {seen}")
    rooted = [line for line in seen if "0" in line.split()[1:5]]
    check(not rooted, f"root processes: {rooted}")

    script = "find / -xdev -type f -perm /6000; getcap -r /usr /bin /sbin /lib 2>/dev/null"
    found = docker(["run", "--rm", "--network", "none", "--entrypoint", "sh", run.site.image, "-c", script])
    check(found.out.strip() == "", f"setuid, setgid or capable files: {found.out}")


def root_filesystem_is_never_written(run: Run) -> None:
    """Serving with a writable root changes nothing on it but the mount points."""
    name = run.site.start("writable", ["run", "all"], read_only=False)
    run.site.ready(name, "all")
    serve_traffic(run.site, name)
    changed = docker(["diff", name]).out.split("\n")
    mounts = [RUN_DIR, WEB_ROOT, "/tmp", *DATA, ETC]
    written = []
    for line in changed:
        if not line.strip():
            continue
        path = line.split(" ", 1)[1]
        if any(m == path or m.startswith(path.rstrip("/") + "/") for m in mounts):
            continue
        written.append(line)
    check(not written, f"the root filesystem was written: {written}")


def changed_certificate_is_served(run: Run) -> None:
    """A replaced certificate is served within a minute by the same web server."""
    serving(run)
    before = [line for line in processes(run.site, run.serving) if "nginx: master" in line]
    check(len(before) == 1, f"nginx master: {before}")

    renewed = run.site.certificate("renewed.localhost")
    deadline = time.monotonic() + CERT_WAIT_S
    while served_fingerprint(run.site, run.serving) != renewed:
        check(time.monotonic() < deadline, f"not served within {CERT_WAIT_S} s: {renewed}")
        time.sleep(2)
    after = [line for line in processes(run.site, run.serving) if "nginx: master" in line]
    check(after == before, f"the web server restarted: {before} -> {after}")


def two_builds_same_digest(run: Run) -> None:
    """The same packages build the same archive, byte for byte."""
    arches = archive_arches(run.archive)
    with tempfile.TemporaryDirectory(prefix="server-image-again-") as scratch:
        again = Path(scratch) / "again.tar"
        digest = build_archive(run.packages, run.version, arches, again)
        first = archive_digest(run.archive)
        check(digest == first, f"digests differ: {first} then {digest}")
        sums = [hashlib.sha256(p.read_bytes()).hexdigest() for p in (run.archive, again)]
        check(sums[0] == sums[1], f"archives differ: {sums}")


CASES: dict[str, Callable[[Run], None]] = {
    "image_is_built_from_the_packages_only": image_is_built_from_the_packages_only,
    "image_runs_unprivileged_serve": image_runs_unprivileged_serve,
    "entry_roles_exist": entry_roles_exist,
    "no_root_process_and_no_setuid_file": no_root_process_and_no_setuid_file,
    "root_filesystem_is_never_written": root_filesystem_is_never_written,
    "changed_certificate_is_served": changed_certificate_is_served,
    "two_builds_same_digest": two_builds_same_digest,
}


def test(version: str, packages: Path, archive: Path | None, only: str) -> int:
    """Runs the cases against `archive`, or one built for this host."""
    check(HOST_ARCH != "", f"no image architecture for {platform.machine()}")
    with tempfile.TemporaryDirectory(prefix="server-image-test-") as scratch:
        if archive is None:
            archive = Path(scratch) / "image.tar"
            build_archive(packages, version, [HOST_ARCH], archive)
        check(HOST_ARCH in archive_arches(archive), f"{archive.name} holds no {HOST_ARCH} image")
        site = Site(load(archive))
        run = Run(version, packages, archive, site)
        passed = 0
        try:
            site.settings()
            for name, case in CASES.items():
                if only and name != only:
                    continue
                try:
                    case(run)
                    passed += 1
                    print(f"ok    {name}")
                except (Failure, subprocess.TimeoutExpired) as error:
                    print(f"FAIL  {name}: {error}")
        finally:
            site.clean()
    count = 1 if only else len(CASES)
    print(f"server-image: {passed} of {count} cases passed")
    return 0 if passed == count else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    steps = parser.add_subparsers(dest="step", required=True)
    build = steps.add_parser("build", help="the image index as an OCI archive")
    build.add_argument("version")
    build.add_argument("packages", type=Path)
    build.add_argument("out", type=Path)
    build.add_argument("--arch", action="append", choices=sorted(ARCHES))
    build.add_argument("--fresh", action="store_true", help="reuse no cached layer")
    tested = steps.add_parser("test", help="the cases, against an archive")
    tested.add_argument("version")
    tested.add_argument("packages", type=Path)
    tested.add_argument("--archive", type=Path)
    tested.add_argument("--only", choices=sorted(CASES), default="")
    args = parser.parse_args()

    try:
        if args.step == "build":
            arches = args.arch or sorted(ARCHES)
            cache = Cache.NONE if args.fresh else Cache.FETCHED
            digest = build_archive(args.packages, args.version, arches, args.out, cache)
            print(f"server-image: {args.out.name} {digest}")
            return 0
        return test(args.version, args.packages, args.archive, args.only)
    except (Failure, subprocess.TimeoutExpired) as error:
        print(f"server-image: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
