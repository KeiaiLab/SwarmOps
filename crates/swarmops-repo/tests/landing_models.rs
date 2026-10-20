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
//! Every prototype test owns a fresh store, under the order checker.
//!
//! # The multi-process harness
//!
//! The rest of the file races landers against each other, as `swarmops-store`'s
//! `tests/contention.rs` races writers: the parent creates a store whose `main` holds the
//! workload's base ([`base_files`]), then runs this test binary again once per lander
//! (testkit's `child` runner: a deadline, a kill and a reap on drop), each told its part by
//! [`DIR`], [`CHILD`] and the run's [`MODEL`], [`WORKLOAD`], [`LANDERS`], [`LANDS`],
//! [`INTERVAL_MS`], [`DURATION_S`] and [`SEED`]. Each lander opens the store, says it is ready
//! and waits for the go; then it lands its changes ([`change_for`]) one after another, in a
//! closed loop ([`INTERVAL_MS`] 0: [`LANDS`] lands, each as soon as the last one ended) or an
//! open one (each land at its slot, one per interval ± 20 %, until [`DURATION_S`] has passed:
//! [`slot`]), and prints one [`LAND_TAG`] line per land ([`LandRecord`]).
//!
//! A land reads `main`, prepares its changeset there (`Repo::prepare`) and publishes it by the
//! run's model (`Repo::publish_with`). Moved or overlapping, it re-merges incrementally (each
//! file of its change compared at the head it prepared on and the new one: unchanged, kept;
//! changed, merged with `toy_merge`, and a file that does not merge ends the land conflicted),
//! backs off from its fourth attempt as ops spec §3 step 7 says ([`backoff`]), and tries again:
//! at most [`MAX_ATTEMPTS`] attempts, the last of which, if it does not land, ends the land
//! exhausted without a re-merge. An op-heads land, recorded, polls its op-head ref every
//! [`POLL`] until a transaction settled it: gone, it finds its changeset on `main` by the
//! metadata that names its op head; conflicted (`release/ophc-…`), it re-merges and records
//! again. A transaction of another lander settles it, or after [`SETTLE_AFTER`] its own
//! lander's `Repo::settle_op_heads`: the model's next transaction, which the end of a run would
//! otherwise never bring. A land that ends without landing deletes the conflicted ref it may
//! have left.
//!
//! Once every lander has exited, the parent settles what an op-heads run left (nothing, if the
//! landers did their part), checks the gate ([`check_invariants`]) and prints the run's
//! [`RUN_TAG`] line ([`Run`]: the cell, the gate, the host's load and raw `fdatasync` at the
//! run's start and end, and [`Stats`]), and the rule a failed gate broke on a `landing-gate: `
//! line.
//!
//! `every_model_lands_a_small_race_without_loss` runs each model in turn with 4 landers × 5
//! lands, in the default gate: on the real filesystem without its flushes (`NoSyncFs`), every
//! process's calls under the order checker, which judges the flushes as made. The three measurements,
//! `landing_models_closed_loop`, `landing_models_open_loop` and `landing_models_large_change`,
//! are ignored: `scripts/landing-models.sh` runs the M4 plan's matrix with them, one run per
//! cell, model and seed, optimized and under the host's heavy-run lock, and `just check-slow`
//! runs each once, unoptimized, at its default cell. Their defaults are the matrix's cells:
//! 32 landers × 100 lands of W-shared in a closed loop; 50 landers, one land each every 2 s
//! for 120 s of W-shared (the cell Decision 1 scores, [`SCORED_CELL`]); 32 landers × 50 lands
//! of W-large; each model in turn unless [`MODEL`] names one, from seed 1 unless [`SEED`]
//! says. They run in fresh directories under `SWARMOPS_BENCH_DIR` (default
//! `~/.local/state/swarmops-bench`, on the native filesystem), and `.config/nextest.toml` runs
//! them one at a time, beside no other contention measurement.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, LeafKind, RepoPath};
use swarmops_repo::land_models::{
    CONFLICTED_PREFIX, Model, OP_HEAD_KEY, OP_HEAD_PREFIX, Publication, Published, Timed, toy_merge,
};
use swarmops_repo::{Advance, Change, LocalRepo, RepoError, StoreError};
use swarmops_store::{FsOps, MetaStore, RealFs};
use swarmops_testkit::child::{CHILD_TIMEOUT, Child, this_test};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::{NoSyncFs, Watched};
use swarmops_testkit::landing::{
    CANDIDATES, Cell, Change as LandChange, Host, LAND_TAG, LandRecord, MAX_ATTEMPTS, MAX_LATENCY,
    OP_HEAD_REFS, PROBE_APPENDS, Pace, REGISTRY, RUN_TAG, Run, SCORED_CELL, Stats, SyncTimer,
    Verdict, Workload, YARDSTICK, backoff, base_files, change_for, check_invariants, decide,
    fdatasync_probe, load1, open_loop_lands, registry_text, slot, tables, tagged, with_line,
};
use swarmops_testkit::measure::base_dir;
use swarmops_testkit::repo::{commit, path, put, request};
use swarmops_testkit::store::temp_dir;
use swarmops_testkit::test_name;
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

/// Set only in a lander: the run's work directory, which holds the store (`store/`) and the
/// barrier files (`ready-<n>`, `go`).
const DIR: &str = "SWARMOPS_TEST_LANDING_DIR";

/// Set with [`DIR`]: the lander's number, from 0.
const CHILD: &str = "SWARMOPS_TEST_LANDING_CHILD";

/// The model a run measures, by [`Model::name`]. Unset, a measurement runs each in turn.
const MODEL: &str = "SWARMOPS_TEST_LANDING_MODEL";

/// The workload, by [`Workload::name`].
const WORKLOAD: &str = "SWARMOPS_TEST_LANDING_WORKLOAD";

/// How many landers land at once.
const LANDERS: &str = "SWARMOPS_TEST_LANDING_LANDERS";

/// How many lands each lander makes in a closed loop.
const LANDS: &str = "SWARMOPS_TEST_LANDING_LANDS";

/// The open loop's interval between two lands of a lander, in milliseconds; 0 is a closed loop.
const INTERVAL_MS: &str = "SWARMOPS_TEST_LANDING_INTERVAL_MS";

/// How long an open loop offers lands, in seconds.
const DURATION_S: &str = "SWARMOPS_TEST_LANDING_DURATION_S";

/// The seed every draw of the run starts from.
const SEED: &str = "SWARMOPS_TEST_LANDING_SEED";

/// The smoke race's name, which its landers are run with.
const SMOKE: &str = "every_model_lands_a_small_race_without_loss";

