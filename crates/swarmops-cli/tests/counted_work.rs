//! The counted-work gates (the M21 design §2.10, the M21 plan's task M21-3): an operation's
//! counts at the store's file boundary ([`Counters`]) are the same on a store of 1,000
//! changesets and files as on one of 50,000. Counts, not times: what grows with the history shows
//! as a count that differs, with no clock noise.
//!
//! ```text
//!   the small pair (just check)            the A1 pair (a1_*, SWARMOPS_SCALE=1)
//!   built(1,000) ──┐                       bulk::build_small ─┐
//!   built(50,000) ─┴─▶ clone_of ─▶ prepare ─▶ Counts::since ◀─┴─ bulk::open(Large)
//!                      (hard links)  (unmeasured)  around the one operation
//! ```
//!
//! Each gate prepares both stores the same way, unmeasured, then counts one operation on each and
//! compares the counts: object reads and writes, journal writes and syncs, lock waits and
//! timeouts. The times (`*_us`) are not compared.
//!
//! The small pair is built once under `SWARMOPS_BENCH_DIR` and each test works on a clone whose
//! object files are hard links. `bulk`'s shape rewrites a 1,000-entry directory per changeset,
//! about 41 KB a changeset, too much for `just check` at 50,000; here the history alternates one
//! small file over a fixed tree, so 50,000 changesets cost two root trees. The tree,
//! `data/dNNN/fNNN.txt`, is three levels deep like `bulk`'s `gNNN/dNNN/fNNN.txt`. The newest
//! operations of both stores record one changeset each, more than a page of the op log, so that
//! an operation that reads a page (`ws create` does) sees the same page on both.
//!
//! The A1 twins (`a1_*`) run the same gates on `bulk`'s two stores, each cloned, only with
//! `SWARMOPS_SCALE=1` ([`bulk::gate`]); without it each prints `scale-perf: <case> not run`, and
//! while the large store misses a part, `scale-perf: <case> not run (fixture k of n)`.
//!
//! The stores are FORMAT 2 ([`swarmops_store::DEFAULT_INIT_FORMAT`]), which has no view steps; a
//! FORMAT 3 store past `RUN_BACKLOG_MAX` would step its views after a commit on one side only.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{
    ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, InfoReq, LandReq, ModeInput,
    ReadTreeReq, RebaseReq, StoreOps, WsCreateReq, WsStatusReq,
};
use swarmops_core::{
    BlobId, ChangesetId, LeafKind, ObjectKind, Signature, Tree, TreeEntry, TreeId, hash_object,
};
use swarmops_ops::LocalOps;
use swarmops_ops::serve::access::{Access, Action};
use swarmops_repo::{LocalRepo, RawChangeset};
use swarmops_store::fs_ops::counters::{Counters, Counts};
use swarmops_store::{DEFAULT_INIT_FORMAT, RealFs};
use swarmops_testkit::bulk::{self, Scale, Size};
use swarmops_testkit::measure::base_dir;
use swarmops_ws::{Ctx, RealHost};
use tempfile::TempDir;

/// The changesets and files of the small pair's smaller store.
const SMALL: usize = 1_000;

/// The changesets and files of the small pair's larger store.
const LARGE: usize = 50_000;

/// The files of the generated tree's directories.
const PER_DIR: usize = 1_000;

/// The changesets one frame of the generator records, but for the tail's.
const PART: usize = 2_048;

/// The newest operations of a generated history, each of one changeset as a commit records it:
/// more than a page of the op log (`ws create` reads one, 100 operations), so that an operation
/// that reads a page reads the same on both stores of a pair.
const TAIL_OPS: usize = 128;

/// The files of the directory a sparse workspace holds.
const SPARSE_FILES: usize = 100;

/// The bytes of an object file's header (storage-core design, §2): its kind and size, all a
/// look at the object reads.
const OBJECT_HEADER: usize = 16;

/// The identity every operation acts as.
const AGENT: &str = "agent/counted";

