//! The store-level error and crash sweeps' harness (feature `test-hooks`): a store template
//! whose next transaction moves the checkpoint or compacts ([`sweep_template`]), that
//! transaction ([`swept`]), failed or crashed at each of its calls, in this process or in a
//! child ([`sweep_child`], [`run_sweep_child`]), what its outcome says of it ([`Said`]), and
//! what the store must hold then ([`assert_recovers`]). The metadata store's error sweeps use it
//! (`crates/swarmops-store/tests/meta.rs`). Its changesets are its own ([`changeset`], [`id`]).

use std::os::unix::process::ExitStatusExt;
use std::process::Output;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectKind};
use swarmops_store::{
    ChangesetMeta, CommitInfo, CrashAtFs, FailAt, FailAtFs, FailMode, FsOps, Index, InjectedError,
    InsertOutcome, Journal, Layout, MetaStore, MetaWrite, RealFs, StoreError, StoreFormat,
};
use tempfile::TempDir;

use crate::child::{self, SIGABRT};
use crate::faults;
use crate::fs::{self, NoSyncFs};

/// How a child reports what its transaction said, among the test harness's output.
const REPORT: &str = "sweep-child: ";

/// The ref the swept transaction creates.
const SWEPT_REF: &str = "release/swept";

/// Who the swept transaction's ref move names.
const ACTOR: &str = "test/1";

/// The id of the sweeps' changeset `n`: a real hash, so the ids look like a store's.
pub fn id(n: u64) -> ChangesetId {
    ChangesetId::new(swarmops_core::hash_object(
        ObjectKind::Changeset,
        &n.to_le_bytes(),
    ))
}

/// The sweeps' changeset `n`, a root, as a caller hands it to `insert_changeset`: seq and
/// generation 0, since the store assigns both.
pub fn changeset(n: u64) -> ChangesetMeta {
    let delta_hash = swarmops_core::hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
    ChangesetMeta::new(id(n), 0, 0, delta_hash, Vec::new())
}

/// Inserts changeset `n` in a write transaction of its own.
fn insert(store: &MetaStore, n: u64) {
    let mut write = store.write().unwrap();
    let outcome = write.insert_changeset(changeset(n)).unwrap();
    assert!(
        matches!(outcome, InsertOutcome::Inserted { .. }),
        "{outcome:?}"
    );
    write.commit().unwrap();
}

/// The checkpoint of the store at `root`.
fn ckpt(root: &Utf8Path) -> u64 {
    Journal::open(&super::layout_of(root), Arc::new(RealFs::new()))
        .unwrap()
        .ckpt()
        .unwrap()
}

/// The length of the journal of the store at `root`.
fn log_len(root: &Utf8Path) -> u64 {
    super::file_len(super::layout_of(root).journal_log())
}

/// An index handle of its own on the store at `root`.
fn index(root: &Utf8Path) -> Index {
    Index::open(&super::layout_of(root), Arc::new(RealFs::new())).unwrap()
}

/// What a child that ended normally reported (the line it printed after [`REPORT`]).
fn report(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a sweep's child: {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(REPORT))
        .unwrap_or_default()
        .to_owned()
}

/// Set only in the children of the store-level error sweeps: the store root.
pub const SWEEP_ROOT: &str = "SWARMOPS_TEST_META_SWEEP_ROOT";

/// Set with [`SWEEP_ROOT`]: what the child does, `fail:<k>:<errno>:<n>` (the swept transaction
/// `n`, its k-th call and every mutating one after it failing) or `crash:<k>:<n>` (its k-th call
/// aborting it).
pub const SWEEP_PLAN: &str = "SWARMOPS_TEST_META_SWEEP_PLAN";

/// The transaction the store-level sweeps fail or crash: changeset `n`, a root, and
/// `release/swept` created at it, in one frame.
pub fn swept(store: &MetaStore, n: u64) -> Result<CommitInfo, StoreError> {
    let mut write = store.write()?;
    write.insert_changeset(changeset(n))?;
    write.cas_ref(SWEPT_REF, None, Some(id(n)), ACTOR)?;
    write.commit()
}

/// What a transaction's outcome says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// It returned, or failed after its commit point ([`StoreError::PostCommit`]).
    Committed,
    /// It failed otherwise: nothing of it may be in the store.
    NotCommitted,
    /// It aborted (a crash, or the documented abort after a failed cut): either may hold.
    Aborted,
}

/// What `outcome`, a transaction's, says of it.
pub fn said(outcome: &Result<CommitInfo, StoreError>) -> Said {
    match outcome {
        Ok(_) | Err(StoreError::PostCommit { .. }) => Said::Committed,
        Err(_) => Said::NotCommitted,
    }
}

/// What the swept transaction on a [`sweep_template`] store does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// It moves the checkpoint: 31 frames of one changeset each, the 32nd swept.
    Checkpoint,
    /// It compacts: 1,024 changesets in one frame, the swept one takes the tail past the
    /// threshold.
    Compaction,
}

/// A metadata store of `format` for the error sweeps, on a filesystem that never syncs
/// (durability is not their subject): changesets `0..n` past the checkpoint, `n` returned, so
/// that the swept transaction `n` does what `template` names. Its root holds the `FORMAT` of
/// `format`, which every open of it reads, as a repository's root does.
pub fn sweep_template(template: Template, format: StoreFormat) -> (TempDir, Utf8PathBuf, u64) {
    let (dir, root) = super::temp_dir();
    std::fs::write(root.join("FORMAT"), format.marker()).unwrap();
    let layout = Layout::with_format(&root, format);
    let store = MetaStore::create_in(layout, Arc::new(NoSyncFs)).unwrap();
    let n = match template {
        Template::Compaction => {
            let mut write = store.write().unwrap();
            for m in 0..Index::COMPACT_THRESHOLD {
                write.insert_changeset(changeset(m)).unwrap();
            }
            write.commit().unwrap();
            Index::COMPACT_THRESHOLD
        }
        Template::Checkpoint => {
            for m in 0..31 {
                insert(&store, m);
            }
            31
        }
    };
    assert_eq!(ckpt(&root), 0);
    (dir, root, n)
}

