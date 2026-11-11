# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests the server kit in packaging/server/, offline and without root.

    python3 -I scripts/test-server-kit.py

The kit is the files the `swarmops-server` package installs: the serving account, its
directories, the sshd drop-in, the nginx site, the CGI entry, the units and timers, its settings
and `so-kit`, the host steps. Each MANIFEST in the kit lists its files (`install <file>
<destination> <mode>`) and the system paths they rely on (`uses <path>`); backup/ and status/ keep
manifests of their own. The cases read every kit file with parsers of their own, run `so-kit`
against a scratch settings file, and show on a planted copy of the kit that each static rule still
finds what it is for. Where sshd is installed, `sshd -t` reads the drop-in too. Prints a summary;
exit 1 on a failure.
"""

import configparser
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KIT = ROOT / "packaging" / "server"
# Subtrees other slices own; each carries its own MANIFEST.
OWN_SUBTREES = ("backup", "status")

ACCOUNT = "so"
STORE_ROOT = "/srv/so"
AUTHORIZED_KEYS = "/var/lib/swarmops-server/.ssh/authorized_keys"
SSHD_DROPIN = "sshd/swarmops-server.conf"
NGINX_SITE = "nginx/swarmops-server.conf"
SO_KIT = "bin/so-kit"

# A DNS name under a public suffix the kit might be tempted to name, and the hosted copy's.
HOST_NAME = re.compile(r"\b[a-z0-9-]+(?:\.[a-z0-9-]+)*\.(?:cc|com|net|org|io|dev|app|cloud)\b",
                       re.IGNORECASE)
HOSTED_NAME = "swarmops.cc"
# An absolute path in a kit file: what follows a start of word, a quote, `=` or `(`.
ABSOLUTE = re.compile(r"(?:(?<=[\s\"'=(:])|^)(/(?:[A-Za-z0-9._@*-]+/?)+)", re.MULTILINE)
# What sshd may take from a client's environment for the serving account: git's protocol alone.
ACCEPTED_ENV = ["GIT_PROTOCOL"]
# Directives the drop-in must set for the serving account, with their values.
SSHD_REQUIRED = {
    "acceptenv": "GIT_PROTOCOL",
    "authenticationmethods": "publickey",
    "passwordauthentication": "no",
    "kbdinteractiveauthentication": "no",
    "disableforwarding": "yes",
    "permittty": "no",
    "permittunnel": "no",
    "permituserrc": "no",
    "authorizedkeysfile": AUTHORIZED_KEYS,
}
UNIT_SECTIONS = {"Unit", "Service", "Socket", "Timer", "Install"}
KEY_TYPES = ("ed25519", "ecdsa", "rsa")


def manifests(kit: Path) -> list[Path]:
    """Every MANIFEST in the kit, its own first."""
    return sorted(kit.rglob("MANIFEST"), key=lambda path: (path.parent != kit, str(path)))


def entries(kit: Path) -> tuple[dict[str, tuple[str, str]], list[str]]:
    """The kit's installed files (kit path → destination, mode) and the paths it uses."""
    installs: dict[str, tuple[str, str]] = {}
    uses: list[str] = []
    for manifest in manifests(kit):
        base = manifest.parent.relative_to(kit)
        for number, line in enumerate(manifest.read_text().splitlines(), 1):
            words = line.split("#", 1)[0].split()
            if not words:
                continue
            where = f"{manifest.relative_to(kit)}:{number}"
            if words[0] == "install" and len(words) == 4:
                installs[str(base / words[1])] = (words[2], words[3])
                continue
            if words[0] == "uses" and len(words) == 2:
                uses.append(words[1])
                continue
            raise AssertionError(f"{where}: not `install <file> <destination> <mode>` or "
                                 f"`uses <path>`: {line!r}")
    return installs, uses


def kit_files(kit: Path) -> list[str]:
    """The kit's files, as paths relative to the kit, MANIFESTs left out."""
    return sorted(str(path.relative_to(kit)) for path in kit.rglob("*")
                  if path.is_file() and path.name != "MANIFEST")


def own_files(kit: Path) -> list[str]:
    """The kit's files outside the subtrees other slices own."""
    return [name for name in kit_files(kit) if name.split("/", 1)[0] not in OWN_SUBTREES]