/// The ref the stores' heads are on.
const MAIN: &str = "main";

/// The grants of the read grant check: the agent may read.
const ACCESS: &str = "version = 1\n\n[[grant]]\nwho = [\"agent/*\"]\nread = true\n";

/// The operation a gate counts, after the same unmeasured preparation on both stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    /// A commit of one new file into a directory three levels down, `main` moved.
    Commit,
    /// A land of a side changeset onto a `main` that moved on other paths: one attempt.
    Land,
    /// A workspace made of a 100-file directory alone.
    Sparse,
    /// The status of a workspace of the whole tree, nothing touched.
    Status,
    /// A rebase of a side changeset onto a `main` that changed other paths.
    Rebase,
    /// The grants read and a read checked.
    Grant,
    /// The planted variant: [`Work::Commit`], then a listing of the whole tree.
    CommitAndList,
}

/// One store of a pair, a copy the test may change: its operations, and its tree's shape.
struct Store {
    /// The directory the store and its workspaces are in, removed at the end.
    dir: TempDir,
    root: Utf8PathBuf,
    ops: LocalOps,
    /// A directory three levels down: `data/d000` or `bulk`'s `g000/d000`.
    deep: &'static str,
}

/// A count compared by the gates, by name: the times are left out.
fn work_counts(counts: &Counts) -> [(&'static str, u64); 6] {
    [
        ("object_reads", counts.object_reads),
        ("object_writes", counts.object_writes),
        ("journal_writes", counts.journal_writes),
        ("journal_syncs", counts.journal_syncs),
        ("lock_waits", counts.lock_waits),
        ("lock_timeouts", counts.lock_timeouts),
    ]
}

/// The compared counts that differ between `small` and `large`, as `name small → large`.
fn differences(small: &Counts, large: &Counts) -> Vec<String> {
    let pairs = work_counts(small).into_iter().zip(work_counts(large));
    pairs
        .filter(|((_, a), (_, b))| a != b)
        .map(|((name, a), (_, b))| format!("{name} {a} → {b}"))
        .collect()
}

/// What `run` moved on this process's counters, and its answer.
fn counted<T>(run: impl FnOnce() -> T) -> (T, Counts) {
    let before = Counters::process().snapshot();
    let answer = run();
    let after = Counters::process().snapshot();
    (answer, after.since(&before))
}

/// The identity of the operations.
fn agent() -> Identity {
    Identity::parse(AGENT).expect("a valid identity")
}

/// A put of `text` at `path`, a plain file.
fn put(path: &str, text: &str) -> ChangeInput {
    let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
    change.content = Some(ContentInput::Inline(text.to_owned()));
    change.mode = Some(ModeInput::File);
    change
}

impl Store {
    /// How many changesets the store holds.
    fn size(&self) -> u64 {
        let info = self.ops.repo_info(InfoReq::default());
        info.expect("repo info").changesets
    }

    /// The workspace operations' context on this store.
    fn ctx(&self) -> Ctx<'_> {
        Ctx::new(
            &self.ops,
            self.root.clone(),
            Some(agent()),
            Arc::new(RealFs::new()),
            Arc::new(RealHost::default()),
        )
    }

    /// Commits `changes` on `main`'s head, `main` moved along when `advance` says so; the id.
    fn commit(&self, changes: Vec<ChangeInput>, advance: Advance) -> String {
        let mut req = CommitReq::new(changes, "counted".to_owned());
        req.parent = Some(MAIN.into());
        if advance == Advance::Main {
            req.advance = Some(MAIN.to_owned());
        }
        self.ops.commit(&agent(), req).expect("commit").changeset
    }

    /// A side changeset of one file in the deep directory, then `main` moved on other paths:
    /// what a land or a rebase then merges. The side changeset's id.
    fn diverge(&self) -> String {
        let side = self.commit(
            vec![put(&format!("{}/side.txt", self.deep), "side\n")],
            Advance::No,
        );
        self.commit(vec![put("other/x.txt", "other\n")], Advance::Main);
        side
    }

    /// The request that makes workspace `name` beside the store, of the directories `prefixes`
    /// (the whole tree when empty).
    fn workspace(&self, name: &str, prefixes: &[&str]) -> WsCreateReq {
        let mut req = WsCreateReq::new(name);
        let path = Utf8Path::from_path(self.dir.path()).expect("a UTF-8 temp path");
        req.path = Some(path.join(name).to_string());
        req.prefixes = prefixes.iter().map(|p| (*p).to_owned()).collect();
        req.all = prefixes.is_empty();
        req
    }

    /// Spoils the object file of the blob holding `text`: its header kept, so that a look at
    /// it (`ObjectStore::stat`) finds a blob, its payload's bytes flipped, so that a read of it
    /// fails. A new file in its place, never a write into a linked one.
    fn spoil_blob(&self, text: &str) {
        let hex = hash_object(ObjectKind::Blob, text.as_bytes()).to_hex();
        let (fanout, rest) = hex.split_at(2);
        let path = self.root.join("objects").join(fanout).join(rest);
        let mut bytes = fs::read(&path).expect("read the blob's object file");
        for byte in &mut bytes[OBJECT_HEADER..] {
            *byte = !*byte;
        }
        fs::remove_file(&path).expect("remove the blob's object file");
        fs::write(&path, bytes).expect("write a spoilt one");
    }
}

