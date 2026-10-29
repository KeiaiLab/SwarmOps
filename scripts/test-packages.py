# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests scripts/packages.py's `build`, offline and without dpkg or rpm.

    python3 -I scripts/test-packages.py

Each case packs a fake release archive in the release workflow's layout, whose "binaries" are
minimal ELF files with the dynamic section and version needs a case gives them, runs
`packages.py build` on it, and reads what it wrote with readers of its own: the .deb as an ar
archive of tar.xz members, the .rpm as a lead, two headers and a cpio payload, every digest
recomputed. The refusals check that nothing is written. Where `dpkg-deb` is installed, it reads
the .deb too. Prints a summary; exit 1 on a failure.
"""

import gzip
import hashlib
import io
import lzma
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Callable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "packages.py"

VERSION = "9.9.9"
X86 = "x86_64-unknown-linux-gnu"
ARM = "aarch64-unknown-linux-gnu"
MACHINE = {X86: 62, ARM: 183}
LOADER = {X86: "ld-linux-x86-64.so.2", ARM: "ld-linux-aarch64.so.1"}
MTIME = 1_760_000_000
BINARIES = ("so", "swarmops")
DOCS = ("CHANGELOG.md", "LICENSE", "README.md", "THIRD-PARTY-LICENSES")
WEAK = 0x2

# THIRD-PARTY-LICENSES in its generator's line format: a (build) crate is not in the binaries,
# and a (<target> only) crate is in that target's alone.
NOTICES = """Crates (4)
----------

a 1.0.0: MIT OR Apache-2.0; used under MIT [1]
b 2.0.0 (build): Apache-2.0; used under Apache-2.0 [2]
c 3.0.0 (x86_64-unknown-linux-gnu only): Zlib; used under Zlib [3]
d 4.0.0: BSD-3-Clause; used under BSD-3-Clause [4]
"""
LICENSE_OF = {X86: "BSD-3-Clause AND MIT AND Zlib", ARM: "BSD-3-Clause AND MIT"}

# The libraries and version needs of the binaries a case packs, unless it gives others.
GLIBC_NEEDS = [("GLIBC_2.2.5", 0), ("GLIBC_2.34", 0), ("GLIBC_2.39", WEAK)]
GCC_NEEDS = [("GCC_3.0", 0)]


def elf(machine: int, needed: list[str], needs: list[tuple[str, list[tuple[str, int]]]],
        dynamic: bool = True, without: tuple[int, ...] = ()) -> bytes:
    """A minimal 64-bit little-endian ELF: one loadable segment over the whole file and, when
    `dynamic`, a dynamic section with `needed` (DT_NEEDED) and `needs` (Elf64_Verneed records,
    each a library with its (version, flags)), less the tags in `without`."""
    strings = bytearray(b"\0")

    def add(text: str) -> int:
        at = len(strings)
        strings.extend(text.encode() + b"\0")
        return at

    # 1. The version needs: a record per library, its auxiliary entries right after it.
    verneed = bytearray()
    for i, (library, versions) in enumerate(needs):
        file = add(library)
        aux = bytearray()
        for j, (version, flags) in enumerate(versions):
            following = 16 if j < len(versions) - 1 else 0
            aux += struct.pack("<IHHII", 0, flags, 0, add(version), following)
        following = 16 + len(aux) if i < len(needs) - 1 else 0
        verneed += struct.pack("<HHIII", 1, len(versions), file, 16, following) + aux
    needed_at = [add(library) for library in needed]

    # 2. Layout: header, program headers, strings, version needs, dynamic section.
    headers = 2 if dynamic else 1
    strings_at = 64 + 56 * headers
    verneed_at = strings_at + len(strings) + (-len(strings) % 8)
    dynamic_at = verneed_at + len(verneed)
    entries = [(1, at) for at in needed_at] + [(5, strings_at), (10, len(strings))]
    if needs:
        entries += [(0x6FFFFFFE, verneed_at), (0x6FFFFFFF, len(needs))]
    entries = [(tag, value) for tag, value in entries if tag not in without]
    section = b"".join(struct.pack("<qQ", tag, value) for tag, value in entries + [(0, 0)])
    size = dynamic_at + len(section)

    head = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    head += struct.pack("<HHIQQQIHHHHHH", 3, machine, 1, 0, 64, 0, 0, 64, 56, headers, 64, 0, 0)
    program = struct.pack("<IIQQQQQQ", 1, 5, 0, 0, 0, size, size, 0x1000)
    if dynamic:
        program += struct.pack("<IIQQQQQQ", 2, 6, dynamic_at, dynamic_at, dynamic_at,
                               len(section), len(section), 8)
    body = head + program + bytes(strings) + bytes(-len(strings) % 8) + bytes(verneed) + section
    return body if dynamic else body[:strings_at]


def binary(target: str, name: str) -> bytes:
    """The fake binary `name` for `target`: glibc's and libgcc's needs, and its name in the data
    so that the two binaries differ."""
    needed = ["libgcc_s.so.1", "libc.so.6", LOADER[target]]
    needs = [("libgcc_s.so.1", GCC_NEEDS), ("libc.so.6", GLIBC_NEEDS)]
    return elf(MACHINE[target], needed, needs) + name.encode()


def files(target: str) -> dict[str, bytes]:
    """The seven entries' contents, by file name."""
    contents = {name: binary(target, name) for name in BINARIES}
    contents |= {name: f"{name} of the fake release\n".encode() for name in DOCS}
    contents["THIRD-PARTY-LICENSES"] = NOTICES.encode()
    return contents


