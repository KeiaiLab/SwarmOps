# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests the server kit's backup and drill in packaging/server/backup/, offline and without root.

    python3 -I scripts/test-server-backup.py

`so-backup` copies every store under its lock, checks the copy, encrypts the copy, the settings
and the secrets to two recipients, and ships them to an S3-compatible bucket; `rehearse` restores
the newest backup onto an empty directory; `check` alerts when no rehearsal passed within the
period. The bucket here is a stand-in on the loopback address that verifies each request's
signature and enforces the policies in backup/policy/ for the two credentials. The cases run a
real `swarmops` (SWARMOPS_BIN, else the release or debug build under CARGO_TARGET_DIR or
target/), `openssl`, `curl`, `flock` and `tar`; a missing one fails the run and names it. Prints a
summary; exit 1 on a failure.
"""

import configparser
import datetime
import fnmatch
import hashlib
import hmac
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.parse
from collections.abc import Callable
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BACKUP = ROOT / "packaging" / "server" / "backup"
SO_BACKUP = BACKUP / "bin" / "so-backup"
REHEARSE = BACKUP / "bin" / "rehearse"
SO_KIT = ROOT / "packaging" / "server" / "bin" / "so-kit"
# Where the package installs so-backup, the periodic jobs and the rehearsal program.
INSTALLED_SO_BACKUP = "/usr/sbin/so-backup"
JOBS = "/usr/lib/swarmops-server/jobs/"
INSTALLED_REHEARSE = "/usr/lib/swarmops-server/backup/rehearse"
POLICIES = BACKUP / "policy"

TOOLS = ("openssl", "curl", "flock", "tar", "sha256sum")
BUCKET = "so-test"
REGION = "auto"
CREDENTIALS = {
    "serving": ("SERVINGKEYID", "serving-secret+/="),
    "rehearsal": ("REHEARSALKEYID", "rehearsal-secret+/="),
}
# The stand-in's page size for a listing, small so that a listing crosses pages.
PAGE = 3
# Committing processes beside the backup (M28-4), and commits each makes before it starts.
COMMITTERS = 8
COMMITS_BEFORE = 2
# swarmops' exit code for a retryable error, such as E_BUSY while the backup holds the lock.
RETRYABLE_EXIT = 4
WEEK = 7 * 24 * 3600
STAMP = "%Y%m%dT%H%M%SZ"


def swarmops_binary() -> Path:
    """SWARMOPS_BIN, else a build under CARGO_TARGET_DIR or target/."""
    named = os.environ.get("SWARMOPS_BIN")
    if named:
        return Path(named)
    target = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
    for profile in ("release", "debug"):
        if (target / profile / "swarmops").is_file():
            return target / profile / "swarmops"
    raise AssertionError(f"no swarmops binary: set SWARMOPS_BIN or build one under {target}")


# The bucket stand-in: path-style S3, signature version 4, the shipped policies.

def policy(name: str) -> list[dict]:
    """The statements of a shipped policy, its bucket placeholder filled."""
    text = (POLICIES / f"{name}.json").read_text().replace("<bucket>", BUCKET)
    return json.loads(text)["Statement"]


def allowed(statements: list[dict], action: str, resource: str, prefix: str | None) -> bool:
    """An Allow statement names the action and the resource, and its prefix condition, if any,
    matches the listing's prefix."""
    for statement in statements:
        if statement.get("Effect") != "Allow":
            continue
        actions = statement["Action"] if isinstance(statement["Action"], list) else [
            statement["Action"]]
        resources = statement["Resource"] if isinstance(statement["Resource"], list) else [
            statement["Resource"]]
        if not any(fnmatch.fnmatchcase(action, pattern) for pattern in actions):
            continue
        if not any(fnmatch.fnmatchcase(resource, pattern) for pattern in resources):
            continue
        condition = statement.get("Condition", {}).get("StringLike", {}).get("s3:prefix")
        if condition is not None:
            patterns = condition if isinstance(condition, list) else [condition]
            if prefix is None or not any(fnmatch.fnmatchcase(prefix, p) for p in patterns):
                continue
        return True
    return False


def sign(secret: str, date: str, scope: str, text: str) -> str:
    """Signature version 4 of `text` for the scope `date/region/service/aws4_request`."""
    key = ("AWS4" + secret).encode()
    for part in [date] + scope.split("/")[1:]:
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    return hmac.new(key, text.encode(), hashlib.sha256).hexdigest()