/// Whether a commit moves `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Advance {
    Main,
    No,
}

/// Prepares `store` for `work`, unmeasured, then counts `work` on it.
fn count(store: &Store, work: Work) -> Counts {
    match work {
        Work::Commit => {
            let change = put(&format!("{}/new.txt", store.deep), "new\n");
            counted(|| store.commit(vec![change], Advance::Main)).1
        }
        Work::Land => {
            let side = store.diverge();
            let mut req = LandReq::new(side);
            req.max_attempts = 1;
            let (resp, counts) = counted(|| store.ops.land(&agent(), req));
            assert_eq!(resp.expect("land").attempts, 1, "one attempt");
            counts
        }
        Work::Sparse => {
            let changes = (0..SPARSE_FILES)
                .map(|k| put(&format!("sparse/f{k:03}.txt"), &format!("sparse {k}\n")))
                .collect();
            store.commit(changes, Advance::Main);
            let req = store.workspace("sparse", &["sparse"]);
            let (made, counts) = counted(|| swarmops_ws::create(&store.ctx(), req));
            made.expect("ws create");
            counts
        }
        Work::Status => {
            swarmops_ws::create(&store.ctx(), store.workspace("all", &[])).expect("ws create");
            let (status, counts) =
                counted(|| swarmops_ws::status(&store.ctx(), WsStatusReq::new("all")));
            assert!(status.expect("ws status").changes.is_empty(), "untouched");
            counts
        }
        Work::Rebase => {
            // The changed files' blobs are spoilt: a rebase that read one would fail.
            let side = store.diverge();
            store.spoil_blob("side\n");
            store.spoil_blob("other\n");
            let (resp, counts) = counted(|| store.ops.rebase(&agent(), RebaseReq::new(side)));
            resp.expect("rebase");
            counts
        }
        Work::Grant => {
            fs::write(store.root.join("access.toml"), ACCESS).expect("write the grants");
            let check = || {
                let access = Access::load(&store.ops)?;
                access.check(&agent(), Action::Read, MAIN)
            };
            let (granted, counts) = counted(check);
            granted.expect("read granted");
            counts
        }
        Work::CommitAndList => {
            let change = put(&format!("{}/new.txt", store.deep), "new\n");
            let mut list = ReadTreeReq::new(MAIN);
            list.depth = 3;
            list.limit = 2 * LARGE;
            let run = || {
                store.commit(vec![change], Advance::Main);
                store.ops.read_tree(list).expect("list the tree")
            };
            counted(run).1
        }
    }
}

