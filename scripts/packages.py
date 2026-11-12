# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Makes the .deb and .rpm packages of a SwarmOps release archive, and installs them in containers.

    python3 -I scripts/packages.py build <archive> <out dir> [--kit <dir>]
    python3 -I scripts/packages.py smoke <version> <package>...

`build` reads `swarmops-<version>-<target>.tar.gz`, as the release workflow packs it, and writes
`swarmops_<version>_<arch>.deb` and `swarmops-<version>-1.<arch>.rpm` into the out directory from
the archive's files alone: `swarmops` and `so` to /usr/bin, LICENSE, THIRD-PARTY-LICENSES,
README.md and CHANGELOG.md to /usr/share/doc/swarmops, and in the .deb a copyright file made of
LICENSE and THIRD-PARTY-LICENSES. The binaries are the archive's, byte for byte. The same archive
gives the same packages, byte for byte: entries sorted, owned by root, dated at the archive's own
date (the tagged commit's), with fixed modes. The dependencies are the binaries' own: each shared
library their ELF dynamic section needs, and for glibc the newest version they need that is not
weak. A library the script does not know, a binary of another machine, an entry the archive
should not hold or a version the formats cannot carry fails the run before anything is written.

Beside them it writes the server kit's packages, `swarmops-server_<version>_<arch>.deb` and
`swarmops-server-<version>-1.<arch>.rpm`, dated and ordered alike: every file each `MANIFEST`
of the kit directory (`packaging/server/`, its `backup/`, `status/` and any later subtree) names,
at the mode it names, the files under /etc as configuration files, and a post-install script made
from the kit's sysusers.d and tmpfiles.d files that makes the serving account and its directories
and starts nothing. They depend on `swarmops` of the same version and target; removing them
leaves /srv/so as it is. A kit file no MANIFEST names, or a line the script cannot read, fails
the run before anything is written.

    archive                              .deb (dpkg)                .rpm (rpm 4)
    swarmops-X-<target>/so        -->    ar: debian-binary,         lead, signature header,
    swarmops-X-<target>/swarmops         control.tar.xz,            header, xz cpio payload
    swarmops-X-<target>/LICENSE ...      data.tar.xz

`smoke` installs the packages of one target in containers pinned by digest: the .deb with dpkg
in Debian 12, the .rpm with rpm in AlmaLinux 9 (glibc 2.34, the oldest the binaries run on). It
checks what dpkg and rpm record, that `so --version` and `swarmops --version` print the version,
that the packages verify, that `rpm --verify` sees a byte appended to an installed binary, and
that removing them removes their files. A target this host does not run is installed for its own
architecture and run under QEMU's user-mode emulator: the .deb in Debian with that architecture's
C library (multiarch), the .rpm into an AlmaLinux root of that architecture, so that rpm checks
its dependencies against that architecture's glibc. On a target this host runs, it then installs
the server packages beside the client's with apt-get and dnf, which fetch their dependencies:
the accounts, the directories and the units must exist, `so-kit check` must pass, and removing
them must remove their files and keep a file in /srv/so. On an emulated target the server
packages, which hold no binary, are not installed. Needs docker; an emulated target and the
server packages download packages from Debian's and AlmaLinux's mirrors. Exit 1 on a failure.
"""

import argparse
import enum
import hashlib
import io
import lzma
import math
import platform
import re
import secrets
import shlex
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = "swarmops"
BINARIES = ("so", "swarmops")
DOCS = ("CHANGELOG.md", "LICENSE", "README.md", "THIRD-PARTY-LICENSES")
# The docs that are licenses: rpm installs them even with --excludedocs.
LICENSES = ("LICENSE", "THIRD-PARTY-LICENSES")
BIN_DIR = "/usr/bin"
DOC_DIR = f"/usr/share/doc/{PACKAGE}"
# The .deb's copyright file, in Debian's machine-readable format: an image that leaves out
# /usr/share/doc but keeps every */copyright (debian:12-slim) keeps the license texts.
DEB_COPYRIGHT = f"{DOC_DIR}/copyright"
COPYRIGHT_FORMAT = "https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
COPYRIGHT_COMMENT = (
    "swarmops and so are built from SwarmOps' own crates, under the license below, and from",
    "the third-party crates that THIRD-PARTY-LICENSES lists, which follows in full.",
)

MAINTAINER = "SwarmOps Maintainers <maintainers@swarmops.cc>"
HOMEPAGE = "https://swarmops.cc"
SOURCE = "https://github.com/KeiaiLab/SwarmOps"
SUMMARY = "Source control for a swarm of coding agents"
DESCRIPTION = (
    "SwarmOps is source control for a swarm of coding agents: many agent processes on one host "
    "commit into one embedded store at once, and every command answers in one JSON envelope. "
    "It moves the work to and from git, and serves stock git clients.\n"
    "This package installs swarmops and so, its short form."
)
# SwarmOps' own license; the crates compiled into the binaries add theirs (THIRD-PARTY-LICENSES).
OWN_LICENSE = "MIT"
RPM_RELEASE = "1"
# rpm's build host, the same for every build.
BUILD_HOST = "reproducible"

MODE_DIR = 0o755
MODE_BIN = 0o755
MODE_DOC = 0o644
S_IFDIR = 0o040000
S_IFREG = 0o100000

# A release version, X.Y.Z with an optional pre-release of dot-separated alphanumeric parts. Both
# formats order `~` before anything, so 0.2.0-rc.1 becomes 0.2.0~rc.1, older than 0.2.0.
VERSION = re.compile(r"(\d+\.\d+\.\d+)(?:-([0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*))?")
ARCHIVE = re.compile(r"swarmops-(?P<version>.+)-(?P<target>[a-z0-9_]+-unknown-linux-gnu)\.tar\.gz")

XZ_PRESET = 6

# The containers `smoke` installs in, by digest: moved by hand, as release.yml's build image is.
DEBIAN_IMAGE = "debian:12@sha256:d74df7598108a173a621eda4d58a2adbf850b2d9a9d81bc2cca1a6c21174a880"
ALMALINUX_IMAGE = (
    "almalinux:9@sha256:245f8aab832fe800aa42bdab5fad75ebbd8e0b4c9f24820a83c638b177857ae2"
)
ALMALINUX_RELEASE = "9"
# How long one command in a container may take: an emulated target's downloads are the slowest.
COMMAND_TIMEOUT = 900


@dataclass(frozen=True)
class Target:
    """A release target, as each format and tool names its architecture."""

    triple: str
    machine: int  # the ELF header's e_machine
    deb: str  # dpkg's architecture
    rpm: str  # rpm's architecture, and `platform.machine()` on a host of it
    isa: str  # rpm's %{_isa}, which qualifies the arch-specific Provides
    lead: int  # the rpm lead's archnum (rpmrc's arch_canon)


TARGETS = {
    t.triple: t
    for t in (
        Target("x86_64-unknown-linux-gnu", 62, "amd64", "x86_64", "x86-64", 1),
        Target("aarch64-unknown-linux-gnu", 183, "arm64", "aarch64", "aarch-64", 19),
    )
}

# The shared libraries a binary may need, each with the Debian package that has it (the .rpm
# requires the library itself). glibc's are libc6, which the newest glibc version needed versions;
# any other library fails the build until it is added here.
LIBRARIES = {
    "libc.so.6": "libc6",
    "libm.so.6": "libc6",
    "libdl.so.2": "libc6",
    "libpthread.so.0": "libc6",
    "librt.so.1": "libc6",
    "ld-linux-x86-64.so.2": "libc6",
    "ld-linux-aarch64.so.1": "libc6",
    "libgcc_s.so.1": "libgcc-s1",
}
GLIBC_PACKAGE = "libc6"
GLIBC_VERSION = re.compile(r"GLIBC_(\d+(?:\.\d+)*)")


class Failure(Exception):
    """A refusal: printed as an `::error::` line, exit 1."""


# ELF (the generic ABI): the header, program headers, dynamic section and version needs.
ELF_MAGIC = b"\x7fELF"
ELFCLASS64 = 2
ELFDATA2LSB = 1
PT_LOAD = 1
PT_DYNAMIC = 2
DT_NULL = 0
DT_NEEDED = 1
DT_STRTAB = 5
DT_VERNEED = 0x6FFFFFFE
DT_VERNEEDNUM = 0x6FFFFFFF
VER_FLG_WEAK = 0x2


@dataclass(frozen=True)
class Elf:
    """What packaging reads of a binary: its machine and what it needs to run."""

    machine: int
    needed: tuple[str, ...]  # DT_NEEDED: the shared libraries
    versions: tuple[tuple[str, str], ...]  # (library, version) needs not flagged weak


def read_elf(name: str, data: bytes) -> Elf:
    """Reads a 64-bit little-endian ELF executable's machine, libraries and version needs."""
    if data[:4] != ELF_MAGIC or data[4:6] != bytes([ELFCLASS64, ELFDATA2LSB]):
        raise Failure(f"{name} is not a 64-bit little-endian ELF file")
    try:
        return parse_elf(name, data)
    except (struct.error, IndexError, ValueError, UnicodeDecodeError) as error:
        raise Failure(f"{name} is not an ELF file this script reads: {error}") from error


def parse_elf(name: str, data: bytes) -> Elf:
    """The body of `read_elf`: any out-of-range read raises."""
    machine, phoff, phentsize, phnum = (
        struct.unpack_from("<H", data, 18)[0],
        struct.unpack_from("<Q", data, 32)[0],
        struct.unpack_from("<H", data, 54)[0],
        struct.unpack_from("<H", data, 56)[0],
    )
    # p_type, p_flags, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_align
    headers = [struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize) for i in range(phnum)]
    dynamic = next((h for h in headers if h[0] == PT_DYNAMIC), None)
    if dynamic is None:
        raise Failure(f"{name} has no dynamic section: a static binary needs no package of glibc")

    # 1. The dynamic section's entries, up to DT_NULL.
    entries = []
    for at in range(dynamic[2], dynamic[2] + dynamic[5], 16):
        tag, value = struct.unpack_from("<qQ", data, at)
        if tag == DT_NULL:
            break
        entries.append((tag, value))
    values = dict(entries)

    # 2. Addresses to file offsets, through the loadable segment that maps each.
    def offset(address: int) -> int:
        for kind, _, at, vaddr, _, filesz, _, _ in headers:
            if kind == PT_LOAD and vaddr <= address < vaddr + filesz:
                return address - vaddr + at
        raise ValueError(f"no loadable segment maps {address:#x}")

    if DT_STRTAB not in values:
        raise Failure(f"{name} has no string table (DT_STRTAB) in its dynamic section")
    strings = offset(values[DT_STRTAB])

    def string(at: int) -> str:
        start = strings + at
        return data[start : data.index(b"\0", start)].decode()

    needed = tuple(string(value) for tag, value in entries if tag == DT_NEEDED)

    # 3. The version needs: Elf64_Verneed records, each with its Elf64_Vernaux list.
    versions = []
    record = offset(values[DT_VERNEED]) if DT_VERNEED in values else 0
    for _ in range(values.get(DT_VERNEEDNUM, 0)):
        _, count, file, aux, following = struct.unpack_from("<HHIII", data, record)
        at = record + aux
        for _ in range(count):
            _, flags, _, version, following_aux = struct.unpack_from("<IHHII", data, at)
            if not flags & VER_FLG_WEAK:
                versions.append((string(file), string(version)))
            at += following_aux
        record += following
    return Elf(machine, needed, tuple(versions))


