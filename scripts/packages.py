# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Makes the .deb and .rpm packages of a SwarmOps release archive, and installs them in containers.

    python3 -I scripts/packages.py build <archive> <out dir>
    python3 -I scripts/packages.py smoke <version> <.deb> <.rpm>

`build` reads `swarmops-<version>-<target>.tar.gz`, as the release workflow packs it, and writes
`swarmops_<version>_<arch>.deb` and `swarmops-<version>-1.<arch>.rpm` into the out directory from
the archive's files alone: `swarmops` and `so` to /usr/bin, LICENSE, THIRD-PARTY-LICENSES,
README.md and CHANGELOG.md to /usr/share/doc/swarmops. The binaries are the archive's, byte for
byte. The same archive gives the same packages, byte for byte: entries sorted, owned by root,
dated at the archive's own date (the tagged commit's), with fixed modes. The dependencies are the
binaries' own: each shared library their ELF dynamic section needs, and for glibc the newest
version they need that is not weak. A library the script does not know, a binary of another
machine, an entry the archive should not hold or a version the formats cannot carry fails the run
before anything is written.

    archive                              .deb (dpkg)                .rpm (rpm 4)
    swarmops-X-<target>/so        -->    ar: debian-binary,         lead, signature header,
    swarmops-X-<target>/swarmops         control.tar.xz,            header, xz cpio payload
    swarmops-X-<target>/LICENSE ...      data.tar.xz

`smoke` installs both packages of one target in containers pinned by digest: the .deb with dpkg
in Debian 12, the .rpm with rpm in AlmaLinux 9 (glibc 2.34, the oldest the binaries run on). It
checks what dpkg and rpm record, that `so --version` and `swarmops --version` print the version,
that the packages verify, that `rpm --verify` sees a byte appended to an installed binary, and
that removing them removes their files. A target this host does not run is installed for its own
architecture and run under QEMU's user-mode emulator: the .deb in Debian with that architecture's
C library (multiarch), the .rpm into an AlmaLinux root of that architecture, so that rpm checks
its dependencies against that architecture's glibc. Needs docker; an emulated target downloads its
packages from Debian's and AlmaLinux's mirrors. Exit 1 on a failure.
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
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path

PACKAGE = "swarmops"
BINARIES = ("so", "swarmops")
DOCS = ("CHANGELOG.md", "LICENSE", "README.md", "THIRD-PARTY-LICENSES")
# The docs that are licenses: rpm installs them even with --excludedocs.
LICENSES = ("LICENSE", "THIRD-PARTY-LICENSES")
BIN_DIR = "/usr/bin"
DOC_DIR = f"/usr/share/doc/{PACKAGE}"

MAINTAINER = "SwarmOps Maintainers <maintainers@swarmops.cc>"
HOMEPAGE = "https://swarmops.cc"
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


def deb_control(release: Release, depends: str, size: int) -> bytes:
    """The control file: dpkg's fields, the description's lines indented by one space."""
    lines = [
        f"Package: {PACKAGE}",
        f"Version: {release.package_version}",
        f"Architecture: {release.target.deb}",
        f"Maintainer: {MAINTAINER}",
        f"Installed-Size: {size}",
        f"Depends: {depends}",
        "Section: vcs",
        "Priority: optional",
        f"Homepage: {HOMEPAGE}",
        f"Description: {SUMMARY}",
    ]
    for paragraph in DESCRIPTION.split("\n"):
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


def deb_depends(release: Release) -> str:
    """Depends: each library's package, libc6 at the newest glibc the binaries need."""
    needed, versions = dependencies(release)
    glibc = newest_glibc(versions)
    packages = sorted({LIBRARIES[lib] for lib in needed})
    return ", ".join(f"{p} (>= {glibc})" if p == GLIBC_PACKAGE else p for p in packages)


def build_deb(release: Release) -> bytes:
    """The .deb of a release archive."""
    paths = [f.path for f in release.files]
    directories = parents(paths)

    # 1. data.tar.xz: ./ and every directory, then the files, sorted by path.
    entries: list[tuple[str, int, bytes | None]] = [("./", MODE_DIR, None)]
    entries += [(f".{d}/", MODE_DIR, None) for d in directories]
    entries += [(f".{f.path}", f.mode, f.data) for f in release.files]
    entries.sort(key=lambda e: e[0])
    data = tar_xz(entries, release.mtime)

    # 2. control.tar.xz: control and md5sums. Installed-Size is in KiB, a directory counting 1.
    size = sum(math.ceil(len(f.data) / 1024) for f in release.files) + len(directories)
    control = deb_control(release, deb_depends(release), size)
    md5sums = "".join(f"{hashlib.md5(f.data).hexdigest()}  {f.path[1:]}\n" for f in release.files)
    control_tar = tar_xz(
        [("./", MODE_DIR, None), ("./control", MODE_DOC, control),
         ("./md5sums", MODE_DOC, md5sums.encode())],
        release.mtime,
    )
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
    FILEDIGESTALGO = 5011
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
SENSE_EQUAL = 1 << 3
SENSE_FIND_REQUIRES = 1 << 14  # generated from the binaries, as rpmbuild's ELF dependencies are
SENSE_RPMLIB = 1 << 24
# RPMFILE_* flags of a file.
FILE_DOC = 1 << 1
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