def quote(text: str) -> str:
    return urllib.parse.quote(text, safe="-_.~")


class Bucket:
    """Objects by key, and the requests each credential made."""

    def __init__(self) -> None:
        self.objects: dict[str, bytes] = {}
        self.refused: list[str] = []
        self.lock = threading.Lock()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def endpoint(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def keys(self, prefix: str) -> list[str]:
        with self.lock:
            return sorted(key for key in self.objects if key.startswith(prefix))

    def handler(self) -> type[BaseHTTPRequestHandler]:
        bucket = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_: object) -> None:
                pass

            def answer(self, status: int, body: bytes = b"") -> None:
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                if self.command != "HEAD":
                    self.wfile.write(body)

            def who(self, body: bytes) -> str | None:
                """The credential whose signature the request carries, else None."""
                header = self.headers.get("Authorization", "")
                if not header.startswith("AWS4-HMAC-SHA256 "):
                    return None
                fields = dict(part.strip().split("=", 1)
                              for part in header[len("AWS4-HMAC-SHA256 "):].split(","))
                key_id, scope = fields["Credential"].split("/", 1)
                names = [name for name, (k, _) in CREDENTIALS.items() if k == key_id]
                if not names:
                    return None
                payload = self.headers.get("x-amz-content-sha256")
                if payload is None:
                    return None
                if payload != "UNSIGNED-PAYLOAD" and payload != hashlib.sha256(body).hexdigest():
                    return None
                url = urllib.parse.urlsplit(self.path)
                query = sorted(urllib.parse.parse_qsl(url.query, keep_blank_values=True))
                signed = fields["SignedHeaders"].split(";")
                canonical = "\n".join([
                    self.command,
                    "/".join(quote(urllib.parse.unquote(s)) for s in url.path.split("/")),
                    "&".join(f"{quote(k)}={quote(v)}" for k, v in query),
                    "".join(f"{name}:{' '.join(self.headers.get(name, '').split())}\n"
                            for name in signed),
                    fields["SignedHeaders"],
                    payload,
                ])
                stamp = self.headers.get("x-amz-date", "")
                text = "\n".join(["AWS4-HMAC-SHA256", stamp, scope,
                                  hashlib.sha256(canonical.encode()).hexdigest()])
                secret = CREDENTIALS[names[0]][1]
                if not hmac.compare_digest(sign(secret, stamp[:8], scope, text),
                                           fields["Signature"]):
                    return None
                return names[0]

            def handle_any(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                url = urllib.parse.urlsplit(self.path)
                name, _, key = urllib.parse.unquote(url.path).lstrip("/").partition("/")
                if name != BUCKET:
                    return self.answer(404)
                credential = self.who(body)
                if credential is None:
                    bucket.refused.append(f"{self.command} {key}: signature")
                    return self.answer(403)
                statements = policy(credential)
                query = dict(urllib.parse.parse_qsl(url.query, keep_blank_values=True))

                # 1. A listing: ListObjectsV2 with a prefix, a page at a time.
                if self.command == "GET" and not key:
                    prefix = query.get("prefix", "")
                    if not allowed(statements, "s3:ListBucket", f"arn:aws:s3:::{BUCKET}",
                                   prefix):
                        bucket.refused.append(f"LIST {prefix}: {credential}")
                        return self.answer(403)
                    keys = bucket.keys(prefix)
                    start = int(query.get("continuation-token", "0") or 0)
                    page = keys[start:start + PAGE]
                    more = start + PAGE < len(keys)
                    xml = "".join(f"<Contents><Key>{k}</Key></Contents>" for k in page)
                    token = (f"<NextContinuationToken>{start + PAGE}</NextContinuationToken>"
                             if more else "")
                    text = (f"<?xml version=\"1.0\"?><ListBucketResult><Name>{BUCKET}</Name>"
                            f"<IsTruncated>{'true' if more else 'false'}</IsTruncated>"
                            f"{token}{xml}</ListBucketResult>")
                    return self.answer(200, text.encode())

                # 2. One object.
                action = {"GET": "s3:GetObject", "HEAD": "s3:GetObject",
                          "PUT": "s3:PutObject", "DELETE": "s3:DeleteObject"}[self.command]
                if not allowed(statements, action, f"arn:aws:s3:::{BUCKET}/{key}", None):
                    bucket.refused.append(f"{self.command} {key}: {credential}")
                    return self.answer(403)
                with bucket.lock:
                    if self.command == "PUT":
                        bucket.objects[key] = body
                        return self.answer(200)
                    if self.command == "DELETE":
                        bucket.objects.pop(key, None)
                        return self.answer(204)
                    if key not in bucket.objects:
                        return self.answer(404)
                    return self.answer(200, bucket.objects[key])

            do_GET = do_PUT = do_DELETE = do_HEAD = handle_any

        return Handler


def signed_request(bucket: Bucket, credential: str, method: str, key: str,
                   secret: str | None = None) -> int:
    """`curl` signing one request with `credential`, or with its key id and `secret`; the HTTP
    status."""
    key_id, known = CREDENTIALS[credential]
    secret = known if secret is None else secret
    done = subprocess.run(
        ["curl", "-sS", "-o", "/dev/null", "-w", "%{http_code}", "-X", method,
         "--aws-sigv4", f"aws:amz:{REGION}:s3", "-K", "-",
         "-H", f"x-amz-content-sha256: {hashlib.sha256(b'').hexdigest()}",
         f"{bucket.endpoint}/{BUCKET}/{key}"],
        input=f'user = "{key_id}:{secret}"\n', text=True, capture_output=True, check=False)
    return int(done.stdout or 0)


# The world a case runs in: stores, keys, settings, the bucket, an alert recorder.

class World:
    def __init__(self, scratch: Path, bucket: Bucket) -> None:
        self.scratch = scratch
        self.bucket = bucket
        self.swarmops = swarmops_binary()
        self.root = scratch / "srv"
        self.root.mkdir()
        self.keys = scratch / "keys"
        self.keys.mkdir()
        for name in ("recovery", "rehearsal", "stranger"):
            subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
                 "ec_paramgen_curve:P-256", "-nodes", "-subj", f"/CN={name}", "-days", "30",
                 "-keyout", str(self.keys / f"{name}.key"),
                 "-out", str(self.keys / f"{name}.pem")],
                check=True, capture_output=True)
        for name, (key_id, secret) in CREDENTIALS.items():
            (self.keys / f"{name}.credential").write_text(
                f"access_key_id={key_id}\nsecret_access_key={secret}\n")
        self.secrets = scratch / "etc"
        self.secrets.mkdir()
        (self.secrets / "authorized_keys").write_text("SECRET-KEY-LINES\n")
        (self.secrets / "web.htpasswd").write_text("maintainer:SECRET-PASSWORD-HASH\n")
        self.alerts = scratch / "alerts"
        alert = scratch / "alert"
        alert.write_text(f"#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$2\" >>{self.alerts}\n")
        alert.chmod(0o755)
        self.alert = alert

    def store(self, name: str) -> Path:
        path = self.root / name
        self.so(path, "init", str(path), "--name", name, repo=False)
        (path / "access.toml").write_text("version = 1\n")
        return path

    def so(self, store: Path, *arguments: str, repo: bool = True) -> dict:
        command = [str(self.swarmops)] + (["--repo", str(store)] if repo else []) + list(
            arguments)
        done = subprocess.run(command, capture_output=True, text=True, check=False,
                              env={**os.environ, "SWARMOPS_AGENT": "test/backup"})
        assert done.returncode == 0, f"{arguments}: {done.stdout} {done.stderr}"
        return json.loads(done.stdout)

    def commit(self, store: Path, ref: str, text: str, parent: str | None) -> subprocess.CompletedProcess:
        changes = json.dumps([{"op": "put", "path": "f.txt", "content": {"inline": text}}])
        command = [str(self.swarmops), "--repo", str(store), "--as", "test/committer",
                   "commit", "-m", text, "--changes", "-", "--advance", ref]
        if parent:
            command += ["--parent", parent]
        return subprocess.run(command, input=changes, capture_output=True, text=True,
                              check=False)

    def settings(self, side: str, **changes: str) -> Path:
        """The settings for the serving side or the rehearsal side, `changes` overriding: a
        directory holding the kit's kit.conf and this program's backup.conf."""
        values = {
            "root": str(self.root),
            "swarmops": str(self.swarmops),
            "web_user": "www-data",
            "backup_endpoint": self.bucket.endpoint,
            "backup_region": REGION,
            "backup_bucket": BUCKET,
            "backup_credential": str(self.keys / f"{side}.credential"),
            "backup_recovery_recipient": str(self.keys / "recovery.pem"),
            "backup_rehearsal_recipient": str(self.keys / "rehearsal.pem"),
            "backup_secrets": f"{self.secrets / 'authorized_keys'}:{self.secrets / 'web.htpasswd'}",
            "backup_rehearsal_period": str(WEEK),
            "backup_alert": str(self.alert),
        }
        if side == "rehearsal":
            values["backup_rehearsal_key"] = str(self.keys / "rehearsal.key")
        values.update(changes)
        settings = self.scratch / f"{side}-settings"
        settings.mkdir(exist_ok=True)
        for name, ours in (("kit.conf", False), ("backup.conf", True)):
            lines = "".join(f"{key}={value}\n" for key, value in values.items()
                            if key.startswith("backup_") == ours)
            (settings / name).write_text("# settings\n" + lines)
        return settings

    def environment(self, settings: Path) -> dict[str, str]:
        return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "SO_KIT_CONF": str(settings / "kit.conf"),
                "SO_BACKUP_CONF": str(settings / "backup.conf"), "TMPDIR": str(self.scratch)}

    def run(self, settings: Path, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run(["sh", str(SO_BACKUP), *arguments], text=True, capture_output=True,
                              env=self.environment(settings), check=False)

    def backup(self) -> str:
        done = self.run(self.settings("serving"), "backup")
        assert done.returncode == 0, f"backup: {done}"
        return done.stdout.strip()

    def rehearse(self, target: Path, **changes: str) -> subprocess.CompletedProcess:
        return self.run(self.settings("rehearsal", **changes), "rehearse", str(target))

    def alerted(self) -> list[str]:
        return self.alerts.read_text().splitlines() if self.alerts.exists() else []

    def records(self) -> list[str]:
        return self.bucket.keys("rehearsal/records/")


def opened(world: World, key: str, name: str = "recovery") -> dict[str, bytes]:
    """The members of the backup at `key`, decrypted with `name`'s key."""
    sealed = world.scratch / "sealed.cms"
    sealed.write_bytes(world.bucket.objects[key])
    done = subprocess.run(
        ["openssl", "cms", "-decrypt", "-binary", "-inform", "DER", "-in", str(sealed),
         "-recip", str(world.keys / f"{name}.pem"), "-inkey", str(world.keys / f"{name}.key")],
        capture_output=True, check=False)
    assert done.returncode == 0, f"{name} cannot open {key}: {done.stderr!r}"
    with tarfile.open(fileobj=io.BytesIO(done.stdout)) as archive:
        return {member.name: archive.extractfile(member).read()
                for member in archive.getmembers() if member.isfile()}


def manifest(members: dict[str, bytes]) -> list[list[str]]:
    return [line.split(" ", 3) for line in members["manifest"].decode().splitlines()]


def tar_names(data: bytes) -> list[str]:
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        return [member.name for member in archive.getmembers() if member.isfile()]


def ref_targets(world: World, store: Path) -> dict[str, str]:
    refs = world.so(store, "ref", "list")["result"]["refs"]
    return {ref["name"]: ref["target"] for ref in refs}


def stamp(seconds_ago: int) -> str:
    moment = datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=seconds_ago)
    return moment.strftime(STAMP)