def version_key(version: str) -> tuple[int, ...]:
    """`2.34` as (2, 34), to compare versions by number."""
    return tuple(int(part) for part in version.split("."))


# Packaging input: the release archive, read and checked.


@dataclass(frozen=True)
class File:
    """A file the packages install: its absolute path, mode and bytes."""

    path: str
    mode: int
    data: bytes


@dataclass(frozen=True)
class Release:
    """One release archive's packaging input."""

    version: str  # the release's version, as the archive names it
    package_version: str  # the same as dpkg and rpm order it
    target: Target
    mtime: int  # every entry's date: the tagged commit's
    files: tuple[File, ...]  # sorted by path
    elves: tuple[Elf, ...]  # the binaries'
    license: str  # the SPDX expression of what the binaries are under


def package_version(version: str) -> str:
    """The version dpkg and rpm read: X.Y.Z, or X.Y.Z~pre for X.Y.Z-pre."""
    match = VERSION.fullmatch(version)
    if match is None:
        raise Failure(
            f"'{version}' is not X.Y.Z or X.Y.Z-pre (dot-separated letters and digits), the"
            " versions the packages carry"
        )
    release, pre = match.groups()
    return f"{release}~{pre}" if pre else release


def read_archive(path: Path) -> Release:
    """Reads and checks a release archive: its name, seven entries, their date, the binaries."""
    match = ARCHIVE.fullmatch(path.name)
    if match is None or match["target"] not in TARGETS:
        raise Failure(f"{path.name} is not named swarmops-<version>-<target>.tar.gz for a target"
                      f" of {', '.join(TARGETS)}")
    version, target = match["version"], TARGETS[match["target"]]
    packaged = package_version(version)
    top = path.name.removesuffix(".tar.gz")

    # 1. Exactly the directory, both binaries and the four docs, all dated alike.
    with tarfile.open(path, "r:gz") as tar:
        members = tar.getmembers()
        contents = {}
        for member in members:
            stream = tar.extractfile(member)
            contents[member.name.rstrip("/")] = (member, stream.read() if stream else None)
    expected = {top} | {f"{top}/{name}" for name in BINARIES + DOCS}
    if set(contents) != expected or len(members) != len(expected):
        raise Failure(f"{path.name} does not hold exactly {top}/, {', '.join(BINARIES + DOCS)}:"
                      f" it holds {', '.join(sorted(contents))}")
    if not contents[top][0].isdir() or any(not contents[n][0].isfile() for n in expected - {top}):
        raise Failure(f"{path.name}: {top} is not a directory of regular files")
    dates = {member.mtime for member, _ in contents.values()}
    if len(dates) != 1:
        raise Failure(f"{path.name}: its entries are not all dated alike: {sorted(dates)}")

    # 2. The files, at the paths and modes they install with.
    files = [File(f"{BIN_DIR}/{name}", MODE_BIN, contents[f"{top}/{name}"][1]) for name in BINARIES]
    files += [File(f"{DOC_DIR}/{name}", MODE_DOC, contents[f"{top}/{name}"][1]) for name in DOCS]

    # 3. Binaries of the target's machine.
    elves = tuple(read_elf(name, contents[f"{top}/{name}"][1]) for name in BINARIES)
    for name, elf in zip(BINARIES, elves):
        if elf.machine != target.machine:
            raise Failure(f"{name} is for ELF machine {elf.machine}, not {target.triple}'s"
                          f" ({target.machine})")
    notices = contents[f"{top}/THIRD-PARTY-LICENSES"][1].decode()
    return Release(version, packaged, target, dates.pop(),
                   tuple(sorted(files, key=lambda f: f.path)), elves,
                   license_expression(notices, target))


# THIRD-PARTY-LICENSES lists each crate as `<name> <version>[ (build)][ (<target> only)]:
# <expression>; used under <license> [n]...`; a (build) crate is not in the binaries.
CRATE_LINE = re.compile(r"\S+ \S+(?P<marks>(?: \([^)]+\))*): [^;]+; used under (?P<used>.+?) \[")


def license_expression(notices: str, target: Target) -> str:
    """The licenses the binaries are under: SwarmOps' and those of the crates compiled into them."""
    licenses = {OWN_LICENSE}
    crates = 0
    for line in notices.splitlines():
        match = CRATE_LINE.match(line)
        if match is None:
            continue
        crates += 1
        marks = match["marks"]
        if "(build)" in marks or (" only)" in marks and f"({target.triple} only)" not in marks):
            continue
        licenses.update(match["used"].split(" AND "))
    if crates == 0:
        raise Failure("THIRD-PARTY-LICENSES lists no crate this script can read")
    return " AND ".join(sorted(licenses))


Requirement = tuple[str, int, str]  # rpm's (name, RPMSENSE_* flags, version)


@dataclass(frozen=True)
class Spec:
    """One package of a release: what both formats install and declare."""

    name: str
    summary: str
    description: str
    license: str  # the .rpm's License: an SPDX expression
    files: tuple[File, ...]  # both formats'
    deb_files: tuple[File, ...]  # the .deb's alone: its copyright file
    rpm_files: tuple[File, ...]  # the .rpm's alone
    deb_depends: str
    deb_recommends: str  # "" for none
    rpm_requires: tuple[Requirement, ...]  # sorted, rpm's features among them
    rpm_recommends: tuple[Requirement, ...]
    rpm_flags: dict[str, int]  # RPMFILE_* flags by path
    rpm_dirs: tuple[str, ...]  # the directories the .rpm owns
    conffiles: tuple[str, ...]  # configuration files: the operator's once installed
    postinst: str  # the post-install script's body, "" for none