/// The closed-loop measurement's name.
const CLOSED_LOOP: &str = "landing_models_closed_loop";

/// The open-loop measurement's name.
const OPEN_LOOP: &str = "landing_models_open_loop";

/// The large-change measurement's name.
const LARGE_CHANGE: &str = "landing_models_large_change";

/// How often a lander looks whether its op head settled.
const POLL: Duration = Duration::from_millis(2);

/// How long an op head waits after its record for another lander's transaction to settle it
/// before its own lander settles it.
const SETTLE_AFTER: Duration = Duration::from_millis(10);

/// At how many `Busy` errors in a row (the transaction tried again after each) a lander gives
/// up: 5 minutes at the store's default 10 s lock timeout.
const MAX_BUSY_IN_A_ROW: u32 = 30;

/// How long a process waits at the barrier before it gives up.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(120);

/// How long after the go the parent of a closed-loop measurement waits for its landers before
/// it kills them: far past any run of the matrix's closed-loop cells.
const CLOSED_LOOP_DEADLINE: Duration = Duration::from_mins(30);

/// How long past an open loop's duration its parent waits for the landers before it kills
/// them.
const OPEN_LOOP_GRACE: Duration = Duration::from_mins(10);

/// The ref the landers land on.
const TARGET: &str = "main";

/// How a run's processes reach the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The default gate's smoke race: the real filesystem without its flushes (`NoSyncFs`),
    /// every call under the order checker, which judges the flushes as made; no host probe.
    Smoke,
    /// A measurement: the real filesystem, unchecked, the host probed at the run's start and
    /// end.
    Measure,
}

/// One run: a model in a cell, from a seed.
#[derive(Debug, Clone, Copy)]
struct Spec {
    /// The model.
    model: Model,
    /// The cell.
    cell: Cell,
    /// The seed.
    seed: u64,
}

impl Spec {
    /// The environment that tells a lander its run.
    fn env(&self) -> [(&'static str, String); 7] {
        let (lands, interval_ms, duration_s) = match self.cell.pace {
            Pace::Closed { lands } => (lands, 0, 0),
            Pace::Open {
                interval_ms,
                duration_s,
            } => (0, interval_ms, duration_s),
        };
        [
            (MODEL, self.model.name().to_owned()),
            (WORKLOAD, self.cell.workload.name().to_owned()),
            (LANDERS, self.cell.landers.to_string()),
            (LANDS, lands.to_string()),
            (INTERVAL_MS, interval_ms.to_string()),
            (DURATION_S, duration_s.to_string()),
            (SEED, self.seed.to_string()),
        ]
    }

    /// The run [`Spec::env`] told this lander.
    fn from_env() -> Spec {
        let model = std::env::var(MODEL).expect("a lander is told its model");
        let workload = std::env::var(WORKLOAD).expect("a lander is told its workload");
        let interval_ms = env_number(INTERVAL_MS, 0);
        let pace = if interval_ms == 0 {
            Pace::Closed {
                lands: to_usize(env_number(LANDS, 0)),
            }
        } else {
            Pace::Open {
                interval_ms,
                duration_s: env_number(DURATION_S, 0),
            }
        };
        Spec {
            model: Model::from_name(&model).expect("a model's name"),
            cell: Cell {
                workload: Workload::from_name(&workload).expect("a workload's name"),
                landers: to_usize(env_number(LANDERS, 0)),
                pace,
            },
            seed: env_number(SEED, 0),
        }
    }
}

/// The whole number environment variable `name` holds, or `default` if it is unset.
fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value:?} is not a whole number"))
    })
}

/// `n` as a `usize`.
fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("fits in usize")
}

/// `d` in whole microseconds.
fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The store under work directory `work`.
fn store_root(work: &Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(work.join("store")).expect("a UTF-8 work directory")
}