def build_rpm(release: Release) -> bytes:
    """The .rpm of a release archive."""
    target, version = release.target, release.package_version
    evr = f"{version}-{RPM_RELEASE}"

    # 1. The files rpm owns: ours and our doc directory, not /usr/bin or /usr/share/doc.
    owned = sorted([(f.path, S_IFREG | f.mode, f.data) for f in release.files]
                   + [(DOC_DIR, S_IFDIR | MODE_DIR, None)])
    dirnames = sorted({path.rsplit("/", 1)[0] + "/" for path, _, _ in owned})
    flags = {f"{DOC_DIR}/{name}": FILE_LICENSE if name in LICENSES else FILE_DOC for name in DOCS}

    # 2. The payload: a cpio entry per file, in the header's order, then the trailer.
    archive = bytearray()
    for ino, (path, mode, data) in enumerate(owned, start=1):
        nlink = 2 if data is None else 1
        archive += cpio_entry(f".{path}", mode, nlink, release.mtime, ino, data or b"")
    archive += cpio_entry(CPIO_TRAILER, 0, 1, 0, 0, b"")
    payload = lzma.compress(bytes(archive), format=lzma.FORMAT_XZ, preset=XZ_PRESET)

    # 3. Requires: each library, each version need not flagged weak, and rpm's features.
    needed, versions = dependencies(release)
    requires = [(f"{lib}(){ELF64_MARK}", SENSE_FIND_REQUIRES, "") for lib in needed]
    requires += [(f"{lib}({v}){ELF64_MARK}", SENSE_FIND_REQUIRES, "") for lib, v in versions]
    requires += [(n, SENSE_RPMLIB | SENSE_LESS | SENSE_EQUAL, v) for n, v in RPMLIB_FEATURES]
    requires.sort()
    provides = [(PACKAGE, SENSE_EQUAL, evr), (f"{PACKAGE}({target.isa})", SENSE_EQUAL, evr)]

    # 4. The header.
    sizes = [len(data or b"") for _, _, data in owned]
    entries: dict[int, tuple[Kind, object]] = {
        Tag.HEADERI18NTABLE: (Kind.STRING_ARRAY, ["C"]),
        Tag.NAME: (Kind.STRING, PACKAGE),
        Tag.VERSION: (Kind.STRING, version),
        Tag.RELEASE: (Kind.STRING, RPM_RELEASE),
        Tag.SUMMARY: (Kind.I18NSTRING, [SUMMARY]),
        Tag.DESCRIPTION: (Kind.I18NSTRING, [DESCRIPTION]),
        Tag.BUILDTIME: (Kind.INT32, [release.mtime]),
        Tag.BUILDHOST: (Kind.STRING, BUILD_HOST),
        Tag.SIZE: (Kind.INT32, [sum(sizes)]),
        Tag.LICENSE: (Kind.STRING, release.license),
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
    name = f"{PACKAGE}-{evr}".encode()[:65]
    lead = struct.pack(">4sBBhh66shh16s", LEAD_MAGIC, 3, 0, LEAD_BINARY, target.lead, name,
                       LEAD_OS_LINUX, LEAD_HEADER_SIGNATURE, b"")
    return lead + signature + header + payload


def package_names(version: str, target: Target) -> tuple[str, str]:
    """The .deb's and the .rpm's file names. They keep the release's version: GitHub renames an
    asset whose name has a `~`."""
    deb = f"{PACKAGE}_{version}_{target.deb}.deb"
    rpm = f"{PACKAGE}-{version}-{RPM_RELEASE}.{target.rpm}.rpm"
    return deb, rpm


def build(archive: Path, out: Path) -> None:
    """Writes the archive's .deb and .rpm into `out`, once both are made."""
    release = read_archive(archive)
    deb, rpm = build_deb(release), build_rpm(release)
    deb_name, rpm_name = package_names(release.version, release.target)
    out.mkdir(parents=True, exist_ok=True)
    (out / deb_name).write_bytes(deb)
    (out / rpm_name).write_bytes(rpm)
    print(f"{deb_name}: {len(deb)} bytes, Depends: {deb_depends(release)}")
    print(f"{rpm_name}: {len(rpm)} bytes, License: {release.license}")


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


def expect_listed(listing: str, tool: str) -> None:
    """The package manager lists every file the package installs."""
    installed = {f"{BIN_DIR}/{n}" for n in BINARIES} | {f"{DOC_DIR}/{n}" for n in DOCS}
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
        expect_listed(box.run("dpkg", "--listfiles", PACKAGE), "dpkg")
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


def smoke(version: str, deb: Path, rpm: Path) -> None:
    """Installs one target's .deb, then its .rpm."""
    if shutil.which("docker") is None:
        raise Failure("smoke needs docker")
    found = [t for t in TARGETS.values() if package_names(version, t) == (deb.name, rpm.name)]
    if not found:
        raise Failure(f"{deb.name} and {rpm.name} are not one target's packages of {version}")
    with tempfile.TemporaryDirectory() as scratch:
        emulator = Path(scratch) / "qemu"
        smoke_deb(deb.resolve(), version, found[0], emulator)
        smoke_rpm(rpm.resolve(), version, found[0], emulator)
    print(f"{deb.name} and {rpm.name}: installed, ran, verified and removed")


def main() -> int:
    parser = argparse.ArgumentParser(prog="packages.py", description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    make = commands.add_parser("build", help="make the .deb and .rpm of a release archive")
    make.add_argument("archive", type=Path)
    make.add_argument("out", type=Path)
    test = commands.add_parser("smoke", help="install one target's packages in containers")
    test.add_argument("version")
    test.add_argument("deb", type=Path)
    test.add_argument("rpm", type=Path)
    args = parser.parse_args()

    try:
        if args.command == "build":
            build(args.archive, args.out)
        else:
            smoke(args.version, args.deb, args.rpm)
    except (Failure, OSError, tarfile.TarError, subprocess.TimeoutExpired) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
