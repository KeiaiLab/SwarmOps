# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""The one real-entrance harness: stock git and `so` through OpenSSH's sshd and nginx with fcgiwrap.

    python3 -I scripts/server-entrance.py build             # the image, once, with the network
    python3 -I scripts/server-entrance.py run [--bin-dir D] [--only NAME]

`build` makes the test image from packaging/entrance/ (its base pinned by digest), tagged by the
digest of that directory, and does nothing when the tag is already local. `run` never builds: it
starts the image with `--network none`, mounts the `swarmops` and `so` of D (default: the debug
build under CARGO_TARGET_DIR, or target/) at /opt/swarmops/bin, and runs, inside it, as the
client account against the real entrances:

    test                              what passes through sshd and nginx + fcgiwrap
    git_session_over_real_sshd        G16's session: clone (v2, v0, --depth 1), fetch, pull
    git_session_over_nginx_fcgiwrap     --rebase and --no-rebase, push (forward, branch, tag,
                                        deletion), a stale push, a reader's push, strangers
                                        refused by the entrance; ids and `git fsck --strict`
    land_push_over_both               refs/land/main lands, replays and reports a conflict
    so_clone_fetch_push_over_serve    `so clone`, `fetch`, `push`, `sync` over both entrances

Two strict expected failures, each failing the run the day it passes, or fails another way:
`so_sync_lost_race_retries` (a `so sync` that loses its race inside serve is retried; flips
on the interop design's D1 fix) and `so_push_names_its_identity` (a pushed changeset names the
key's identity as its agent; flips on M29-18). Last, the self-check `auth_off_fails_the_run`
plants authentication off at both entrances and needs the strangers' checks to fail.
Exit 1 on any failure.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ENTRANCE = ROOT / "packaging" / "entrance"
IMAGE = "swarmops-entrance"
BINARIES = ("swarmops", "so")
GIT_VERSION = "git version 2.43.0"
STORES = "/srv/so"
SERVING = "so"
CLIENT = "dev"
HOME = f"/home/{CLIENT}"
READY = "/run/entrance-ready"
READY_WAIT_S = 60
COMMAND_WAIT_S = 600
ENTRANCES = ("ssh", "https")
PLANT_AUTH_OFF = "auth_off"
HTTPS_PROMPT = "could not read Password"
HTTPS_REFUSED = "Authentication failed"
SSH_REFUSED = "Permission denied (publickey)"
V2_SPOKEN = "< version 2"

# Ada writes: forward pushes to main, branches and tags of her own (forced too), lands. Bob reads.
ACCESS = """version = 1

[[grant]]
who = ["person/ada"]
push = ["main", "feature/**", "tags/**"]
force = ["feature/**", "tags/**"]
land = ["main"]

[[grant]]
who = ["person/bob"]
read = true
"""


class Failure(Exception):
    """A check that did not hold, with what was seen."""


@dataclass
class Ran:
    """One command's end: its exit status and its output, as text."""

    code: int
    out: str
    err: str

    def envelope(self) -> dict:
        """stdout as `so`'s JSON envelope."""
        try:
            return json.loads(self.out)
        except json.JSONDecodeError as error:
            raise Failure(f"no JSON envelope ({error}): {self.out!r} {self.err!r}") from error


def check(holds: bool, message: str) -> None:
    """Fails with `message` unless `holds`."""
    if not holds:
        raise Failure(message)


# ---------------------------------------------------------------------------------------------
# The image and its container


def image_tag() -> str:
    """The image's tag: the digest of packaging/entrance/'s names, modes and bytes."""
    digest = hashlib.sha256()
    for path in sorted(ENTRANCE.rglob("*")):
        if not path.is_file():
            continue
        digest.update(path.relative_to(ENTRANCE).as_posix().encode() + b"\0")
        digest.update(b"x" if os.access(path, os.X_OK) else b"-")
        digest.update(path.read_bytes())
    return f"{IMAGE}:{digest.hexdigest()[:16]}"


def image_is_local(tag: str) -> bool:
    """Whether docker holds `tag`."""
    found = subprocess.run(["docker", "image", "inspect", tag], capture_output=True, check=False)
    return found.returncode == 0


def build() -> int:
    """Builds the image unless its tag is local; refuses a base not pinned by digest."""
    tag = image_tag()
    base = next(
        line for line in (ENTRANCE / "Dockerfile").read_text().splitlines() if line.startswith("FROM ")
    )
    if "@sha256:" not in base:
        print(f"server-entrance: the base is not pinned by digest: {base}", file=sys.stderr)
        return 1
    if image_is_local(tag):
        print(f"server-entrance: {tag} is local")
        return 0
    built = subprocess.run(["docker", "build", "--load", "--tag", tag, str(ENTRANCE)], check=False)
    if built.returncode != 0:
        return built.returncode

    # The tags of older contents go; one a running container still uses stays.
    listed = subprocess.run(
        ["docker", "image", "ls", "--format", "{{.Repository}}:{{.Tag}}", IMAGE],
        capture_output=True,
        text=True,
        check=False,
    )
    for old in listed.stdout.split():
        if old != tag:
            subprocess.run(["docker", "image", "rm", old], capture_output=True, check=False)
    return 0


@dataclass
class Container:
    """A running entrance container; every command runs in it with `docker exec`."""

    name: str
    plant: str = ""
    stores: list[str] = field(default_factory=list)

    @staticmethod
    def start(tag: str, bin_dir: Path, plant: str = "") -> "Container":
        """Starts `tag` without a network, the build under test mounted, and waits for ready."""
        name = f"swarmops-entrance-{uuid.uuid4().hex[:12]}"
        argv = ["docker", "run", "--detach", "--rm", "--init", "--network", "none"]
        argv += ["--name", name, "--env", f"ENTRANCE_PLANT={plant}"]
        argv += ["--volume", f"{bin_dir}:/opt/swarmops/bin:ro", tag]
        subprocess.run(argv, check=True, stdout=subprocess.DEVNULL)
        container = Container(name, plant)

        # Ready when start.sh has every entrance up; a start that died leaves its log.
        deadline = time.monotonic() + READY_WAIT_S
        while container.run("root", ["test", "-e", READY]).code != 0:
            if time.monotonic() > deadline:
                logs = subprocess.run(["docker", "logs", name], capture_output=True, text=True)
                container.stop()
                raise Failure(f"the entrances did not start: {logs.stdout}{logs.stderr}")
            time.sleep(0.2)
        return container

    def stop(self) -> None:
        """Removes the container."""
        subprocess.run(["docker", "rm", "--force", self.name], capture_output=True, check=False)

    def run(
        self,
        user: str,
        argv: list[str],
        env: dict[str, str] | None = None,
        stdin: str = "",
        cwd: str = "",
    ) -> Ran:
        """`argv` in the container as `user`, never prompting."""
        line = ["docker", "exec", "--interactive", "--user", user]
        line += ["--workdir", cwd or ("/tmp" if user != CLIENT else HOME)]
        for name, value in {"GIT_TERMINAL_PROMPT": "0", **(env or {})}.items():
            line += ["--env", f"{name}={value}"]
        done = subprocess.run(
            line + ["--", self.name, *argv],
            input=stdin,
            capture_output=True,
            text=True,
            timeout=COMMAND_WAIT_S,
            check=False,
        )
        return Ran(done.returncode, done.stdout, done.stderr)


# ---------------------------------------------------------------------------------------------
# The site: stores on the serving side, clones on the client's


@dataclass
class Site:
    """The container seen from the tests: stores to serve, and the client's git and `so`."""

    box: Container

    def url(self, entrance: str, who: str, store: str) -> str:
        """The store's address through `entrance`, as `who` (a key alias or a web login)."""
        if entrance == "ssh":
            return f"ssh://so-{who}/{store}.git"
        return f"https://{who}@so-host/{store}.git"

    def serving(self, argv: list[str], stdin: str = "") -> Ran:
        """`argv` as the serving account."""
        return self.box.run(SERVING, argv, stdin=stdin)

    def store(self, name: str) -> str:
        """Makes store `name` holding a two-commit git history as `main`, under ACCESS; the
        history's tip."""
        path = f"{STORES}/{name}"
        seed = f"/tmp/seed-{name}"
        script = (
            f"set -e; git init -q {seed}; cd {seed}; "
            "for n in one two; do echo $n >>a.txt; git add a.txt; "
            "git -c user.name=Seed -c user.email=seed@so-host.invalid commit -qm $n; done; "
            f"swarmops init {path} >/dev/null; "
            f"swarmops --as agent/setup --repo {path} git import {seed} --ref HEAD --into main "
            ">/dev/null; git rev-parse HEAD"
        )
        made = self.serving(["sh", "-c", script])
        check(made.code == 0, f"store {name}: {made.err}{made.out}")
        wrote = self.serving(["sh", "-c", f"cat >{path}/access.toml"], stdin=ACCESS)
        check(wrote.code == 0, f"access.toml of {name}: {wrote.err}")
        return made.out.strip()

    def store_show(self, store: str, rev: str) -> dict:
        """`swarmops show rev` of the served store; its result."""
        shown = self.serving(["swarmops", "--repo", f"{STORES}/{store}", "show", rev]).envelope()
        check(shown.get("ok") is True, f"show {rev}: {shown}")
        return shown["result"]

    def store_clean(self, store: str) -> None:
        """The served store's `verify --deep` finds nothing."""
        verified = self.serving(["swarmops", "--repo", f"{STORES}/{store}", "verify", "--deep"])
        check(verified.code == 0, f"verify --deep of {store}: {verified.out}{verified.err}")

    def git(self, args: list[str], cwd: str = HOME, env: dict[str, str] | None = None) -> Ran:
        """git as the client in `cwd`."""
        return self.box.run(CLIENT, ["git", *args], env=env, cwd=cwd)

    def ok(self, args: list[str], cwd: str = HOME) -> Ran:
        """git as the client, which must succeed."""
        done = self.git(args, cwd)
        check(done.code == 0, f"git {' '.join(args)}: exit {done.code}: {done.err}")
        return done

    def rev(self, cwd: str, rev: str) -> str:
        """The client's `rev-parse` of `rev`."""
        return self.ok(["rev-parse", "--verify", rev], cwd).out.strip()

    def commit(self, cwd: str, path: str, text: str) -> str:
        """A commit writing `text` to `path`; its id."""
        wrote = self.box.run(CLIENT, ["sh", "-c", f"printf %s '{text}' >{path}"], cwd=cwd)
        check(wrote.code == 0, wrote.err)
        self.ok(["add", path], cwd)
        self.ok(["commit", "--quiet", "-m", path], cwd)
        return self.rev(cwd, "HEAD")

    def remote_head(self, url: str, ref: str = "refs/heads/main") -> str:
        """What `git ls-remote` reads for `ref` at `url`, or ''."""
        listed = self.ok(["ls-remote", url, ref]).out.split()
        return listed[0] if listed else ""

    def so(self, args: list[str], env: dict[str, str] | None = None) -> Ran:
        """`so` as the client."""
        return self.box.run(CLIENT, ["so", *args], env=env)

    def so_ok(self, args: list[str], env: dict[str, str] | None = None) -> dict:
        """`so` as the client, which must succeed; its result."""
        done = self.so(args, env)
        envelope = done.envelope()
        check(done.code == 0 and envelope.get("ok") is True, f"so {' '.join(args)}: {done.out}{done.err}")
        return envelope["result"]

    def so_commit(self, store: str, agent: str, path: str, text: str) -> str:
        """A native changeset by `agent` in the client's `store`, moving its main; its id."""
        changes = json.dumps([{"path": path, "op": "put", "content": {"inline": text}}])
        argv = ["so", "--repo", store, "--as", agent, "commit", "-m", path]
        done = self.box.run(CLIENT, argv + ["--changes", "-", "--advance", "main"], stdin=changes)
        check(done.code == 0, f"so commit: {done.out}{done.err}")
        return done.envelope()["result"]["changeset"]


# ---------------------------------------------------------------------------------------------
# The tests


def strangers_refused(site: Site, entrance: str, store: str) -> None:
    """The entrance itself refuses a caller it does not know, before serve: over ssh a key on no
    line, over HTTPS no login (a 401 asks for one) and a wrong password."""
    if entrance == "ssh":
        done = site.git(["ls-remote", site.url("ssh", "eve", store)])
        check(done.code != 0 and SSH_REFUSED in done.err, f"sshd let eve's key in: {done.err}")
        return
    done = site.git(["ls-remote", site.url("https", "eve", store)])
    check(done.code != 0 and HTTPS_PROMPT in done.err, f"nginx asked no login: {done.err}")
    wrong = f"https://ada:wrong@so-host/{store}.git"
    done = site.git(["ls-remote", wrong])
    check(done.code != 0 and HTTPS_REFUSED in done.err, f"nginx took a wrong password: {done.err}")


def git_session(site: Site, entrance: str) -> None:
    """G16's session through one entrance (the roadmap's Topology 1, criteria 1, 2, 3 and 5)."""
    store = f"session-{entrance}"
    seed = site.store(store)
    url = site.url(entrance, "ada", store)

    # 1. Clones with protocol v2, v0 and --depth 1: each holds the seed's ids, fsck-clean.
    site.ok(["clone", "--quiet", url, "v2"], f"{HOME}")
    site.ok(["-c", "protocol.version=0", "clone", "--quiet", url, "v0"])
    site.ok(["config", "protocol.version", "0"], f"{HOME}/v0")
    site.ok(["clone", "--quiet", "--depth", "1", url, "shallow"])
    for clone in ("v2", "v0"):
        where = f"{HOME}/{clone}"
        check(site.rev(where, "origin/main") == seed, f"{clone}: main is not the seed's id")
        site.ok(["fsck", "--strict"], where)
    check(site.rev(f"{HOME}/shallow", "HEAD") == seed, "the shallow clone's head")
    traced = site.git(["ls-remote", "origin"], f"{HOME}/v2", {"GIT_TRACE_PACKET": "1"})
    check(V2_SPOKEN in traced.err, f"protocol v2 did not pass the entrance: {traced.err[-2000:]}")

    # 2. Pushes: a fast-forward of main, a branch, a lightweight tag, the branch's deletion.
    v2, v0 = f"{HOME}/v2", f"{HOME}/v0"
    forward = site.commit(v2, "b.txt", "b")
    site.ok(["push", "--quiet", "origin", "main"], v2)
    site.ok(["push", "--quiet", "origin", "HEAD:refs/heads/feature/x"], v2)
    site.ok(["tag", "v1.0"], v2)
    site.ok(["push", "--quiet", "origin", "v1.0"], v2)
    check(site.remote_head(url, "refs/tags/v1.0") == forward, "the tag is not served")
    site.ok(["push", "--quiet", "origin", ":feature/x"], v2)
    check(site.remote_head(url, "refs/heads/feature/x") == "", "the branch is still served")

    # 3. Over v0: pull --rebase onto it with a commit of its own, push; v2 pulls --no-rebase.
    site.commit(v0, "c.txt", "c")
    site.ok(["pull", "--quiet", "--rebase"], v0)
    site.ok(["push", "--quiet", "origin", "main"], v0)
    site.ok(["fetch", "--quiet"], v2)
    site.ok(["pull", "--quiet", "--no-rebase"], v2)
    check(site.rev(v2, "HEAD") == site.rev(v0, "HEAD"), "the two clones differ after the pulls")

    # 4. Pushes from a stale base, plain and forced, move nothing and fail on the client; the
    #    store stays clean.
    head = site.remote_head(url)
    site.ok(["reset", "--quiet", "--hard", "HEAD~1"], v0)
    site.commit(v0, "stale.txt", "stale")
    for refspec in ("main", "+main"):
        stale = site.git(["push", "origin", refspec], v0)
        check(stale.code != 0, f"a stale push of {refspec} passed: {stale.err}")
        check(site.remote_head(url) == head, f"a stale push of {refspec} moved main")
    site.store_clean(store)

    # 5. A reader's push is refused by the access file; strangers by the entrance.
    site.ok(["clone", "--quiet", site.url(entrance, "bob", store), "bob"])
    bob = f"{HOME}/bob"
    site.commit(bob, "bob.txt", "bob")
    refused = site.git(["push", "origin", "HEAD:refs/heads/feature/bob"], bob)
    check(refused.code != 0 and "E_POLICY_DENIED" in refused.err, f"bob pushed: {refused.err}")
    strangers_refused(site, entrance, store)
    site.box.run(CLIENT, ["rm", "-rf", "v2", "v0", "shallow", "bob"])


def git_session_over_real_sshd(site: Site) -> None:
    """G16's session through OpenSSH's sshd and a forced command."""
    git_session(site, "ssh")


def git_session_over_nginx_fcgiwrap(site: Site) -> None:
    """G16's session through nginx, its web login, and fcgiwrap running serve --cgi."""
    git_session(site, "https")


def land_push_over_both(site: Site) -> None:
    """A push to refs/land/main lands, is told and fetched; the same push replays; a conflict
    is refused naming its path and moves nothing; through each entrance."""
    for entrance in ENTRANCES:
        store = f"land-{entrance}"
        site.store(store)
        url = site.url(entrance, "ada", store)
        work, rival = f"{HOME}/land-{entrance}", f"{HOME}/rival-{entrance}"
        site.ok(["clone", "--quiet", url, work])
        site.ok(["clone", "--quiet", url, rival])

        # 1. The land, told on a remote: line, then fetched.
        site.commit(work, "landed.txt", "landed")
        pushed = site.ok(["push", "origin", "HEAD:refs/land/main"], work)
        landed = site.remote_head(url)
        told = f"remote: landed onto main as {landed}"
        check(told in pushed.err, f"{entrance}: the land was not told: {pushed.err}")
        site.ok(["fetch", "--quiet", "origin"], work)
        check(site.rev(work, "origin/main") == landed, f"{entrance}: fetch missed the land")

        # 2. The same push, its move lost: replayed.
        site.ok(["update-ref", "refs/remotes/origin/main", "HEAD~1"], work)
        again = site.ok(["push", "origin", "HEAD:refs/land/main"], work)
        check(f"remote: landed already onto main as {landed}" in again.err, again.err)
        check(site.remote_head(url) == landed, f"{entrance}: the replay moved main")

        # 3. A conflict on a.txt: refused, the path named, main unmoved.
        site.commit(work, "a.txt", "ours")
        site.ok(["push", "origin", "HEAD:refs/land/main"], work)
        head = site.remote_head(url)
        site.commit(rival, "a.txt", "theirs")
        conflict = site.git(["push", "origin", "HEAD:refs/land/main"], rival)
        check(conflict.code != 0, f"{entrance}: the conflict landed")
        check("E_CONFLICT" in conflict.err and "a.txt" in conflict.err, conflict.err)
        check(site.remote_head(url) == head, f"{entrance}: the conflict moved main")
        site.store_clean(store)


def so_clone_fetch_push_over_serve(site: Site) -> None:
    """`so clone`, `push`, `fetch` and `sync` against serve through each entrance."""
    for entrance in ENTRANCES:
        store = f"so-{entrance}"
        seed = site.store(store)
        url = site.url(entrance, "ada", store)
        local = f"local-{entrance}"

        # 1. The clone imports main as the seed's history.
        cloned = site.so_ok(["--as", f"person/ada-{entrance}", "clone", url, "--into", local])
        check(cloned["fetch"]["head"]["sha"] == seed, f"{entrance}: clone: {cloned}")

        # 2. A native change pushed: the remote serves it.
        site.so_commit(local, "agent/a1", "native.txt", "native")
        pushed = site.so_ok(["--repo", local, "push", "origin"])
        check(pushed["status"] == "pushed", f"{entrance}: push: {pushed}")
        check(site.remote_head(url) == pushed["new"], f"{entrance}: the push is not served")

        # 3. A git client lands on the remote; fetch imports it, sync fast-forwards main.
        work = f"{HOME}/git-{entrance}"
        site.ok(["clone", "--quiet", url, work])
        site.commit(work, "from-git.txt", "git")
        site.ok(["push", "--quiet", "origin", "HEAD:refs/land/main"], work)
        landed = site.remote_head(url)
        fetched = site.so_ok(["--repo", local, "fetch", "origin"])
        check(fetched["head"]["sha"] == landed, f"{entrance}: fetch: {fetched}")
        synced = site.so_ok(["--as", f"person/ada-{entrance}", "--repo", local, "sync", "origin"])
        check(synced["status"] == "fast_forwarded", f"{entrance}: sync: {synced}")
        site.store_clean(store)


def so_sync_lost_race_retries(site: Site) -> None:
    """Two stores sync at once; the one whose push loses the race inside serve (another push
    lands between its advertisement and its commands) fetches and retries. Today serve's
    pre-receive refuses it as `non-fast-forward` (its proc-receive, losing later, as `stale`),
    the client reads either as `declined`, and sync gives up."""
    store = "race"
    site.store(store)
    url = site.url("ssh", "ada", store)
    for side in ("r1", "r2"):
        site.so_ok(["--as", f"person/ada-{side}", "clone", url, "--into", side])
        site.so_commit(side, f"agent/{side}", f"{side}.txt", side)

    # r1's push connection waits for r2's whole sync before it sends its commands.
    racer = ["so", "--as", "person/ada-r2", "--repo", f"{HOME}/r2", "sync", "origin"]
    env = {
        "GIT_SSH_COMMAND": "race-ssh",
        "GIT_SSH_VARIANT": "ssh",
        "RACE_MARKER": f"{HOME}/race.marker",
        "RACE_COMMAND": json.dumps(racer),
    }
    done = site.so(["--as", "person/ada-r1", "--repo", "r1", "sync", "origin"], env)
    raced = site.box.run(CLIENT, ["cat", env["RACE_MARKER"]]).out.strip()
    check(raced == "0", f"the race did not run to its end: {raced!r} {done.err}")
    envelope = done.envelope()
    result = envelope.get("result") or {}
    check(
        envelope.get("ok") is True and result.get("status") == "pushed" and result.get("attempts", 0) >= 2,
        f"lost race not retried: {json.dumps(envelope.get('error') or result)}",
    )


def so_push_names_its_identity(site: Site) -> None:
    """A changeset a `so push` imports names the key's identity as its agent (M29-18)."""
    store = "identity"
    site.store(store)
    site.so_ok(["--as", "person/ada-id", "clone", site.url("ssh", "ada", store), "--into", "id"])
    site.so_commit("id", "agent/a1", "who.txt", "who")
    site.so_ok(["--repo", "id", "push", "origin"])
    agent = site.store_show(store, "main")["author"]["agent"]
    check(agent == "person/ada", f"the pushed changeset's agent is {agent}")


Test = Callable[[Site], None]
TESTS: dict[str, Test] = {
    "git_session_over_real_sshd": git_session_over_real_sshd,
    "git_session_over_nginx_fcgiwrap": git_session_over_nginx_fcgiwrap,
    "land_push_over_both": land_push_over_both,
    "so_clone_fetch_push_over_serve": so_clone_fetch_push_over_serve,
}

# Each strict expected failure with the words its known failure carries: it fails the run when
# it passes (flip it to TESTS) or fails without them (a new failure, not the known one).
EXPECTED: dict[str, tuple[Test, str, str]] = {
    "so_sync_lost_race_retries": (so_sync_lost_race_retries, "refused (declined)", "interop D1"),
    "so_push_names_its_identity": (so_push_names_its_identity, "unknown/unknown", "M29-18"),
}


def auth_off_fails_the_run(tag: str, bin_dir: Path) -> list[str]:
    """With authentication planted off at both entrances, each strangers' check fails; the
    problems found, none when the self-check holds."""
    problems = []
    box = Container.start(tag, bin_dir, PLANT_AUTH_OFF)
    try:
        site = Site(box)
        site.store("planted")
        for entrance in ENTRANCES:
            try:
                strangers_refused(site, entrance, "planted")
            except Failure:
                continue
            problems.append(f"{entrance}: the strangers' check passed with authentication off")
    finally:
        box.stop()
    return problems


# ---------------------------------------------------------------------------------------------
# The run


def default_bin_dir() -> Path:
    """The debug build: CARGO_TARGET_DIR's, or target/'s."""
    target = os.environ.get("CARGO_TARGET_DIR") or str(ROOT / "target")
    return Path(target) / "debug"


def run(bin_dir: Path, only: str) -> int:
    """Every test, the expected failures and the self-check (or `only` one of them)."""
    tag = image_tag()
    missing = [name for name in BINARIES if not (bin_dir / name).is_file()]
    if missing:
        print(f"server-entrance: {bin_dir} lacks {', '.join(missing)}", file=sys.stderr)
        return 1
    if not image_is_local(tag):
        print(f"server-entrance: {tag} is not local: run `build` first", file=sys.stderr)
        return 1

    failed: list[str] = []
    box = Container.start(tag, bin_dir.resolve())
    try:
        site = Site(box)
        version = box.run(CLIENT, ["git", "--version"]).out.strip()
        if version != GIT_VERSION:
            failed.append(f"the image's git is {version!r}, not {GIT_VERSION!r}")

        # 1. The tests.
        for name, test in TESTS.items():
            if only and name != only:
                continue
            started = time.monotonic()
            try:
                test(site)
                print(f"ok      {name} ({time.monotonic() - started:.1f} s)")
            except (Failure, subprocess.TimeoutExpired) as error:
                print(f"FAILED  {name}: {error}")
                failed.append(name)

        # 2. The strict expected failures.
        for name, (test, known, flips_on) in EXPECTED.items():
            if only and name != only:
                continue
            try:
                test(site)
            except (Failure, subprocess.TimeoutExpired) as error:
                if known in str(error):
                    print(f"xfail   {name}: until {flips_on}: {error}")
                    continue
                print(f"FAILED  {name}: not its known failure ({known!r}): {error}")
                failed.append(name)
                continue
            print(f"FAILED  {name}: passes now ({flips_on}?): make it a test")
            failed.append(name)
    finally:
        box.stop()

    # 3. The self-check.
    if not only or only == "auth_off_fails_the_run":
        problems = auth_off_fails_the_run(tag, bin_dir.resolve())
        for problem in problems:
            print(f"FAILED  auth_off_fails_the_run: {problem}")
        if problems:
            failed.append("auth_off_fails_the_run")
        else:
            print("ok      auth_off_fails_the_run")

    print(f"server-entrance: {len(failed)} failed" + (f": {', '.join(failed)}" if failed else ""))
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("build", help="build the test image unless it is local")
    running = commands.add_parser("run", help="run the tests offline")
    running.add_argument("--bin-dir", type=Path, default=default_bin_dir())
    running.add_argument("--only", default="", help="one test, expected failure or self-check")
    args = parser.parse_args()
    if args.command == "build":
        return build()
    return run(args.bin_dir, args.only)


if __name__ == "__main__":
    sys.exit(main())
