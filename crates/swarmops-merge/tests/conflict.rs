//! The conflict types of the three-way merge (storage spec §4): every kind carries the name the
//! operations spec's conflict report gives it, and a closure is a content merger.

use std::cell::Cell;

use swarmops_core::LeafKind;
use swarmops_merge::conflict::{ConflictHunk, ConflictKind, ContentMerge, ContentMerger};

/// The names operations spec §3 gives the kinds of a conflicting file, in its order: the
/// backquoted names its sentence "`kind` 는 `content`·`binary`·… 일곱 가운데 하나이고" lists.
fn ops_spec_kinds() -> Vec<&'static str> {
    const SPEC: &str = include_str!(
        "../../../docs/superpowers/specs/2026-10-15-swarmops-operations-landing-design.md"
    );
    let start = SPEC.find("\n## 3. ").expect("operations spec §3's heading");
    let section = &SPEC[start..];
    let section = section[1..]
        .find("\n## ")
        .map_or(section, |end| &section[..=end]);
    let sentence = section
        .lines()
        .find_map(|line| line.strip_prefix("`kind` 는 "))
        .expect("operations spec §3 names the kinds of a conflicting file");
    let (list, _) = sentence.split_once(' ').expect("the list ends at a space");
    list.split('·')
        .map(|name| {
            name.strip_prefix('`')
                .and_then(|name| name.strip_suffix('`'))
                .unwrap_or_else(|| panic!("{name:?} is not a backquoted name"))
        })
        .collect()
}

/// One conflict of each kind, in the order operations spec §3 lists them. The `match` has no
/// wildcard: a kind added to `ConflictKind` fails to compile here until it is listed.
fn one_of_each() -> Vec<ConflictKind> {
    let hunk = || ConflictHunk {
        base: 2..4,
        ours: 2..3,
        theirs: 2..2,
        base_text: b"c\nd\n".to_vec(),
        ours_text: b"x\n".to_vec(),
        theirs_text: Vec::new(),
    };
    let kinds = vec![
        ConflictKind::Content {
            hunks: vec![hunk()],
        },
        ConflictKind::Binary,
        ConflictKind::DeleteModify,
        ConflictKind::ModifyDelete,
        ConflictKind::AddAdd {
            hunks: vec![hunk()],
        },
        ConflictKind::Mode {
            ours: LeafKind::Exec,
            theirs: LeafKind::Symlink,
        },
        ConflictKind::TypeChange,
    ];
    for kind in &kinds {
        match kind {
            ConflictKind::Content { .. }
            | ConflictKind::Binary
            | ConflictKind::DeleteModify
            | ConflictKind::ModifyDelete
            | ConflictKind::AddAdd { .. }
            | ConflictKind::Mode { .. }
            | ConflictKind::TypeChange => {}
        }
    }
    kinds
}

/// Each kind's name is the one operations spec §3's conflict report gives it (a file's `kind`, an
/// entry of the report's `kinds`), and the report names no kind the merge lacks.
#[test]
fn every_kind_has_its_ops_spec_name() {
    let names: Vec<&str> = one_of_each().iter().map(ConflictKind::name).collect();
    assert_eq!(names, ops_spec_kinds());
}

/// A closure that captures is a content merger, called with the base's, ours' and theirs'
/// contents in that order: here one that takes theirs where ours left the base alone, and counts
/// its calls.
#[test]
fn a_closure_is_a_content_merger() {
    let calls = Cell::new(0);
    let merge: ContentMerger<'_> = &|base, ours, theirs| {
        calls.set(calls.get() + 1);
        if ours == base {
            ContentMerge::Clean(theirs.to_vec())
        } else {
            ContentMerge::Binary
        }
    };
    assert_eq!(
        merge(b"a\n", b"a\n", b"b\n"),
        ContentMerge::Clean(b"b\n".to_vec())
    );
    assert_eq!(merge(b"a\n", b"c\n", b"b\n"), ContentMerge::Binary);
    assert_eq!(calls.get(), 2);
}