def dependencies(release: Release) -> tuple[list[str], list[tuple[str, str]]]:
    """The libraries both binaries need, and their (library, version) needs not flagged weak."""
    needed = sorted({lib for elf in release.elves for lib in elf.needed})
    unknown = [lib for lib in needed if lib not in LIBRARIES]
    if unknown:
        raise Failure(f"the binaries need {', '.join(unknown)}, which packages.py maps to no"
                      " package: add it to LIBRARIES")
    versions = sorted({need for elf in release.elves for need in elf.versions})
    return needed, versions


def newest_glibc(versions: list[tuple[str, str]]) -> str:
    """The newest GLIBC_x.y that any binary needs and is not weak (2.34 for GLIBC_2.34)."""
    found = [m[1] for _, v in versions if (m := GLIBC_VERSION.fullmatch(v))]
    if not found:
        raise Failure("the binaries need no glibc version: is glibc not among their libraries?")
    return max(found, key=version_key)


# The .deb: an ar archive of debian-binary, control.tar.xz and data.tar.xz (deb(5)).


def tar_xz(entries: list[tuple[str, int, bytes | None]], mtime: int) -> bytes:
    """A GNU tar of (name, mode, bytes or None for a directory), in order, owned by root, xz'd."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for name, mode, data in entries:
            info = tarfile.TarInfo(name)
            info.mode, info.mtime = mode, mtime
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            if data is None:
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
                continue
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return lzma.compress(raw.getvalue(), format=lzma.FORMAT_XZ, preset=XZ_PRESET)


def ar_archive(members: list[tuple[str, bytes]], mtime: int) -> bytes:
    """A common-format ar archive of (name, bytes), each owned by root, mode 0644."""
    out = bytearray(b"!<arch>\n")
    for name, data in members:
        out += f"{name:<16}{mtime:<12}{0:<6}{0:<6}{0o100644:<8o}{len(data):<10}`\n".encode()
        out += data
        out += b"\n" * (len(data) % 2)
    return bytes(out)


def parents(paths: list[str]) -> list[str]:
    """Every directory above the paths, as absolute paths, parents first: /usr, /usr/bin, …"""
    found = set()
    for path in paths:
        parts = path.strip("/").split("/")[:-1]
        found.update("/" + "/".join(parts[:n]) for n in range(1, len(parts) + 1))
    return sorted(found)


def deb_control(release: Release, spec: Spec, size: int) -> bytes:
    """The control file: dpkg's fields, the description's lines indented by one space."""
    lines = [
        f"Package: {spec.name}",
        f"Version: {release.package_version}",
        f"Architecture: {release.target.deb}",
        f"Maintainer: {MAINTAINER}",
        f"Installed-Size: {size}",
        f"Depends: {spec.deb_depends}",
    ]
    if spec.deb_recommends:
        lines.append(f"Recommends: {spec.deb_recommends}")
    lines += [
        "Section: vcs",
        "Priority: optional",
        f"Homepage: {HOMEPAGE}",
        f"Description: {spec.summary}",
    ]
    for paragraph in spec.description.split("\n"):
        lines += [" " + line for line in wrap(paragraph)] + [" ."]
    return ("\n".join(lines[:-1]) + "\n").encode()


def wrap(text: str, width: int = 78) -> list[str]:
    """`text` in lines of at most `width` characters, broken at spaces."""
    lines, line = [], ""
    for word in text.split():
        if line and len(line) + 1 + len(word) > width:
            lines.append(line)
            line = word
            continue
        line = f"{line} {word}" if line else word
    return lines + [line]


def folded(text: str) -> list[str]:
    """`text` as a field's continuation lines: each indented by one space, an empty one ` .`."""
    return [f" {line.rstrip()}" if line.strip() else " ." for line in text.rstrip().splitlines()]


def deb_copyright(release: Release) -> bytes:
    """The copyright file: LICENSE as every file's license, THIRD-PARTY-LICENSES as a comment."""
    texts = {f.path: f.data for f in release.files}
    try:
        license_text = texts[f"{DOC_DIR}/LICENSE"].decode()
        notices = texts[f"{DOC_DIR}/THIRD-PARTY-LICENSES"].decode()
    except UnicodeDecodeError as error:
        raise Failure(f"LICENSE or THIRD-PARTY-LICENSES is not UTF-8: {error}") from error
    holders = [line.strip() for line in license_text.splitlines() if line.startswith("Copyright")]
    if not holders:
        raise Failure("LICENSE has no Copyright line for the .deb's copyright file")

    # 1. The header: the upstream, then the third-party notices as its comment.
    lines = [
        f"Format: {COPYRIGHT_FORMAT}",
        f"Upstream-Name: {PACKAGE}",
        f"Upstream-Contact: {MAINTAINER}",
        f"Source: {SOURCE}",
        f"Comment: {COPYRIGHT_COMMENT[0]}",
    ]
    lines += [f" {line}" for line in COPYRIGHT_COMMENT[1:]] + [" ."] + folded(notices)

    # 2. Every file: SwarmOps' copyright holders and license, its text in full.
    lines += ["", "Files: *", f"Copyright: {holders[0]}"] + [f" {h}" for h in holders[1:]]
    lines += [f"License: {OWN_LICENSE}"] + folded(license_text)
    return ("\n".join(lines) + "\n").encode()


def deb_depends(release: Release) -> str:
    """Depends: each library's package, libc6 at the newest glibc the binaries need."""
    needed, versions = dependencies(release)
    glibc = newest_glibc(versions)
    packages = sorted({LIBRARIES[lib] for lib in needed})
    return ", ".join(f"{p} (>= {glibc})" if p == GLIBC_PACKAGE else p for p in packages)


def build_deb(release: Release, spec: Spec) -> bytes:
    """One package of a release archive, as a .deb."""
    files = tuple(sorted(spec.files + spec.deb_files, key=lambda f: f.path))
    directories = parents([f.path for f in files])

    # 1. data.tar.xz: ./ and every directory, then the files, sorted by path.
    entries: list[tuple[str, int, bytes | None]] = [("./", MODE_DIR, None)]
    entries += [(f".{d}/", MODE_DIR, None) for d in directories]
    entries += [(f".{f.path}", f.mode, f.data) for f in files]
    entries.sort(key=lambda e: e[0])
    data = tar_xz(entries, release.mtime)

    # 2. control.tar.xz: control, md5sums (configuration files are dpkg's conffiles, summed at
    # install) and, when the package has them, conffiles and postinst, sorted by name.
    # Installed-Size is in KiB, a directory counting 1.
    size = sum(math.ceil(len(f.data) / 1024) for f in files) + len(directories)
    control = deb_control(release, spec, size)
    md5sums = "".join(f"{hashlib.md5(f.data).hexdigest()}  {f.path[1:]}\n" for f in files
                      if f.path not in spec.conffiles)
    members = [("./", MODE_DIR, None), ("./control", MODE_DOC, control),
               ("./md5sums", MODE_DOC, md5sums.encode())]
    if spec.conffiles:
        conffiles = "".join(f"{path}\n" for path in spec.conffiles)
        members.append(("./conffiles", MODE_DOC, conffiles.encode()))
    if spec.postinst:
        members.append(("./postinst", MODE_BIN, f"{SHEBANG}\n{spec.postinst}".encode()))
    members.sort(key=lambda m: m[0])
    control_tar = tar_xz(members, release.mtime)
    return ar_archive(
        [("debian-binary", b"2.0\n"), ("control.tar.xz", control_tar), ("data.tar.xz", data)],
        release.mtime,
    )


# The .rpm (rpm 4's package format): a lead, a signature header, the header, then the payload, a
# cpio archive (newc) compressed with xz. A header is an index of (tag, type, offset, count)
# entries over a data store, opened by a region tag whose 16-byte trailer ends the store.


class Kind(enum.IntEnum):
    """A header entry's type (rpmTagType)."""

    INT16 = 3
    INT32 = 4
    STRING = 6
    BIN = 7
    STRING_ARRAY = 8
    I18NSTRING = 9