/// The small pair's two stores, each made in a directory of its own.
fn small_pair() -> (Store, Store) {
    (history(SMALL), history(LARGE))
}

/// Counts `work` on both stores of the small pair: the two counts.
fn on_small_pair(work: Work) -> (Counts, Counts) {
    let (small, large) = small_pair();
    (count(&small, work), count(&large, work))
}

/// The gate: `work` counts the same on both stores of the small pair.
fn same_on_small_pair(work: Work) -> (Counts, Counts) {
    let (small, large) = on_small_pair(work);
    let differ = differences(&small, &large);
    assert!(
        differ.is_empty(),
        "{work:?} counts differ, {SMALL} → {LARGE} changesets: {differ:?}\n\
         small {small:?}\nlarge {large:?}"
    );
    (small, large)
}

#[test]
fn a_one_file_commit_counts_the_same() {
    same_on_small_pair(Work::Commit);
}

#[test]
fn one_land_attempt_counts_the_same() {
    same_on_small_pair(Work::Land);
}

#[test]
fn a_sparse_create_of_100_files_counts_the_same() {
    same_on_small_pair(Work::Sparse);
}

#[test]
fn status_of_an_untouched_workspace_reads_no_object() {
    let (small, large) = same_on_small_pair(Work::Status);
    assert_eq!(small.object_reads, 0, "1,000 files: no object read");
    assert_eq!(large.object_reads, 0, "50,000 files: no object read");
}

#[test]
fn a_rebase_onto_other_paths_reads_no_blob() {
    same_on_small_pair(Work::Rebase);
}

#[test]
fn a_read_grant_check_counts_the_same() {
    same_on_small_pair(Work::Grant);
}

/// The gates' self-check: an operation that also lists the whole tree reads more objects on the
/// larger store, and the comparison says so.
#[test]
fn a_planted_whole_tree_read_is_caught() {
    let (small, large) = on_small_pair(Work::CommitAndList);
    let differ = differences(&small, &large);
    assert!(
        differ.iter().any(|d| d.starts_with("object_reads ")),
        "a whole-tree read must differ: {differ:?}"
    );
}

/// The A1 twins, by name: each ends on `scale-perf: <name> not run` without the gate.
const A1_CASES: [&str; 6] = [
    "a1_one_file_commit_counts_the_same",
    "a1_one_land_attempt_counts_the_same",
    "a1_sparse_create_of_100_files_counts_the_same",
    "a1_status_of_an_untouched_workspace_reads_no_object",
    "a1_rebase_onto_other_paths_reads_no_blob",
    "a1_read_grant_check_counts_the_same",
];

/// `bulk`'s directory three levels down, in both of its stores.
const BULK_DEEP: &str = "g000/d000";

/// The gate on A1's pair, `bulk`'s two stores, each cloned: `work` counts the same on both.
/// `None`, its `not run` line out, while the gate is shut or the large store not whole.
fn same_on_a1_pair(case: &str, work: Work) -> Option<(Counts, Counts)> {
    // 1. The gate, then the large store, never built here.
    if bulk::gate(case) == Scale::NotRun {
        return None;
    }
    let Some(large) = bulk::open(Size::Large) else {
        let (k, n) = bulk::parts(Size::Large);
        println!("scale-perf: {case} not run (fixture {k} of {n})");
        return None;
    };
    let small = bulk::build_small();

    // 2. Both cloned, and a page of the op log made alike: `bulk` records a part a frame.
    let small = clone_of(&small.store, BULK_DEEP);
    let large = clone_of(&large.store, BULK_DEEP);
    for store in [&small, &large] {
        for k in 0..TAIL_OPS {
            store.commit(vec![put("tail.txt", &format!("{k}\n"))], Advance::Main);
        }
    }

    // 3. The counts, side by side, then the gate.
    let (small_counts, large_counts) = (count(&small, work), count(&large, work));
    let sizes = format!("{} vs {} changesets", small.size(), large.size());
    let pairs = work_counts(&small_counts)
        .into_iter()
        .zip(work_counts(&large_counts));
    for ((name, a), (_, b)) in pairs {
        bulk::say(case, &format!("{name} {a} vs {b} ({sizes})"));
    }
    let differ = differences(&small_counts, &large_counts);
    assert!(
        differ.is_empty(),
        "{case}: counts differ, {sizes}: {differ:?}"
    );
    Some((small_counts, large_counts))
}