/// Waits until `path` exists, or fails after [`BARRIER_TIMEOUT`].
fn wait_for(path: &Path) {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// When this test binary runs as a lander ([`DIR`] set): lands its part of the run, its
/// filesystem as `mode` says, and returns `true`. In the parent, `false`.
fn as_lander(mode: Mode) -> bool {
    let Some(work) = std::env::var_os(DIR).map(PathBuf::from) else {
        return false;
    };
    let number = std::env::var(CHILD)
        .expect("a lander is told its number")
        .parse()
        .expect("a lander's number");
    let spec = Spec::from_env();
    let root = store_root(&work);
    let timer = Arc::new(SyncTimer::new(&root));
    let fs: Arc<dyn FsOps> = match mode {
        Mode::Smoke => checked(Watched(Arc::clone(&timer), NoSyncFs)),
        Mode::Measure => Arc::new(Watched(Arc::clone(&timer), RealFs)),
    };
    let lander = Lander {
        repo: LocalRepo::open_with_fs(&root, fs).expect("open the store"),
        timer,
        spec,
        number,
    };
    fs::write(work.join(format!("ready-{number}")), b"").expect("say ready");
    wait_for(&work.join("go"));
    let go = Instant::now();
    let lands = match spec.cell.pace {
        Pace::Closed { lands } => lands,
        Pace::Open {
            interval_ms,
            duration_s,
        } => open_loop_lands(
            spec.seed,
            number,
            Duration::from_millis(interval_ms),
            Duration::from_secs(duration_s),
        ),
    };
    for land in 0..lands {
        if let Pace::Open { interval_ms, .. } = spec.cell.pace {
            let due = go + slot(spec.seed, number, land, Duration::from_millis(interval_ms));
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
        let record = lander.land(land);
        println!("{LAND_TAG}{}", record.to_line());
    }
    true
}

/// A lander: its store, the timer of its syncs, its run and its number.
struct Lander {
    /// The store, opened with every filesystem call through [`Lander::timer`].
    repo: LocalRepo,
    /// The time its write transactions spend in the journal's and the ref files' syncs.
    timer: Arc<SyncTimer>,
    /// The run.
    spec: Spec,
    /// Its number, from 0.
    number: usize,
}

/// How a land ended.
enum Ending {
    /// Changeset `id` is on the target: the land's, or one the lock holder made from it.
    Landed { id: ChangesetId, relocated: bool },
    /// A file of the change did not merge with what the target gained.
    Conflicted,
    /// [`MAX_ATTEMPTS`] attempts, none of which landed.
    Exhausted,
}

impl Lander {
    /// Land `land` of this lander: see the module documentation.
    fn land(&self, land: usize) -> LandRecord {
        let change = change_for(self.spec.seed, self.number, land, self.spec.cell.workload);
        let started = Instant::now();
        let (journal, refs) = (self.timer.journal_us(), self.timer.ref_us());
        let mut record = LandRecord::default();
        let mut onto = self.head();
        let mut ours = self.files(&change, onto, land);
        let op_head = format!("{OP_HEAD_PREFIX}{}-{land}", self.number);
        let ending = loop {
            record.attempts += 1;
            thread::sleep(backoff(self.spec.seed, self.number, land, record.attempts));
            let p = self.prepare(onto, &ours, &op_head);
            let actual = match self.publish(&p, &mut record) {
                Published::Landed { id, relocated, .. } => break Ending::Landed { id, relocated },
                Published::Moved { actual } | Published::Overlap { actual } => actual,
                Published::Pending { op_head } => {
                    match self.await_settle(&p, &op_head, &mut record) {
                        Some(id) => {
                            break Ending::Landed {
                                id,
                                relocated: id != p.id,
                            };
                        }
                        None => self.head(),
                    }
                }
            };
            // As the land loop does (ops spec §3 step 7): out of attempts, the land ends before
            // it would re-merge for another.
            if record.attempts == MAX_ATTEMPTS {
                break Ending::Exhausted;
            }
            match remerge(&self.repo, onto, actual, &ours) {
                Some(merged) => (ours, onto) = (merged, actual),
                None => break Ending::Conflicted,
            }
        };
        record.latency_us = micros(started.elapsed());
        record.journal_sync_us = self.timer.journal_us().saturating_sub(journal);
        record.ref_sync_us = self.timer.ref_us().saturating_sub(refs);
        match ending {
            Ending::Landed { id, relocated } => {
                record.id = Some(id.to_string());
                record.relocated = relocated;
            }
            Ending::Conflicted => record.conflicted = true,
            Ending::Exhausted => record.exhausted = true,
        }
        if record.id.is_none() && self.spec.model == Model::OpHeads {
            self.drop_conflicted(&op_head);
        }
        record
    }

    /// Who this lander moves the target as.
    fn actor(&self) -> String {
        format!("landing/lander-{}", self.number)
    }

    /// What the target names now.
    fn head(&self) -> ChangesetId {
        self.repo
            .get_ref(TARGET)
            .expect("read the target")
            .expect("the target exists")
            .target
    }

    /// The files `change` puts on `head`, land `land` of this lander: its own, and
    /// [`REGISTRY`] as `head` holds it with the change's line replaced.
    fn files(&self, change: &LandChange, head: ChangesetId, land: usize) -> Vec<(String, Vec<u8>)> {
        let mut files = change.puts.clone();
        if let Some(line) = change.registry_line {
            let registry = read(&self.repo, head, REGISTRY).expect("the base holds REGISTRY");
            let text = registry_text(self.number, land);
            files.push((REGISTRY.to_owned(), with_line(&registry, line, &text)));
        }
        files
    }

    /// The land's changeset on `onto`, its objects durable; an op-heads land's names its op
    /// head.
    fn prepare(
        &self,
        onto: ChangesetId,
        files: &[(String, Vec<u8>)],
        op_head: &str,
    ) -> Publication {
        let puts = files
            .iter()
            .map(|(p, content)| (path(p), LeafKind::File, content.clone()))
            .collect();
        let metadata = if self.spec.model == Model::OpHeads {
            vec![(OP_HEAD_KEY.to_owned(), op_head.as_bytes().to_vec())]
        } else {
            Vec::new()
        };
        self.repo
            .prepare(TARGET, onto, puts, Vec::new(), &self.actor(), metadata)
            .unwrap_or_else(|e| panic!("lander {}: prepare: {e}", self.number))
    }

    /// Publishes `p` by the run's model, tried again after each `Busy`; its lock hold goes to
    /// `record`.
    fn publish(&self, p: &Publication, record: &mut LandRecord) -> Published {
        self.retrying(record, || self.repo.publish_with(self.spec.model, p))
    }

    /// Settles every recorded op head; its lock hold goes to `record`.
    fn settle(&self, record: &mut LandRecord) {
        self.retrying(record, || self.repo.settle_op_heads());
    }

    /// `transaction`'s outcome, the transaction tried again after each `Busy` (counted in
    /// `record`, which gets its lock hold too).
    fn retrying<T>(
        &self,
        record: &mut LandRecord,
        transaction: impl Fn() -> Result<Timed<T>, RepoError>,
    ) -> T {
        let mut in_a_row = 0;
        loop {
            match transaction() {
                Ok(Timed { value, lock_hold }) => {
                    record.lock_hold_us.push(micros(lock_hold));
                    return value;
                }
                Err(RepoError::Store(StoreError::Busy)) => {
                    record.busy += 1;
                    in_a_row += 1;
                    assert!(
                        in_a_row < MAX_BUSY_IN_A_ROW,
                        "lander {}: Busy {in_a_row} times in a row",
                        self.number
                    );
                }
                Err(e) => panic!("lander {}: a write transaction failed: {e}", self.number),
            }
        }
    }

    /// Polls op head `op_head`, which `p` recorded, until a transaction has settled it: the
    /// changeset that landed it on the target, or `None` when it did not settle (its changeset
    /// under the conflicted ref instead). Still recorded [`SETTLE_AFTER`] after the record,
    /// it settles it itself.
    fn await_settle(
        &self,
        p: &Publication,
        op_head: &str,
        record: &mut LandRecord,
    ) -> Option<ChangesetId> {
        let conflicted = conflicted_name(op_head);
        let recorded = Instant::now();
        let mut settled = false;
        loop {
            let refs: HashMap<String, ChangesetId> = self
                .repo
                .list_refs()
                .expect("list the refs")
                .into_iter()
                .map(|(name, state)| (name, state.target))
                .collect();
            if !refs.contains_key(op_head) {
                if refs.get(&conflicted) == Some(&p.id) {
                    return None;
                }
                return Some(self.find_settled(p, op_head));
            }
            if !settled && recorded.elapsed() >= SETTLE_AFTER {
                self.settle(record);
                settled = true;
                continue;
            }
            assert!(
                recorded.elapsed() < MAX_LATENCY,
                "lander {}: op head {op_head} never settled",
                self.number
            );
            thread::sleep(POLL);
        }
    }

    /// The changeset on the target that op head `op_head`, recorded on `p.onto`, settled as:
    /// the one above `p.onto` on the target's first-parent chain whose metadata names it.
    fn find_settled(&self, p: &Publication, op_head: &str) -> ChangesetId {
        let generation = |id| {
            self.repo
                .changeset(id)
                .expect("read a changeset")
                .expect("a changeset on the target")
                .generation
        };
        let head = self.head();
        let above = generation(head).saturating_sub(generation(p.onto));
        self.repo
            .log_first_parent(head, to_usize(above))
            .expect("read the target's first parents")
            .into_iter()
            .find(|stored| {
                stored
                    .changeset
                    .metadata
                    .iter()
                    .any(|(key, value)| key == OP_HEAD_KEY && value == op_head.as_bytes())
            })
            .unwrap_or_else(|| {
                panic!(
                    "lander {}: op head {op_head} is gone, and no changeset on the target names it",
                    self.number
                )
            })
            .id
    }

    /// Deletes the conflicted ref of op head `op_head`, if a settle left one.
    fn drop_conflicted(&self, op_head: &str) {
        let conflicted = conflicted_name(op_head);
        let Some(state) = self.repo.get_ref(&conflicted).expect("read a ref") else {
            return;
        };
        let mut in_a_row = 0;
        loop {
            match self
                .repo
                .cas_ref(&conflicted, Some(state.target), None, &self.actor())
            {
                Ok(_) => return,
                Err(RepoError::Store(StoreError::Busy)) => {
                    in_a_row += 1;
                    assert!(
                        in_a_row < MAX_BUSY_IN_A_ROW,
                        "Busy {in_a_row} times in a row"
                    );
                }
                Err(e) => panic!("lander {}: delete {conflicted}: {e}", self.number),
            }
        }
    }
}

/// The conflicted ref of op head `op_head`: `release/ophc-<rest>` for `release/oph-<rest>`.
fn conflicted_name(op_head: &str) -> String {
    let rest = op_head
        .strip_prefix(OP_HEAD_PREFIX)
        .expect("an op head's name");
    format!("{CONFLICTED_PREFIX}{rest}")
}

/// The bytes of the file at `p` in changeset `cs`, if one is there.
fn read(repo: &LocalRepo, cs: ChangesetId, p: &str) -> Option<Vec<u8>> {
    repo.read_blob_at(cs, &path(p))
        .expect("read a file")
        .map(|(_, _, content)| content)
}

/// `ours`, prepared on `old`, re-merged onto `new`: each file unchanged between the two kept as
/// it is, each changed one merged with `toy_merge`; `None` when one does not merge.
fn remerge(
    repo: &LocalRepo,
    old: ChangesetId,
    new: ChangesetId,
    ours: &[(String, Vec<u8>)],
) -> Option<Vec<(String, Vec<u8>)>> {
    ours.iter()
        .map(|(p, content)| {
            let (base, theirs) = (read(repo, old, p), read(repo, new, p));
            if base == theirs {
                return Some((p.clone(), content.clone()));
            }
            let merged = toy_merge(base.as_deref()?, content, theirs.as_deref()?)?;
            Some((p.clone(), merged))
        })
        .collect()
}

/// Commits the files of `cell`'s base ([`base_files`]) as a root changeset, `main` moved to it.
fn commit_base(repo: &LocalRepo, cell: &Cell) -> ChangesetId {
    let changes = base_files(cell.landers, cell.workload)
        .into_iter()
        .map(|(p, content)| put(&p, &content))
        .collect();
    let mut req = request(&[], changes, "the landing base");
    req.advance = Some(Advance {
        name: TARGET.to_owned(),
        actor: "landing/base".to_owned(),
    });
    repo.commit(req).expect("commit the base").id
}

/// The host's 1-minute load and, in a measurement, its raw `fdatasync` probe in work directory
/// `work` (0 in the smoke race, which does not flush).
fn host_now(work: &Path, mode: Mode) -> (f64, u64) {
    let probe = match mode {
        Mode::Smoke => Duration::ZERO,
        Mode::Measure => fdatasync_probe(work, PROBE_APPENDS).expect("probe fdatasync"),
    };
    (load1(), micros(probe))
}

/// Runs `spec` in a fresh directory under `parent`: a store whose `main` holds the base, the
/// landers (this test binary run again as test `test`, every process's filesystem as `mode`
/// says) let go together and waited for until `deadline` after the go, then the gate. Prints the
/// run's [`RUN_TAG`] line (and a failed gate's rule) and returns the run with its gate.
fn run(
    spec: &Spec,
    parent: &Path,
    test: &str,
    mode: Mode,
    deadline: Duration,
) -> (Run, Result<(), String>) {
    let work = parent.join(format!(
        "{}-{}-{}",
        spec.model.name(),
        spec.cell.workload.name(),
        spec.seed
    ));
    fs::create_dir_all(&work).expect("create the work directory");
    let root = store_root(&work);
    let fs: Arc<dyn FsOps> = match mode {
        Mode::Smoke => checked(NoSyncFs),
        Mode::Measure => Arc::new(RealFs),
    };
    let repo = LocalRepo::create_with_fs(&root, "landing", fs).expect("create the store");
    let base = commit_base(&repo, &spec.cell);
    let (load_start, fdatasync_start_us) = host_now(&work, mode);
    let (records, wall) = race(spec, &work, test, deadline);
    if spec.model == Model::OpHeads {
        repo.settle_op_heads().expect("the final settle");
    }
    let gate = check_invariants(&root, TARGET, base, &records);
    let (load_end, fdatasync_end_us) = host_now(&work, mode);
    let elapsed = match spec.cell.pace {
        Pace::Closed { .. } => wall,
        Pace::Open { duration_s, .. } => Duration::from_secs(duration_s),
    };
    let run = Run {
        model: spec.model.name().to_owned(),
        cell: spec.cell,
        seed: spec.seed,
        gate: gate.is_ok(),
        stats: Stats::from(&records, elapsed),
        host: Host {
            load_start,
            load_end,
            fdatasync_start_us,
            fdatasync_end_us,
        },
    };
    println!("{RUN_TAG}{} wall_ms={}", run.to_line(), wall.as_millis());
    if let Err(rule) = &gate {
        println!("landing-gate: {rule}");
    }
    (run, gate)
}

/// Spawns the landers of `spec` on the store in `work`, lets them go together once all are
/// ready, waits for them until `deadline` after the go, and returns each one's records, in
/// order, and the time from the go until the last one ended.
fn race(
    spec: &Spec,
    work: &Path,
    test: &str,
    deadline: Duration,
) -> (Vec<Vec<LandRecord>>, Duration) {
    let mut children: Vec<Child> = (0..spec.cell.landers)
        .map(|number| {
            let mut command = this_test(&test_name!(test));
            command
                .env(DIR, work)
                .env(CHILD, number.to_string())
                .envs(spec.env());
            Child::spawn(&mut command)
        })
        .collect();
    let ready = Instant::now() + BARRIER_TIMEOUT;
    while !(0..spec.cell.landers).all(|number| work.join(format!("ready-{number}")).exists()) {
        if Instant::now() >= ready {
            let outputs: Vec<String> = children
                .iter_mut()
                .map(|child| {
                    String::from_utf8_lossy(&child.finish(Instant::now()).0.stderr).into_owned()
                })
                .collect();
            panic!("the landers never got ready: {outputs:#?}");
        }
        thread::sleep(Duration::from_millis(1));
    }
    let go = Instant::now();
    fs::write(work.join("go"), b"").expect("say go");
    let mut records = Vec::with_capacity(children.len());
    for (number, child) in children.into_iter().enumerate() {
        let output = child.wait(go + deadline);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "lander {number}: {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let lands: Vec<LandRecord> = tagged(&stdout, LAND_TAG)
            .map(|line| LandRecord::parse(line).expect("a lander's record"))
            .collect();
        records.push(lands);
    }
    (records, go.elapsed())
}

/// A measurement of `cell`: each model [`MODEL`] names (unset, each in turn) runs it once from
/// seed [`SEED`] (default 1), in a fresh directory under `measure::base_dir()`, unchecked; the
/// test fails if a run failed its gate.
fn measure(test: &str, cell: Cell) {
    let models = match std::env::var(MODEL) {
        Ok(name) => {
            vec![Model::from_name(&name).expect("SWARMOPS_TEST_LANDING_MODEL names a model")]
        }
        Err(_) => Model::ALL.to_vec(),
    };
    let seed = env_number(SEED, 1);
    let deadline = match cell.pace {
        Pace::Closed { .. } => CLOSED_LOOP_DEADLINE,
        Pace::Open { duration_s, .. } => Duration::from_secs(duration_s) + OPEN_LOOP_GRACE,
    };
    let dir = tempfile::Builder::new()
        .prefix("landing-models-")
        .tempdir_in(base_dir())
        .expect("create the measurement's directory");
    let mut failed = Vec::new();
    for model in models {
        let spec = Spec { model, cell, seed };
        if let (_, Err(rule)) = run(&spec, dir.path(), test, Mode::Measure, deadline) {
            failed.push(format!("{}: {rule}", model.name()));
        }
    }
    assert!(failed.is_empty(), "runs failed the gate: {failed:#?}");
}

/// The workload [`WORKLOAD`] names, or `default`.
fn env_workload(default: Workload) -> Workload {
    std::env::var(WORKLOAD).map_or(default, |name| {
        Workload::from_name(&name).expect("SWARMOPS_TEST_LANDING_WORKLOAD names a workload")
    })
}

#[test]
fn every_model_lands_a_small_race_without_loss() {
    if as_lander(Mode::Smoke) {
        return;
    }
    let cell = Cell {
        workload: Workload::Shared,
        landers: 4,
        pace: Pace::Closed { lands: 5 },
    };
    let (dir, _) = temp_dir();
    for model in Model::ALL {
        let spec = Spec {
            model,
            cell,
            seed: 1,
        };
        let (run, gate) = run(&spec, dir.path(), SMOKE, Mode::Smoke, CHILD_TIMEOUT);
        gate.unwrap_or_else(|rule| panic!("{}: {rule}", model.name()));
        assert_eq!(
            run.stats.landed + run.stats.conflicts + run.stats.exhausted,
            20
        );
    }
}

#[test]
fn check_invariants_rejects_a_lost_doubled_or_non_linear_landing() {
    let landed = |id: ChangesetId| LandRecord {
        id: Some(id.to_string()),
        attempts: 1,
        ..LandRecord::default()
    };
    let rule = |root: &Utf8Path, base, reported: &[Vec<LandRecord>]| {
        let err = check_invariants(root, TARGET, base, reported).expect_err("the gate fails");
        err.split_once(": ")
            .expect("a rule, then what broke it")
            .0
            .to_owned()
    };

    // main: base <- one <- two, each moved there in one step.
    let (_dir, root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("a.txt", b"a\n")]);
    let one = advance(&repo, &[base], vec![put("b.txt", b"1\n")]);
    let two = advance(&repo, &[one], vec![put("b.txt", b"2\n")]);
    let off_main = commit(&repo, &[base], vec![put("c.txt", b"c\n")]);
    let ok = |reported: &[Vec<LandRecord>]| check_invariants(&root, TARGET, base, reported);
    assert_eq!(ok(&[vec![landed(one), landed(two)]]), Ok(()));
    assert_eq!(ok(&[vec![landed(one)], vec![], vec![landed(two)]]), Ok(()));
    let conflicted = LandRecord {
        conflicted: true,
        attempts: 2,
        ..LandRecord::default()
    };
    assert_eq!(ok(&[vec![landed(one), conflicted, landed(two)]]), Ok(()));

    let lost = [vec![landed(one), landed(two), landed(off_main)]];
    assert_eq!(rule(&root, base, &lost), "lost landing");
    let doubled = [vec![landed(one), landed(two)], vec![landed(two)]];
    assert_eq!(rule(&root, base, &doubled), "doubled landing");
    assert_eq!(
        rule(&root, base, &[vec![landed(two)]]),
        "unreported changeset"
    );
    let reversed = [vec![landed(two), landed(one)]];
    assert_eq!(rule(&root, base, &reversed), "out of order");
    let slow = LandRecord {
        latency_us: micros(MAX_LATENCY) + 1,
        ..landed(two)
    };
    assert_eq!(rule(&root, base, &[vec![landed(one), slow]]), "slow land");
    // The gate's names of the op-head refs are the prototypes'.
    assert_eq!(OP_HEAD_REFS, [OP_HEAD_PREFIX, CONFLICTED_PREFIX]);
    for prefix in OP_HEAD_REFS {
        let name = format!("{prefix}t-0");
        repo.cas_ref(&name, None, Some(two), ACTOR).unwrap();
        assert_eq!(
            rule(&root, base, &[vec![landed(one), landed(two)]]),
            "op-head ref left"
        );
        repo.cas_ref(&name, Some(two), None, ACTOR).unwrap();
    }
    assert_eq!(ok(&[vec![landed(one), landed(two)]]), Ok(()));
    assert_verifies(&repo);

    // A merge on main: main moved from one to a changeset whose parents are one and another.
    let (_dir, root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("a.txt", b"a\n")]);
    let one = advance(&repo, &[base], vec![put("b.txt", b"1\n")]);
    let side = commit(&repo, &[base], vec![put("c.txt", b"c\n")]);
    let merge = advance(&repo, &[one, side], vec![put("d.txt", b"d\n")]);
    let reported = [vec![landed(one), landed(merge)]];
    assert_eq!(rule(&root, base, &reported), "a merge on the chain");

    // A move that is not a one-step fast-forward: main moved from x to y, whose first parent
    // is the base, not x.
    let (_dir, root, repo) = new_store();
    let base = advance(&repo, &[], vec![put("a.txt", b"a\n")]);
    let x = advance(&repo, &[base], vec![put("x.txt", b"x\n")]);
    let y = commit(&repo, &[base], vec![put("y.txt", b"y\n")]);
    repo.cas_ref(TARGET, Some(x), Some(y), ACTOR).unwrap();
    assert_eq!(rule(&root, base, &[vec![landed(y)]]), "not a fast-forward");
}

