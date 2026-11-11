//! The sources of every crate answer FORMAT 3 wherever they ask for FORMAT 2 (the FORMAT 3
//! design §2.1): FORMAT 3 shares FORMAT 2's journal, offset and ref file layouts, so a gate written
//! as `format == StoreFormat::V2`, or a match arm `_ => StoreFormat::V2`, answers FORMAT 3 by
//! accident. A gate on those layouts asks the format's journal family; a match names each format.

use std::fs;
use std::path::{Path, PathBuf};

/// A place in the sources: a file and a line number, from 1.
type Place = (PathBuf, usize);

/// Whether `c` may be part of an identifier.
fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `text` without the path segments it ends in (`a::b::`), and those segments' names in order.
fn strip_path(text: &str) -> (&str, Vec<&str>) {
    let mut rest = text;
    let mut segments = Vec::new();
    while let Some(before) = rest.strip_suffix("::") {
        let start = before
            .char_indices()
            .rev()
            .take_while(|&(_, c)| ident_char(c))
            .last()
            .map_or(before.len(), |(at, _)| at);
        segments.insert(0, &before[start..]);
        rest = &before[..start];
    }
    (rest, segments)
}

/// Whether the code before a `V2` on its line, `before`, makes it a bare FORMAT 2 gate: an
/// `==` or `!=` against `StoreFormat::V2` (any crate path before it), or a wildcard arm
/// `_ => V2` or `_ => StoreFormat::V2`.
fn bare_v2(before: &str) -> bool {
    let (rest, segments) = strip_path(before);
    let rest = rest.trim_end();
    let of_store_format = segments.last() == Some(&"StoreFormat");
    if rest.ends_with("==") || rest.ends_with("!=") {
        return of_store_format;
    }
    let Some(arm) = rest.strip_suffix("=>") else {
        return false;
    };
    let arm = arm.trim_end();
    let wildcard = arm
        .strip_suffix('_')
        .is_some_and(|head| !head.ends_with(ident_char));
    wildcard && (segments.is_empty() || of_store_format)
}

/// The numbers of the lines of `source` that hold a bare FORMAT 2 gate ([`bare_v2`]), comments
/// passed over.
fn offending_lines(source: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    for (n, line) in (1..).zip(source.lines()) {
        // 1. The code of the line, without a comment.
        let code = line.split_once("//").map_or(line, |(code, _)| code);

        // 2. Each `V2` that is a word of its own.
        let found = code.match_indices("V2").any(|(at, _)| {
            let alone_before = !code[..at].ends_with(ident_char);
            let alone_after = !code[at + 2..].starts_with(ident_char);
            alone_before && alone_after && bare_v2(&code[..at])
        });
        if found {
            lines.push(n);
        }
    }
    lines
}

/// The `.rs` files under `dir`, recursively.
fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// Every bare FORMAT 2 gate in the `src/` of each crate of the workspace at `root`.
fn offenders(root: &Path) -> Vec<Place> {
    let mut files = Vec::new();
    for krate in fs::read_dir(root.join("crates")).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    let mut found = Vec::new();
    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        for line in offending_lines(&source) {
            found.push((file.clone(), line));
        }
    }
    found
}

/// No crate's sources gate on FORMAT 2 alone: each such gate answers FORMAT 3 explicitly.
#[test]
fn no_bare_v2_comparison_in_sources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let found = offenders(&root);
    assert!(
        found.is_empty(),
        "these lines gate on FORMAT 2 alone and answer FORMAT 3 by accident: {found:?}"
    );
}

/// The detector catches each planted shape, with or without a crate path, and passes the
/// shapes that answer FORMAT 3 and the comments.
#[test]
fn the_v2_detector_catches_a_planted_one() {
    let planted = [
        "if format == StoreFormat::V2 {",
        "let two = layout.format() != swarmops_store::StoreFormat::V2;",
        "    _ => StoreFormat::V2,",
        "    _ => V2,",
        "    _ => crate::format::StoreFormat::V2,",
    ];
    for line in planted {
        assert_eq!(offending_lines(line), [1], "{line:?} is not caught");
    }

    let kept = [
        "if format.journal_family() == JournalFamily::Two {",
        "    StoreFormat::V2 | StoreFormat::V3 => ENTRY_LEN_V2,",
        "    2 => StoreFormat::V2,",
        "// a gate written as `format == StoreFormat::V2` answers FORMAT 3",
        "if format == StoreFormat::V20 {",
        "    other => panic!(\"{other:?}\"),",
        "if version == V2_COUNT {",
    ];
    for line in kept {
        assert_eq!(
            offending_lines(line),
            Vec::<usize>::new(),
            "{line:?} is caught"
        );
    }

    // Found in a file of a planted tree, at its line.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("crates/planted/src/inner");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("gate.rs"),
        "fn f() {}\nlet v2 = f == StoreFormat::V2;\n",
    )
    .unwrap();
    assert_eq!(offenders(dir.path()), [(src.join("gate.rs"), 2)]);
}