#[test]
#[ignore = "slow, ~48 s at 20,480 changesets (release, load 1,000): clones bulk's large store by hard links"]
fn a1_one_file_commit_counts_the_same() {
    same_on_a1_pair(A1_CASES[0], Work::Commit);
}

#[test]
#[ignore = "slow, ~24 s at 20,480 changesets (release, load 1,000): clones bulk's large store by hard links"]
fn a1_one_land_attempt_counts_the_same() {
    same_on_a1_pair(A1_CASES[1], Work::Land);
}

#[test]
#[ignore = "slow, ~28 s at 20,480 changesets (release, load 1,000): clones bulk's large store by hard links"]
fn a1_sparse_create_of_100_files_counts_the_same() {
    same_on_a1_pair(A1_CASES[2], Work::Sparse);
}

#[test]
#[ignore = "slow, ~21 s at 20,480 changesets (release, load 1,000): clones bulk's large store and writes all its files"]
fn a1_status_of_an_untouched_workspace_reads_no_object() {
    let Some((small, large)) = same_on_a1_pair(A1_CASES[3], Work::Status) else {
        return;
    };
    assert_eq!(small.object_reads, 0, "the small store: no object read");
    assert_eq!(large.object_reads, 0, "the large store: no object read");
}

#[test]
#[ignore = "slow, ~30 s at 20,480 changesets (release, load 1,000): clones bulk's large store by hard links"]
fn a1_rebase_onto_other_paths_reads_no_blob() {
    same_on_a1_pair(A1_CASES[4], Work::Rebase);
}

#[test]
#[ignore = "slow, ~28 s at 20,480 changesets (release, load 1,000): clones bulk's large store by hard links"]
fn a1_read_grant_check_counts_the_same() {
    same_on_a1_pair(A1_CASES[5], Work::Grant);
}

/// The A1 twins' gate: without `SWARMOPS_SCALE` each says `not run` and makes nothing; with it
/// and no large store whole, each says how far the store is and builds none.
#[test]
fn an_a1_twin_without_its_store_says_not_run() {
    let bench = tempfile::tempdir().unwrap();
    for case in A1_CASES {
        // 1. The gate shut: exactly the `not run` line.
        let shut = scale_lines(case, bench.path(), Scale::NotRun);
        assert_eq!(shut, [format!("scale-perf: {case} not run")], "{case}");

        // 2. The gate open, no part of the large store: how far it is.
        let open = scale_lines(case, bench.path(), Scale::Run);
        let far = format!("scale-perf: {case} not run (fixture 0 of {LARGE_PARTS})");
        assert_eq!(open, [far], "{case}");
    }

    // Nothing was built.
    let left: Vec<_> = fs::read_dir(bench.path()).unwrap().collect();
    assert!(
        left.is_empty(),
        "an A1 twin without its store made {left:?}"
    );
}

/// The parts of the large store [`an_a1_twin_without_its_store_says_not_run`] names.
const LARGE_PARTS: usize = 4;