#[test]
#[ignore = "measurement, minutes of fsync'd lands under the heavy lock"]
fn landing_models_closed_loop() {
    if as_lander(Mode::Measure) {
        return;
    }
    let cell = Cell {
        workload: env_workload(Workload::Shared),
        landers: to_usize(env_number(LANDERS, 32)),
        pace: Pace::Closed {
            lands: to_usize(env_number(LANDS, 100)),
        },
    };
    measure(CLOSED_LOOP, cell);
}

#[test]
#[ignore = "measurement, minutes of fsync'd lands under the heavy lock"]
fn landing_models_open_loop() {
    if as_lander(Mode::Measure) {
        return;
    }
    let cell = Cell {
        workload: env_workload(SCORED_CELL.workload),
        landers: to_usize(env_number(LANDERS, 50)),
        pace: Pace::Open {
            interval_ms: env_number(INTERVAL_MS, 2_000),
            duration_s: env_number(DURATION_S, 120),
        },
    };
    measure(OPEN_LOOP, cell);
}

#[test]
#[ignore = "measurement, minutes of fsync'd lands under the heavy lock"]
fn landing_models_large_change() {
    if as_lander(Mode::Measure) {
        return;
    }
    let cell = Cell {
        workload: Workload::Large,
        landers: to_usize(env_number(LANDERS, 32)),
        pace: Pace::Closed {
            lands: to_usize(env_number(LANDS, 50)),
        },
    };
    measure(LARGE_CHANGE, cell);
}

