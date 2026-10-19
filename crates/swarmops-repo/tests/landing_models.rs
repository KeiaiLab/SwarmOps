//! The four landing models M4 measures before it builds one (decision 10; the M4 plan's Task
//! 1): their prototypes (`swarmops_repo::land_models`, behind `test-hooks`), each publishing a
//! changeset prepared on the head a land read, by its own rule for a head that moved since.
//!
//! - CasLoop, the yardstick: the insert and a compare-and-swap of the target from that head,
//!   in one frame; a moved head records nothing and goes back to the lander.
//! - Pushrebase: the lock holder reads the target again and relocates a change whose paths are
//!   disjoint from those the target gained onto the moved head, in the same transaction.
//! - Metalog: Pushrebase, and the overlapping files merged in the lock.
//! - OpHeads: a land records its changeset under an op-head ref of its own, with no
//!   compare-and-swap of the target, and the next transaction settles every recorded land onto
//!   the target, conflicts left as data.
//!
//! Every test owns a fresh store, under the order checker.

use std::collections::BTreeSet;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, LeafKind, RepoPath};
use swarmops_repo::land_models::{Model, Published, toy_merge};
use swarmops_repo::{Advance, Change, LocalRepo};
use swarmops_store::{MetaStore, RealFs};
use swarmops_testkit::faults::checked;
use swarmops_testkit::repo::{path, put, request};
use swarmops_testkit::store::temp_dir;
use tempfile::TempDir;

/// Who lands, as the ref moves name them.
const ACTOR: &str = "test/lander";

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, camino::Utf8PathBuf, LocalRepo) {
    let (dir, root) = temp_dir();
    let root = root.join("store");
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs)).unwrap();
    (dir, root, repo)
}