def settle(scratch: Path) -> None:
    """Waits for the processes that name `scratch`: a ref move in a store with `access.toml`
    starts a detached `serve --prepare`, which writes on after its commit returned."""
    deadline = time.monotonic() + 120
    needle = str(scratch).encode()
    while True:
        running = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit() or int(entry.name) == os.getpid():
                continue
            try:
                if needle in (entry / "cmdline").read_bytes():
                    running.append(entry.name)
            except OSError:
                continue
        if not running:
            return
        assert time.monotonic() < deadline, f"processes still use the scratch: {running}"
        time.sleep(0.05)


# The cases.

def backup_beside_committers_restores_equal(world: World) -> None:
    """A backup taken while 8 processes commit restores on an empty directory with every ref
    equal to the manifest, each one a changeset its writer was told it made, and `verify --deep`
    clean (M28-4)."""
    store = world.store("busy")
    stop = threading.Event()
    made: dict[str, set[str]] = {f"release/w{n}": set() for n in range(COMMITTERS)}
    errors: list[str] = []

    def committer(ref: str) -> None:
        parent = None
        count = 0
        while not stop.is_set():
            done = world.commit(store, ref, f"{ref} {count}", parent)
            if done.returncode == RETRYABLE_EXIT:
                continue
            if done.returncode != 0:
                errors.append(f"{ref}: {done.stdout}")
                return
            parent = json.loads(done.stdout)["result"]["changeset"]
            made[ref].add(parent)
            count += 1

    threads = [threading.Thread(target=committer, args=(ref,)) for ref in made]
    for thread in threads:
        thread.start()
    try:
        deadline = time.monotonic() + 60
        while any(len(ids) < COMMITS_BEFORE for ids in made.values()) and not errors:
            assert time.monotonic() < deadline, "the committers did not start"
            time.sleep(0.05)
        key = world.backup()
        before = {ref: len(ids) for ref, ids in made.items()}
        while any(len(made[ref]) <= before[ref] for ref in made) and not errors:
            assert time.monotonic() < deadline + 60, "the committers stopped during the backup"
            time.sleep(0.05)
    finally:
        stop.set()
        for thread in threads:
            thread.join()
    assert not errors, f"a committer failed: {errors}"

    target = world.scratch / "drill"
    target.mkdir()
    done = world.rehearse(target)
    assert done.returncode == 0, f"rehearse: {done}"
    restored = ref_targets(world, target / "stores" / "busy")
    assert set(restored) == set(made), f"refs restored: {sorted(restored)}"
    for ref, changeset in restored.items():
        assert changeset in made[ref], f"{ref} restored to a changeset nobody made"
    lines = [line for line in manifest(opened(world, key)) if line[:2] == ["refs", "busy"]]
    assert len(lines) == 1, f"manifest refs lines for busy: {lines}"
    recorded = {ref["name"]: ref["target"] for ref in json.loads(lines[0][2])["refs"]}
    assert recorded == restored, "restored refs differ from the manifest"
    verified = world.so(target / "stores" / "busy", "verify", "--deep")["result"]
    assert verified["problems"] == [], f"verify --deep: {verified}"