#[test]
fn a_land_that_does_not_merge_ends_conflicted_and_leaves_no_ref() {
    // An op-heads lander, in this process, whose land replaces line 0 of REGISTRY while
    // another land that replaces it too waits, recorded, on the same head.
    let (_dir, root, repo) = new_store();
    let spec = Spec {
        model: Model::OpHeads,
        cell: Cell {
            workload: Workload::Conflict,
            landers: 2,
            pace: Pace::Closed { lands: 1 },
        },
        seed: 1,
    };
    let base = commit_base(&repo, &spec.cell);
    let land = (0..10_000)
        .find(|&land| change_for(spec.seed, 1, land, Workload::Conflict).registry_line == Some(0))
        .expect("a land of lander 1 that replaces line 0");
    let registry = read(&repo, base, REGISTRY).unwrap();
    let other = repo
        .prepare(
            TARGET,
            base,
            puts(&[(REGISTRY, &with_line(&registry, 0, "the other land"))]),
            vec![],
            ACTOR,
            vec![(OP_HEAD_KEY.to_owned(), b"release/oph-0-0".to_vec())],
        )
        .unwrap();
    let pending = repo.publish_with(Model::OpHeads, &other).unwrap().value;
    assert!(matches!(pending, Published::Pending { .. }), "{pending:?}");

    let timer = Arc::new(SyncTimer::new(&root));
    let lander = Lander {
        repo: LocalRepo::open_with_fs(&root, checked(Watched(Arc::clone(&timer), RealFs))).unwrap(),
        timer,
        spec,
        number: 1,
    };
    // Its record settles the other land onto main; its own settle then leaves it conflicted,
    // and its re-merge onto the other land's REGISTRY does not merge.
    let record = lander.land(land);
    assert!(record.conflicted, "{record:?}");
    assert_eq!((record.id.as_deref(), record.attempts), (None, 1));
    assert!(!record.exhausted && !record.relocated);
    assert_eq!(main_head(&repo), other.id);
    let refs: Vec<String> = repo
        .list_refs()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(refs, ["main"], "no op-head ref is left");
    let other_landed = LandRecord {
        id: Some(other.id.to_string()),
        attempts: 1,
        ..LandRecord::default()
    };
    assert_eq!(
        check_invariants(&root, TARGET, base, &[vec![other_landed], vec![record]]),
        Ok(())
    );
}