/// Commits `changes` on `parents`, moving `main` to the new changeset.
fn advance(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    let mut req = request(parents, changes, "a test commit");
    req.advance = Some(Advance {
        name: "main".to_owned(),
        actor: ACTOR.to_owned(),
    });
    repo.commit(req).unwrap().id
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// What ref `name` names, if it exists.
fn target(repo: &LocalRepo, name: &str) -> Option<ChangesetId> {
    repo.get_ref(name).unwrap().map(|state| state.target)
}

/// Each file `files` names as a put of a regular file.
fn puts(files: &[(&str, &[u8])]) -> Vec<(RepoPath, LeafKind, Vec<u8>)> {
    files
        .iter()
        .map(|(p, content)| (path(p), LeafKind::File, content.to_vec()))
        .collect()
}

/// A journal record as a test compares it: its kind and its key (a changeset's id, a ref's name).
type Entry = (&'static str, Vec<u8>);

/// Every frame of the journal at `root`, as its records' [`Entry`]s, in journal order.
fn frames(root: &Utf8Path) -> Vec<Vec<Entry>> {
    let store = MetaStore::open(root).unwrap();
    let entries = store
        .read()
        .unwrap()
        .journal_after(None, usize::MAX)
        .unwrap();
    let mut frames: Vec<(u64, Vec<Entry>)> = Vec::new();
    for entry in entries {
        match frames.last_mut() {
            Some((seq, records)) if *seq == entry.seq => records.push((entry.kind, entry.key)),
            _ => frames.push((entry.seq, vec![(entry.kind, entry.key)])),
        }
    }
    frames.into_iter().map(|(_, records)| records).collect()
}

/// A frame's record of changeset `id`.
fn changeset_record(id: ChangesetId) -> Entry {
    ("changeset", id.0.0.to_vec())
}

/// A frame's record of a move of ref `name`.
fn ref_move(name: &str) -> Entry {
    ("ref_move", name.as_bytes().to_vec())
}

/// The content of the file at `p` in changeset `cs`.
fn file_at(repo: &LocalRepo, cs: ChangesetId, p: &str) -> Vec<u8> {
    repo.read_blob_at(cs, &path(p)).unwrap().expect("a file").2
}

/// Ten lines, `line <l>\n`.
fn registry_lines() -> Vec<String> {
    (0..10).map(|l| format!("line {l}\n")).collect()
}

/// [`registry_lines`], as a file.
fn registry() -> Vec<u8> {
    registry_lines().concat().into_bytes()
}

/// [`registry`] with line `l` replaced by `text` and a line break.
fn registry_with(l: usize, text: &str) -> Vec<u8> {
    let mut lines = registry_lines();
    lines[l] = format!("{text}\n");
    lines.concat().into_bytes()
}

/// Asserts that the store at `root` verifies clean, objects re-hashed.
#[track_caller]
fn assert_verifies(repo: &LocalRepo) {
    let report = repo.verify(true).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

#[test]
fn toy_merge_decides_line_replacements() {
    let merge = |base: &str, ours: &str, theirs: &str| {
        toy_merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes())
            .map(|merged| String::from_utf8(merged).unwrap())
    };
    let some = |s: &str| Some(s.to_owned());
    // Equal sides, or one side unchanged, decide at once, whatever the line counts.
    assert_eq!(merge("a\n", "x\ny\n", "x\ny\n"), some("x\ny\n"));
    assert_eq!(merge("a\nb\n", "a\nb\n", "a\nb\nc\n"), some("a\nb\nc\n"));
    assert_eq!(merge("a\nb\n", "a\nb\nc\n", "a\nb\n"), some("a\nb\nc\n"));
    // Distinct lines changed on each side merge, adjacent ones too.
    assert_eq!(
        merge("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n"),
        some("A\nb\nC\n")
    );
    assert_eq!(merge("a\nb\n", "A\nb\n", "a\nB\n"), some("A\nB\n"));
    // A line changed the same way on both sides is taken once.
    assert_eq!(
        merge("a\nb\nc\n", "A\nb\nc\n", "A\nb\nC\n"),
        some("A\nb\nC\n")
    );
    // One line changed differently on both sides does not merge.
    assert_eq!(merge("a\nb\n", "A\nb\n", "X\nb\n"), None);
    // Nor do sides with differing line counts.
    assert_eq!(merge("a\nb\n", "a\nb\nc\n", "A\nb\n"), None);
    assert_eq!(merge("a\nb\n", "A\nb\n", "a\n"), None);
    // The final line need not end with a line break.
    assert_eq!(merge("a\nb", "A\nb", "a\nB"), some("A\nB"));
}

#[test]
fn every_model_publishes_onto_an_unmoved_head_in_one_frame() {
    for model in [Model::CasLoop, Model::Pushrebase, Model::Metalog] {
        let (_dir, root, repo) = new_store();
        let base = advance(&repo, &[], vec![put("a.txt", b"a\n")]);
        let p = repo
            .prepare(
                "main",
                base,
                puts(&[("b.txt", b"b\n")]),
                vec![],
                ACTOR,
                vec![],
            )
            .unwrap();
        let before = frames(&root).len();
        let published = repo.publish_with(model, &p).unwrap().value;
        let landed = Published::Landed {
            id: p.id,
            onto: base,
            relocated: false,
        };
        assert_eq!(published, landed, "{model:?}");
        assert_eq!(main_head(&repo), p.id, "{model:?}");
        let new = &frames(&root)[before..];
        assert_eq!(
            new,
            [vec![changeset_record(p.id), ref_move("main")]],
            "{model:?}"
        );
        assert_eq!(file_at(&repo, p.id, "b.txt"), b"b\n");
        assert_verifies(&repo);
    }

    // OpHeads records the land under its op head, and leaves the target alone until a
    // transaction settles it.
    let (_dir, root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("a.txt", b"a\n")]);
    let op_head = "release/oph-t-1";
    let metadata = vec![("oph".to_owned(), op_head.as_bytes().to_vec())];
    let p = repo
        .prepare(
            "main",
            base,
            puts(&[("b.txt", b"b\n")]),
            vec![],
            ACTOR,
            metadata,
        )
        .unwrap();
    let before = frames(&root).len();
    let published = repo.publish_with(Model::OpHeads, &p).unwrap().value;
    let pending = Published::Pending {
        op_head: op_head.to_owned(),
    };
    assert_eq!(published, pending);
    assert_eq!(main_head(&repo), base, "the target is left alone");
    assert_eq!(target(&repo, op_head), Some(p.id));
    let new = &frames(&root)[before..];
    assert_eq!(new, [vec![changeset_record(p.id), ref_move(op_head)]]);

    let settled = repo.settle_op_heads().unwrap().value;
    assert_eq!(settled, 1);
    assert_eq!(main_head(&repo), p.id, "main moved onto it");
    assert_eq!(target(&repo, op_head), None, "its op head is gone");
    let new = &frames(&root)[before + 1..];
    assert_eq!(new, [vec![ref_move("main"), ref_move(op_head)]]);
    assert_verifies(&repo);
}

#[test]
fn the_cas_loop_reports_a_moved_head_and_records_nothing() {
    let (_dir, root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("a.txt", b"a\n")]);
    let p = repo
        .prepare(
            "main",
            base,
            puts(&[("b.txt", b"b\n")]),
            vec![],
            ACTOR,
            vec![],
        )
        .unwrap();
    let moved = advance(&repo, &[base], vec![put("c.txt", b"c\n")]);
    let (before, changesets) = (frames(&root).len(), repo.info().unwrap().changesets);
    let published = repo.publish_with(Model::CasLoop, &p).unwrap().value;
    assert_eq!(published, Published::Moved { actual: moved });
    assert_eq!(frames(&root).len(), before, "no frame");
    assert_eq!(repo.info().unwrap().changesets, changesets);
    assert_eq!(repo.changeset(p.id).unwrap(), None);
    assert_eq!(main_head(&repo), moved);
}