/// A copy of the store at `template`, in a temp dir of its own.
pub fn copy_of(template: &Utf8Path) -> (TempDir, Utf8PathBuf) {
    let (dir, root) = super::temp_dir();
    fs::copy_dir(template.as_std_path(), root.as_std_path());
    (dir, root)
}

/// How many filesystem calls `run` makes on a copy of the store at `template`, one index file
/// synced at a time, no call failing.
pub fn calls_of(template: &Utf8Path, run: impl FnOnce(&MetaStore)) -> u64 {
    let (_dir, root) = copy_of(template);
    let counter = Arc::new(CrashAtFs::new(NoSyncFs, 0));
    let mut store = MetaStore::open_with_fs(&root, Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
    store.set_parallelism(1);
    run(&store);
    counter.calls()
}

/// The store at `root`, after transactions that each `said` what they did of their changeset
/// (`(n, said)`), on a template of changesets `0..template`, the first of them the swept one
/// (with `release/swept`): it opens (on a filesystem that never syncs, under the order checker:
/// durability is not the subject here), takes a write (its recovery runs) and a flush; the
/// template's changesets are all there; a transaction's changeset is there exactly when it said
/// it committed (either, after an abort), the swept one's ref with it; and the index holds each
/// changeset once, the checkpoint at the journal's end.
#[track_caller]
pub fn assert_recovers(root: &Utf8Path, template: u64, outcomes: &[(u64, Said)], what: &str) {
    let store = MetaStore::open_with_fs(root, faults::checked(NoSyncFs))
        .unwrap_or_else(|e| panic!("{what}: the store does not open: {e}"));
    store
        .write()
        .and_then(MetaWrite::commit)
        .unwrap_or_else(|e| panic!("{what}: the next write fails: {e}"));
    store
        .flush()
        .unwrap_or_else(|e| panic!("{what}: the flush fails: {e}"));
    let read = store.read().unwrap();
    let mut held = template;
    for (at, &(n, said)) in outcomes.iter().enumerate() {
        let there = read.changeset(id(n)).unwrap().is_some();
        match said {
            Said::Committed => assert!(there, "{what}: changeset {n} said it committed"),
            Said::NotCommitted => assert!(!there, "{what}: changeset {n} said it did not commit"),
            Said::Aborted => {}
        }
        if at == 0 {
            let swept_ref = read.get_ref(SWEPT_REF).unwrap();
            assert_eq!(swept_ref.is_some(), there, "{what}: one frame holds both");
        }
        held += u64::from(there);
    }
    assert_eq!(read.count_changesets().unwrap(), held, "{what}");
    for m in [0, template / 2, template - 1] {
        assert!(
            read.changeset(id(m)).unwrap().is_some(),
            "{what}: changeset {m} is lost"
        );
    }
    drop(read);
    let index = index(root);
    assert_eq!(
        index.len(),
        held,
        "{what}: each changeset once in the index"
    );
    assert_eq!(
        ckpt(root),
        log_len(root),
        "{what}: the flush checkpointed it all"
    );
}

/// What a child of the store-level sweeps does: the swept transaction `n` on the store at
/// `root`, its calls failing or crashing as `plan` says; reports what it said.
pub fn sweep_child(root: &Utf8Path, plan: &str) {
    let parts: Vec<&str> = plan.split(':').collect();
    let k: u64 = parts[1].parse().unwrap();
    let (fs, n): (Arc<dyn FsOps>, u64) = match parts[0] {
        "fail" => {
            let error = match parts[2] {
                "eio" => InjectedError::Eio,
                _ => InjectedError::Enospc,
            };
            let fail = FailAt::new(k, FailMode::Persistent, error);
            (
                faults::checked(FailAtFs::new(NoSyncFs, fail)),
                parts[3].parse().unwrap(),
            )
        }
        _ => (
            faults::checked(CrashAtFs::new(NoSyncFs, k)),
            parts[2].parse().unwrap(),
        ),
    };
    let mut store = MetaStore::open_with_fs(root, fs).unwrap();
    store.set_parallelism(1);
    let outcome = swept(&store, n);
    println!("{REPORT}{:?}", said(&outcome));
}

/// Runs a child of the store-level sweeps on `root` with `plan` (see [`SWEEP_PLAN`]), test
/// `test` again (by the name the harness knows it by: [`test_name!`](crate::test_name!) in the
/// test's own module, which runs [`sweep_child`]), and returns what its transaction said:
/// aborted only by the documented abort (a cut after a failed journal append that fails too),
/// which says why on stderr, or by a planned crash.
pub fn run_sweep_child(test: &str, root: &Utf8Path, plan: &str) -> Said {
    let child = child::run(
        child::this_test(test)
            .env(SWEEP_ROOT, root)
            .env(SWEEP_PLAN, plan),
    );
    let stderr = String::from_utf8_lossy(&child.stderr);
    if child.status.signal() == Some(SIGABRT) {
        assert!(
            plan.starts_with("crash") || stderr.contains("could not be cut back off it"),
            "{plan}: an abort that is not the documented one\n{stderr}"
        );
        return Said::Aborted;
    }
    let stdout = String::from_utf8_lossy(&child.stdout);
    match report(&child).as_str() {
        "Committed" => Said::Committed,
        "NotCommitted" => Said::NotCommitted,
        other => panic!("{plan}: {other:?}\n{stdout}\n{stderr}"),
    }
}