#[test]
fn open_loop_slots_keep_one_land_per_interval() {
    let interval = Duration::from_millis(2_000);
    let duration = Duration::from_secs(120);
    let (fewest, most) = (interval.mul_f64(0.6), interval.mul_f64(1.4));
    for lander in 0..50 {
        let lands = open_loop_lands(1, lander, interval, duration);
        assert!((60..=61).contains(&lands), "lander {lander}: {lands} lands");
        assert_eq!(lands, open_loop_lands(1, lander, interval, duration));
        let slots: Vec<Duration> = (0..=lands)
            .map(|land| slot(1, lander, land, interval))
            .collect();
        assert_eq!(
            slots[3],
            slot(1, lander, 3, interval),
            "the same draw each time"
        );
        // One land per interval, each moved by at most 20 % of it: two in a row are 0.6 to
        // 1.4 intervals apart.
        for (land, pair) in slots.windows(2).enumerate() {
            let gap = pair[1].saturating_sub(pair[0]);
            assert!(
                (fewest..=most).contains(&gap),
                "lander {lander}, lands {land} and {}: {gap:?} apart",
                land + 1
            );
        }
        // The last land is due before the end (give or take its 20 %), the one after it not.
        assert!(slots[lands - 1] < duration + interval.mul_f64(0.2));
        assert!(slots[lands] >= duration.saturating_sub(interval.mul_f64(0.2)));
    }
    // The landers' first lands spread over the first interval.
    let mut firsts: Vec<Duration> = (0..50).map(|lander| slot(1, lander, 0, interval)).collect();
    firsts.sort_unstable();
    assert!(firsts[0] < interval.mul_f64(0.2), "{firsts:?}");
    assert!(firsts[49] > interval.mul_f64(0.8), "{firsts:?}");
    assert_eq!(open_loop_lands(1, 0, Duration::ZERO, duration), 0);
}

