"""Writes THIRD-PARTY-LICENSES, the licenses of the third-party crates in the release binaries.

    python3 -I scripts/third-party-licenses.py          # writes THIRD-PARTY-LICENSES
    python3 -I scripts/third-party-licenses.py --check  # writes nothing; fails if it would differ

The release builds `swarmops` and `so` with `cargo build --release --locked -p swarmops-cli
--bins`, for each target of release.yml's build matrix, without the `test-hooks` feature. This
script lists every third-party crate that build compiles, by `cargo tree` on Cargo.lock, and takes
each one's license files from its package, which `cargo metadata` finds in cargo's registry cache.
`just check` runs `--check`, so that the file cannot fall behind Cargo.lock, deny.toml or the
targets. It reads only what cargo has already fetched, and needs nothing but Python 3.11 or newer.

A crate is used under the licenses of its expression that deny.toml's `[licenses] allow` list
accepts, as cargo-deny accepts it: where the expression offers a choice, the one whose licenses
that list names first. Its notices are the files at its package root whose names start with
LICENSE, LICENCE, COPYING, COPYRIGHT, NOTICE or UNLICENSE, but for those that carry only the text
of a license its expression offers and it is not used under. A crate that compiles C (a
build-dependency on `cc`), or has such files below its package root, can carry notices that this
rule does not find: REVIEWED says, for each version read by hand, what is taken from which file,
and any other version fails until it has been read.
"""

import argparse
import difflib
import json
import os
import re
import subprocess
import sys
import textwrap
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "THIRD-PARTY-LICENSES"
PACKAGE = "swarmops-cli"
CRATES_IO = (
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
)
WIDTH = 100

# The crate versions read by hand. Each names its files below the package root and any other file
# with a notice, and what is taken from each: ("text", license, note) the whole file,
# ("header", license, note) its leading comment, or ("omit", license, reason) nothing. Every
# file must still carry the license named here.
REVIEWED = {
    # c/ holds BLAKE3's C and assembly, which carry no notice of their own: the crate's license
    # files cover them.
    ("blake3", "1.8.7"): {},
    ("faster-hex", "0.10.1"): {
        "LICENSE-THIRD-PARTY/Rust Project Developers": (
            "text", "MIT", "stdsimd's, which its encoder is based on",
        ),
        "LICENSE-THIRD-PARTY/fast-hex": (
            "text", "MIT", "fast-hex's, which its AVX2 decoder is modified from",
        ),
    },
    # src/unicode_tables/ is generated from the Unicode Character Database, whose terms of use
    # LICENSE-UNICODE carries: the tables of the gitignore patterns' regular expressions
    # (globset, through regex-automata), from M5's workspace scan.
    ("regex-syntax", "0.8.11"): {
        "src/unicode_tables/LICENSE-UNICODE": (
            "text", "Unicode-DFS-2016", "the Unicode data its Unicode tables are generated from",
        ),
    },
    # zstd/ is zstd's C library, which zstd-sys compiles with zstd's default features (its
    # dictionary builder and legacy decoders among them), under BSD-3-Clause or GPL-2.0 at the
    # user's option. Its files carry that dual-license header, but for the dictionary builder's
    # libdivsufsort-lite, whose files carry an MIT notice of their own.
    ("zstd-sys", "2.1.2+zstd.1.5.7"): {
        "zstd/LICENSE": ("text", "BSD-3-Clause", "zstd's C library, which zstd-sys compiles"),
        "zstd/COPYING": ("omit", "GPL-2.0", "zstd's other license, which is not taken"),
        "zstd/lib/dictBuilder/divsufsort.c": (
            "header", "MIT", "libdivsufsort-lite, in zstd's dictionary builder",
        ),
    },
}