/// The `scale-perf: ` lines of A1 twin `case`, run alone, its gate `scale`, under `bench`, the
/// large store [`LARGE_PARTS`] parts long.
fn scale_lines(case: &str, bench: &Path, scale: Scale) -> Vec<String> {
    let test = swarmops_testkit::test_name!(case);
    let mut command = swarmops_testkit::child::this_test(&test);
    command
        .env(bulk::LARGE_ENV, (LARGE_PARTS * bulk::PART).to_string())
        .env("SWARMOPS_BENCH_DIR", bench);
    match scale {
        Scale::Run => command.env(bulk::SCALE_ENV, "1"),
        Scale::NotRun => command.env_remove(bulk::SCALE_ENV),
    };
    let output = swarmops_testkit::child::run(&mut command);
    assert!(output.status.success(), "{case}: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("scale-perf: "))
        .map(str::to_owned)
        .collect()
}

/// What the generator writes, versioned: a change to the stores' shape changes this, and every
/// store built before is built again.
const GENERATOR: &str = "counted-work 3";

/// The name of a built store's record, written once it is whole: the generator, the format and
/// the size.
const MADE: &str = "made.txt";

/// A copy of the small pair's store of `changesets` changesets and files: see [`built`].
fn history(changesets: usize) -> Store {
    let store = built(changesets);
    clone_of(&store, "data/d000")
}

/// The store of `changesets` changesets and files under `SWARMOPS_BENCH_DIR`, built once and
/// reused while its record matches: the build puts every object durably, minutes at 50,000, so
/// each test works on a clone ([`clone_of`]). Two tests that need it build it one after the
/// other, under a lock beside it. Its directory names the format: a checkout whose `init` makes
/// another format builds its own, and does not remove this one under a clone being made.
fn built(changesets: usize) -> Utf8PathBuf {
    let base = base_dir();
    let name = format!(
        "counted-{changesets}-format-{}",
        DEFAULT_INIT_FORMAT.version()
    );
    let dir = base.join(&name);
    let lock =
        fs::File::create(base.join(format!("{name}.lock"))).expect("create the store's lock file");
    lock.lock().expect("lock the store");
    let store = Utf8PathBuf::from_path_buf(dir.join("store")).expect("a UTF-8 bench path");

    // 1. Whole, by this generator in this format: reused as it is.
    let record = format!(
        "{GENERATOR}\nformat {}\nchangesets {changesets}\n",
        DEFAULT_INIT_FORMAT.version()
    );
    if fs::read_to_string(dir.join(MADE)).is_ok_and(|made| made == record) {
        return store;
    }

    // 2. Else made again from nothing, the record written last.
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", dir.display()),
    }
    fs::create_dir_all(&dir).expect("create the store's directory");
    generate(&store, changesets);
    fs::write(dir.join(MADE), record).expect("write the record");
    store
}

/// Makes at `root` a store of `changesets` changesets and files, `main` on its head: the tree
/// `data/dNNN/fNNN.txt`, and a history that alternates `hist.txt` beside it.
fn generate(root: &Utf8Path, changesets: usize) {
    let repo = LocalRepo::create(root, "counted").expect("create the store");

    // 1. The tree, and two roots that differ in `hist.txt` alone.
    let mut objects = Vec::new();
    let data = data_tree(changesets, &mut objects);
    let roots = [0, 1].map(|turn| {
        let hist = format!("turn {turn}\n").into_bytes();
        let hist_id = BlobId::new(hash_object(ObjectKind::Blob, &hist));
        objects.push((ObjectKind::Blob, hist));
        let root = Tree::new(vec![
            TreeEntry::dir("data", data),
            TreeEntry::leaf("hist.txt", LeafKind::File, hist_id),
        ])
        .expect("a valid root");
        objects.push((ObjectKind::Tree, root.encode()));
        root.id()
    });
    repo.put_objects(objects).expect("put the objects");

    // 2. The history: one chain, each changeset on the other root; a frame a part, then a frame
    //    each for the last `TAIL_OPS`.
    let bulk = changesets - TAIL_OPS;
    let frames = (0..bulk)
        .step_by(PART)
        .map(|from| from..(from + PART).min(bulk));
    let tail = (bulk..changesets).map(|k| k..k + 1);
    let mut parent: Option<ChangesetId> = None;
    for frame in frames.chain(tail) {
        let mut batch = repo.raw_batch();
        for k in frame {
            let mut raw = RawChangeset::new(
                Vec::new(),
                roots[k % 2],
                signature(k),
                signature(k),
                format!("counted {k}").into_bytes(),
                Vec::new(),
                Vec::new(),
            );
            raw.parents = parent.into_iter().collect();
            parent = Some(batch.push(raw).expect("push a changeset"));
        }
        batch.commit().expect("record the part");
    }

    // 3. `main` on the head, and the metadata flushed.
    repo.cas_ref(MAIN, None, parent, "counted")
        .expect("move main");
    repo.flush_metadata().expect("flush the metadata");
}