def tmpfiles_paths(kit: Path) -> list[str]:
    """The paths tmpfiles.d lines of the kit make: type, path, mode, user, group, age."""
    paths = []
    for name in own_files(kit):
        if not name.startswith("tmpfiles.d/"):
            continue
        for line in (kit / name).read_text().splitlines():
            words = line.split()
            if not words or words[0].startswith("#"):
                continue
            assert len(words) in (6, 7), f"{name}: a tmpfiles line has 6 or 7 fields: {line!r}"
            assert words[0] in ("d", "f", "z"), f"{name}: type {words[0]!r} is not d, f or z"
            assert re.fullmatch(r"0[0-7]{3}", words[2]), f"{name}: mode {words[2]!r}"
            paths.append(words[1])
    return paths


def listed_paths(kit: Path) -> list[str]:
    """Every path the kit may name: what it installs, makes and uses."""
    installs, uses = entries(kit)
    return [destination for destination, _ in installs.values()] + tmpfiles_paths(kit) + uses


def is_listed(path: str, listed: list[str]) -> bool:
    """`path` is a listed path, or lies below one."""
    path = path.rstrip("/") or "/"
    return any(path == entry or path.startswith(entry.rstrip("/") + "/") for entry in listed)


def named_paths(text: str) -> list[str]:
    """The absolute paths a kit file names, glob characters and trailing punctuation dropped;
    an nginx `location` names a URL, not a path."""
    found = []
    text = "\n".join(line for line in text.splitlines()
                     if not line.lstrip().startswith("location "))
    for match in ABSOLUTE.finditer(text):
        path = match.group(1).split("*", 1)[0].rstrip("/.")
        if path.count("/") >= 2 or path in ("/srv", "/run", "/etc", "/var", "/usr", "/bin"):
            found.append(path or "/")
    return found


def unlisted_paths(kit: Path) -> list[str]:
    """`file: path` for each absolute path a kit file names that no manifest or tmpfiles line
    lists."""
    listed = listed_paths(kit)
    return [f"{name}: {path}" for name in own_files(kit)
            for path in named_paths((kit / name).read_text())
            if not is_listed(path, listed)]


def host_names(kit: Path) -> list[str]:
    """`file:line: name` for each host name in the kit."""
    found = []
    for name in kit_files(kit) + [str(m.relative_to(kit)) for m in manifests(kit)]:
        for number, line in enumerate((kit / name).read_text().splitlines(), 1):
            for match in HOST_NAME.finditer(line):
                found.append(f"{name}:{number}: {match.group(0)}")
            if HOSTED_NAME in line.lower() and not HOST_NAME.search(line):
                found.append(f"{name}:{number}: {HOSTED_NAME}")
    return found


def sshd_directives(text: str) -> tuple[list[tuple[str, str]], list[tuple[str, str]]]:
    """The drop-in's directives before its `Match User` block and inside it."""
    before: list[tuple[str, str]] = []
    inside: list[tuple[str, str]] = []
    current = before
    closed = False
    for line in text.splitlines():
        words = line.split("#", 1)[0].split(None, 1)
        if not words:
            continue
        key, value = words[0].lower(), (words[1].strip() if len(words) > 1 else "")
        if key == "match":
            if value == f"User {ACCOUNT}":
                current = inside
                continue
            assert value.lower() == "all", f"sshd drop-in: Match {value!r} is not ours"
            closed = True
            current = before
            continue
        assert not closed, f"sshd drop-in: {key} after `Match all`"
        current.append((key, value))
    assert closed, "sshd drop-in: the Match block does not end with `Match all`"
    return before, inside


def sshd_problems(text: str) -> list[str]:
    """What the drop-in gets wrong for the serving account."""
    before, inside = sshd_directives(text)
    problems = [f"{key} outside the Match block" for key, _ in before]
    seen: dict[str, list[str]] = {}
    for key, value in inside:
        seen.setdefault(key, []).append(value)
    accepted = [name for value in seen.get("acceptenv", []) for name in value.split()]
    if accepted != ACCEPTED_ENV:
        problems.append(f"AcceptEnv takes {accepted}, not {ACCEPTED_ENV}")
    for key, value in SSHD_REQUIRED.items():
        if key == "acceptenv":
            continue
        if seen.get(key) != [value]:
            problems.append(f"{key} is {seen.get(key)}, not [{value!r}]")
    for key in ("setenv", "permituserenvironment", "forcecommand"):
        if key in seen:
            problems.append(f"{key} is set")
    return problems