NOTICE_NAME = re.compile(r"(?i)^(licen[cs]e|copying|copyright|notice|unlicense)")
# A file named for a license, which a crate not used under that license may leave out; any other
# name, such as LICENSE-MIT-tracing-forest, can be a third party's notice and is never left out.
LICENSE_FILE_NAME = re.compile(
    r"(?i)^(licen[cs]e|copying|unlicense)"
    r"([-_.](mit0?|apache|apache2|a2|a2llvm|apache-2\.0|apache-2\.0_with_llvm-exception|cc0"
    r"|zlib|unicode|bsd|bsd-2-clause|bsd-3-clause|isc|mpl|mpl-2\.0|unlicense|gpl))?(\.md|\.txt)?$"
)
GRANT = (
    "permission is hereby granted, free of charge, to any person obtaining a copy of this software"
)
CONDITION = "the above copyright notice and this permission notice shall be included"
REDISTRIBUTION = (
    "redistribution and use in source and binary forms, with or without modification, are"
    " permitted provided that the following conditions are met"
)
APACHE_TERMS = "terms and conditions for use, reproduction, and distribution"
LLVM_EXCEPTION = "llvm exceptions to the apache 2.0 license"
# What identifies each license's text, in a file folded to lower case, with straight quotes and
# single spaces: every phrase of the first tuple, and none of the second.
SIGNATURES = (
    ("Apache-2.0 WITH LLVM-exception", (APACHE_TERMS, LLVM_EXCEPTION), ()),
    ("Apache-2.0", (APACHE_TERMS, "apache license"), (LLVM_EXCEPTION,)),
    ("MIT", (GRANT, CONDITION), ()),
    ("MIT-0", (GRANT,), (CONDITION,)),
    ("BSD-3-Clause", (REDISTRIBUTION, "neither the name"), ()),
    ("BSD-2-Clause", (REDISTRIBUTION,), ("neither the name",)),
    (
        "ISC",
        ("permission to use, copy, modify, and/or distribute this software for any purpose",),
        (),
    ),
    (
        "Zlib",
        (
            "this software is provided 'as-is', without any express or implied warranty",
            "altered source versions must be plainly marked as such",
        ),
        (),
    ),
    ("Unicode-3.0", ("unicode license v3",), ()),
    (
        "Unicode-DFS-2016",
        (
            "unicode, inc. license agreement - data files and software",
            "permission is hereby granted, free of charge, to any person obtaining a copy of the"
            " unicode data files",
        ),
        (),
    ),
    ("CC0-1.0", ("cc0 1.0 universal",), ()),
    ("Unlicense", ("this is free and unencumbered software released into the public domain",), ()),
    ("MPL-2.0", ("mozilla public license version 2.0",), ()),
    ("GPL-2.0", ("gnu general public license", "version 2, june 1991"), ()),
)
QUOTES = str.maketrans({"‘": "'", "’": "'", "“": '"', "”": '"'})


def fail(message):
    print(f"third-party-licenses: {message}", file=sys.stderr)
    sys.exit(1)


def cargo(*args):
    run = subprocess.run(["cargo", *args], cwd=ROOT, capture_output=True, text=True, check=False)
    if run.returncode != 0:
        fail(f"`cargo {' '.join(args)}` failed:\n{run.stderr}")
    return run.stdout


def release_targets():
    workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text(encoding="utf-8")
    targets = sorted(set(re.findall(r"^\s*- target: ([A-Za-z0-9_.-]+)\s*$", workflow, re.M)))
    if not targets:
        fail("release.yml's build matrix names no target (`- target: <triple>`)")
    return targets


def compiled(target, edges):
    """The (name, version) of each package that `cargo tree` shows for the release build."""
    shown = cargo(
        "tree", "--locked", "-p", PACKAGE, "--target", target, "--edges", edges,
        "--prefix", "none", "--format", "{p}",
    )
    found = set()
    for line in shown.splitlines():
        words = line.split()
        if len(words) < 2 or not words[1].startswith("v"):
            fail(f"cannot read `cargo tree`'s line {line!r}")
        found.add((words[0], words[1][1:]))
    return found


def allowed_licenses():
    with open(ROOT / "deny.toml", "rb") as config:
        licenses = tomllib.load(config).get("licenses", {})
    unread = sorted(set(licenses) - {"version", "allow"})
    if unread:
        fail(f"deny.toml's [licenses] has {', '.join(unread)}, which this script does not read")
    return list(licenses.get("allow", []))