Member = tuple[tarfile.TarInfo, bytes | None]


def archive(directory: Path, target: str = X86, version: str = VERSION,
            change: Callable[[dict[str, bytes]], None] | None = None,
            dates: Callable[[str], int] = lambda _: MTIME, level: int = 9,
            name: str | None = None,
            members: Callable[[list[Member]], None] | None = None) -> Path:
    """Packs a release archive as the release workflow does: the directory, then its entries
    sorted by name, owned by root, gzip'd at `level`. `change` edits the entries first; `dates`
    dates each entry by name; `members` edits the tar members last."""
    top = f"swarmops-{version}-{target}"
    contents = files(target)
    if change:
        change(contents)

    # 1. The tar members: the directory, then the files.
    packed: list[Member] = []
    for path, data in [(top, None)] + [(f"{top}/{n}", contents[n]) for n in sorted(contents)]:
        info = tarfile.TarInfo(path)
        info.mtime = dates(path)
        info.uname = info.gname = "root"
        if data is None:
            info.type, info.mode = tarfile.DIRTYPE, 0o755
        else:
            info.size = len(data)
            info.mode = 0o755 if path.rsplit("/", 1)[1] in BINARIES else 0o644
        packed.append((info, data))
    if members:
        members(packed)

    # 2. The archive.
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for info, data in packed:
            tar.addfile(info, None if data is None else io.BytesIO(data))
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / (name or f"{top}.tar.gz")
    path.write_bytes(gzip_bytes(raw.getvalue(), level))
    return path


def gzip_bytes(data: bytes, level: int) -> bytes:
    """`data` gzip'd at `level`, with no name and no date in the header."""
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", compresslevel=level, mtime=0, filename="") as gz:
        gz.write(data)
    return out.getvalue()


def build(path: Path, out: Path) -> subprocess.CompletedProcess:
    """Runs `packages.py build`."""
    return subprocess.run([sys.executable, "-I", str(SCRIPT), "build", str(path), str(out)],
                          capture_output=True, text=True, check=False)


def built(path: Path, out: Path) -> tuple[bytes, bytes]:
    """The .deb and .rpm `build` writes, which must be the only files in `out`."""
    done = build(path, out)
    assert done.returncode == 0, f"build exited {done.returncode}: {done.stderr}"
    debs, rpms = sorted(out.glob("*.deb")), sorted(out.glob("*.rpm"))
    assert len(debs) == 1 and len(rpms) == 1, f"build wrote {sorted(out.iterdir())}"
    return debs[0].read_bytes(), rpms[0].read_bytes()