def flipped_bit_is_refused(world: World) -> None:
    """One flipped bit in the shipped backup fails the rehearsal: it alerts, restores nothing and
    writes no record."""
    world.store("one")
    key = world.backup()
    data = bytearray(world.bucket.objects[key])
    data[len(data) // 2] ^= 0x01
    world.bucket.objects[key] = bytes(data)
    target = world.scratch / "drill"
    target.mkdir()
    done = world.rehearse(target)
    assert done.returncode == 1, f"rehearse of a flipped backup: {done}"
    assert any(line.startswith("fail ") for line in world.alerted()), "no alert"
    assert not world.records(), f"a record for a flipped backup: {world.records()}"
    assert not (target / "stores").exists(), "a store restored from a flipped backup"


def manifest_names_every_part(world: World) -> None:
    """The manifest names the release, the time and every part, with its size and digest, and
    every store under the root has a part; nothing else is in the backup."""
    for name in ("alpha", "beta"):
        world.store(name)
    members = opened(world, world.backup())
    lines = manifest(members)
    assert lines[0] == ["swarmops-backup", "1"], f"header: {lines[0]}"
    kinds = [line[0] for line in lines]
    assert kinds.count("release") == 1 and kinds.count("time") == 1, f"manifest: {kinds}"
    parts = {line[1]: line[2:] for line in lines if line[0] == "part"}
    expected = {"parts/stores/alpha.tar", "parts/stores/beta.tar", "parts/settings.tar",
                "parts/secrets.tar"}
    assert set(parts) == expected, f"parts: {sorted(parts)}"
    assert set(members) == expected | {"manifest"}, f"members: {sorted(members)}"
    for name, (size, digest) in parts.items():
        assert int(size) == len(members[name]), f"{name}: size"
        assert digest == hashlib.sha256(members[name]).hexdigest(), f"{name}: digest"
    assert "./access.toml" in tar_names(members["parts/stores/alpha.tar"]), "access.toml"
    refs = [line[1] for line in lines if line[0] == "refs"]
    assert sorted(refs) == ["alpha", "beta"], f"refs lines: {refs}"


def secrets_are_a_separate_part(world: World) -> None:
    """The settings' secret files are in the secrets part and in no other part."""
    world.store("one")
    members = opened(world, world.backup())
    secrets = members["parts/secrets.tar"]
    names = tar_names(secrets)
    assert any(name.endswith("authorized_keys") for name in names), f"secrets: {names}"
    assert any(name.endswith("web.htpasswd") for name in names), f"secrets: {names}"
    for name, data in members.items():
        if name == "parts/secrets.tar":
            continue
        assert b"SECRET-" not in data, f"a secret in {name}"
    settings = tar_names(members["parts/settings.tar"])
    assert "kit.conf" in settings and "backup.conf" in settings, f"settings part: {settings}"


def backup_has_two_recipients(world: World) -> None:
    """The recovery key and the rehearsal key each open a backup, another key does not, and a
    backup without both recipients is refused."""
    world.store("one")
    key = world.backup()
    for name in ("recovery", "rehearsal"):
        assert "manifest" in opened(world, key, name), name
    try:
        opened(world, key, "stranger")
    except AssertionError:
        pass
    else:
        raise AssertionError("a third key opens the backup")
    done = world.run(world.settings("serving", backup_rehearsal_recipient=""), "backup")
    assert done.returncode != 0, f"a backup with one recipient: {done}"
    assert world.bucket.keys("backups/") == [key], "a backup shipped with one recipient"


def skipped_drill_alerts(world: World) -> None:
    """`check` on the serving side alerts when no rehearsal record exists."""
    done = world.run(world.settings("serving"), "check")
    assert done.returncode == 1, f"check without a record: {done}"
    assert any(line.startswith("fail ") for line in world.alerted()), "no alert"


def rehearsal_without_its_key_alerts(world: World) -> None:
    """A rehearsal whose key is missing alerts and writes no record."""
    world.store("one")
    world.backup()
    target = world.scratch / "drill"
    target.mkdir()
    missing = str(world.scratch / "no-such.key")
    for changes in ({"backup_rehearsal_key": missing}, {"backup_rehearsal_key": ""}):
        done = world.rehearse(target, **changes)
        assert done.returncode == 1, f"rehearse without its key ({changes}): {done}"
    assert len([line for line in world.alerted() if line.startswith("fail ")]) == 2, \
        f"alerts: {world.alerted()}"
    assert not world.records(), "a record without the key"


def backup_credential_cannot_delete(world: World) -> None:
    """Neither shipped policy grants a delete; the serving credential ships and checks under its
    policy and is refused a delete and a read of a backup, and the rehearsal credential a
    delete."""
    for name in ("serving", "rehearsal"):
        for statement in policy(name):
            actions = statement["Action"] if isinstance(statement["Action"], list) else [
                statement["Action"]]
            assert not any("Delete" in a or a in ("*", "s3:*") for a in actions), \
                f"{name}: {actions}"
    world.store("one")
    key = world.backup()
    assert signed_request(world.bucket, "serving", "DELETE", key) == 403, "serving deletes"
    assert signed_request(world.bucket, "serving", "GET", key) == 403, "serving reads a backup"
    assert signed_request(world.bucket, "rehearsal", "DELETE", key) == 403, "rehearsal deletes"
    assert world.bucket.keys("backups/") == [key], "the backup is gone"
    assert signed_request(world.bucket, "rehearsal", "GET", key) == 200, "self-check: a read"
    assert signed_request(world.bucket, "rehearsal", "GET", key, "wrong") == 403, \
        "self-check: a wrong signature is read"


def rehearse_on_empty_directory_restores_equal(world: World) -> None:
    """`rehearse` restores the newest backup on an empty directory with every ref equal, writes
    one record with the two recovery numbers and sends the test alert; a directory that is not
    empty is refused. Older backups fill more than one page of the listing."""
    store = world.store("one")
    for older in range(PAGE + 1):
        world.bucket.objects[f"backups/{stamp(WEEK + older)}-1.cms"] = b"older"
    assert world.commit(store, "main", "first", None).returncode == 0
    world.backup()
    head = world.commit(store, "main", "second", ref_targets(world, store)["main"])
    assert head.returncode == 0
    world.backup()
    busy = world.scratch / "busy"
    busy.mkdir()
    (busy / "file").write_text("x")
    assert world.rehearse(busy).returncode == 2, "a directory that is not empty"
    target = world.scratch / "drill"
    target.mkdir()
    done = world.rehearse(target)
    assert done.returncode == 0, f"rehearse: {done}"
    assert ref_targets(world, target / "stores" / "one") == ref_targets(world, store)
    assert (target / "settings" / "kit.conf").is_file(), "settings not restored"
    assert (target / "settings" / "backup.conf").is_file(), "backup settings not restored"
    assert (target / "secrets").is_dir(), "secrets not restored"
    records = world.records()
    assert len(records) == 1, f"records: {records}"
    record = json.loads(world.bucket.objects[records[0]])
    assert record["result"] == "pass" and record["stores"] == 1, f"record: {record}"
    assert record["backup"] == world.bucket.keys("backups/")[-1], "not the newest backup"
    assert isinstance(record["restore_seconds"], int), f"record: {record}"
    assert isinstance(record["backup_age_seconds"], int), f"record: {record}"
    assert any(line.startswith("test ") for line in world.alerted()), "no test alert"


def record_older_than_a_period_alerts(world: World) -> None:
    """`check` alerts when the newest record is older than the period, and is quiet when one is
    newer; the serving credential reads the records."""
    world.bucket.objects[f"rehearsal/records/{stamp(WEEK + 3600)}.json"] = b"{}"
    done = world.run(world.settings("serving"), "check")
    assert done.returncode == 1, f"check with an old record: {done}"
    assert len(world.alerted()) == 1, f"alerts: {world.alerted()}"
    world.bucket.objects[f"rehearsal/records/{stamp(3600)}.json"] = b"{}"
    done = world.run(world.settings("serving"), "check")
    assert done.returncode == 0, f"check with a fresh record: {done}"
    assert len(world.alerted()) == 1, f"alerts: {world.alerted()}"


def backup_takes_the_store_lock(world: World) -> None:
    """The copy waits for `meta/lock`: a holder that writes a file into the store before it lets
    go finds that file in the backup."""
    store = world.store("held")
    holding = world.scratch / "holding"
    marker = store / "held-marker"
    holder = subprocess.Popen(
        ["flock", str(store / "meta" / "lock"), "sh", "-c",
         f"touch {holding}; sleep 1; echo held > {marker}"])
    try:
        deadline = time.monotonic() + 60
        while not holding.exists():
            assert time.monotonic() < deadline, "the holder did not take the lock"
            time.sleep(0.01)
        members = opened(world, world.backup())
    finally:
        holder.wait()
    assert "./held-marker" in tar_names(members["parts/stores/held.tar"]), \
        "the copy did not wait for the lock"


def backup_leaves_out_the_derived_cache(world: World) -> None:
    """`meta/derived/` is a cache readers write without the store's lock: the copy leaves it
    out, so a reader's write during the copy cannot fail the backup."""
    store = world.store("one")
    assert world.commit(store, "main", "first", None).returncode == 0
    cache = store / "meta" / "derived" / "planted" / "1"
    cache.mkdir(parents=True)
    (cache / "value").write_text("cached")
    names = tar_names(opened(world, world.backup())["parts/stores/one.tar"])
    assert not [name for name in names if name.startswith("./meta/derived")], \
        f"the derived cache is in the copy: {names}"
    assert "./access.toml" in names, "self-check: the copy holds the store"


def files_are_installed_and_units_confined(_: World) -> None:
    """The subtree's MANIFEST installs every file of it and nothing it lacks; each service runs
    a job file of the kit's jobs directory, which runs so-backup, as the serving account with
    the system read-only; each timer is wanted by timers.target; the rehearsal program is where
    `so-kit run rehearse` starts it."""
    installs = {}
    for line in (BACKUP / "MANIFEST").read_text().splitlines():
        words = line.split("#", 1)[0].split()
        if words and words[0] == "install":
            installs[words[1]] = words[2:]
    files = sorted(str(path.relative_to(BACKUP)) for path in BACKUP.rglob("*")
                   if path.is_file() and path.name != "MANIFEST")
    assert sorted(installs) == files, f"installed {sorted(installs)}, present {files}"
    assert installs["bin/so-backup"] == [INSTALLED_SO_BACKUP, "0755"], "so-backup"
    assert installs["bin/rehearse"] == [INSTALLED_REHEARSE, "0755"], "rehearse"
    jobs = {destination: name for name, (destination, _mode) in installs.items()
            if destination.startswith(JOBS)}
    for name in files:
        if not name.startswith("systemd/"):
            continue
        unit = configparser.ConfigParser(interpolation=None, strict=True)
        unit.optionxform = str
        unit.read_string((BACKUP / name).read_text())
        if name.endswith(".timer"):
            assert unit.get("Install", "WantedBy") == "timers.target", f"{name}: WantedBy"
            continue
        service = unit["Service"]
        job = service["ExecStart"]
        assert job in jobs, f"{name}: ExecStart is no job file"
        assert f"exec {INSTALLED_SO_BACKUP} " in (BACKUP / jobs[job]).read_text(), \
            f"{jobs[job]}: runs no so-backup step"
        for key, value in (("User", "so"), ("NoNewPrivileges", "yes"),
                           ("ProtectSystem", "strict"), ("ProtectHome", "yes"),
                           ("PrivateTmp", "yes")):
            assert service.get(key) == value, f"{name}: {key}"


def settings_refuse_unknown_keys(world: World) -> None:
    """A misspelt backup key is refused, naming it; a key of so-kit's is left to so-kit."""
    done = world.run(world.settings("serving", backup_bukket="x"), "check")
    assert done.returncode == 2 and "backup_bukket" in done.stderr, f"misspelt: {done}"
    done = world.run(world.settings("serving", web_root="/var/www"), "check")
    assert "web_root" not in done.stderr, f"so-kit's key refused: {done}"


def kit_settings_pass_so_kit(world: World) -> None:
    """The kit's settings file, as the backup reads it, passes so-kit, which refuses a key it
    does not know: the backup's keys live in a file of their own."""
    settings = world.settings("serving")
    done = subprocess.run(["sh", str(SO_KIT), "check"], text=True, capture_output=True,
                          env=world.environment(settings), check=False)
    assert done.returncode == 0, f"so-kit check: {done}"

    # 1. Self-check: a backup key in kit.conf is refused by so-kit.
    with (settings / "kit.conf").open("a") as kit:
        kit.write(f"backup_bucket={BUCKET}\n")
    done = subprocess.run(["sh", str(SO_KIT), "check"], text=True, capture_output=True,
                          env=world.environment(settings), check=False)
    assert done.returncode == 2, f"self-check: so-kit took a backup key: {done}"


def rehearse_role_runs_the_drill(world: World) -> None:
    """`so-kit run rehearse` with no directory restores the newest backup onto a new empty
    directory and writes one record: the rehearsal CronJob's one command."""
    store = world.store("one")
    assert world.commit(store, "main", "first", None).returncode == 0
    world.backup()

    # 1. The rehearsal program, pointed at this tree's so-backup, and so-kit pointed at it.
    program = world.scratch / "rehearse"
    program.write_text(REHEARSE.read_text().replace(INSTALLED_SO_BACKUP, str(SO_BACKUP), 1))
    program.chmod(0o755)
    settings = world.settings("rehearsal", rehearse=str(program))
    done = subprocess.run(["sh", str(SO_KIT), "run", "rehearse"], text=True,
                          capture_output=True, env=world.environment(settings), check=False)
    assert done.returncode == 0, f"so-kit run rehearse: {done}"
    records = world.records()
    assert len(records) == 1, f"records: {records}"
    assert json.loads(world.bucket.objects[records[0]])["stores"] == 1, "record"


CASES: tuple[tuple[str, Callable[[World], None]], ...] = (
    ("a backup beside committers restores equal", backup_beside_committers_restores_equal),
    ("a flipped bit is refused", flipped_bit_is_refused),
    ("the manifest names every part", manifest_names_every_part),
    ("secrets are a separate part", secrets_are_a_separate_part),
    ("a backup has two recipients", backup_has_two_recipients),
    ("a skipped drill alerts", skipped_drill_alerts),
    ("a rehearsal without its key alerts", rehearsal_without_its_key_alerts),
    ("the backup credential cannot delete", backup_credential_cannot_delete),
    ("rehearse on an empty directory restores equal", rehearse_on_empty_directory_restores_equal),
    ("a record older than a period alerts", record_older_than_a_period_alerts),
    ("the backup takes the store's lock", backup_takes_the_store_lock),
    ("the backup leaves out the derived cache", backup_leaves_out_the_derived_cache),
    ("files are installed and units confined", files_are_installed_and_units_confined),
    ("settings refuse unknown keys", settings_refuse_unknown_keys),
    ("the kit's settings pass so-kit", kit_settings_pass_so_kit),
    ("the rehearse role runs the drill", rehearse_role_runs_the_drill),
)


def main() -> int:
    missing = [tool for tool in TOOLS if shutil.which(tool) is None]
    if missing:
        print(f"test-server-backup: FAILED missing tools: {', '.join(missing)}")
        return 1

    failures = []
    for label, case in CASES:
        bucket = Bucket()
        try:
            with tempfile.TemporaryDirectory(prefix="test-server-backup-") as scratch:
                try:
                    case(World(Path(scratch), bucket))
                finally:
                    settle(Path(scratch))
        except Exception as error:  # a case's crash is its failure, not the run's
            failures.append(f"{label}: {type(error).__name__}: {error}")
        finally:
            bucket.close()

    print(f"test-server-backup: {len(CASES) - len(failures)} of {len(CASES)} cases passed")
    for failure in failures:
        print(f"test-server-backup: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