/// A copy of the store at `from` in a directory of its own beside it, its tree's deep directory
/// `deep`: the object files hard-linked (a store never writes into one), the rest copied.
fn clone_of(from: &Utf8Path, deep: &'static str) -> Store {
    let parent = from
        .parent()
        .and_then(Utf8Path::parent)
        .expect("a bench path");
    let dir = TempDir::new_in(parent).expect("a directory beside the store");
    let root = Utf8Path::from_path(dir.path())
        .expect("a UTF-8 bench path")
        .join("store");
    copy_tree(from.as_std_path(), root.as_std_path(), Link::Copy);
    let ops = LocalOps::open(&root).expect("open the copy");
    Store {
        dir,
        root,
        ops,
        deep,
    }
}

/// How [`copy_tree`] makes a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// A hard link: the object files, which a store never writes into.
    Hard,
    /// A copy of the bytes.
    Copy,
}

/// Copies the directory `from` to `to`; `link` says how its files are made, and everything
/// under `objects/` is hard-linked.
fn copy_tree(from: &Path, to: &Path, link: Link) {
    fs::create_dir_all(to).expect("create a directory of the copy");
    for entry in fs::read_dir(from).expect("read a directory of the store") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        let kind = entry.file_type().expect("an entry's type");

        // A directory: its files hard-linked from `objects/` down.
        if kind.is_dir() {
            let below = if entry.file_name() == "objects" {
                Link::Hard
            } else {
                link
            };
            copy_tree(&entry.path(), &target, below);
            continue;
        }

        // A file.
        match link {
            Link::Hard => fs::hard_link(entry.path(), &target).expect("link an object file"),
            Link::Copy => {
                fs::copy(entry.path(), &target).expect("copy a file of the store");
            }
        }
    }
}

/// The `data` directory of `files` files, `PER_DIR` to a directory; its objects into `objects`.
fn data_tree(files: usize, objects: &mut Vec<(ObjectKind, Vec<u8>)>) -> TreeId {
    let mut dirs = Vec::new();
    for d in 0..files.div_ceil(PER_DIR) {
        let mut entries = Vec::new();
        for f in 0..PER_DIR.min(files - d * PER_DIR) {
            let text = format!("file {d} {f}\n").into_bytes();
            let id = BlobId::new(hash_object(ObjectKind::Blob, &text));
            objects.push((ObjectKind::Blob, text));
            entries.push(TreeEntry::leaf(format!("f{f:03}.txt"), LeafKind::File, id));
        }
        let tree = Tree::new(entries).expect("a valid directory");
        objects.push((ObjectKind::Tree, tree.encode()));
        dirs.push(TreeEntry::dir(format!("d{d:03}"), tree.id()));
    }
    let tree = Tree::new(dirs).expect("a valid data directory");
    objects.push((ObjectKind::Tree, tree.encode()));
    tree.id()
}

/// A fixed signature for changeset `k`.
fn signature(k: usize) -> Signature {
    let secs = 1_760_000_000 + i64::try_from(k).expect("a changeset index fits");
    Signature::new(
        b"counted/agent".to_vec(),
        b"agent@counted.swarmops.local".to_vec(),
        secs,
        0,
        false,
    )
    .expect("a zero offset is valid")
}