#[test]
fn pushrebase_relocates_a_disjoint_change_onto_the_moved_head() {
    for model in [Model::Pushrebase, Model::Metalog] {
        let (_dir, root, repo) = new_store();
        let files = vec![put("a.txt", b"a\n"), put("docs/x", b"x\n")];
        let base = advance(&repo, &[], files);
        let changes = puts(&[("b.txt", b"b\n"), ("docs/y", b"y\n")]);
        let p = repo
            .prepare("main", base, changes, vec![path("a.txt")], ACTOR, vec![])
            .unwrap();
        let actual = advance(&repo, &[base], vec![put("c.txt", b"c\n")]);
        let before = frames(&root).len();
        let published = repo.publish_with(model, &p).unwrap().value;
        let Published::Landed {
            id,
            onto,
            relocated,
        } = published
        else {
            panic!("{model:?}: {published:?}");
        };
        assert_eq!((onto, relocated), (actual, true), "{model:?}");
        assert_eq!(main_head(&repo), id);
        let new = &frames(&root)[before..];
        assert_eq!(new, [vec![changeset_record(id), ref_move("main")]]);
        let relocated = repo.changeset(id).unwrap().expect("L' is stored").changeset;
        assert_eq!(relocated.parents, [actual]);
        // Its tree is its delta applied to the moved head's: what a commit of the same changes
        // on that head makes.
        let same = vec![
            Change {
                path: path("a.txt"),
                op: swarmops_repo::ChangeOp::Delete,
            },
            put("b.txt", b"b\n"),
            put("docs/y", b"y\n"),
        ];
        let on_actual = repo.commit(request(&[actual], same, "same")).unwrap();
        assert_eq!(relocated.root, on_actual.root, "{model:?}");
        assert_eq!(
            relocated.delta_hash().unwrap(),
            p.changeset.delta_hash().unwrap()
        );
        assert_eq!(repo.changeset(p.id).unwrap(), None, "L is not in the index");
        assert_verifies(&repo);
    }
}

#[test]
fn pushrebase_sends_back_an_overlap_at_a_parent_or_child_path() {
    // (what main gained, what the land puts, whether it overlaps)
    let cases: [(&str, &str, bool); 3] = [
        ("a/b", "a/b/c", true),
        ("a/b/c", "a/b", true),
        ("a/bc", "a/b", false),
    ];
    for (gained, landed, overlaps) in cases {
        let (_dir, root, repo) = new_store();
        let base = advance(&repo, &[], vec![put("a/x", b"x\n")]);
        let p = repo
            .prepare(
                "main",
                base,
                puts(&[(landed, b"l\n")]),
                vec![],
                ACTOR,
                vec![],
            )
            .unwrap();
        let actual = advance(&repo, &[base], vec![put(gained, b"g\n")]);
        let (before, changesets) = (frames(&root).len(), repo.info().unwrap().changesets);
        let published = repo.publish_with(Model::Pushrebase, &p).unwrap().value;
        if overlaps {
            assert_eq!(
                published,
                Published::Overlap { actual },
                "{gained} {landed}"
            );
            assert_eq!(frames(&root).len(), before, "nothing written");
            assert_eq!(repo.info().unwrap().changesets, changesets);
            assert_eq!(main_head(&repo), actual);
        } else {
            assert!(
                matches!(published, Published::Landed { relocated: true, onto, .. } if onto == actual),
                "{gained} {landed}: {published:?}"
            );
            assert_eq!(file_at(&repo, main_head(&repo), gained), b"g\n");
            assert_eq!(file_at(&repo, main_head(&repo), landed), b"l\n");
        }
        assert_verifies(&repo);
    }
}