/// A run of `model` in the scored cell from seed `seed`, its gate passed, with a scored p99 of
/// `p99_ms`, `lands_per_s` and a retry rate of `retry_pct` %, nothing exhausted.
fn scored(model: &str, seed: u64, p99_ms: u64, lands_per_s: f64, retry_pct: f64) -> Run {
    Run {
        model: model.to_owned(),
        cell: SCORED_CELL,
        seed,
        gate: true,
        stats: Stats {
            landed: 3_000,
            lands_per_s,
            latency_p50_us: p99_ms * 500,
            latency_p99_us: p99_ms * 1_000,
            retry_rate: retry_pct / 100.0,
            conflicts: 0,
            exhausted: 0,
            busy: 0,
            lock_hold_p50_us: 9_000,
            lock_hold_p99_us: 20_000,
            journal_share: 0.5,
            ref_share: 0.25,
            relocations: 900,
        },
        host: Host {
            load_start: 12.0,
            load_end: 13.5,
            fdatasync_start_us: 3_800,
            fdatasync_end_us: 3_900,
        },
    }
}

/// A matrix's runs: three of each model in the scored cell, with the p99s `p99_ms` (the
/// yardstick's, then the candidates'), 25 lands/s and 1 % retries, and three of each in a
/// closed-loop cell.
fn matrix(p99_ms: [u64; 4]) -> Vec<Run> {
    let closed = Cell {
        workload: Workload::Disjoint,
        landers: 32,
        pace: Pace::Closed { lands: 100 },
    };
    let mut runs = Vec::new();
    for (model, p99) in std::iter::once(YARDSTICK).chain(CANDIDATES).zip(p99_ms) {
        for seed in 1..=3 {
            runs.push(scored(model, seed, p99, 25.0, 1.0));
            runs.push(Run {
                cell: closed,
                ..scored(model, seed, p99 / 2, 60.0, 0.0)
            });
        }
    }
    runs
}

/// `model`'s runs in the scored cell.
fn scored_runs<'r>(runs: &'r mut [Run], model: &str) -> impl Iterator<Item = &'r mut Run> {
    runs.iter_mut()
        .filter(move |run| run.model == model && run.cell == SCORED_CELL)
}

/// Whether `a` and `b` are within a millionth of each other.
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * b.abs().max(1.0)
}

#[test]
fn decision_1_chooses_the_lowest_scored_p99_or_less_work_within_ten_percent() {
    // metalog's p99 is the lowest, 280 ms; pushrebase's, 300 ms, is within 10 % of it (308 ms)
    // and pushrebase does less work under the lock.
    let decision = decide(&matrix([100, 300, 280, 250]), SCORED_CELL).unwrap();
    assert_eq!(
        (decision.chosen.as_str(), decision.eligible),
        ("pushrebase", true)
    );
    let models: Vec<&str> = decision.verdicts.iter().map(|v| v.model.as_str()).collect();
    assert_eq!(models, ["cas_loop", "pushrebase", "metalog", "op_heads"]);
    assert!(decision.verdicts.iter().all(Verdict::eligible));
    // Past the 10 %, the lowest.
    let decision = decide(&matrix([100, 309, 280, 250]), SCORED_CELL).unwrap();
    assert_eq!(decision.chosen, "metalog");
    // The yardstick is never chosen, however low its p99.
    assert_eq!(
        decide(&matrix([1, 300, 280, 250]), SCORED_CELL)
            .unwrap()
            .chosen,
        "pushrebase"
    );

    // The scored numbers are medians over the runs: one fast and one slow run of three do not
    // move metalog's p99 off 280 ms, nor one run under 20 lands/s its throughput off 25.
    let mut runs = matrix([100, 309, 280, 250]);
    let mut metalog: Vec<&mut Run> = scored_runs(&mut runs, "metalog").collect();
    metalog[0].stats.latency_p99_us = 10_000;
    metalog[1].stats.latency_p99_us = 900_000;
    metalog[2].stats.lands_per_s = 5.0;
    let decision = decide(&runs, SCORED_CELL).unwrap();
    let verdict = &decision.verdicts[2];
    assert!(close(verdict.latency_p99_us, 280_000.0), "{verdict:?}");
    assert!(close(verdict.lands_per_s, 25.0), "{verdict:?}");
    assert_eq!((verdict.runs, verdict.scored_runs), (6, 3));
    assert_eq!(decision.chosen, "metalog");
}

#[test]
fn decision_1_takes_op_heads_only_at_half_the_best_other_p99() {
    // op_heads at 140 ms, half metalog's 280 ms: chosen.
    assert_eq!(
        decide(&matrix([100, 300, 280, 140]), SCORED_CELL)
            .unwrap()
            .chosen,
        "op_heads"
    );
    // At 141 ms it is not, although its p99 is the lowest.
    assert_eq!(
        decide(&matrix([100, 300, 280, 141]), SCORED_CELL)
            .unwrap()
            .chosen,
        "pushrebase"
    );
    // The only eligible candidate has none to beat: pushrebase and metalog miss 500 ms.
    let decision = decide(&matrix([100, 600, 700, 400]), SCORED_CELL).unwrap();
    assert_eq!(
        (decision.chosen.as_str(), decision.eligible),
        ("op_heads", true)
    );
}

#[test]
fn a_failed_gate_or_a_missed_scored_condition_makes_a_candidate_ineligible() {
    // metalog at 200 ms is chosen (pushrebase's 300 ms is past 220).
    let base = matrix([100, 300, 200, 250]);
    assert_eq!(decide(&base, SCORED_CELL).unwrap().chosen, "metalog");
    let metalog_after = |change: &dyn Fn(&mut [Run])| {
        let mut runs = base.clone();
        change(&mut runs);
        let decision = decide(&runs, SCORED_CELL).unwrap();
        assert_eq!(decision.chosen, "pushrebase");
        decision.verdicts[2].clone()
    };
    // One failed gate, in a cell that is not scored.
    let verdict = metalog_after(&|runs| {
        let closed = runs
            .iter_mut()
            .find(|run| run.model == "metalog" && run.cell != SCORED_CELL)
            .unwrap();
        closed.gate = false;
    });
    assert_eq!((verdict.failed_gates, verdict.met), (1, [true; 4]));
    // A median under 20 lands/s; a median p99 of 500 ms, not under it; a median retry rate of
    // 5 %, not under it; one land exhausted in one scored run.
    let scored_change = |field: fn(&mut Stats)| {
        move |runs: &mut [Run]| scored_runs(runs, "metalog").for_each(|run| field(&mut run.stats))
    };
    let verdict = metalog_after(&scored_change(|stats| stats.lands_per_s = 19.9));
    assert_eq!(verdict.met, [false, true, true, true]);
    let verdict = metalog_after(&scored_change(|stats| stats.latency_p99_us = 500_000));
    assert_eq!(verdict.met, [true, false, true, true]);
    let verdict = metalog_after(&scored_change(|stats| stats.retry_rate = 0.05));
    assert_eq!(verdict.met, [true, true, false, true]);
    let verdict = metalog_after(&|runs| {
        scored_runs(runs, "metalog").next().unwrap().stats.exhausted = 1;
    });
    assert_eq!(
        (verdict.exhausted, verdict.met),
        (1, [true, true, true, false])
    );
    assert!(!verdict.eligible());
}