def parse(expression):
    """An SPDX license expression as a tree; `/`, the old separator, is OR."""
    tokens = ["OR" if t == "/" else t for t in re.findall(r"[()/]|[^\s()/]+", expression)]
    position = 0

    def peek():
        return tokens[position] if position < len(tokens) else None

    def take():
        nonlocal position
        position += 1
        return tokens[position - 1] if position <= len(tokens) else None

    def name():
        token = take()
        if token in (None, "(", ")", "AND", "OR", "WITH"):
            raise ValueError(token)
        return token

    def primary():
        if peek() == "(":
            take()
            node = disjunction()
            if take() != ")":
                raise ValueError("(")
            return node
        license_id = name()
        if peek() == "WITH":
            take()
            license_id = f"{license_id} WITH {name()}"
        return ("license", license_id)

    def conjunction():
        nodes = [primary()]
        while peek() == "AND":
            take()
            nodes.append(primary())
        return nodes[0] if len(nodes) == 1 else ("and", nodes)

    def disjunction():
        nodes = [conjunction()]
        while peek() == "OR":
            take()
            nodes.append(conjunction())
        return nodes[0] if len(nodes) == 1 else ("or", nodes)

    tree = disjunction()
    if peek() is not None:
        raise ValueError(peek())
    return tree


def offered(tree):
    kind, value = tree
    return {value} if kind == "license" else set().union(*(offered(node) for node in value))


def choose(tree, allow):
    """The licenses a crate is used under, or None when the allow list accepts no choice."""
    kind, value = tree
    if kind == "license":
        return [value] if value in allow else None
    choices = [choose(node, allow) for node in value]
    if kind == "and":
        if None in choices:
            return None
        return list(dict.fromkeys(license_id for choice in choices for license_id in choice))
    usable = [choice for choice in choices if choice is not None]
    if not usable:
        return None
    return min(usable, key=lambda choice: sorted(allow.index(license_id) for license_id in choice))


def licenses_in(text):
    folded = " ".join(text.lower().translate(QUOTES).split())
    return {
        license_id
        for license_id, present, absent in SIGNATURES
        if all(p in folded for p in present) and not any(p in folded for p in absent)
    }


def clean(text):
    lines = [line.rstrip() for line in text.replace("\r\n", "\n").replace("\r", "\n").split("\n")]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def read_text(path, crate):
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        fail(f"{crate}: cannot read {path.name}: {error}")


def header_comment(text, crate, file):
    comment = re.match(r"\s*/\*(.*?)\*/", text, re.S)
    if comment is None:
        fail(f"{crate}: {file} does not start with a comment")
    return clean("\n".join(re.sub(r"^\s*\* ?", "", line) for line in comment[1].split("\n")))


def notice_files_below_root(directory):
    found = []
    for parent, _, files in os.walk(directory):
        for file in files:
            relative = Path(parent, file).relative_to(directory)
            if len(relative.parts) > 1 and any(NOTICE_NAME.match(part) for part in relative.parts):
                found.append(relative.as_posix())
    return sorted(found)


def notices(package, crate, offers, chosen):
    """A crate's texts, each (license, where, note, text), in the order the file lists them."""
    directory = Path(package["manifest_path"]).parent
    texts = []
    carried = set()
    for entry in sorted(os.listdir(directory)):
        if not NOTICE_NAME.match(entry) or not (directory / entry).is_file():
            continue
        text = clean(read_text(directory / entry, crate))
        found = licenses_in(text)
        if found and not found & set(chosen) and found <= offers:
            if not LICENSE_FILE_NAME.match(entry):
                fail(f"{crate}: {entry} carries only {', '.join(sorted(found))}, which the crate"
                     " is not used under, but its name says it may be a third party's notice:"
                     " read it, and add the version to REVIEWED")
            continue
        carried |= found
        texts.append((" and ".join(sorted(found)) or "Notice", entry, None, text))
    for license_id in chosen:
        if license_id not in carried:
            fail(f"{crate}: no file at its package root carries the text of {license_id}, which"
                 " it is used under: take it from the crate's repository at that version, and"
                 " add it to this script")

    key = (package["name"], package["version"])
    below_root = notice_files_below_root(directory)
    compiles_c = any(d["name"] == "cc" and d["kind"] == "build" for d in package["dependencies"])
    if key not in REVIEWED:
        if compiles_c or below_root:
            what = "compiles C" if compiles_c else f"has {', '.join(below_root)}"
            fail(f"{crate} {what}, which can carry notices of their own: read them, and add the"
                 " version to REVIEWED")
        return texts
    unread = sorted(set(below_root) - set(REVIEWED[key]))
    if unread:
        fail(f"{crate}: REVIEWED does not say what is taken from {', '.join(unread)}")
    for file, (taken, license_id, note) in REVIEWED[key].items():
        if not (directory / file).is_file():
            fail(f"{crate}: REVIEWED names {file}, which its package does not have")
        whole = read_text(directory / file, crate)
        text = header_comment(whole, crate, file) if taken == "header" else clean(whole)
        if license_id not in licenses_in(text):
            fail(f"{crate}: {file} no longer carries the {license_id} that REVIEWED names")
        if taken != "omit":
            where = f"the leading comment of {file}" if taken == "header" else file
            texts.append((license_id, where, note, text))
    return texts