class Sig(enum.IntEnum):
    """The signature header's tags (rpmSigTag): digests only, no signature."""

    HEADERSIGNATURES = 62
    SHA1 = 269
    SHA256 = 273
    SIZE = 1000
    MD5 = 1004
    PAYLOADSIZE = 1007


class Tag(enum.IntEnum):
    """The header's tags (rpmTag) this package carries."""

    HEADERIMMUTABLE = 63
    HEADERI18NTABLE = 100
    NAME = 1000
    VERSION = 1001
    RELEASE = 1002
    SUMMARY = 1004
    DESCRIPTION = 1005
    BUILDTIME = 1006
    BUILDHOST = 1007
    SIZE = 1009
    LICENSE = 1014
    PACKAGER = 1015
    URL = 1020
    OS = 1021
    ARCH = 1022
    FILESIZES = 1028
    FILEMODES = 1030
    FILERDEVS = 1033
    FILEMTIMES = 1034
    FILEDIGESTS = 1035
    FILELINKTOS = 1036
    FILEFLAGS = 1037
    FILEUSERNAME = 1039
    FILEGROUPNAME = 1040
    SOURCERPM = 1044
    FILEVERIFYFLAGS = 1045
    PROVIDENAME = 1047
    REQUIREFLAGS = 1048
    REQUIRENAME = 1049
    REQUIREVERSION = 1050
    FILEDEVICES = 1095
    FILEINODES = 1096
    FILELANGS = 1097
    PROVIDEFLAGS = 1112
    PROVIDEVERSION = 1113
    DIRINDEXES = 1116
    BASENAMES = 1117
    DIRNAMES = 1118
    PAYLOADFORMAT = 1124
    PAYLOADCOMPRESSOR = 1125
    PAYLOADFLAGS = 1126
    POSTIN = 1024
    POSTINPROG = 1086
    FILEDIGESTALGO = 5011
    RECOMMENDNAME = 5046
    RECOMMENDVERSION = 5047
    RECOMMENDFLAGS = 5048
    PAYLOADDIGEST = 5092
    PAYLOADDIGESTALGO = 5093


HEADER_MAGIC = b"\x8e\xad\xe8\x01\x00\x00\x00\x00"
LEAD_MAGIC = b"\xed\xab\xee\xdb"
LEAD_BINARY = 0
LEAD_OS_LINUX = 1
LEAD_HEADER_SIGNATURE = 5  # RPMSIGTYPE_HEADERSIG: a signature header follows the lead
REGION_SIZE = 16
SHA256_ALGO = 8  # PGPHASHALGO_SHA256, for the file and payload digests
# RPMSENSE_* flags of a dependency.
SENSE_LESS = 1 << 1
SENSE_GREATER = 1 << 2
SENSE_EQUAL = 1 << 3
SENSE_INTERP = 1 << 8  # the interpreter of a scriptlet
SENSE_SCRIPT_POST = 1 << 10  # needed by the post-install scriptlet
SENSE_FIND_REQUIRES = 1 << 14  # generated from the binaries, as rpmbuild's ELF dependencies are
SENSE_RPMLIB = 1 << 24
# RPMFILE_* flags of a file.
FILE_CONFIG = 1 << 0
FILE_DOC = 1 << 1
FILE_NOREPLACE = 1 << 4  # an edited configuration file stays; the package's goes beside it
FILE_LICENSE = 1 << 7
VERIFY_ALL = 0xFFFFFFFF  # rpm -V checks every attribute
# The rpm features the package needs, with the rpm version that brought each (as rpmbuild adds).
RPMLIB_FEATURES = (
    ("rpmlib(CompressedFileNames)", "3.0.4-1"),
    ("rpmlib(FileDigests)", "4.6.0-1"),
    ("rpmlib(PayloadFilesHavePrefix)", "4.0-1"),
    ("rpmlib(PayloadIsXz)", "5.2-1"),
)
# Shared-library dependencies are marked so for 64-bit ELF, on every 64-bit architecture.
ELF64_MARK = "(64bit)"
CPIO_MAGIC = "070701"
CPIO_TRAILER = "TRAILER!!!"


def encode(kind: Kind, value) -> tuple[bytes, int, int]:
    """An entry's value in the data store: its bytes, count and alignment."""
    if kind == Kind.INT16:
        return b"".join(struct.pack(">H", v) for v in value), len(value), 2
    if kind == Kind.INT32:
        return b"".join(struct.pack(">I", v) for v in value), len(value), 4
    if kind == Kind.STRING:
        return value.encode() + b"\0", 1, 1
    if kind == Kind.BIN:
        return bytes(value), len(value), 1
    return b"".join(s.encode() + b"\0" for s in value), len(value), 1


def rpm_header(region: int, entries: dict[int, tuple[Kind, object]]) -> bytes:
    """A header: the region entry, then the entries by tag, their data in the same order."""
    index, store = [], bytearray()
    for tag in sorted(entries):
        kind, value = entries[tag]
        data, count, align = encode(kind, value)
        store += b"\0" * (-len(store) % align)
        index.append((tag, kind, len(store), count))
        store += data

    # The region: its entry points at the trailer, which closes the store and counts every entry.
    count = len(index) + 1
    trailer = len(store)
    store += struct.pack(">iiii", region, Kind.BIN, -count * 16, REGION_SIZE)
    index.insert(0, (region, Kind.BIN, trailer, REGION_SIZE))
    head = HEADER_MAGIC + struct.pack(">ii", count, len(store))
    return head + b"".join(struct.pack(">iiii", *entry) for entry in index) + bytes(store)


def cpio_entry(name: str, mode: int, nlink: int, mtime: int, ino: int, data: bytes) -> bytes:
    """One newc cpio entry: header, NUL-ended name and data, each padded to 4 bytes."""
    name_bytes = name.encode() + b"\0"
    fields = (ino, mode, 0, 0, nlink, mtime, len(data), 0, 0, 0, 0, len(name_bytes), 0)
    out = (CPIO_MAGIC + "".join(f"{field:08x}" for field in fields)).encode() + name_bytes
    out += b"\0" * (-len(out) % 4)
    return out + data + b"\0" * (-len(data) % 4)