def unit(path: Path) -> configparser.ConfigParser:
    """A systemd unit file read as INI, keys kept as written, repeated keys refused."""
    parser = configparser.ConfigParser(interpolation=None, strict=True, delimiters=("=",))
    parser.optionxform = str  # type: ignore[assignment,method-assign]
    parser.read_string(path.read_text(), source=str(path))
    unknown = set(parser.sections()) - UNIT_SECTIONS
    assert not unknown, f"{path.name}: sections {sorted(unknown)}"
    assert parser.has_section("Unit"), f"{path.name}: no [Unit]"
    assert parser.get("Unit", "Description", fallback=""), f"{path.name}: no Description"
    return parser


def nginx_problems(text: str) -> list[str]:
    """Unbalanced braces, a directive without `;`, a location that passes git to CGI without
    authentication, or a plain-HTTP server that does more than redirect and answer ACME."""
    problems = []
    depth = 0
    servers: list[list[str]] = []
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("server {"):
            servers.append([])
        if servers:
            servers[-1].append(line)
        depth += line.count("{") - line.count("}")
        if depth < 0:
            problems.append(f"line {number}: a `}}` closes nothing")
        if not line.endswith(("{", "}", ";")):
            problems.append(f"line {number}: no `;`: {line!r}")
    if depth:
        problems.append(f"{depth} braces left open")
    for server in servers:
        body = "\n".join(server)
        plain = re.search(r"listen\s+(?:\[::\]:)?80\b", body) is not None
        if plain and ("fastcgi_pass" in body or "root /var/cache/swarmops-server/www" in body):
            problems.append("the plain-HTTP server reaches a repository or the page")
        if "fastcgi_pass" in body and "auth_basic_user_file" not in body:
            problems.append("a server passes git to CGI without authentication")
        if "fastcgi_pass" in body and "ssl_certificate" not in body:
            problems.append("a server passes git to CGI without TLS")
    return problems


def run_kit(scratch: Path, *arguments: str, stdin: str = "",
            settings: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    """`so-kit` with a scratch settings file whose paths are under `scratch`."""
    conf = scratch / "kit.conf"
    values = {
        "root": str(scratch / "srv"),
        "swarmops": str(scratch / "swarmops"),
        "host_keys": str(scratch / "keys"),
        "web_root": str(scratch / "www"),
        "authorized_keys": str(scratch / "authorized_keys"),
    }
    values.update(settings or {})
    conf.write_text("".join(f"{key}={value}\n" for key, value in values.items()))
    environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "SO_KIT_CONF": str(conf)}
    return subprocess.run(["sh", str(KIT / SO_KIT), *arguments], input=stdin, text=True,
                          capture_output=True, env=environment, check=False)


def fake_swarmops(scratch: Path) -> Path:
    """A `swarmops` that answers `--version` and makes a store's directory on `init`."""
    path = scratch / "swarmops"
    path.write_text("#!/bin/sh\n"
                    "case \"$1\" in\n"
                    "--version) echo 'swarmops 9.9.9 (0123456789ab 2026-11-12)' ;;\n"
                    "init) mkdir -p \"$2\" && echo init > \"$2/FORMAT\" ;;\n"
                    "*) exit 64 ;;\n"
                    "esac\n")
    path.chmod(0o755)
    return path


def key_pair(directory: Path, kind: str, comment: str) -> Path:
    """A fresh key pair of `kind` in `directory`; its public half's path."""
    directory.mkdir(parents=True, exist_ok=True)
    private = directory / f"ssh_host_{kind}_key"
    subprocess.run(["ssh-keygen", "-q", "-t", kind, "-N", "", "-C", comment, "-f", str(private)],
                   check=True, capture_output=True)
    return private.with_suffix(".pub")


def fingerprint(public: Path) -> str:
    """ssh-keygen's SHA256 fingerprint of a public key."""
    out = subprocess.run(["ssh-keygen", "-l", "-E", "sha256", "-f", str(public)],
                         check=True, capture_output=True, text=True).stdout
    return out.split()[1]


def planted(scratch: Path, name: str, change: Callable[[str], str]) -> Path:
    """A copy of the kit with `change` applied to one file."""
    copy = scratch / "kit"
    shutil.copytree(KIT, copy)
    target = copy / name
    target.write_text(change(target.read_text()))
    return copy


# The cases.