# Readers, written apart from the script's writers.


def read_ar(data: bytes) -> list[tuple[str, int, bytes]]:
    """An ar archive's (name, mtime, bytes), checking each header's fixed fields."""
    assert data[:8] == b"!<arch>\n", "the .deb is not an ar archive"
    members, at = [], 8
    while at < len(data):
        header = data[at : at + 60].decode()
        assert header[58:60] == "`\n", f"bad ar header {header!r}"
        name, mtime = header[:16].rstrip(), int(header[16:28])
        owner, group, mode = header[28:34].strip(), header[34:40].strip(), header[40:48].strip()
        assert (owner, group, mode) == ("0", "0", "100644"), f"{name}: {owner} {group} {mode}"
        size = int(header[48:58])
        members.append((name, mtime, data[at + 60 : at + 60 + size]))
        at += 60 + size + size % 2
    return members


def read_tar_xz(data: bytes) -> list[tuple[tarfile.TarInfo, bytes | None]]:
    """A tar.xz's entries in order, each with its bytes (None for a directory)."""
    with tarfile.open(fileobj=io.BytesIO(lzma.decompress(data)), mode="r:") as tar:
        out = []
        for info in tar:
            stream = tar.extractfile(info)
            out.append((info, stream.read() if stream else None))
        return out


def read_header(data: bytes, at: int, region: int) -> tuple[dict[int, tuple[int, object]], int]:
    """An rpm header at `at`: its entries by tag as (type, value), and where it ends. Checks the
    region trailer, that entries come by tag with their data in order, and their alignment."""
    assert data[at : at + 8] == b"\x8e\xad\xe8\x01\0\0\0\0", "bad header magic"
    count, size = struct.unpack_from(">ii", data, at + 8)
    index = [struct.unpack_from(">iiii", data, at + 16 + 16 * i) for i in range(count)]
    store = data[at + 16 + 16 * count : at + 16 + 16 * count + size]

    # 1. The region: first entry, trailer at the store's end counting every entry.
    assert index[0] == (region, 7, size - 16, 16), f"region entry {index[0]}"
    trailer = struct.unpack_from(">iiii", store, size - 16)
    assert trailer == (region, 7, -16 * count, 16), f"region trailer {trailer}"

    # 2. The entries, by tag, with their data in order and aligned.
    entries, end = {}, 0
    tags = [tag for tag, _, _, _ in index[1:]]
    assert tags == sorted(tags), f"entries not sorted by tag: {tags}"
    for tag, kind, offset, n in index[1:]:
        assert offset >= end, f"tag {tag}: data overlaps the one before"
        assert offset % {3: 2, 4: 4}.get(kind, 1) == 0, f"tag {tag}: misaligned"
        if kind == 3:
            value, end = list(struct.unpack_from(f">{n}H", store, offset)), offset + 2 * n
        elif kind == 4:
            value, end = list(struct.unpack_from(f">{n}I", store, offset)), offset + 4 * n
        elif kind == 7:
            value, end = store[offset : offset + n], offset + n
        else:
            parts = store[offset:].split(b"\0")[:n]
            value = [p.decode() for p in parts]
            end = offset + sum(len(p) + 1 for p in parts)
            value = value[0] if kind == 6 else value
        entries[tag] = (kind, value)
    assert end <= size - 16, "data runs into the region trailer"
    return entries, at + 16 + 16 * count + size


def read_cpio(data: bytes) -> list[tuple[str, int, int, int, bytes]]:
    """A newc cpio's (name, mode, nlink, mtime, bytes) up to its trailer, which must end it."""
    out, at = [], 0
    while True:
        assert data[at : at + 6] == b"070701", f"bad cpio magic at {at}"
        fields = [int(data[at + 6 + 8 * i : at + 14 + 8 * i], 16) for i in range(13)]
        _, mode, uid, gid, nlink, mtime, size, _, _, _, _, name_size, _ = fields
        name = data[at + 110 : at + 110 + name_size - 1].decode()
        at += 110 + name_size
        at += -at % 4
        body = data[at : at + size]
        at += size + (-size % 4)
        if name == "TRAILER!!!":
            assert at == len(data), "data after the cpio trailer"
            return out
        assert (uid, gid) == (0, 0), f"{name}: owned by {uid}:{gid}"
        out.append((name, mode, nlink, mtime, body))