def build_rpm(release: Release, spec: Spec) -> bytes:
    """One package of a release archive, as an .rpm."""
    target, version = release.target, release.package_version
    evr = f"{version}-{RPM_RELEASE}"

    # 1. The files rpm owns: the package's and its own directories, not /usr/bin or /usr/share/doc.
    owned = sorted([(f.path, S_IFREG | f.mode, f.data) for f in spec.files + spec.rpm_files]
                   + [(d, S_IFDIR | MODE_DIR, None) for d in spec.rpm_dirs])
    dirnames = sorted({path.rsplit("/", 1)[0] + "/" for path, _, _ in owned})
    flags = spec.rpm_flags

    # 2. The payload: a cpio entry per file, in the header's order, then the trailer.
    archive = bytearray()
    for ino, (path, mode, data) in enumerate(owned, start=1):
        nlink = 2 if data is None else 1
        archive += cpio_entry(f".{path}", mode, nlink, release.mtime, ino, data or b"")
    archive += cpio_entry(CPIO_TRAILER, 0, 1, 0, 0, b"")
    payload = lzma.compress(bytes(archive), format=lzma.FORMAT_XZ, preset=XZ_PRESET)

    # 3. Requires as the spec gives them; it provides itself, and itself on the target.
    requires = spec.rpm_requires
    provides = [(spec.name, SENSE_EQUAL, evr), (f"{spec.name}({target.isa})", SENSE_EQUAL, evr)]

    # 4. The header.
    sizes = [len(data or b"") for _, _, data in owned]
    entries: dict[int, tuple[Kind, object]] = {
        Tag.HEADERI18NTABLE: (Kind.STRING_ARRAY, ["C"]),
        Tag.NAME: (Kind.STRING, spec.name),
        Tag.VERSION: (Kind.STRING, version),
        Tag.RELEASE: (Kind.STRING, RPM_RELEASE),
        Tag.SUMMARY: (Kind.I18NSTRING, [spec.summary]),
        Tag.DESCRIPTION: (Kind.I18NSTRING, [spec.description]),
        Tag.BUILDTIME: (Kind.INT32, [release.mtime]),
        Tag.BUILDHOST: (Kind.STRING, BUILD_HOST),
        Tag.SIZE: (Kind.INT32, [sum(sizes)]),
        Tag.LICENSE: (Kind.STRING, spec.license),
        Tag.PACKAGER: (Kind.STRING, MAINTAINER),
        Tag.URL: (Kind.STRING, HOMEPAGE),
        Tag.OS: (Kind.STRING, "linux"),
        Tag.ARCH: (Kind.STRING, target.rpm),
        Tag.FILESIZES: (Kind.INT32, sizes),
        Tag.FILEMODES: (Kind.INT16, [mode for _, mode, _ in owned]),
        Tag.FILERDEVS: (Kind.INT16, [0] * len(owned)),
        Tag.FILEMTIMES: (Kind.INT32, [release.mtime] * len(owned)),
        Tag.FILEDIGESTS: (Kind.STRING_ARRAY,
                          [hashlib.sha256(d).hexdigest() if d is not None else ""
                           for _, _, d in owned]),
        Tag.FILELINKTOS: (Kind.STRING_ARRAY, [""] * len(owned)),
        Tag.FILEFLAGS: (Kind.INT32, [flags.get(path, 0) for path, _, _ in owned]),
        Tag.FILEUSERNAME: (Kind.STRING_ARRAY, ["root"] * len(owned)),
        Tag.FILEGROUPNAME: (Kind.STRING_ARRAY, ["root"] * len(owned)),
        # A binary package names its source package; a package without one is a source package.
        Tag.SOURCERPM: (Kind.STRING, f"{PACKAGE}-{evr}.src.rpm"),
        Tag.FILEVERIFYFLAGS: (Kind.INT32, [VERIFY_ALL] * len(owned)),
        Tag.PROVIDENAME: (Kind.STRING_ARRAY, [n for n, _, _ in provides]),
        Tag.PROVIDEFLAGS: (Kind.INT32, [f for _, f, _ in provides]),
        Tag.PROVIDEVERSION: (Kind.STRING_ARRAY, [v for _, _, v in provides]),
        Tag.REQUIRENAME: (Kind.STRING_ARRAY, [n for n, _, _ in requires]),
        Tag.REQUIREFLAGS: (Kind.INT32, [f for _, f, _ in requires]),
        Tag.REQUIREVERSION: (Kind.STRING_ARRAY, [v for _, _, v in requires]),
        Tag.FILEDEVICES: (Kind.INT32, [1] * len(owned)),
        Tag.FILEINODES: (Kind.INT32, list(range(1, len(owned) + 1))),
        Tag.FILELANGS: (Kind.STRING_ARRAY, [""] * len(owned)),
        Tag.DIRINDEXES: (Kind.INT32,
                         [dirnames.index(p.rsplit("/", 1)[0] + "/") for p, _, _ in owned]),
        Tag.BASENAMES: (Kind.STRING_ARRAY, [p.rsplit("/", 1)[1] for p, _, _ in owned]),
        Tag.DIRNAMES: (Kind.STRING_ARRAY, dirnames),
        Tag.PAYLOADFORMAT: (Kind.STRING, "cpio"),
        Tag.PAYLOADCOMPRESSOR: (Kind.STRING, "xz"),
        Tag.PAYLOADFLAGS: (Kind.STRING, str(XZ_PRESET)),
        Tag.FILEDIGESTALGO: (Kind.INT32, [SHA256_ALGO]),
        Tag.PAYLOADDIGEST: (Kind.STRING_ARRAY, [hashlib.sha256(payload).hexdigest()]),
        Tag.PAYLOADDIGESTALGO: (Kind.INT32, [SHA256_ALGO]),
    }
    if spec.postinst:
        entries[Tag.POSTIN] = (Kind.STRING, spec.postinst)
        entries[Tag.POSTINPROG] = (Kind.STRING, SHELL)
    if spec.rpm_recommends:
        entries[Tag.RECOMMENDNAME] = (Kind.STRING_ARRAY, [n for n, _, _ in spec.rpm_recommends])
        entries[Tag.RECOMMENDFLAGS] = (Kind.INT32, [f for _, f, _ in spec.rpm_recommends])
        entries[Tag.RECOMMENDVERSION] = (Kind.STRING_ARRAY,
                                         [v for _, _, v in spec.rpm_recommends])
    header = rpm_header(Tag.HEADERIMMUTABLE, entries)

    # 5. The signature header: digests of the header, and of the header and payload.
    signature = rpm_header(Sig.HEADERSIGNATURES, {
        Sig.SHA1: (Kind.STRING, hashlib.sha1(header).hexdigest()),
        Sig.SHA256: (Kind.STRING, hashlib.sha256(header).hexdigest()),
        Sig.SIZE: (Kind.INT32, [len(header) + len(payload)]),
        Sig.MD5: (Kind.BIN, hashlib.md5(header + payload).digest()),
        Sig.PAYLOADSIZE: (Kind.INT32, [len(archive)]),
    })
    signature += b"\0" * (-len(signature) % 8)

    # 6. The lead, which rpm 4 reads only for its magic and signature type.
    name = f"{spec.name}-{evr}".encode()[:65]
    lead = struct.pack(">4sBBhh66shh16s", LEAD_MAGIC, 3, 0, LEAD_BINARY, target.lead, name,
                       LEAD_OS_LINUX, LEAD_HEADER_SIGNATURE, b"")
    return lead + signature + header + payload


# The packages: the client's from the archive alone, the server kit's from the kit and LICENSE.


def client_spec(release: Release) -> Spec:
    """`swarmops`: the archive's binaries and docs, their libraries as dependencies."""
    needed, versions = dependencies(release)
    requires = [(f"{lib}(){ELF64_MARK}", SENSE_FIND_REQUIRES, "") for lib in needed]
    requires += [(f"{lib}({v}){ELF64_MARK}", SENSE_FIND_REQUIRES, "") for lib, v in versions]
    requires += [(n, SENSE_RPMLIB | SENSE_LESS | SENSE_EQUAL, v) for n, v in RPMLIB_FEATURES]
    flags = {f"{DOC_DIR}/{name}": FILE_LICENSE if name in LICENSES else FILE_DOC for name in DOCS}
    return Spec(
        name=PACKAGE, summary=SUMMARY, description=DESCRIPTION, license=release.license,
        files=release.files,
        deb_files=(File(DEB_COPYRIGHT, MODE_DOC, deb_copyright(release)),), rpm_files=(),
        deb_depends=deb_depends(release), deb_recommends="",
        rpm_requires=tuple(sorted(requires)), rpm_recommends=(),
        rpm_flags=flags, rpm_dirs=(DOC_DIR,), conffiles=(), postinst="",
    )


SERVER = "swarmops-server"
KIT = ROOT / "packaging" / "server"
MANIFEST = "MANIFEST"
SERVER_DOC_DIR = f"/usr/share/doc/{SERVER}"
SERVER_SUMMARY = "The SwarmOps server kit: one host serving stores over ssh and HTTPS"
SERVER_DESCRIPTION = (
    "The server kit of SwarmOps: the serving account and its directories, an sshd drop-in, a "
    "web site with the CGI entry, units and timers, so-kit for the host steps, the backup and "
    "restore drill, and the status probe and page.\n"
    "It starts nothing: the operator enables what the host runs."
)
# What the kit's programs run beyond a base system: so-backup (curl with --aws-sigv4, openssl's
# CMS with AES-GCM) and so-status (git). The daemons the operator enables are recommended.
SERVER_DEB_DEPENDS = ("curl (>= 7.75)", "git", "openssl (>= 3.0)")
SERVER_DEB_RECOMMENDS = "fcgiwrap, nginx, openssh-server"
SERVER_RPM_REQUIRES = (
    ("curl", SENSE_GREATER | SENSE_EQUAL, "7.75"),
    ("openssl", SENSE_GREATER | SENSE_EQUAL, "3.0"),
    ("/usr/bin/git", 0, ""),
)
SERVER_RPM_RECOMMENDS = (("fcgiwrap", 0, ""), ("nginx", 0, ""), ("openssh-server", 0, ""))
# The post-install script's interpreter and the programs it runs that a package provides.
SHELL = "/bin/sh"
SHEBANG = f"#!{SHELL}"
POSTINST_PROGRAMS = ("/usr/sbin/groupadd", "/usr/sbin/useradd")
CONFIG_DIR = "/etc/"
SYSUSERS_DIR = "/usr/lib/sysusers.d/"
TMPFILES_DIR = "/usr/lib/tmpfiles.d/"
# A MANIFEST line's mode: permission bits only, no set-id or sticky bit.
MODE_BITS = 0o777
NO_AGE = "-"  # sysusers' automatic id, tmpfiles' no cleanup