def every_kit_file_parses(_: Path) -> None:
    """Every manifest line parses and every own file is installed by one; units read as INI;
    shell scripts pass `sh -n`; the nginx site balances."""
    installs, _uses = entries(KIT)
    files = own_files(KIT)
    assert files, "the kit has no file"
    missing = [name for name in files if name not in installs]
    assert not missing, f"no manifest installs {missing}"
    gone = [name for name in installs if not (KIT / name).is_file()]
    assert not gone, f"the manifest installs files the kit lacks: {gone}"
    for name, (destination, mode) in installs.items():
        assert destination.startswith("/"), f"{name}: destination {destination!r}"
        assert re.fullmatch(r"0[0-7]{3}", mode), f"{name}: mode {mode!r}"
    for name in files:
        path = KIT / name
        if name.startswith("systemd/"):
            unit(path)
        if path.read_text().startswith("#!/bin/sh"):
            subprocess.run(["sh", "-n", str(path)], check=True, capture_output=True)
            assert installs[name][1] == "0755", f"{name}: a script is not installed 0755"
    assert tmpfiles_paths(KIT), "no tmpfiles line"
    problems = nginx_problems((KIT / NGINX_SITE).read_text())
    assert not problems, f"nginx site: {problems}"
    site = (KIT / NGINX_SITE).read_text()
    for label, change in (
        ("no authentication", lambda s: s.replace("auth_basic_user_file", "# off", 1)),
        ("git over plain HTTP",
         lambda s: s.replace("return 301", "fastcgi_pass x;\n return 301", 1)),
        ("an open brace", lambda s: s + "server {\n"),
    ):
        assert nginx_problems(change(site)), f"nginx site: planted {label} not found"


def units_name_only_listed_paths(scratch: Path) -> None:
    """Every absolute path a kit file names is installed, made or used per a manifest; a planted
    path is found."""
    found = unlisted_paths(KIT)
    assert not found, f"paths no manifest lists: {found}"
    copy = planted(scratch, "systemd/swarmops-fcgiwrap.service",
                   lambda text: text.replace("ReadWritePaths=",
                                             "ReadWritePaths=/opt/elsewhere ", 1))
    found = unlisted_paths(copy)
    assert any("/opt/elsewhere" in line for line in found), f"planted path not found: {found}"


def units_are_confined(_: Path) -> None:
    """The units run as the serving account, never root, with the system read-only."""
    for name in own_files(KIT):
        if not name.endswith(".service"):
            continue
        service = unit(KIT / name)
        assert service.get("Service", "User", fallback="") == ACCOUNT, f"{name}: User"
        for key, value in (("NoNewPrivileges", "yes"), ("ProtectSystem", "strict"),
                           ("ProtectHome", "yes"), ("PrivateTmp", "yes")):
            assert service.get("Service", key, fallback="") == value, f"{name}: {key}"
    timer = unit(KIT / "systemd/swarmops-server-page.timer")
    assert timer.get("Install", "WantedBy", fallback="") == "timers.target", "timer: WantedBy"


def forced_command_carries_restrict(scratch: Path) -> None:
    """`so-kit key-line` prints a key line whose options are the forced `serve` command with the
    caller's identity and `restrict`; a key with options, a private key, two keys and a bad
    identity are refused, and `add-key` refuses a key twice."""
    public = key_pair(scratch / "client", "ed25519", "ada@laptop")
    blob = public.read_text().split()
    done = run_kit(scratch, "key-line", "person/ada", str(public))
    assert done.returncode == 0, f"key-line: {done.returncode}: {done.stderr}"
    line = done.stdout.rstrip("\n")
    command = f'command="{scratch / "swarmops"} serve --root {scratch / "srv"} --as person/ada"'
    assert line == f"{command},restrict {blob[0]} {blob[1]} person/ada", line
    # The options field ends where the key's type starts; outside the quoted command, the
    # options are comma-separated words.
    options = line[:line.index(f" {blob[0]} ")]
    assert "restrict" in re.sub(r'"[^"]*"', "", options).split(","), options

    refused = [
        ("options", ["key-line", "person/ada", "-"], f'from="10.0.0.1" {blob[0]} {blob[1]}\n'),
        ("private", ["key-line", "person/ada", str(public.with_suffix(""))], ""),
        ("two keys", ["key-line", "person/ada", "-"], public.read_text() * 2),
        ("quote", ["key-line", 'person/a"da', str(public)], ""),
        ("space", ["key-line", "person/a da", str(public)], ""),
        ("dot", ["key-line", "person/.ada", str(public)], ""),
        ("family", ["key-line", "Person/ada", str(public)], ""),
        ("unknown type", ["key-line", "person/ada", "-"], f"ssh-dss {blob[1]}\n"),
    ]
    for label, arguments, stdin in refused:
        done = run_kit(scratch, *arguments, stdin=stdin)
        assert done.returncode == 2 and done.stdout == "", f"{label}: {done}"
        assert done.stderr.startswith("so-kit: "), f"{label}: {done.stderr!r}"

    keys = scratch / "authorized_keys"
    keys.write_text("")
    done = run_kit(scratch, "add-key", "person/ada", str(public))
    assert done.returncode == 0, f"add-key: {done.stderr}"
    assert keys.read_text() == line + "\n", keys.read_text()
    done = run_kit(scratch, "add-key", "person/bob", str(public))
    assert done.returncode == 2 and "already" in done.stderr, f"add-key twice: {done}"
    assert keys.read_text() == line + "\n", "a refused add-key wrote"


