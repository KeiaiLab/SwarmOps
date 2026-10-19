//! Path history (storage spec §5's `LastModified`; operations spec §1's `path_history` and
//! `log --path`): the newest changeset on a first-parent chain whose delta has an entry at a path
//! or below it, and the history that chasing it gives, from one touch to the newest touch below
//! that one's first parent.
//!
//! Every test owns a fresh store. The long chains and the random ones are committed through a
//! filesystem that never syncs (`NoSyncFs`): their subject is what the store holds and what
//! history reads of it, which a flush left out does not change.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{BlobId, ChangesetId, EntryTarget, LeafKind, ObjectKind, hash_object};
use swarmops_repo::merge::tree_diff::PathFilter;
use swarmops_repo::{
    Change, Derivation, LastModified, LastTouch, LocalRepo, PathEvent, PathKey, RepoError,
};
use swarmops_store::FsOps;
use swarmops_testkit::fs::{Call, NoSyncFs, Watch, Watched};
use swarmops_testkit::repo::{blob, delete, new_repo, path, put, put_as, request};
use tempfile::TempDir;

/// A fresh store in a temp dir of its own, on a filesystem that never syncs.
fn unsynced_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).expect("create");
    (dir, root, repo)
}

/// Commits `changes` on `parent` (a root without one) with `message`; one that changes nothing is
/// committed too, with an empty delta. Its id.
#[track_caller]
fn commit_on(
    repo: &LocalRepo,
    parent: Option<ChangesetId>,
    changes: Vec<Change>,
    message: &str,
) -> ChangesetId {
    let parents: Vec<ChangesetId> = parent.into_iter().collect();
    let mut req = request(&parents, changes, message);
    req.allow_empty = true;
    repo.commit(req).expect("commit").id
}

/// A first-parent chain on `repo`, a changeset per step of `steps`, the root first: their ids.
fn chain_of(repo: &LocalRepo, steps: Vec<Vec<Change>>) -> Vec<ChangesetId> {
    let mut chain: Vec<ChangesetId> = Vec::new();
    for (i, changes) in steps.into_iter().enumerate() {
        let id = commit_on(repo, chain.last().copied(), changes, &format!("step {i}"));
        chain.push(id);
    }
    chain
}

/// What a path holds after a changeset, as the tests compare it: a leaf (its kind and blob), a
/// directory (whatever its tree), or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Leaf(LeafKind, BlobId),
    Dir,
    Nothing,
}

impl From<Option<EntryTarget>> for Held {
    fn from(entry: Option<EntryTarget>) -> Held {
        match entry {
            Some(EntryTarget::Leaf(kind, blob)) => Held::Leaf(kind, blob),
            Some(EntryTarget::Dir(_)) => Held::Dir,
            None => Held::Nothing,
        }
    }
}

/// `events` as the positions of their changesets on `chain`, each with what the path holds after
/// it.
fn on_chain(chain: &[ChangesetId], events: &[PathEvent]) -> Vec<(usize, Held)> {
    events
        .iter()
        .map(|event| {
            let at = chain
                .iter()
                .position(|&cs| cs == event.changeset)
                .unwrap_or_else(|| panic!("{} is not on the chain", event.changeset));
            (at, Held::from(event.entry))
        })
        .collect()
}