def kit_files(kit: Path) -> tuple[File, ...]:
    """The files every MANIFEST under `kit` names: `install <file> <destination> <mode>`, the
    file relative to its MANIFEST; `uses <path>` names a path the kit relies on and installs
    nothing. Every other file under `kit` must be installed, each destination once."""
    manifests = sorted(kit.rglob(MANIFEST))
    if not manifests:
        raise Failure(f"the kit directory holds no {MANIFEST}")
    installed: dict[str, File] = {}
    named: set[Path] = set()

    # 1. Each MANIFEST's lines.
    for manifest in manifests:
        where = manifest.relative_to(kit).as_posix()
        for number, line in enumerate(manifest.read_text().splitlines(), start=1):
            words = line.split()
            if not words or words[0].startswith("#"):
                continue
            if words[0] == "uses" and len(words) == 2 and words[1].startswith("/"):
                continue
            if words[0] != "install" or len(words) != 4:
                raise Failure(f"{where}:{number}: neither `install <file> <destination> <mode>`"
                              " nor `uses <path>`")
            source, destination, mode = manifest.parent / words[1], words[2], words[3]
            if ".." in Path(words[1]).parts or not source.is_file() or source.is_symlink():
                raise Failure(f"{where}:{number}: {words[1]} is no regular file of the kit")
            if Path(destination).as_posix() != destination or not destination.startswith("/") \
                    or ".." in destination.split("/"):
                raise Failure(f"{where}:{number}: {destination} is no plain absolute path")
            if destination in installed:
                raise Failure(f"{where}:{number}: {destination} is installed twice")
            if not re.fullmatch(r"0[0-7]{3}", mode) or int(mode, 8) & ~MODE_BITS:
                raise Failure(f"{where}:{number}: mode {mode} is not 0<three octal digits>")
            installed[destination] = File(destination, int(mode, 8), source.read_bytes())
            named.add(source.resolve())

    # 2. Nothing left out.
    files = {p.resolve() for p in kit.rglob("*") if p.is_file() and p.name != MANIFEST}
    left = sorted(p.relative_to(kit.resolve()).as_posix() for p in files - named)
    if left:
        raise Failure(f"no {MANIFEST} installs {', '.join(left)}")
    return tuple(sorted(installed.values(), key=lambda f: f.path))


def config_lines(files: tuple[File, ...], directory: str) -> list[list[str]]:
    """The words of each line of the files installed in `directory`, comments left out."""
    lines = []
    for f in files:
        if not f.path.startswith(directory):
            continue
        try:
            text = f.data.decode()
        except UnicodeDecodeError as error:
            raise Failure(f"{f.path} is not UTF-8: {error}") from error
        for line in text.splitlines():
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            try:
                lines.append(shlex.split(line))
            except ValueError as error:
                raise Failure(f"{f.path}: {line!r}: {error}") from error
    return lines


def postinst(files: tuple[File, ...]) -> str:
    """The post-install script, made from the kit's sysusers.d and tmpfiles.d files: the groups,
    the accounts, then the directories and files with their modes. Idempotent; starts nothing;
    removes nothing."""
    groups, users, paths = [], [], []

    # 1. sysusers.d: `g <group> -` and `u <account> - <comment> <home> <shell>`, each account
    # with a group of its name.
    for words in config_lines(files, SYSUSERS_DIR):
        if words[:1] == ["g"] and len(words) == 3 and words[2] == NO_AGE:
            groups.append(words[1])
            continue
        if words[:1] != ["u"] or len(words) != 6 or words[2] != NO_AGE:
            raise Failure(f"sysusers.d: {shlex.join(words)} is not a `g` or `u` line this script"
                          " reads")
        _, name, _, comment, home, shell = words
        groups.append(name)
        users.append(f"getent passwd {name} >/dev/null || useradd --system --gid {name}"
                     f" --home-dir {home} --no-create-home --shell {shell}"
                     f" --comment {shlex.quote(comment)} {name}")

    # 2. tmpfiles.d: `d` and `f` lines, `<path> <mode> <user> <group> -`.
    for words in config_lines(files, TMPFILES_DIR):
        if len(words) != 6 or words[0] not in ("d", "f") or words[5] != NO_AGE \
                or not re.fullmatch(r"0[0-7]{3}", words[2]):
            raise Failure(f"tmpfiles.d: {shlex.join(words)} is not a `d` or `f` line this script"
                          " reads")
        kind, path, mode, user, group, _ = words
        if kind == "d":
            paths.append(f"install -d -m {mode} -o {user} -g {group} {path}")
            continue
        paths.append(f"[ -e {path} ] || : >{path}")
        paths += [f"chown {user}:{group} {path}", f"chmod {mode} {path}"]

    lines = [
        "# Made by packages.py from the package's sysusers.d and tmpfiles.d files.",
        "set -e",
    ]
    lines += [f"getent group {g} >/dev/null || groupadd --system {g}" for g in groups]
    return "\n".join(lines + users + paths) + "\n"


def server_copyright(license_text: str) -> bytes:
    """The server .deb's copyright file: SwarmOps' license for every file, the kit holding no
    third party's code."""
    holders = [line.strip() for line in license_text.splitlines() if line.startswith("Copyright")]
    if not holders:
        raise Failure("LICENSE has no Copyright line for the .deb's copyright file")
    lines = [
        f"Format: {COPYRIGHT_FORMAT}",
        f"Upstream-Name: {SERVER}",
        f"Upstream-Contact: {MAINTAINER}",
        f"Source: {SOURCE}",
        "",
        "Files: *",
        f"Copyright: {holders[0]}",
    ]
    lines += [f" {h}" for h in holders[1:]] + [f"License: {OWN_LICENSE}"] + folded(license_text)
    return ("\n".join(lines) + "\n").encode()


def server_spec(release: Release, kit: Path) -> Spec:
    """`swarmops-server`: the kit, on `swarmops` of the same version and target."""
    files = kit_files(kit)
    license_data = next(f.data for f in release.files if f.path == f"{DOC_DIR}/LICENSE")
    evr = f"{release.package_version}-{RPM_RELEASE}"

    # 1. Requires: the client of this target and version, what the kit's programs run, what the
    # post-install script runs, and rpm's features.
    requires = [(f"{PACKAGE}({release.target.isa})", SENSE_EQUAL, evr), *SERVER_RPM_REQUIRES]
    requires += [(SHELL, SENSE_INTERP | SENSE_SCRIPT_POST, "")]
    requires += [(p, SENSE_SCRIPT_POST, "") for p in POSTINST_PROGRAMS]
    requires += [(n, SENSE_RPMLIB | SENSE_LESS | SENSE_EQUAL, v) for n, v in RPMLIB_FEATURES]
    depends = sorted([f"{PACKAGE} (= {release.package_version})", *SERVER_DEB_DEPENDS])

    # 2. Configuration files: everything under /etc; the .rpm keeps an edited one.
    conffiles = tuple(f.path for f in files if f.path.startswith(CONFIG_DIR))
    flags = {path: FILE_CONFIG | FILE_NOREPLACE for path in conffiles}
    flags[f"{SERVER_DOC_DIR}/LICENSE"] = FILE_LICENSE

    # 3. The directories the .rpm owns: the kit's own, named after it.
    license_file = File(f"{SERVER_DOC_DIR}/LICENSE", MODE_DOC, license_data)
    own = [d for d in parents([f.path for f in files + (license_file,)])
           if any(part.startswith(SERVER) for part in d.split("/"))]
    try:
        license_text = license_data.decode()
    except UnicodeDecodeError as error:
        raise Failure(f"LICENSE is not UTF-8: {error}") from error
    return Spec(
        name=SERVER, summary=SERVER_SUMMARY, description=SERVER_DESCRIPTION, license=OWN_LICENSE,
        files=files,
        deb_files=(File(f"{SERVER_DOC_DIR}/copyright", MODE_DOC, server_copyright(license_text)),),
        rpm_files=(license_file,),
        deb_depends=", ".join(depends), deb_recommends=SERVER_DEB_RECOMMENDS,
        rpm_requires=tuple(sorted(requires)), rpm_recommends=SERVER_RPM_RECOMMENDS,
        rpm_flags=flags, rpm_dirs=tuple(own), conffiles=conffiles, postinst=postinst(files),
    )