#[test]
fn with_no_eligible_candidate_decision_1_takes_the_most_conditions_met() {
    // Every candidate under 20 lands/s, op_heads past 500 ms too; the yardstick alone is
    // eligible, and is not chosen.
    let mut runs = matrix([100, 300, 280, 600]);
    for model in CANDIDATES {
        scored_runs(&mut runs, model).for_each(|run| run.stats.lands_per_s = 15.0);
    }
    // pushrebase and metalog meet three conditions each: the lower retry rate, metalog's.
    scored_runs(&mut runs, "pushrebase").for_each(|run| run.stats.retry_rate = 0.02);
    let decision = decide(&runs, SCORED_CELL).unwrap();
    assert_eq!(
        (decision.chosen.as_str(), decision.eligible),
        ("metalog", false)
    );
    let met: Vec<usize> = decision
        .verdicts
        .iter()
        .map(Verdict::conditions_met)
        .collect();
    assert_eq!(met, [4, 3, 3, 2]);
    assert!(decision.verdicts[0].eligible());
    // As many conditions and the same retry rate: the order of work, which the rule leaves.
    scored_runs(&mut runs, "pushrebase").for_each(|run| run.stats.retry_rate = 0.01);
    assert_eq!(decide(&runs, SCORED_CELL).unwrap().chosen, "pushrebase");
}

#[test]
fn decision_1_needs_a_scored_run_of_every_model() {
    for model in std::iter::once(YARDSTICK).chain(CANDIDATES) {
        let runs: Vec<Run> = matrix([100, 300, 280, 250])
            .into_iter()
            .filter(|run| !(run.model == model && run.cell == SCORED_CELL))
            .collect();
        let error = decide(&runs, SCORED_CELL).unwrap_err();
        assert!(error.contains(model), "{error}");
    }
}

#[test]
fn run_and_land_lines_read_back() {
    let run = scored("metalog", 2, 280, 24.5, 1.25);
    let printed = format!("noise\n{RUN_TAG}{} wall_ms=120000\n", run.to_line());
    let read: Vec<Option<Run>> = tagged(&printed, RUN_TAG).map(Run::parse).collect();
    assert_eq!(read, [Some(run.clone())]);
    let closed = Run {
        cell: Cell {
            workload: Workload::Large,
            landers: 32,
            pace: Pace::Closed { lands: 50 },
        },
        gate: false,
        ..run
    };
    assert_eq!(Run::parse(&closed.to_line()), Some(closed));
    assert_eq!(Run::parse("model=metalog workload=shared"), None);

    let record = LandRecord {
        id: Some("ab".repeat(32)),
        attempts: 3,
        relocated: true,
        conflicted: false,
        exhausted: false,
        latency_us: 1_234,
        lock_hold_us: vec![10, 20],
        journal_sync_us: 7,
        ref_sync_us: 8,
        busy: 1,
    };
    assert_eq!(LandRecord::parse(&record.to_line()), Some(record));
    let unlanded = LandRecord {
        conflicted: true,
        ..LandRecord::default()
    };
    assert_eq!(LandRecord::parse(&unlanded.to_line()), Some(unlanded));
}

#[test]
fn the_tables_give_each_cell_s_medians_and_ranges() {
    let mut runs = matrix([100, 300, 280, 250]);
    scored_runs(&mut runs, "metalog")
        .next()
        .unwrap()
        .stats
        .lands_per_s = 20.0;
    let text = tables(&runs);
    let scored = text
        .split(&SCORED_CELL.describe())
        .nth(1)
        .expect("the scored cell's table");
    let rows: Vec<&str> = scored
        .lines()
        .filter(|line| line.starts_with("| ") && !line.starts_with("| model"))
        .take(4)
        .collect();
    assert_eq!(rows.len(), 4, "{text}");
    assert!(
        rows[0].starts_with("| cas_loop | 3/3 | 25.0 [25.0–25.0] |"),
        "{text}"
    );
    assert!(
        rows[2].starts_with("| metalog | 3/3 | 25.0 [20.0–25.0] | 140.0 [140.0–140.0] | 280.0 [280.0–280.0] | 1.00 [1.00–1.00] |"),
        "{text}"
    );
    assert!(rows[2].ends_with("| 12.0–13.5 | 3.8–3.9 |"), "{text}");
    assert_eq!(
        text.matches("| model | gate |").count(),
        2,
        "one table per cell"
    );
}

/// Set to the directory of a matrix's run logs (`scripts/landing-models.sh` sets it):
/// `landing_models_decision` reads every run's [`RUN_TAG`] line in its `.log` files, prints the
/// cells' tables and applies Decision 1. Unset, that test decides nothing.
const LOGS: &str = "SWARMOPS_TEST_LANDING_LOGS";

#[test]
#[ignore = "applies Decision 1 to the run logs SWARMOPS_TEST_LANDING_LOGS names; scripts/landing-models.sh runs it last"]
fn landing_models_decision() {
    let Some(logs) = std::env::var_os(LOGS).map(PathBuf::from) else {
        eprintln!("landing_models_decision: {LOGS} is not set; nothing decided");
        return;
    };
    let mut files: Vec<PathBuf> = fs::read_dir(&logs)
        .unwrap_or_else(|e| panic!("read {}: {e}", logs.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|file| file.extension().is_some_and(|ext| ext == "log"))
        .collect();
    files.sort();
    let mut runs = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read a run's log");
        for line in tagged(&text, RUN_TAG) {
            let run = Run::parse(line)
                .unwrap_or_else(|| panic!("{}: not a run's line: {line}", file.display()));
            runs.push(run);
        }
    }
    assert!(!runs.is_empty(), "no run's line in {}", logs.display());
    // Decision 1's scored cell, unless the cell's variables name another (a smoke run's).
    let scored = Cell {
        workload: env_workload(SCORED_CELL.workload),
        landers: to_usize(env_number(LANDERS, 50)),
        pace: Pace::Open {
            interval_ms: env_number(INTERVAL_MS, 2_000),
            duration_s: env_number(DURATION_S, 120),
        },
    };
    println!("{}", tables(&runs));
    println!("scored in: {}\n", scored.describe());
    let decision = decide(&runs, scored).unwrap_or_else(|e| panic!("{e}"));
    for line in decision.lines() {
        println!("{line}");
    }
}