def sshd_dropin_accepts_git_protocol_only(scratch: Path) -> None:
    """The drop-in's Match block takes GIT_PROTOCOL alone from the client, keys alone, no
    forwarding, tty or rc; planted extras are found; sshd -t reads it where installed."""
    text = (KIT / SSHD_DROPIN).read_text()
    assert not sshd_problems(text), sshd_problems(text)
    for label, change in (
        ("a second variable", lambda t: t.replace("AcceptEnv GIT_PROTOCOL",
                                                  "AcceptEnv GIT_PROTOCOL LANG")),
        ("passwords", lambda t: t.replace("PasswordAuthentication no",
                                          "PasswordAuthentication yes")),
        ("environment", lambda t: t.replace("Match all", "    PermitUserEnvironment yes\n"
                                                         "Match all")),
    ):
        assert sshd_problems(change(text)), f"planted {label} not found"

    sshd = shutil.which("sshd") or ("/usr/sbin/sshd" if Path("/usr/sbin/sshd").exists() else "")
    if not sshd:
        SKIPPED.append("sshd -t (no sshd)")
        return
    host_key = key_pair(scratch / "host", "ed25519", "host").with_suffix("")
    config = scratch / "sshd_config"
    config.write_text(f"HostKey {host_key}\nInclude {KIT / SSHD_DROPIN}\n")
    done = subprocess.run([sshd, "-t", "-f", str(config)], capture_output=True, text=True,
                          check=False)
    assert done.returncode == 0, f"sshd -t: {done.stderr}"

    # sshd's effective settings: the block binds the serving account and no other.
    def effective(user: str) -> str:
        return subprocess.run([sshd, "-T", "-f", str(config), "-C",
                               f"user={user},host=client,addr=192.0.2.1"],
                              capture_output=True, text=True, check=True).stdout
    served = effective(ACCOUNT)
    for line in ("acceptenv GIT_PROTOCOL", "permittty no", "passwordauthentication no",
                 f"authorizedkeysfile {AUTHORIZED_KEYS}"):
        assert f"\n{line}\n" in served, f"sshd -T for {ACCOUNT}: no {line!r}"
    assert "\nacceptenv GIT_PROTOCOL\n" not in effective("other"), "the block binds others"


def kit_holds_no_host_name(scratch: Path) -> None:
    """No kit file names a host; a planted swarmops.cc is found."""
    found = host_names(KIT)
    assert not found, f"host names in the kit: {found}"
    copy = planted(scratch, "kit.conf", lambda text: text + f"# {HOSTED_NAME}\n")
    found = host_names(copy)
    assert any(HOSTED_NAME in line for line in found), f"planted {HOSTED_NAME}: {found}"
    copy = scratch / "second"
    shutil.copytree(KIT, copy)
    (copy / NGINX_SITE).write_text((copy / NGINX_SITE).read_text().replace(
        "server_name _;", f"server_name hub.{HOSTED_NAME.upper()};", 1))
    assert host_names(copy), "a planted server_name is not found"