def package_names(version: str, target: Target, name: str = PACKAGE) -> tuple[str, str]:
    """A package's .deb and .rpm file names. They keep the release's version: GitHub renames an
    asset whose name has a `~`."""
    deb = f"{name}_{version}_{target.deb}.deb"
    rpm = f"{name}-{version}-{RPM_RELEASE}.{target.rpm}.rpm"
    return deb, rpm


def build(archive: Path, out: Path, kit: Path) -> None:
    """Writes the archive's packages, the client's and the server kit's, into `out` once all four
    are made."""
    release = read_archive(archive)
    made = {}
    for spec in (client_spec(release), server_spec(release, kit)):
        deb_name, rpm_name = package_names(release.version, release.target, spec.name)
        made[deb_name] = (build_deb(release, spec), f"Depends: {spec.deb_depends}")
        made[rpm_name] = (build_rpm(release, spec), f"License: {spec.license}")
    out.mkdir(parents=True, exist_ok=True)
    for name, (data, what) in made.items():
        (out / name).write_bytes(data)
        print(f"{name}: {len(data)} bytes, {what}")


# `smoke`: each package installed with its own tool, in a container pinned by digest.


class Network(enum.Enum):
    """Whether a container reaches the network."""

    NONE = "none"
    BRIDGE = "bridge"


class Container:
    """A container that `run` executes commands in; removed when the `with` block ends."""

    def __init__(self, image: str, packages: Path, network: Network) -> None:
        self.image, self.packages, self.network = image, packages, network
        self.name = f"swarmops-packages-{secrets.token_hex(4)}"

    def __enter__(self) -> "Container":
        docker("run", "--detach", "--rm", "--name", self.name, "--network", self.network.value,
               "--env", "DEBIAN_FRONTEND=noninteractive",
               "--volume", f"{self.packages}:/packages:ro", self.image, "sleep", "infinity")
        return self

    def __exit__(self, *_) -> None:
        subprocess.run(["docker", "rm", "--force", self.name], capture_output=True, check=False)

    def run(self, *argv: str, expect: int = 0) -> str:
        """Runs `argv` in the container, prints it and its output; fails on another exit status."""
        print(f"[{self.image.split('@')[0]}] $ {' '.join(argv)}", flush=True)
        done = subprocess.run(["docker", "exec", self.name, *argv], capture_output=True,
                              text=True, timeout=COMMAND_TIMEOUT, check=False)
        output = done.stdout + done.stderr
        sys.stdout.write(output if output.endswith("\n") or not output else output + "\n")
        if done.returncode != expect:
            raise Failure(f"'{' '.join(argv)}' exited {done.returncode}, not {expect}")
        return done.stdout

    def copy_out(self, path: str, to: Path) -> None:
        docker("cp", f"{self.name}:{path}", str(to))

    def copy_in(self, path: Path, to: str) -> None:
        docker("cp", str(path), f"{self.name}:{to}")


def docker(*argv: str) -> None:
    """Runs a docker command that must succeed."""
    done = subprocess.run(["docker", *argv], capture_output=True, text=True,
                          timeout=COMMAND_TIMEOUT, check=False)
    if done.returncode != 0:
        raise Failure(f"'docker {' '.join(argv)}' exited {done.returncode}: {done.stderr.strip()}")


def expect_versions(box: Container, emulator: list[str], root: str, version: str) -> None:
    """Both installed binaries print `swarmops <version>` on stdout."""
    for name in BINARIES:
        printed = box.run(*emulator, f"{root}{BIN_DIR}/{name}", "--version").strip()
        if printed != f"{PACKAGE} {version}":
            raise Failure(f"{name} --version printed '{printed}', not '{PACKAGE} {version}'")


def expect_listed(listing: str, tool: str, extra: tuple[str, ...] = ()) -> None:
    """The package manager lists every file the package installs: both formats' and `extra`."""
    installed = {f"{BIN_DIR}/{n}" for n in BINARIES} | {f"{DOC_DIR}/{n}" for n in DOCS}
    installed |= set(extra)
    missing = sorted(installed - set(listing.splitlines()))
    if missing:
        raise Failure(f"{tool} does not list {', '.join(missing)} as installed")


def expect_removed(box: Container, root: str) -> None:
    """Removing the package removed the binaries and the doc directory."""
    for path in [f"{BIN_DIR}/{n}" for n in BINARIES] + [DOC_DIR]:
        box.run("test", "-e", f"{root}{path}", expect=1)


def qemu(target: Target) -> str:
    """Debian's statically linked user-mode emulator for the target."""
    return f"qemu-{target.rpm}-static"


def smoke_deb(deb: Path, version: str, target: Target, emulator: Path) -> None:
    """Installs the .deb in Debian with dpkg; for an emulated target, copies out its emulator."""
    emulated = target.rpm != platform.machine()
    network = Network.BRIDGE if emulated else Network.NONE
    with Container(DEBIAN_IMAGE, deb.parent, network) as box:
        run_with: list[str] = []

        # 1. An emulated target: its C library and libgcc (multiarch), and the emulator.
        if emulated:
            box.run("dpkg", "--add-architecture", target.deb)
            box.run("apt-get", "update", "--quiet")
            box.run("apt-get", "install", "--yes", "--quiet", "--no-install-recommends",
                    "qemu-user-static", f"libc6:{target.deb}", f"libgcc-s1:{target.deb}")
            run_with = [qemu(target)]

        # 2. Install; what dpkg records; both binaries; the files; their checksums.
        box.run("dpkg", "--install", f"/packages/{deb.name}")
        status = box.run("dpkg-query", "--show", "--showformat",
                         "${Version} ${Architecture} ${db:Status-Abbrev}", PACKAGE).strip()
        recorded = f"{package_version(version)} {target.deb} ii"
        if status != recorded:
            raise Failure(f"dpkg records '{status}', not '{recorded}'")
        expect_versions(box, run_with, "", version)
        expect_listed(box.run("dpkg", "--listfiles", PACKAGE), "dpkg", (DEB_COPYRIGHT,))
        if box.run("dpkg", "--verify", PACKAGE).strip():
            raise Failure("dpkg --verify reports a file that differs from the package")

        # 3. Remove.
        box.run("dpkg", "--purge", PACKAGE)
        expect_removed(box, "")
        if emulated:
            box.copy_out(f"/usr/bin/{qemu(target)}", emulator)


def smoke_rpm(rpm: Path, version: str, target: Target, emulator: Path) -> None:
    """Installs the .rpm in AlmaLinux with rpm: into a root of the target's architecture when it
    is emulated, with the emulator `smoke_deb` left."""
    emulated = target.rpm != platform.machine()
    network = Network.BRIDGE if emulated else Network.NONE
    with Container(ALMALINUX_IMAGE, rpm.parent, network) as box:
        package = f"/packages/{rpm.name}"
        root, run_with, foreign = "", [], []

        # 1. An emulated target: an AlmaLinux root of its architecture with glibc and libgcc,
        # without scriptlets, which would run that architecture's programs.
        if emulated:
            root = "/sysroot"
            box.run("dnf", "--assumeyes", "--quiet", f"--forcearch={target.rpm}",
                    f"--installroot={root}", f"--releasever={ALMALINUX_RELEASE}",
                    "--setopt=install_weak_deps=False", "--setopt=tsflags=noscripts",
                    "install", "glibc", "libgcc")
            box.copy_in(emulator, f"/usr/local/bin/{qemu(target)}")
            run_with, foreign = [qemu(target), "-L", root], ["--ignorearch"]

        # 2. Every digest; install, which checks the dependencies; what rpm records; both
        # binaries; the files; their attributes.
        checked = box.run("rpm", "--checksig", "--verbose", package)
        for digest in ("Header SHA256 digest", "Header SHA1 digest", "Payload SHA256 digest",
                       "MD5 digest"):
            if f"{digest}: OK" not in checked:
                raise Failure(f"rpm --checksig does not find the {digest} OK")
        on_root = [f"--root={root}"] if root else []
        box.run("rpm", *on_root, "--install", *foreign, package)
        recorded = box.run("rpm", *on_root, "--query", "--queryformat",
                           "%{VERSION}-%{RELEASE} %{ARCH}", PACKAGE).strip()
        wanted = f"{package_version(version)}-{RPM_RELEASE} {target.rpm}"
        if recorded != wanted:
            raise Failure(f"rpm records '{recorded}', not '{wanted}'")
        expect_versions(box, run_with, root, version)
        expect_listed(box.run("rpm", *on_root, "--query", "--list", PACKAGE), "rpm")
        box.run("rpm", *on_root, "--verify", PACKAGE)

        # 3. A byte appended to a binary: rpm --verify must see it, which it does not when the
        # package asks it to check nothing.
        box.run("sh", "-c", f"printf x >>'{root}{BIN_DIR}/{BINARIES[0]}'")
        box.run("rpm", *on_root, "--verify", PACKAGE, expect=1)

        # 4. Remove.
        box.run("rpm", *on_root, "--erase", PACKAGE)
        expect_removed(box, root)