def version_key(version):
    return [(0, int(p), "") if p.isdigit() else (1, 0, p) for p in re.split(r"[.+-]", version)]


def wrap(text, indent=""):
    return textwrap.wrap(
        text, WIDTH, subsequent_indent=indent, break_long_words=False, break_on_hyphens=False
    )


def render(crates, targets, texts):
    command = f"    cargo build --release --locked -p {PACKAGE} --bins --target <target>"
    paragraphs = [
        "`swarmops` and `so` in each SwarmOps release archive are built from SwarmOps' own"
        f" crates, under the MIT License in LICENSE, and from the {len(crates)} third-party"
        " crates below, at the versions Cargo.lock pins: every crate that the release build"
        f" compiles for {' or '.join(targets)}, without the `test-hooks` feature:",
        command,
        "(build) marks a crate that only runs while the binaries are built: a build script's"
        " dependency, or a procedural macro. (<target> only) marks one that only that target"
        " compiles. Each crate's source is on crates.io, at"
        " https://crates.io/crates/<name>/<version>.",
        "Each crate is listed with the license expression its package declares and the license"
        " it is used under, which the [licenses] allow list of deny.toml accepts: where the"
        " expression offers a choice, the one whose licenses that list names first. The texts"
        " follow, each once, with the crates it is from. They are the files of each crate's"
        " package, as crates.io serves it, that carry the text of a license the crate is used"
        " under, its copyright statements and other notices (a Notice carries no license text),"
        " and the notices of code it includes from other projects, such as zstd's C library in"
        " zstd-sys.",
        "The Rust standard library, which every Rust binary links, has notices of its own:"
        " share/doc/rust/COPYRIGHT-library.html in the Rust toolchain that rust-toolchain.toml"
        " pins.",
        "scripts/third-party-licenses.py writes this file from Cargo.lock, and `just check` fails"
        " when the file no longer matches it.",
    ]
    title = "Third-party licenses of the SwarmOps release binaries"
    lines = [title, "=" * len(title), ""]
    for paragraph in paragraphs:
        lines += ([paragraph] if paragraph == command else wrap(paragraph)) + [""]
    heading = f"Crates ({len(crates)})"
    lines += ["", heading, "-" * len(heading), ""]
    for crate in crates:
        marks = "".join(f" ({mark})" for mark in crate["marks"])
        refs = ", ".join(f"[{ref}]" for ref in crate["refs"])
        lines.append(
            f"{crate['name']} {crate['version']}{marks}: {crate['expression']};"
            f" used under {' AND '.join(crate['chosen'])} {refs}"
        )
    heading = f"Texts ({len(texts)})"
    lines += ["", "", heading, "-" * len(heading), ""]
    for number, block in enumerate(texts, start=1):
        groups = {}
        for crate, where, note in block["from"]:
            groups.setdefault((where, note), []).append(crate)
        sources = "; ".join(
            f"{', '.join(names)} ({where}{f': {note}' if note else ''})"
            for (where, note), names in groups.items()
        )
        lines += ["=" * WIDTH, *wrap(f"[{number}] {block['license']}, from {sources}", "    ")]
        lines += ["=" * WIDTH, "", block["text"], "", ""]
    return "\n".join(lines).rstrip("\n") + "\n"