def read_rpm(data: bytes) -> dict:
    """An .rpm's lead fields, headers and cpio entries, every digest checked."""
    magic, major, _, kind, arch, name = struct.unpack_from(">4sBBhh66s", data, 0)
    system, sigtype = struct.unpack_from(">hh", data, 76)
    assert (magic, major, kind, system, sigtype) == (b"\xed\xab\xee\xdb", 3, 0, 1, 5), "lead"

    # The signature header, padded to 8 bytes with zeros; the header after it.
    signature, at = read_header(data, 96, 62)
    assert data[at : at + (-at % 8)] == bytes(-at % 8), "signature padding"
    at += -at % 8
    header, payload_at = read_header(data, at, 63)
    head, payload = data[at:payload_at], data[payload_at:]

    # The digests: of the header, of the header and payload, of the payload.
    assert signature[269] == (6, hashlib.sha1(head).hexdigest()), "header SHA-1"
    assert signature[273] == (6, hashlib.sha256(head).hexdigest()), "header SHA-256"
    assert signature[1000] == (4, [len(head) + len(payload)]), "header and payload size"
    assert signature[1004] == (7, hashlib.md5(head + payload).digest()), "header and payload MD5"
    assert header[5092] == (8, [hashlib.sha256(payload).hexdigest()]), "payload SHA-256"
    archive = lzma.decompress(payload)
    assert signature[1007] == (4, [len(archive)]), "payload size"
    lead_name = name.rstrip(b"\0").decode()
    return {"arch": arch, "name": lead_name, "header": header, "cpio": read_cpio(archive)}


def value(header: dict, tag: int):
    return header[tag][1]


# The cases.