def root_page_shows_host_key_fingerprints(scratch: Path) -> None:
    """`so-kit page` writes one page with each host key's fingerprint, the release and its
    digest, and the operator's texts when the settings name them, escaped."""
    keys = scratch / "keys"
    publics = [key_pair(keys, kind, "root@host") for kind in KEY_TYPES]
    binary = fake_swarmops(scratch)
    (scratch / "www").mkdir()
    terms = scratch / "terms.txt"
    terms.write_text("Keep another copy <b>&</b> read this.\n")
    done = run_kit(scratch, "page", settings={"terms": str(terms)})
    assert done.returncode == 0, f"page: {done.stderr}"
    page = (scratch / "www" / "index.html").read_text()
    for public in publics:
        assert fingerprint(public) in page, f"{public.name}: fingerprint missing"
    assert "swarmops 9.9.9 (0123456789ab 2026-11-12)" in page, "release missing"
    digest = subprocess.run(["sha256sum", str(binary)], check=True, capture_output=True,
                            text=True).stdout.split()[0]
    assert digest in page, "the binary's digest is missing"
    assert "Keep another copy &lt;b&gt;&amp;&lt;/b&gt; read this." in page, "terms not escaped"
    assert "Privacy" not in page, "a text the settings do not name is shown"
    assert str(scratch) not in page, "the page names a host path"
    assert not list((scratch / "www").glob("*.tmp*")), "a temporary file is left"

    done = run_kit(scratch, "page", settings={"host_keys": str(scratch / "none")})
    assert done.returncode == 2 and "no host key" in done.stderr, f"no keys: {done}"
    before = (scratch / "www" / "index.html").read_bytes()
    done = run_kit(scratch, "page", settings={"swarmops": str(scratch / "none")})
    assert done.returncode == 2 and "swarmops" in done.stderr, f"no binary: {done}"
    assert (scratch / "www" / "index.html").read_bytes() == before, "a refused page wrote"


def create_store_writes_access(scratch: Path) -> None:
    """`so-kit create-store` makes a store under the root and an access.toml granting the named
    identity read and land onto main; it refuses a bad name, a store that exists, and a caller
    that is not the root's owner."""
    fake_swarmops(scratch)
    (scratch / "srv").mkdir(mode=0o700)
    done = run_kit(scratch, "create-store", "acme", "person/ada")
    assert done.returncode == 0, f"create-store: {done.stderr}"
    access = (scratch / "srv" / "acme" / "access.toml").read_text()
    assert "version = 1" in access and 'who = ["person/ada"]' in access, access
    assert 'land = ["main"]' in access and "read = true" in access, access
    assert (scratch / "srv" / "acme" / "FORMAT").exists(), "init did not run"
    for label, name in (("exists", "acme"), ("upper", "Acme"), ("slash", "a/b"),
                        ("dot", ".acme"), ("long", "a" * 64)):
        done = run_kit(scratch, "create-store", name, "person/ada")
        assert done.returncode == 2, f"{label}: {done}"
    if os.geteuid() != 0:
        done = run_kit(scratch, "create-store", "beta", "person/ada",
                       settings={"root": "/"})
        assert done.returncode == 2 and "owner" in done.stderr, f"owner: {done}"


def settings_refuse_unknown_keys(scratch: Path) -> None:
    """A settings file with a misspelt key is refused, naming the key."""
    done = run_kit(scratch, "page", settings={"web_rot": str(scratch)})
    assert done.returncode == 2 and "web_rot" in done.stderr, f"misspelt: {done}"


SKIPPED: list[str] = []
CASES: tuple[tuple[str, Callable[[Path], None]], ...] = (
    ("every kit file parses", every_kit_file_parses),
    ("units name only listed paths", units_name_only_listed_paths),
    ("units are confined", units_are_confined),
    ("the forced command carries restrict", forced_command_carries_restrict),
    ("the sshd drop-in accepts GIT_PROTOCOL only", sshd_dropin_accepts_git_protocol_only),
    ("the kit holds no host name", kit_holds_no_host_name),
    ("the root page shows the host key fingerprints", root_page_shows_host_key_fingerprints),
    ("create-store writes access.toml", create_store_writes_access),
    ("settings refuse unknown keys", settings_refuse_unknown_keys),
)


def main() -> int:
    failures = []
    for label, case in CASES:
        with tempfile.TemporaryDirectory(prefix="test-server-kit-") as scratch:
            try:
                case(Path(scratch))
            except Exception as error:  # a case's crash is its failure, not the run's
                failures.append(f"{label}: {type(error).__name__}: {error}")

    print(f"test-server-kit: {len(CASES) - len(failures)} of {len(CASES)} cases passed")
    if SKIPPED:
        print(f"test-server-kit: skipped: {'; '.join(SKIPPED)}")
    for failure in failures:
        print(f"test-server-kit: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