#[test]
fn metalog_merges_separated_lines_of_a_gained_file_in_the_lock() {
    let (_dir, root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("REGISTRY", &registry())]);
    let ours = registry_with(4, "landed 4");
    let p = repo
        .prepare(
            "main",
            base,
            puts(&[("REGISTRY", &ours)]),
            vec![],
            ACTOR,
            vec![],
        )
        .unwrap();
    let theirs = registry_with(1, "gained 1");
    let actual = advance(&repo, &[base], vec![put("REGISTRY", &theirs)]);
    let before = frames(&root).len();
    let published = repo.publish_with(Model::Metalog, &p).unwrap().value;
    let Published::Landed {
        id,
        onto,
        relocated,
    } = published
    else {
        panic!("{published:?}");
    };
    assert_eq!((onto, relocated), (actual, true));
    let merged = String::from_utf8(file_at(&repo, id, "REGISTRY")).unwrap();
    let lines: Vec<&str> = merged.lines().collect();
    assert_eq!((lines[1], lines[4]), ("gained 1", "landed 4"), "{merged}");
    assert_eq!(lines.len(), 10);
    let relocated = repo.changeset(id).unwrap().unwrap().changeset;
    assert_eq!(relocated.parents, [actual]);
    let new = &frames(&root)[before..];
    assert_eq!(new, [vec![changeset_record(id), ref_move("main")]]);
    assert_verifies(&repo);

    // Pushrebase sends the same land back.
    let (_dir, _root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("REGISTRY", &registry())]);
    let p = repo
        .prepare(
            "main",
            base,
            puts(&[("REGISTRY", &ours)]),
            vec![],
            ACTOR,
            vec![],
        )
        .unwrap();
    let actual = advance(&repo, &[base], vec![put("REGISTRY", &theirs)]);
    let published = repo.publish_with(Model::Pushrebase, &p).unwrap().value;
    assert_eq!(published, Published::Overlap { actual });
}

#[test]
fn op_heads_leave_a_conflict_as_data() {
    let (_dir, _root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("REGISTRY", &registry())]);
    let record = |onto: ChangesetId, n: u64, text: &str| {
        let op_head = format!("release/oph-t-{n}");
        let metadata = vec![("oph".to_owned(), op_head.clone().into_bytes())];
        let content = registry_with(0, text);
        let p = repo
            .prepare(
                "main",
                onto,
                puts(&[("REGISTRY", &content)]),
                vec![],
                ACTOR,
                metadata,
            )
            .unwrap();
        let published = repo.publish_with(Model::OpHeads, &p).unwrap().value;
        assert_eq!(published, Published::Pending { op_head });
        p.id
    };
    let one = record(base, 1, "one");
    // The next record settles the first: main moves onto it.
    let two = record(base, 2, "two");
    assert_eq!(main_head(&repo), one);
    assert_eq!(target(&repo, "release/oph-t-1"), None);
    // Line 0 changed differently on both sides: the second stays data, under a conflicted ref.
    assert_eq!(repo.settle_op_heads().unwrap().value, 0);
    assert_eq!(main_head(&repo), one);
    assert_eq!(target(&repo, "release/oph-t-2"), None);
    assert_eq!(target(&repo, "release/ophc-t-2"), Some(two));
    let refs: BTreeSet<String> = repo
        .list_refs()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        refs,
        BTreeSet::from(["main".to_owned(), "release/ophc-t-2".to_owned()])
    );

    // Its lander records it again on main, re-merged by hand here: the conflicted ref goes in
    // the same frame, and the next settle lands it.
    let again = record(one, 2, "one and two");
    assert_eq!(target(&repo, "release/ophc-t-2"), None);
    assert_eq!(target(&repo, "release/oph-t-2"), Some(again));
    assert_eq!(repo.settle_op_heads().unwrap().value, 1);
    assert_eq!(main_head(&repo), again);
    assert_eq!(
        file_at(&repo, again, "REGISTRY"),
        registry_with(0, "one and two")
    );
    assert_verifies(&repo);
}