def deb_layout(scratch: Path) -> None:
    """The .deb: three members dated at the archive, control fields, md5sums, every entry."""
    contents = files(X86)
    deb, _ = built(archive(scratch / "in"), scratch / "out")
    members = read_ar(deb)
    assert [m[0] for m in members] == ["debian-binary", "control.tar.xz", "data.tar.xz"]
    assert all(m[1] == MTIME for m in members), "ar members not dated at the archive"
    assert members[0][2] == b"2.0\n"

    # 1. control and md5sums.
    # tarfile reads a directory's name without its trailing slash: ./ as `.`.
    control = {info.name: data for info, data in read_tar_xz(members[1][2])}
    assert list(control) == [".", "./control", "./md5sums"], list(control)
    fields = dict(line.split(": ", 1) for line in control["./control"].decode().splitlines()
                  if not line.startswith(" "))
    size = sum(-(-len(d) // 1024) for d in contents.values()) + 5
    want = {"Package": "swarmops", "Version": VERSION, "Architecture": "amd64",
            "Maintainer": "SwarmOps Maintainers <maintainers@swarmops.cc>",
            "Installed-Size": str(size), "Depends": "libc6 (>= 2.34), libgcc-s1",
            "Section": "vcs", "Priority": "optional", "Homepage": "https://swarmops.cc"}
    assert {k: fields.get(k) for k in want} == want, fields
    paths = [f"usr/bin/{n}" for n in BINARIES] + [f"usr/share/doc/swarmops/{n}" for n in DOCS]
    sums = "".join(f"{hashlib.md5(contents[p.rsplit('/', 1)[1]]).hexdigest()}  {p}\n"
                   for p in sorted(paths))
    assert control["./md5sums"].decode() == sums, control["./md5sums"]

    # 2. data: every directory and file, sorted, owned by root, dated at the archive.
    data = read_tar_xz(members[2][2])
    names = [info.name for info, _ in data]
    dirs = [".", "./usr", "./usr/bin", "./usr/share", "./usr/share/doc", "./usr/share/doc/swarmops"]
    assert names == sorted(dirs + [f"./{p}" for p in paths]), names
    for info, body in data:
        base = info.name.rsplit("/", 1)[-1]
        mode = 0o755 if info.isdir() or base in BINARIES else 0o644
        assert (info.uid, info.gid, info.uname, info.gname) == (0, 0, "root", "root"), info.name
        assert (info.mode, info.mtime) == (mode, MTIME), (info.name, oct(info.mode), info.mtime)
        assert info.isdir() or body == contents[base], f"{info.name}: not the archive's bytes"


def rpm_layout(scratch: Path) -> None:
    """The .rpm: lead, digests, the header's values, requires without weak needs, the payload."""
    contents = files(X86)
    _, rpm = built(archive(scratch / "in"), scratch / "out")
    package = read_rpm(rpm)
    header = package["header"]
    assert (package["arch"], package["name"]) == (1, f"swarmops-{VERSION}-1")

    # 1. The package's values.
    strings = {1000: "swarmops", 1001: VERSION, 1002: "1", 1022: "x86_64", 1021: "linux",
               1014: LICENSE_OF[X86], 1044: f"swarmops-{VERSION}-1.src.rpm",
               1124: "cpio", 1125: "xz"}
    assert {t: value(header, t) for t in strings} == strings
    assert value(header, 1006) == [MTIME], "build time"
    assert value(header, 1047) == ["swarmops", "swarmops(x86-64)"]
    assert value(header, 1113) == [f"{VERSION}-1"] * 2

    # 2. Requires: each library and each version need but the weak GLIBC_2.39, then rpmlib's.
    requires = value(header, 1049)
    want = [f"{LOADER[X86]}()(64bit)", "libc.so.6()(64bit)", "libc.so.6(GLIBC_2.2.5)(64bit)",
            "libc.so.6(GLIBC_2.34)(64bit)", "libgcc_s.so.1()(64bit)",
            "libgcc_s.so.1(GCC_3.0)(64bit)", "rpmlib(CompressedFileNames)",
            "rpmlib(FileDigests)", "rpmlib(PayloadFilesHavePrefix)", "rpmlib(PayloadIsXz)"]
    assert requires == want, requires

    # 3. Files: the doc directory and seven files, in path order, digests of their bytes.
    paths = [f"/usr/bin/{n}" for n in BINARIES] + ["/usr/share/doc/swarmops"]
    paths += [f"/usr/share/doc/swarmops/{n}" for n in DOCS]
    dirnames, bases, indexes = value(header, 1118), value(header, 1117), value(header, 1116)
    assert [dirnames[i] + b for i, b in zip(indexes, bases)] == paths, (dirnames, bases)
    data = [contents.get(p.rsplit("/", 1)[1]) if i != 2 else None for i, p in enumerate(paths)]
    digests = [hashlib.sha256(d).hexdigest() if d is not None else "" for d in data]
    assert value(header, 1035) == digests, "file digests"
    modes = [0o100755, 0o100755, 0o40755] + [0o100644] * 4
    assert value(header, 1030) == modes, [oct(m) for m in value(header, 1030)]
    assert value(header, 1037) == [0, 0, 0, 2, 128, 2, 128], "doc and license flags"
    # rpm --verify checks every attribute of every file, the digests being SHA-256 (8).
    assert header[1045] == (4, [0xFFFFFFFF] * len(paths)), "file verify flags"
    assert header[5011] == (4, [8]), "file digest algorithm"

    # 4. The payload: the same files, prefixed with `.`, dated at the archive.
    cpio = package["cpio"]
    assert [c[0] for c in cpio] == [f".{p}" for p in paths], [c[0] for c in cpio]
    for (name, mode, nlink, mtime, body), want_mode, want_data in zip(cpio, modes, data):
        assert (mode, mtime, nlink) == (want_mode, MTIME, 2 if want_data is None else 1), name
        assert body == (want_data or b""), f"{name}: not the archive's bytes"


def aarch64(scratch: Path) -> None:
    """An aarch64 archive: arm64 and aarch64, its own loader, without x86_64-only crates."""
    deb, rpm = built(archive(scratch / "in", target=ARM), scratch / "out")
    control = {info.name: data for info, data in read_tar_xz(read_ar(deb)[1][2])}
    assert b"Architecture: arm64\n" in control["./control"]
    package = read_rpm(rpm)
    header = package["header"]
    assert package["arch"] == 19, "lead archnum"
    assert value(header, 1022) == "aarch64"
    assert value(header, 1047) == ["swarmops", "swarmops(aarch-64)"]
    assert value(header, 1014) == LICENSE_OF[ARM]
    assert f"{LOADER[ARM]}()(64bit)" in value(header, 1049)
    assert sorted(p.name for p in (scratch / "out").iterdir()) == [
        f"swarmops-{VERSION}-1.aarch64.rpm", f"swarmops_{VERSION}_arm64.deb"]


def deterministic(scratch: Path) -> None:
    """The same entries give the same packages, byte for byte, however the archive was gzip'd."""
    first = built(archive(scratch / "a"), scratch / "out-a")
    second = built(archive(scratch / "b"), scratch / "out-b")
    third = built(archive(scratch / "c", level=1), scratch / "out-c")
    assert first == second, "two builds of one archive differ"
    assert first == third, "the archive's compression changed the packages"


def prerelease(scratch: Path) -> None:
    """0.2.0-rc.1 is 0.2.0~rc.1 inside both packages; the file names keep the release's version."""
    version = "9.9.9-rc.1"
    deb, rpm = built(archive(scratch / "in", version=version), scratch / "out")
    control = {i.name: d for i, d in read_tar_xz(read_ar(deb)[1][2])}["./control"]
    assert b"Version: 9.9.9~rc.1\n" in control, control
    assert value(read_rpm(rpm)["header"], 1001) == "9.9.9~rc.1"
    assert sorted(p.name for p in (scratch / "out").iterdir()) == [
        f"swarmops-{version}-1.x86_64.rpm", f"swarmops_{version}_amd64.deb"]


def drop(name: str) -> Callable[[dict[str, bytes]], None]:
    return lambda contents: contents.pop(name)


def put(name: str, data: bytes) -> Callable[[dict[str, bytes]], None]:
    return lambda contents: contents.__setitem__(name, data)


def duplicate(name: str) -> Callable[[list[Member]], None]:
    """A second member of the file `name`, the same as the first."""
    def edit(packed: list[Member]) -> None:
        packed.append(next(m for m in packed if m[0].name.endswith(f"/{name}")))
    return edit


def retype(suffix: str, kind: bytes, linkname: str = "") -> Callable[[list[Member]], None]:
    """The member whose name ends with `suffix` made of type `kind`, with no data."""
    def edit(packed: list[Member]) -> None:
        for i, (info, _) in enumerate(packed):
            if info.name.endswith(suffix):
                info.type, info.linkname, info.size = kind, linkname, 0
                packed[i] = (info, b"" if kind == tarfile.REGTYPE else None)
    return edit


# The refusals: (label, archive arguments, a line of the error).
REFUSALS = (
    ("an entry too many", {"change": put("SECURITY.md", b"x")}, "does not hold exactly"),
    ("an entry missing", {"change": drop("README.md")}, "does not hold exactly"),
    ("an entry twice", {"members": duplicate("README.md")}, "does not hold exactly"),
    ("a symlink entry", {"members": retype("/README.md", tarfile.SYMTYPE, "LICENSE")},
     "is not a directory of regular files"),
    ("the directory a file", {"members": retype(f"-{X86}", tarfile.REGTYPE)},
     "is not a directory of regular files"),
    ("entries dated apart", {"dates": lambda p: MTIME + p.endswith("so")}, "not all dated alike"),
    ("the other machine's binary", {"change": put("so", binary(ARM, "so"))},
     "for ELF machine 183"),
    ("a library no package has",
     {"change": put("so", elf(62, ["libc.so.6", "libssl.so.3"],
                              [("libc.so.6", GLIBC_NEEDS)]))},
     "libssl.so.3, which packages.py maps to no package"),
    ("a static binary", {"change": put("so", elf(62, [], [], dynamic=False))},
     "has no dynamic section"),
    ("no glibc version",
     {"change": lambda c: c.update(so=elf(62, ["libc.so.6"], []),
                                   swarmops=elf(62, ["libc.so.6"], []))},
     "need no glibc version"),
    ("not an ELF file", {"change": put("so", b"#!/bin/sh\necho swarmops 9.9.9\n")},
     "not a 64-bit little-endian ELF"),
    ("a dynamic section without DT_STRTAB",
     {"change": put("so", elf(62, ["libc.so.6"], [("libc.so.6", GLIBC_NEEDS)], without=(5,)))},
     "no string table (DT_STRTAB)"),
    ("a truncated ELF file", {"change": put("so", binary(X86, "so")[:100])},
     "not an ELF file this script reads"),
    ("no crate in THIRD-PARTY-LICENSES", {"change": put("THIRD-PARTY-LICENSES", b"none\n")},
     "lists no crate"),
    ("build metadata", {"version": "9.9.9+build.1"}, "is not X.Y.Z or X.Y.Z-pre"),
    ("a target with no build", {"name": "swarmops-9.9.9-riscv64gc-unknown-linux-gnu.tar.gz"},
     "is not named swarmops-<version>-<target>.tar.gz"),
)


def refusals(scratch: Path) -> None:
    """Each refusal exits 1 with its `::error::` line and writes nothing."""
    for i, (label, arguments, message) in enumerate(REFUSALS):
        out = scratch / f"out-{i}"
        done = build(archive(scratch / f"in-{i}", **arguments), out)
        assert done.returncode == 1, f"{label}: exited {done.returncode}: {done.stderr}"
        assert "::error::" in done.stderr and message in done.stderr, f"{label}: {done.stderr}"
        assert not out.exists() or not any(out.iterdir()), f"{label}: wrote {list(out.iterdir())}"


def dpkg_deb(scratch: Path) -> None:
    """dpkg-deb reads the .deb's fields and extracts the archive's files."""
    deb_path = scratch / "out" / f"swarmops_{VERSION}_amd64.deb"
    built(archive(scratch / "in"), scratch / "out")
    field = subprocess.run(["dpkg-deb", "--field", str(deb_path), "Version", "Depends"],
                           capture_output=True, text=True, check=True).stdout
    assert field == f"Version: {VERSION}\nDepends: libc6 (>= 2.34), libgcc-s1\n", field
    subprocess.run(["dpkg-deb", "--info", str(deb_path)], capture_output=True, check=True)
    subprocess.run(["dpkg-deb", "-x", str(deb_path), str(scratch / "root")], check=True)
    for name, data in files(X86).items():
        where = "usr/bin" if name in BINARIES else "usr/share/doc/swarmops"
        assert (scratch / "root" / where / name).read_bytes() == data, f"dpkg-deb -x: {name}"


CASES: tuple[tuple[str, Callable[[Path], None]], ...] = (
    ("the .deb's layout", deb_layout),
    ("the .rpm's layout and digests", rpm_layout),
    ("aarch64", aarch64),
    ("byte for byte", deterministic),
    ("a pre-release", prerelease),
    (f"{len(REFUSALS)} refusals", refusals),
)


def main() -> int:
    cases = list(CASES)
    skipped = []
    if shutil.which("dpkg-deb"):
        cases.append(("dpkg-deb reads it", dpkg_deb))
    else:
        skipped.append("dpkg-deb reads it (no dpkg-deb)")

    failures = []
    for label, case in cases:
        with tempfile.TemporaryDirectory(prefix="test-packages-") as scratch:
            try:
                case(Path(scratch))
            except Exception as error:  # a case's crash is its failure, not the run's
                failures.append(f"{label}: {type(error).__name__}: {error}")

    print(f"test-packages: {len(cases) - len(failures)} of {len(cases)} cases passed")
    if skipped:
        print(f"test-packages: skipped: {'; '.join(skipped)}")
    for failure in failures:
        print(f"test-packages: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