/// `LastModified` is built in: `last_modified`, version 1, not eager, a memo every 64 steps. A
/// repository derives it with no registration, created or opened, and refuses to register it
/// again; its value of a changeset that leaves the path alone is the last one that touched it.
#[test]
fn last_modified_is_built_in() {
    assert_eq!(
        (
            LastModified::NAME,
            LastModified::VERSION,
            LastModified::EAGER,
            LastModified::MEMO_EVERY,
        ),
        ("last_modified", 1, false, Some(64))
    );
    let (_dir, root, mut repo) = new_repo();
    let chain = chain_of(&repo, vec![vec![put("a", b"1")], vec![put("b", b"1")]]);
    let key = PathKey(path("a"));
    let touch = repo.derived::<LastModified>(chain[1], &key).unwrap();
    assert_eq!(touch, LastTouch(Some(chain[0])));
    match repo.register(LastModified) {
        Err(RepoError::Derived { name, why }) => {
            assert_eq!(name, "last_modified", "{why}");
            assert!(why.contains("registered already"), "{why}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let reopened = LocalRepo::open(&root).unwrap();
    let read = reopened.snapshot().unwrap();
    assert_eq!(
        read.last_modified(chain[1], &path("b")).unwrap(),
        Some(chain[1])
    );
    assert_eq!(
        read.last_modified(chain[1], &path("a")).unwrap(),
        Some(chain[0])
    );
}

/// A path's history follows it through every kind of change: deleted and added again, made
/// executable, made a symlink, made a directory (whose own changes below it count), and a file
/// again; the changesets that leave it alone are not in it. Each event names what the path holds
/// after its changeset, nothing after a delete. A history from an older changeset starts there,
/// a limit keeps the newest events, and `log_paths` lists the same changesets.
#[test]
fn history_follows_a_path_through_delete_re_add_and_type_change() {
    let (_dir, _root, repo) = new_repo();
    let chain = chain_of(
        &repo,
        vec![
            vec![put("a", b"1"), put("b", b"1")],
            vec![put("b", b"2")],
            vec![delete("a")],
            vec![put("a", b"2")],
            vec![put_as("a", LeafKind::Exec, b"2")],
            vec![put_as("a", LeafKind::Symlink, b"b")],
            vec![delete("a"), put("a/x", b"x")],
            vec![put("a/y", b"y")],
            vec![put("b", b"3")],
            vec![delete("a/x"), delete("a/y"), put("a", b"3")],
        ],
    );
    let head = chain[9];
    let read = repo.snapshot().unwrap();

    let history = read.path_history(head, &path("a"), 100).unwrap();
    let want = [
        (9, Held::Leaf(LeafKind::File, blob(b"3"))),
        (7, Held::Dir),
        (6, Held::Dir),
        (5, Held::Leaf(LeafKind::Symlink, blob(b"b"))),
        (4, Held::Leaf(LeafKind::Exec, blob(b"2"))),
        (3, Held::Leaf(LeafKind::File, blob(b"2"))),
        (2, Held::Nothing),
        (0, Held::Leaf(LeafKind::File, blob(b"1"))),
    ];
    assert_eq!(on_chain(&chain, &history), want);
    // An event's entry is what the path holds after it.
    for event in &history {
        assert_eq!(
            event.entry,
            read.entry_at(event.changeset, &path("a")).unwrap()
        );
    }
    // Below the directory it was for a while.
    let below = read.path_history(head, &path("a/x"), 100).unwrap();
    let below_want = [
        (9, Held::Nothing),
        (6, Held::Leaf(LeafKind::File, blob(b"x"))),
    ];
    assert_eq!(on_chain(&chain, &below), below_want);
    // From an older changeset, and the newest few.
    let older = read.path_history(chain[5], &path("a"), 100).unwrap();
    assert_eq!(on_chain(&chain, &older), want[3..]);
    let newest = read.path_history(head, &path("a"), 3).unwrap();
    assert_eq!(on_chain(&chain, &newest), want[..3]);
    let none: Vec<PathEvent> = Vec::new();
    assert_eq!(read.path_history(head, &path("a"), 0).unwrap(), none);
    // The same changesets, as `log --path` lists them.
    let logged = read
        .log_paths(head, &PathFilter::under(vec![path("a")]), 100)
        .unwrap();
    let ids: Vec<ChangesetId> = logged.iter().map(|stored| stored.id).collect();
    let touches: Vec<ChangesetId> = want.iter().map(|&(at, _)| chain[at]).collect();
    assert_eq!(ids, touches);
    assert_eq!(
        read.last_modified(chain[1], &path("a")).unwrap(),
        Some(chain[0])
    );
    assert_eq!(
        read.last_modified(chain[8], &path("a")).unwrap(),
        Some(chain[7])
    );
}

/// A directory's history is that of everything below it: a change to any path below `src` is
/// one of its events, while `src-old` and `srcs/x`, which share its first bytes but are not below
/// it, are not. Once the last file below it is gone, nothing is at `src`.
#[test]
fn a_directory_path_has_the_history_of_its_subtree() {
    let (_dir, _root, repo) = new_repo();
    let chain = chain_of(
        &repo,
        vec![
            vec![put("src/lib.rs", b"1"), put("README", b"1")],
            vec![put("src/main.rs", b"1")],
            vec![put("README", b"2")],
            vec![put("src/deep/mod.rs", b"1")],
            vec![put("src-old", b"1"), put("srcs/x", b"1")],
            vec![delete("src/lib.rs")],
            vec![delete("src/main.rs"), delete("src/deep/mod.rs")],
        ],
    );
    let head = chain[6];
    let read = repo.snapshot().unwrap();
    let history = read.path_history(head, &path("src"), 100).unwrap();
    let want = [
        (6, Held::Nothing),
        (5, Held::Dir),
        (3, Held::Dir),
        (1, Held::Dir),
        (0, Held::Dir),
    ];
    assert_eq!(on_chain(&chain, &history), want);
    let deep = read.path_history(head, &path("src/deep"), 100).unwrap();
    assert_eq!(
        on_chain(&chain, &deep),
        [(6, Held::Nothing), (3, Held::Dir)]
    );
    let file = read.path_history(head, &path("src/lib.rs"), 100).unwrap();
    let file_want = [
        (5, Held::Nothing),
        (0, Held::Leaf(LeafKind::File, blob(b"1"))),
    ];
    assert_eq!(on_chain(&chain, &file), file_want);
    // `src-old` and `srcs` sort right after `src` and its directory: neither is below it.
    let old = read.path_history(head, &path("src-old"), 100).unwrap();
    assert_eq!(
        on_chain(&chain, &old),
        [(4, Held::Leaf(LeafKind::File, blob(b"1")))]
    );
    assert_eq!(
        read.last_modified(chain[4], &path("src")).unwrap(),
        Some(chain[3])
    );
}

/// A path no changeset on the chain touches has no history: no last modifier from any changeset,
/// an empty history, and nothing for `log --path`. So for a path that never existed, one below a
/// file, and one that exists only after the changeset asked about. A changeset the snapshot does
/// not hold is `NotFound`.
#[test]
fn a_path_that_never_existed_has_no_history() {
    let (_dir, _root, repo) = new_repo();
    let chain = chain_of(
        &repo,
        vec![
            vec![put("README", b"1")],
            vec![put("README", b"2")],
            vec![put("later", b"1")],
        ],
    );
    let read = repo.snapshot().unwrap();
    for (from, p) in [
        (chain[2], "nope"),
        (chain[2], "README/below"),
        (chain[2], "nope/deeper"),
        (chain[1], "later"),
    ] {
        assert_eq!(read.last_modified(from, &path(p)).unwrap(), None, "{p}");
        let none: Vec<PathEvent> = Vec::new();
        assert_eq!(read.path_history(from, &path(p), 10).unwrap(), none, "{p}");
        let filter = PathFilter::under(vec![path(p)]);
        let logged = read.log_paths(from, &filter, 10).unwrap();
        assert_eq!(logged.len(), 0, "{p}: {logged:?}");
    }
    let missing = ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"));
    for got in [
        read.last_modified(missing, &path("README")).map(|_| ()),
        read.path_history(missing, &path("README"), 1).map(|_| ()),
        read.log_paths(missing, &PathFilter::all(), 1).map(|_| ()),
        read.log_paths(missing, &PathFilter::under(Vec::new()), 1)
            .map(|_| ()),
    ] {
        match got {
            Err(RepoError::NotFound(what)) => assert_eq!(what, missing.to_string()),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}

/// `log_paths` lists the changesets whose delta has an entry its filter matches, newest first:
/// with every path, those that change anything (an empty changeset changes nothing); with no
/// prefix, none; with prefixes, each changeset once, however many of them it touches.
#[test]
fn log_paths_lists_the_changesets_whose_delta_the_filter_matches() {
    let (_dir, _root, repo) = new_repo();
    let chain = chain_of(
        &repo,
        vec![
            vec![put("a", b"1"), put("d/e/f", b"1")],
            vec![],
            vec![put("d/x", b"1")],
            vec![put("b", b"1")],
            vec![put("a", b"2"), put("d/e/f", b"2")],
        ],
    );
    let head = chain[4];
    let read = repo.snapshot().unwrap();
    let logged = |filter: &PathFilter, limit: usize| -> Vec<usize> {
        let stored = read.log_paths(head, filter, limit).unwrap();
        stored
            .iter()
            .map(|s| chain.iter().position(|&cs| cs == s.id).unwrap())
            .collect()
    };
    assert_eq!(logged(&PathFilter::all(), 100), [4, 3, 2, 0]);
    assert_eq!(logged(&PathFilter::all(), 2), [4, 3]);
    assert_eq!(
        logged(&PathFilter::under(Vec::new()), 100),
        Vec::<usize>::new()
    );
    let nested = PathFilter::under(vec![path("d"), path("d/e"), path("a")]);
    assert_eq!(logged(&nested, 100), [4, 2, 0]);
    assert_eq!(logged(&nested, 1), [4]);
    let one = PathFilter::under(vec![path("d/e")]);
    assert_eq!(logged(&one, 100), [4, 0]);
    // What a page holds is the stored changeset, record and object.
    let first = read.log_paths(head, &one, 1).unwrap();
    assert_eq!(
        first[0].changeset,
        read.changeset(head).unwrap().unwrap().changeset
    );
}

/// Counts the reads of `last_modified`'s value files under one directory, and the links that
/// place one there (a put that stores a value).
struct ValueCalls {
    dir: PathBuf,
    reads: AtomicU64,
    puts: AtomicU64,
}

impl ValueCalls {
    /// The reads and the puts counted since the last call.
    fn take(&self) -> (u64, u64) {
        (
            self.reads.swap(0, Ordering::SeqCst),
            self.puts.swap(0, Ordering::SeqCst),
        )
    }
}

impl Watch for ValueCalls {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if call.path().starts_with(&self.dir) {
            if call.method == "read" {
                self.reads.fetch_add(1, Ordering::SeqCst);
            } else if call.method == "link_noclobber" {
                self.puts.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

/// The changesets whose `last_modified` values the store at `root` holds, by id (the file names'
/// key suffix left out).
fn stored_subjects(root: &Utf8Path) -> BTreeSet<String> {
    let dir = root.join("meta/derived/last_modified/1");
    let mut subjects = BTreeSet::new();
    for fanout in fs::read_dir(&dir).unwrap() {
        let fanout = fanout.unwrap();
        let prefix = fanout.file_name().into_string().unwrap();
        for file in fs::read_dir(fanout.path()).unwrap() {
            let name = file.unwrap().file_name().into_string().unwrap();
            let (rest, _key) = name.split_once('.').expect("a key suffix");
            subjects.insert(format!("{prefix}{rest}"));
        }
    }
    subjects
}

/// A path touched at changeset 0 and left alone by the 1,000 after it: the first query from the
/// head walks the chain once, looking each changeset's value up once, derives one value (at 0)
/// and stores it at the chain's origin and at every 64th changeset from there, 16 memos. Then a
/// query of the head reads its memo and derives nothing, and a query from the middle of the chain
/// walks at most 64 steps to the memo below it, storing nothing.
#[test]
fn a_long_quiet_stretch_is_walked_once() {
    let (_dir, root, writer) = unsynced_repo();
    let mut chain = vec![commit_on(
        &writer,
        None,
        vec![put("p", b"p"), put("q", b"0")],
        "0",
    )];
    for i in 1..=1000_u32 {
        let content = i.to_string();
        let parent = chain.last().copied();
        chain.push(commit_on(
            &writer,
            parent,
            vec![put("q", content.as_bytes())],
            &content,
        ));
    }
    drop(writer);
    let head = chain[1000];
    let canonical = root.canonicalize_utf8().unwrap();
    let calls = Arc::new(ValueCalls {
        dir: canonical
            .join("meta/derived/last_modified")
            .into_std_path_buf(),
        reads: AtomicU64::new(0),
        puts: AtomicU64::new(0),
    });
    let watched: Arc<dyn FsOps> = Arc::new(Watched(Arc::clone(&calls), NoSyncFs));
    let repo = LocalRepo::open_with_fs(&root, watched).unwrap();
    let read = repo.snapshot().unwrap();

    assert_eq!(
        read.last_modified(head, &path("p")).unwrap(),
        Some(chain[0])
    );
    let (reads, puts) = calls.take();
    assert_eq!(reads, 1001, "each changeset's value is looked up once");
    assert_eq!(puts, 17, "16 memos, and the value derived at changeset 0");
    let mut want: BTreeSet<String> = (0..16).map(|k| chain[1000 - 64 * k].to_string()).collect();
    want.insert(chain[0].to_string());
    assert_eq!(stored_subjects(&root), want);

    assert_eq!(
        read.last_modified(head, &path("p")).unwrap(),
        Some(chain[0])
    );
    let (reads, puts) = calls.take();
    assert!(reads <= 2, "the head's memo: {reads} reads");
    assert_eq!(puts, 0, "nothing derived again");

    assert_eq!(
        read.last_modified(chain[500], &path("p")).unwrap(),
        Some(chain[0])
    );
    let (reads, puts) = calls.take();
    assert!(
        reads <= 65,
        "at most 64 steps to the memo below: {reads} reads"
    );
    assert_eq!(puts, 0, "a walk of 64 steps or fewer stores no memo");

    let history = read.path_history(head, &path("p"), 10).unwrap();
    assert_eq!(
        on_chain(&chain, &history),
        [(0, Held::Leaf(LeafKind::File, blob(b"p")))]
    );
}

/// A xorshift64* generator, so that the random chains are the same in every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

/// The leaves the random chains change: `c` and `c/k` exclude each other (a file, or a directory
/// holding `k`), so that a chain changes a path from file to directory and back.
const LEAVES: [&str; 5] = ["a", "c", "c/k", "d/x", "d/e/z"];

/// The paths the random chains ask about: the leaves, the directories above them, a path below a
/// leaf, and paths no chain ever touches.
const ASKED: [&str; 10] = [
    "a",
    "c",
    "c/k",
    "d",
    "d/x",
    "d/e",
    "d/e/z",
    "c/k/below",
    "e",
    "zz",
];

/// A tree as the random chains model it: each leaf's kind and content byte.
type Model = BTreeMap<&'static str, (LeafKind, u8)>;

/// What `model` holds at `p` and below it.
fn subtree<'m>(model: &'m Model, p: &str) -> Vec<(&'m &'static str, &'m (LeafKind, u8))> {
    let below = format!("{p}/");
    model
        .iter()
        .filter(|(leaf, _)| **leaf == p || leaf.starts_with(&below))
        .collect()
}

/// What `model` holds at `p`: a leaf, a directory (a leaf below it), or nothing.
fn held_in(model: &Model, p: &str) -> Held {
    match model.get(p) {
        Some(&(kind, byte)) => Held::Leaf(kind, blob(&[byte])),
        None if !subtree(model, p).is_empty() => Held::Dir,
        None => Held::Nothing,
    }
}

/// One random step from `model`: the next tree, and the changes that make it (none, sometimes).
fn random_step(rng: &mut Rng, model: &Model) -> (Model, Vec<Change>) {
    const KINDS: [LeafKind; 3] = [LeafKind::File, LeafKind::Exec, LeafKind::Symlink];
    let mut next = model.clone();
    for leaf in LEAVES {
        match rng.below(10) {
            0..=4 => {}
            5..=7 => {
                let kind = KINDS[rng.below(3)];
                let byte = u8::try_from(rng.below(3)).unwrap();
                next.insert(leaf, (kind, byte));
            }
            _ => {
                next.remove(leaf);
            }
        }
    }
    if next.contains_key("c") && next.contains_key("c/k") {
        let loser = if rng.below(2) == 0 { "c" } else { "c/k" };
        next.remove(loser);
    }
    let mut changes = Vec::new();
    for leaf in LEAVES {
        if model.get(leaf) == next.get(leaf) {
            continue;
        }
        changes.push(match next.get(leaf) {
            Some(&(kind, byte)) => put_as(leaf, kind, &[byte]),
            None => delete(leaf),
        });
    }
    (next, changes)
}

/// How many random chains [`last_modified_is_the_newest_touch_on_the_chain`] commits:
/// `PROPTEST_CASES` (`just check-slow` sets 2000), or 32.
fn chains() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(32)
}

/// Against a reference walk over random chains: `last_modified(cs, p)` is the newest changeset on
/// the chain up to `cs` whose tree differs from its parent's at `p` or below it (a native commit's
/// delta lists exactly the leaves it changes), `path_history` lists every such changeset newest
/// first with what `p` holds after it, and `log_paths` those that touch either of two paths. The
/// chains (up to 10 changesets of puts, kind changes, deletes, a file becoming a directory and
/// back, and empty changesets) come from seeded generators, the same in every run: this crate
/// has no `proptest` dev-dependency, and M2 keeps `Cargo.lock` to its Task 2.
#[test]
fn last_modified_is_the_newest_touch_on_the_chain() {
    let (_dir, _root, repo) = unsynced_repo();
    for seed in 0..chains() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::try_from(seed).unwrap());
        let mut models: Vec<Model> = Vec::new();
        let mut chain: Vec<ChangesetId> = Vec::new();
        for i in 0..=rng.below(10) {
            let (next, changes) = random_step(&mut rng, models.last().unwrap_or(&Model::new()));
            let message = format!("seed {seed} step {i}");
            chain.push(commit_on(&repo, chain.last().copied(), changes, &message));
            models.push(next);
        }
        let empty = Model::new();
        let changed = |i: usize, p: &str| {
            let before = if i == 0 { &empty } else { &models[i - 1] };
            subtree(before, p) != subtree(&models[i], p)
        };
        let read = repo.snapshot().unwrap();
        for (j, &from) in chain.iter().enumerate() {
            for p in ASKED {
                let touches: Vec<usize> = (0..=j).rev().filter(|&i| changed(i, p)).collect();
                let at = format!("seed {seed}, changeset {j}, path {p}");
                let newest = touches.first().map(|&i| chain[i]);
                assert_eq!(read.last_modified(from, &path(p)).unwrap(), newest, "{at}");
                let limit = 1 + rng.below(4);
                let history = read.path_history(from, &path(p), limit).unwrap();
                let want: Vec<(usize, Held)> = touches
                    .iter()
                    .take(limit)
                    .map(|&i| (i, held_in(&models[i], p)))
                    .collect();
                assert_eq!(on_chain(&chain, &history), want, "{at}, limit {limit}");
            }
            let pair = [ASKED[rng.below(ASKED.len())], ASKED[rng.below(ASKED.len())]];
            let filter = PathFilter::under(pair.iter().map(|p| path(p)).collect());
            let want: Vec<ChangesetId> = (0..=j)
                .rev()
                .filter(|&i| changed(i, pair[0]) || changed(i, pair[1]))
                .map(|i| chain[i])
                .collect();
            let logged: Vec<ChangesetId> = read
                .log_paths(from, &filter, 100)
                .unwrap()
                .iter()
                .map(|stored| stored.id)
                .collect();
            assert_eq!(logged, want, "seed {seed}, changeset {j}, paths {pair:?}");
        }
    }
}