def generate():
    targets = release_targets()
    allow = allowed_licenses()
    platforms = [arg for target in targets for arg in ("--filter-platform", target)]
    metadata = json.loads(cargo("metadata", "--format-version", "1", "--locked", *platforms))
    members = set(metadata["workspace_members"])
    workspace = {(p["name"], p["version"]) for p in metadata["packages"] if p["id"] in members}
    packages = {}
    for package in metadata["packages"]:
        key = (package["name"], package["version"])
        if package["id"] in members:
            continue
        if key in packages:
            fail(f"two packages are {key[0]} {key[1]}: {packages[key]['id']}, {package['id']}")
        packages[key] = package

    built = {target: compiled(target, "normal,build") - workspace for target in targets}
    linked = set().union(*(compiled(target, "normal,no-proc-macro") for target in targets))
    crates = []
    for key in sorted(set().union(*built.values()), key=lambda k: (k[0], version_key(k[1]))):
        crate = f"{key[0]} {key[1]}"
        package = packages.get(key)
        if package is None:
            fail(f"`cargo metadata` does not list {crate}, which `cargo tree` shows")
        if package["source"] not in CRATES_IO:
            fail(f"{crate} is not from crates.io but from {package['source']}")
        if not package["license"]:
            fail(f"{crate} declares no license expression: read its license, and extend this"
                 " script")
        try:
            tree = parse(package["license"])
        except ValueError:
            fail(f"{crate}: cannot read its license expression {package['license']!r}")
        chosen = choose(tree, allow)
        if chosen is None:
            fail(f"{crate}: deny.toml's allow list accepts none of {package['license']!r}")
        marks = [] if key in linked else ["build"]
        only = [target for target in targets if key in built[target]]
        if len(only) < len(targets):
            marks.append(f"{', '.join(only)} only")
        crates.append({
            "name": key[0],
            "version": key[1],
            "marks": marks,
            "expression": package["license"],
            "chosen": chosen,
            "texts": notices(package, crate, offered(tree), chosen),
        })
    built_keys = {(crate["name"], crate["version"]) for crate in crates}
    stale = sorted(f"{name} {version}" for name, version in set(REVIEWED) - built_keys)
    if stale:
        fail(f"REVIEWED names {', '.join(stale)}, which the release does not build: remove it")

    texts = []
    numbers = {}
    for crate in crates:
        crate["refs"] = []
        for license_id, where, note, text in crate["texts"]:
            if text not in numbers:
                texts.append({"license": license_id, "text": text, "from": []})
                numbers[text] = len(texts)
            number = numbers[text]
            texts[number - 1]["from"].append((f"{crate['name']} {crate['version']}", where, note))
            if number not in crate["refs"]:
                crate["refs"].append(number)
    return render(crates, targets, texts), len(crates), len(texts)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--check", action="store_true", help="write nothing, and fail if the file would differ"
    )
    args = parser.parse_args()
    content, crate_count, text_count = generate()
    summary = f"{crate_count} crates, {text_count} texts"
    if not args.check:
        OUTPUT.write_text(content, encoding="utf-8", newline="\n")
        print(f"third-party-licenses: wrote {OUTPUT.name} ({summary})")
        return
    committed = OUTPUT.read_text(encoding="utf-8") if OUTPUT.is_file() else ""
    if committed != content:
        diff = list(difflib.unified_diff(
            committed.splitlines(), content.splitlines(),
            f"{OUTPUT.name} (committed)", f"{OUTPUT.name} (from Cargo.lock)", lineterm="", n=1,
        ))
        print("\n".join(diff[:80]))
        if len(diff) > 80:
            print(f"... and {len(diff) - 80} more lines")
        fail(f"{OUTPUT.name} does not match Cargo.lock, deny.toml and the crates' packages: run"
             " `python3 -I scripts/third-party-licenses.py` and commit the file")
    print(f"third-party-licenses: {OUTPUT.name} matches Cargo.lock ({summary})")


if __name__ == "__main__":
    main()