@dataclass(frozen=True)
class KitState:
    """What an installed server package leaves on the host, read from the kit."""

    files: tuple[str, ...]  # every installed file
    units: tuple[str, ...]  # the systemd units among them
    accounts: tuple[tuple[str, str, str], ...]  # (account, home, shell)
    groups: tuple[str, ...]
    paths: tuple[tuple[str, str], ...]  # (path, `stat -c '%a %U %G'`) of tmpfiles.d's lines


UNIT_DIR = "/usr/lib/systemd/system/"
# A file the smoke writes in /srv/so before removing the server package, which must keep it.
KEPT = "/srv/so/kept-by-removal"


def kit_state(kit: Path) -> KitState:
    """The kit's files, units, accounts, groups and tmpfiles.d paths."""
    files = kit_files(kit)
    postinst(files)  # refuses lines the state below would misread
    users = config_lines(files, SYSUSERS_DIR)
    entries = config_lines(files, TMPFILES_DIR)
    return KitState(
        files=tuple(f.path for f in files),
        units=tuple(f.path for f in files if f.path.startswith(UNIT_DIR)),
        accounts=tuple((w[1], w[4], w[5]) for w in users if w[0] == "u"),
        groups=tuple(w[1] for w in users),
        paths=tuple((w[1], f"{int(w[2], 8):o} {w[3]} {w[4]}") for w in entries),
    )


def expect_server(box: Container, state: KitState) -> None:
    """The accounts, groups, directories, files and units exist; so-kit reads its settings."""
    for name, home, shell in state.accounts:
        entry = box.run("getent", "passwd", name).strip().split(":")
        if entry[5:] != [home, shell]:
            raise Failure(f"account {name}: home and shell {entry[5:]}, not {[home, shell]}")
    for group in state.groups:
        box.run("getent", "group", group)
    for path, wanted in state.paths:
        found = box.run("stat", "-c", "%a %U %G", path).strip()
        if found != wanted:
            raise Failure(f"{path}: mode and owners '{found}', not '{wanted}'")
    for unit in state.units:
        box.run("test", "-f", unit)
    box.run("/usr/sbin/so-kit", "check")


def expect_server_removed(box: Container, state: KitState) -> None:
    """Removing the server package removed its files and kept /srv/so's."""
    for path in state.files:
        box.run("test", "-e", path, expect=1)
    box.run("test", "-f", KEPT)


def smoke_server_deb(client: Path, server: Path, version: str, target: Target,
                     state: KitState) -> None:
    """Installs the server .deb beside the client's with apt-get, which fetches its dependencies."""
    with Container(DEBIAN_IMAGE, server.parent, Network.BRIDGE) as box:
        box.run("apt-get", "update", "--quiet")
        box.run("apt-get", "install", "--yes", "--quiet", "--no-install-recommends",
                f"/packages/{client.name}", f"/packages/{server.name}")
        status = box.run("dpkg-query", "--show", "--showformat",
                         "${Version} ${Architecture} ${db:Status-Abbrev}", SERVER).strip()
        recorded = f"{package_version(version)} {target.deb} ii"
        if status != recorded:
            raise Failure(f"dpkg records '{status}' for {SERVER}, not '{recorded}'")
        expect_server(box, state)
        listed = set(box.run("dpkg", "--listfiles", SERVER).splitlines())
        missing = sorted(set(state.files) - listed)
        if missing:
            raise Failure(f"dpkg does not list {', '.join(missing)} as installed")
        if box.run("dpkg", "--verify", SERVER).strip():
            raise Failure(f"dpkg --verify reports a file of {SERVER} that differs")

        # Purge: its files and configuration files go, /srv/so's stay.
        box.run("sh", "-c", f": >{KEPT}")
        box.run("dpkg", "--purge", SERVER)
        expect_server_removed(box, state)


def smoke_server_rpm(client: Path, server: Path, version: str, target: Target,
                     state: KitState) -> None:
    """Installs the server .rpm beside the client's with dnf, which fetches its dependencies."""
    with Container(ALMALINUX_IMAGE, server.parent, Network.BRIDGE) as box:
        checked = box.run("rpm", "--checksig", "--verbose", f"/packages/{server.name}")
        if "Payload SHA256 digest: OK" not in checked:
            raise Failure(f"rpm --checksig does not find {server.name}'s payload digest OK")
        box.run("dnf", "--assumeyes", "--quiet", "--setopt=install_weak_deps=False", "install",
                f"/packages/{client.name}", f"/packages/{server.name}")
        recorded = box.run("rpm", "--query", "--queryformat", "%{VERSION}-%{RELEASE} %{ARCH}",
                           SERVER).strip()
        wanted = f"{package_version(version)}-{RPM_RELEASE} {target.rpm}"
        if recorded != wanted:
            raise Failure(f"rpm records '{recorded}' for {SERVER}, not '{wanted}'")
        expect_server(box, state)
        listed = set(box.run("rpm", "--query", "--list", SERVER).splitlines())
        missing = sorted(set(state.files) - listed)
        if missing:
            raise Failure(f"rpm does not list {', '.join(missing)} as installed")
        configs = set(box.run("rpm", "--query", "--configfiles", SERVER).splitlines())
        if configs != {f for f in state.files if f.startswith(CONFIG_DIR)}:
            raise Failure(f"rpm lists {sorted(configs)} as {SERVER}'s configuration files")
        box.run("rpm", "--verify", SERVER)

        # Erase: its files go, /srv/so's stay.
        box.run("sh", "-c", f": >{KEPT}")
        box.run("rpm", "--erase", SERVER)
        expect_server_removed(box, state)


def smoke(version: str, packages: list[Path], kit: Path) -> None:
    """Installs one target's client .deb, then its .rpm; on a target this host runs, the server
    packages after them."""
    if shutil.which("docker") is None:
        raise Failure("smoke needs docker")
    given = sorted(p.name for p in packages)
    found = [t for t in TARGETS.values()
             if given == sorted(package_names(version, t) + package_names(version, t, SERVER))]
    if not found:
        raise Failure(f"{', '.join(given)} are not one target's four packages of {version}")
    target, by_name = found[0], {p.name: p.resolve() for p in packages}
    deb, rpm = (by_name[n] for n in package_names(version, target))
    server_deb, server_rpm = (by_name[n] for n in package_names(version, target, SERVER))
    state = kit_state(kit)

    # 1. The client, as before.
    with tempfile.TemporaryDirectory() as scratch:
        emulator = Path(scratch) / "qemu"
        smoke_deb(deb, version, target, emulator)
        smoke_rpm(rpm, version, target, emulator)
    print(f"{deb.name} and {rpm.name}: installed, ran, verified and removed")

    # 2. The server kit: no binary of its own, so not installed under emulation.
    if target.rpm != platform.machine():
        print(f"{server_deb.name} and {server_rpm.name}: not installed, {target.triple} is"
              " emulated here")
        return
    smoke_server_deb(deb, server_deb, version, target, state)
    smoke_server_rpm(rpm, server_rpm, version, target, state)
    print(f"{server_deb.name} and {server_rpm.name}: installed, set up, verified and removed")


def main() -> int:
    parser = argparse.ArgumentParser(prog="packages.py", description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    make = commands.add_parser("build", help="make the .deb and .rpm of a release archive")
    make.add_argument("archive", type=Path)
    make.add_argument("out", type=Path)
    make.add_argument("--kit", type=Path, default=KIT, help="the server kit (packaging/server)")
    test = commands.add_parser("smoke", help="install one target's packages in containers")
    test.add_argument("version")
    test.add_argument("packages", type=Path, nargs="+")
    args = parser.parse_args()

    try:
        if args.command == "build":
            build(args.archive, args.out, args.kit)
        else:
            smoke(args.version, args.packages, args.kit)
    except (Failure, OSError, tarfile.TarError, subprocess.TimeoutExpired) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
