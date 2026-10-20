//! First-parent ancestry (storage spec §4's merge base; M2 Decisions 7 and 8): the walk back along
//! first parents (`RepoRead::first_parents`), the ancestor `n` first-parent steps back
//! (`RepoRead::ancestor`, which a revision's `~N` takes), whether a changeset is a first-parent
//! ancestor of another (`RepoRead::is_ancestor`), and the newest changeset two first-parent chains
//! share (`RepoRead::merge_base`). A walk across at most `SKIP_AFTER` generations takes narrow
//! index lookups and derives nothing; a longer one lifts along the first-parent skip list.
//!
//! Every test owns a fresh store. Random histories are committed through a filesystem that never
//! syncs (`NoSyncFs`), and long chains are built straight through the stores, as the derivation
//! tests build theirs. A build with the `test-hooks` feature, as `just check` builds, also walks
//! the random histories with thresholds below `SKIP_AFTER`, so that their short gaps lift too.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    BlobId, Changeset, ChangesetId, LeafKind, ObjectKind, Tree, TreeEntry, hash_object,
};
use swarmops_repo::{Derivation, FirstParentSkipList, LocalRepo, RepoError, RepoRead, SKIP_AFTER};
use swarmops_store::{ChangesetMeta, FsObjectStore, FsOps, Layout, MetaStore, ObjectStore};
use swarmops_testkit::fs::{NoSyncFs, RecordingFs};
use swarmops_testkit::measure;
use swarmops_testkit::repo::{path, put, request, signature};
use tempfile::TempDir;

/// A fresh store in a temp dir of its own whose filesystem never syncs, for histories of many
/// commits; the directory lives as long as the `TempDir`.
fn unsynced_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).expect("create");
    (dir, root, repo)
}

/// Commits a changeset on `parents` (a root without any) that puts file `f` holding `message`,
/// with that message; its id.
#[track_caller]
fn commit_on(repo: &LocalRepo, parents: &[ChangesetId], message: &str) -> ChangesetId {
    let req = request(parents, vec![put("f", message.as_bytes())], message);
    repo.commit(req).expect("commit").id
}

/// A first-parent chain of `n` changesets on `base` (the first a root without one), each on the
/// one before, their messages `name` and their place in the chain: their ids, the first first.
fn chain_on(repo: &LocalRepo, base: Option<ChangesetId>, n: usize, name: &str) -> Vec<ChangesetId> {
    let mut ids: Vec<ChangesetId> = Vec::with_capacity(n);
    for i in 0..n {
        let parents: Vec<ChangesetId> = ids.last().copied().or(base).into_iter().collect();
        ids.push(commit_on(repo, &parents, &format!("{name} {i}")));
    }
    ids
}

/// The id of a changeset no store holds.
fn unknown_id() -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, b"never committed"))
}

#[track_caller]
fn assert_not_found<T: std::fmt::Debug>(got: Result<T, RepoError>, what: &str) {
    match got {
        Err(RepoError::NotFound(missing)) => assert_eq!(missing, what),
        other => panic!("expected NotFound({what:?}), got {other:?}"),
    }
}

/// The subjects of the skip lists the store holds, from the files under
/// `meta/derived/first_parent_skip_list/1/` (`<2 hex>/<62 hex>`).
fn skip_lists(root: &Utf8Path) -> BTreeSet<String> {
    let dir = root.join(format!(
        "meta/derived/{}/{}",
        FirstParentSkipList::NAME,
        FirstParentSkipList::VERSION
    ));
    let mut subjects = BTreeSet::new();
    let fanouts = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return subjects,
        Err(e) => panic!("read {dir}: {e}"),
    };
    for fanout in fanouts {
        let fanout = fanout.unwrap();
        let prefix = fanout.file_name().into_string().unwrap();
        for file in fs::read_dir(fanout.path()).unwrap() {
            let rest = file.unwrap().file_name().into_string().unwrap();
            subjects.insert(format!("{prefix}{rest}"));
        }
    }
    subjects
}

/// The bytes of `meta/journal.ckpt`, which change when the checkpoint moves.
fn checkpoint(root: &Utf8Path) -> Vec<u8> {
    fs::read(root.join("meta/journal.ckpt")).unwrap()
}

/// `main~k` is the changeset `k` first-parent steps back from `main`'s head, for every `k` up to
/// the root, and nothing one step past it; a merge on the way is walked through its first parent.
/// Checked when every changeset is past the checkpoint (21 commits and a ref move: no checkpoint
/// yet, the store checkpoints every 32 frames), and again after 20 more commits and a ref move,
/// when the checkpoint has moved and the walk crosses it. `RepoRead::ancestor` and
/// `RepoRead::first_parents` give the same changesets. A `~N` past the root by far is found to
/// name nothing from the head's generation alone, without a derived value.
#[test]
fn resolve_tilde_walks_with_first_parent() {
    let (_dir, root, repo) = unsynced_repo();
    let created = checkpoint(&root);
    let first = chain_on(&repo, None, 10, "c");
    let side = commit_on(&repo, &[first[0]], "side");
    let merge = commit_on(&repo, &[first[9], side], "merge");
    let mut c: Vec<ChangesetId> = first;
    c.push(merge);
    c.extend(chain_on(&repo, Some(merge), 9, "after the merge"));
    repo.cas_ref("main", None, c.last().copied(), "test/agent")
        .unwrap();
    assert_eq!(checkpoint(&root), created, "no checkpoint yet");
    walks_main(&repo.snapshot().unwrap(), &c);

    let head = *c.last().unwrap();
    c.extend(chain_on(&repo, Some(head), 20, "later"));
    repo.cas_ref("main", Some(head), c.last().copied(), "test/agent")
        .unwrap();
    assert_ne!(checkpoint(&root), created, "the checkpoint moved");
    walks_main(&repo.snapshot().unwrap(), &c);

    assert!(skip_lists(&root).is_empty(), "no walk derived a skip list");
}

/// What [`resolve_tilde_walks_with_first_parent`] checks in `read`, whose `main` names the head
/// of first-parent chain `c` (the root first).
fn walks_main(read: &RepoRead<'_, swarmops_store::FsObjectStore>, c: &[ChangesetId]) {
    let head = *c.last().unwrap();
    let walked: Vec<ChangesetId> = read.first_parents(head).collect::<Result<_, _>>().unwrap();
    let back: Vec<ChangesetId> = c.iter().rev().copied().collect();
    assert_eq!(walked, back, "first_parents");
    for (k, &want) in (0_u64..).zip(&back) {
        assert_eq!(
            read.resolve(&format!("main~{k}")).unwrap(),
            want,
            "main~{k}"
        );
        assert_eq!(read.ancestor(head, k).unwrap(), Some(want), "ancestor {k}");
    }
    let past = c.len();
    assert_not_found(
        read.resolve(&format!("main~{past}")),
        &format!("main~{past}"),
    );
    assert_eq!(
        read.ancestor(head, u64::try_from(past).unwrap()).unwrap(),
        None
    );
    assert_eq!(
        read.resolve("main~3~2").unwrap(),
        back[5],
        "suffixes add up"
    );
    let far = format!("main~{}", u64::MAX);
    assert_not_found(read.resolve(&far), &far);
    assert_eq!(read.ancestor(head, u64::MAX).unwrap(), None);
}

/// Every walk names changesets the snapshot holds: an id it does not hold, or one committed since,
/// is `NotFound` for each of them, whichever side it is on, and a walk from one ends at once.
#[test]
fn ancestry_of_a_changeset_the_snapshot_lacks_is_not_found() {
    let (_dir, _root, repo) = unsynced_repo();
    let c = chain_on(&repo, None, 3, "c");
    let read = repo.snapshot().unwrap();
    let later = commit_on(&repo, &[c[2]], "later");
    for missing in [unknown_id(), later] {
        let what = missing.to_string();
        let mut walk = read.first_parents(missing);
        assert_not_found(walk.next().expect("an error"), &what);
        assert!(walk.next().is_none(), "the walk ended");
        assert_not_found(read.ancestor(missing, 0), &what);
        assert_not_found(read.ancestor(missing, 1), &what);
        assert_not_found(read.is_ancestor(missing, c[2]), &what);
        assert_not_found(read.is_ancestor(c[0], missing), &what);
        assert_not_found(read.merge_base(missing, c[2]), &what);
        assert_not_found(read.merge_base(c[2], missing), &what);
    }
    // A changeset is its own ancestor, and its own merge base.
    assert!(read.is_ancestor(c[1], c[1]).unwrap());
    assert_eq!(read.merge_base(c[1], c[1]).unwrap(), Some(c[1]));
    assert!(
        !read.is_ancestor(c[2], c[1]).unwrap(),
        "a descendant is no ancestor"
    );
}

/// The merge base of two first-parent chains is the changeset they forked at, whichever comes
/// first; of a changeset and one of its ancestors, the ancestor. A merge whose second parent is
/// the other chain's head does not make that head its ancestor: history is first-parent (M2
/// Decision 8), so the base is still the fork point. Chains from two roots share nothing.
#[test]
fn merge_base_of_two_chains_is_their_fork_point() {
    let (_dir, _root, repo) = unsynced_repo();
    let main = chain_on(&repo, None, 6, "main");
    let side = chain_on(&repo, Some(main[2]), 3, "side");
    let read = repo.snapshot().unwrap();
    let (fork, head, tip) = (main[2], main[5], side[2]);
    assert_eq!(read.merge_base(head, tip).unwrap(), Some(fork));
    assert_eq!(read.merge_base(tip, head).unwrap(), Some(fork));
    assert_eq!(read.merge_base(side[0], main[3]).unwrap(), Some(fork));
    assert_eq!(read.merge_base(head, main[3]).unwrap(), Some(main[3]));
    assert_eq!(read.merge_base(main[0], tip).unwrap(), Some(main[0]));
    assert!(read.is_ancestor(fork, tip).unwrap());
    assert!(!read.is_ancestor(main[3], tip).unwrap());

    let merged = commit_on(&repo, &[head, tip], "merge");
    let read = repo.snapshot().unwrap();
    assert_eq!(read.merge_base(merged, tip).unwrap(), Some(fork));
    assert!(read.is_ancestor(head, merged).unwrap());
    assert!(!read.is_ancestor(tip, merged).unwrap(), "a second parent");
    assert_eq!(read.ancestor(merged, 1).unwrap(), Some(head));

    let other = chain_on(&repo, None, 3, "another root");
    let read = repo.snapshot().unwrap();
    assert_eq!(read.merge_base(merged, other[2]).unwrap(), None);
    assert_eq!(read.merge_base(other[0], main[0]).unwrap(), None);
    assert!(!read.is_ancestor(main[0], other[2]).unwrap());
}

/// A xorshift64* generator, so that the random histories are the same in every run.
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

/// How many random histories [`ancestor_and_is_ancestor_agree_with_a_walk`] commits:
/// `PROPTEST_CASES` (`just check-slow` sets 2000), or 32.
fn cases() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(32)
}

/// A random history of `n` changesets, each as the indices of its parents: none for a root (the
/// first, and one in eight of the others); else a first parent, most often the changeset just
/// before so that first-parent chains run long, and one time in three a second parent, which
/// often raises the generation by more than one step of the first-parent chain.
fn random_history(rng: &mut Rng, n: usize) -> Vec<Vec<usize>> {
    let mut history: Vec<Vec<usize>> = Vec::with_capacity(n);
    for i in 0..n {
        if i == 0 || rng.below(8) == 0 {
            history.push(Vec::new());
            continue;
        }
        let first = if rng.below(4) == 0 {
            rng.below(i)
        } else {
            i - 1
        };
        let mut parents = vec![first];
        let second = rng.below(i);
        if rng.below(3) == 0 && second != first {
            parents.push(second);
        }
        history.push(parents);
    }
    history
}

/// The first-parent chain of changeset `i` of `history`: `i`, its first parent, and so on to a
/// root.
fn chain_of(history: &[Vec<usize>], i: usize) -> Vec<usize> {
    let mut chain = vec![i];
    while let Some(&parent) = history[*chain.last().unwrap()].first() {
        chain.push(parent);
    }
    chain
}

/// The walks [`agree`] checks: `RepoRead`'s own, or the same with another threshold than
/// `SKIP_AFTER` (the `test-hooks` build's).
struct Walks<'w> {
    ancestor: &'w dyn Fn(ChangesetId, u64) -> Result<Option<ChangesetId>, RepoError>,
    is_ancestor: &'w dyn Fn(ChangesetId, ChangesetId) -> Result<bool, RepoError>,
    merge_base: &'w dyn Fn(ChangesetId, ChangesetId) -> Result<Option<ChangesetId>, RepoError>,
}

/// Checks `walks` over `history`, committed as `ids`, against a walk of its first-parent chains:
/// the ancestor of every changeset at every distance up to one past its root (and at `u64::MAX`),
/// and for every pair whether the first is an ancestor of the second and their merge base.
fn agree(history: &[Vec<usize>], ids: &[ChangesetId], at: &str, walks: &Walks<'_>) {
    let chains: Vec<Vec<usize>> = (0..history.len()).map(|i| chain_of(history, i)).collect();
    for (i, chain) in chains.iter().enumerate() {
        for k in 0..=chain.len() {
            let want = chain.get(k).map(|&j| ids[j]);
            let got = (walks.ancestor)(ids[i], u64::try_from(k).unwrap()).unwrap();
            assert_eq!(got, want, "{at}: ancestor({i}, {k}) in {history:?}");
        }
        let got = (walks.ancestor)(ids[i], u64::MAX).unwrap();
        assert_eq!(got, None, "{at}: ancestor({i}, MAX)");
        for (j, other) in chains.iter().enumerate() {
            let got = (walks.is_ancestor)(ids[i], ids[j]).unwrap();
            assert_eq!(
                got,
                other.contains(&i),
                "{at}: is_ancestor({i}, {j}) in {history:?}"
            );
            let base = chain.iter().find(|&k| other.contains(k)).map(|&k| ids[k]);
            let got = (walks.merge_base)(ids[i], ids[j]).unwrap();
            assert_eq!(got, base, "{at}: merge_base({i}, {j}) in {history:?}");
        }
    }
}

/// Against a walk of the first-parent chains of random histories (up to 16 changesets, roots among
/// them, a second parent one time in three): `first_parents` gives each chain, and `ancestor`,
/// `is_ancestor` and `merge_base` agree with it for every changeset, distance and pair. The
/// histories come from seeded generators, the same in every run: this crate has no `proptest`
/// dev-dependency, and M2 keeps `Cargo.lock` to its Task 2. A `test-hooks` build walks each again
/// with thresholds of 0, 1 and 3 generations in place of `SKIP_AFTER`, so that the lifts along the
/// skip list meet second parents, roots and chains shorter than a jump.
#[test]
fn ancestor_and_is_ancestor_agree_with_a_walk() {
    let (_dir, _root, repo) = unsynced_repo();
    for seed in 0..cases() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::try_from(seed).unwrap());
        let n = 2 + rng.below(15);
        let history = random_history(&mut rng, n);
        let mut ids: Vec<ChangesetId> = Vec::with_capacity(n);
        for (i, parents) in history.iter().enumerate() {
            let parents: Vec<ChangesetId> = parents.iter().map(|&p| ids[p]).collect();
            ids.push(commit_on(
                &repo,
                &parents,
                &format!("seed {seed} changeset {i}"),
            ));
        }
        let read = repo.snapshot().unwrap();
        for (i, &id) in ids.iter().enumerate() {
            let walked: Vec<ChangesetId> =
                read.first_parents(id).collect::<Result<_, _>>().unwrap();
            let want: Vec<ChangesetId> = chain_of(&history, i).iter().map(|&j| ids[j]).collect();
            assert_eq!(
                walked, want,
                "seed {seed}: first_parents({i}) in {history:?}"
            );
        }
        let walks = Walks {
            ancestor: &|cs, n| read.ancestor(cs, n),
            is_ancestor: &|a, of| read.is_ancestor(a, of),
            merge_base: &|a, b| read.merge_base(a, b),
        };
        agree(&history, &ids, &format!("seed {seed}, SKIP_AFTER"), &walks);
        #[cfg(feature = "test-hooks")]
        for skip_after in [0, 1, 3] {
            let walks = Walks {
                ancestor: &|cs, n| read.ancestor_with_skip_after(cs, n, skip_after),
                is_ancestor: &|a, of| read.is_ancestor_with_skip_after(a, of, skip_after),
                merge_base: &|a, b| read.merge_base_with_skip_after(a, b, skip_after),
            };
            agree(
                &history,
                &ids,
                &format!("seed {seed}, skip_after {skip_after}"),
                &walks,
            );
        }
    }
}

/// How many changesets a batch of [`bulk_chain`] puts and records at once.
const BATCH: usize = 8_192;

/// A first-parent chain of `n` changesets in a fresh store at `root`, `main` at its head, built
/// straight through the stores without a sync (`NoSyncFs`), as the derivation tests build theirs:
/// every changeset has the same root tree, which holds a file for each of `files` (none: the empty
/// root), its index as its message, and no delta; the objects go in batches of [`BATCH`], then
/// their records in one `MetaWrite` transaction per batch, the last also creating `main`. Their
/// ids, the root first.
fn bulk_chain(root: &Utf8Path, n: usize, files: &[(&str, &[u8])]) -> Vec<ChangesetId> {
    LocalRepo::create_with_fs(root, "test", Arc::new(NoSyncFs)).expect("create");
    let fs: Arc<dyn FsOps> = Arc::new(NoSyncFs);
    let objects = FsObjectStore::open_shared(Layout::new(root), Arc::clone(&fs));
    let mut entries = Vec::new();
    for &(name, content) in files {
        objects.put(ObjectKind::Blob, content).unwrap();
        let blob = BlobId(hash_object(ObjectKind::Blob, content));
        entries.push(TreeEntry::leaf(name, LeafKind::File, blob));
    }
    let tree = if entries.is_empty() {
        Tree::empty_root()
    } else {
        Tree::new(entries).unwrap()
    };
    objects.put(ObjectKind::Tree, &tree.encode()).unwrap();
    let meta = MetaStore::open_with_fs(root, fs).unwrap();
    let mut ids: Vec<ChangesetId> = Vec::with_capacity(n);
    for start in (0..n).step_by(BATCH) {
        let end = n.min(start + BATCH);
        let mut items = Vec::new();
        let mut records = Vec::new();
        for i in start..end {
            let changeset = Changeset {
                parents: ids.last().copied().into_iter().collect(),
                root: tree.id(),
                delta: Vec::new(),
                author: signature(),
                committer: signature(),
                message: i.to_string().into_bytes(),
                extra_headers: Vec::new(),
                metadata: Vec::new(),
            };
            let id = changeset.id().unwrap();
            records.push(ChangesetMeta {
                id,
                seq: 0,
                generation: 0,
                delta_hash: changeset.delta_hash().unwrap(),
                parents: changeset.parents.clone(),
            });
            items.push((ObjectKind::Changeset, changeset.encode().unwrap()));
            ids.push(id);
        }
        objects.put_batch(items).unwrap();
        objects.flush().unwrap();
        let mut write = meta.write().unwrap();
        for record in records {
            write.insert_changeset(record).unwrap();
        }
        if end == n {
            write
                .cas_ref("main", None, ids.last().copied(), "test/agent")
                .unwrap();
        }
        write.commit().unwrap();
    }
    ids
}

/// The calls of `calls` that read a skip list, and whether every call it made is one: a walk that
/// derives nothing makes no other filesystem call (the index is read through its own handles).
fn skip_list_reads(calls: &[String]) -> usize {
    let prefix = format!(
        "read meta/derived/{}/{}/",
        FirstParentSkipList::NAME,
        FirstParentSkipList::VERSION
    );
    assert!(
        calls.iter().all(|call| call.starts_with(&prefix)),
        "only skip-list reads: {calls:?}"
    );
    calls.len()
}

/// On a chain of 3,000 changesets, a gap of `SKIP_AFTER` generations is walked by narrow lookups
/// and touches no derived value; `is_ancestor(root, head)`, across 2,999, lifts along the skip
/// list: on a cold store it derives the list of every changeset of the chain (one file each),
/// and answers `true`; asked again in a new snapshot, it derives nothing and reads at most two
/// lists for each of its ⌊log2 2,999⌋ + 1 = 12 jumps at most (the list of where it lands, and the
/// list one level down that vouches for the jump), as do `ancestor(head, 2,999)` and
/// `merge_base(head, root)`.
#[test]
fn a_long_gap_lifts_along_the_skip_list() {
    const N: usize = 3_000;
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let c = bulk_chain(&root, N, &[]);
    let (head, gap) = (c[N - 1], u64::try_from(N - 1).unwrap());
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(root.clone().into_std_path_buf());
    let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();

    let read = repo.snapshot().unwrap();
    calls();
    let short = N - 1 - usize::try_from(SKIP_AFTER).unwrap();
    assert!(read.is_ancestor(c[short], head).unwrap());
    assert!(!read.is_ancestor(c[short + 1], c[short]).unwrap());
    assert_eq!(read.ancestor(head, SKIP_AFTER).unwrap(), Some(c[short]));
    assert_eq!(read.merge_base(head, c[short]).unwrap(), Some(c[short]));
    assert_eq!(calls(), Vec::<String>::new(), "narrow lookups only");
    assert!(skip_lists(&root).is_empty());

    assert!(read.is_ancestor(c[0], head).unwrap());
    let derived = calls();
    let placed = derived
        .iter()
        .filter(|call| call.starts_with("link_noclobber meta/derived/first_parent_skip_list/1/"))
        .count();
    assert_eq!(placed, N, "every list of the chain, once");
    assert_eq!(skip_lists(&root).len(), N);

    let read = repo.snapshot().unwrap();
    calls();
    assert!(read.is_ancestor(c[0], head).unwrap());
    let reads = skip_list_reads(&calls());
    assert!((1..=2 * 12).contains(&reads), "{reads} lists read");
    assert!(!read.is_ancestor(c[1], c[0]).unwrap());
    assert!(read.is_ancestor(c[1], head).unwrap());
    assert!(skip_list_reads(&calls()) <= 2 * 2 * 12);
    assert_eq!(read.ancestor(head, gap).unwrap(), Some(c[0]));
    assert_eq!(read.ancestor(head, gap + 1).unwrap(), None);
    assert_eq!(read.ancestor(head, gap - 1).unwrap(), Some(c[1]));
    assert_eq!(read.ancestor(head, 2_000).unwrap(), Some(c[N - 1 - 2_000]));
    assert!(skip_list_reads(&calls()) <= 4 * 2 * 12);
    assert_eq!(read.merge_base(head, c[0]).unwrap(), Some(c[0]));
    assert_eq!(read.merge_base(c[1_500], head).unwrap(), Some(c[1_500]));
    assert!(skip_list_reads(&calls()) <= 2 * 2 * 12);
    assert_eq!(read.resolve("main~2999").unwrap(), c[0]);
    assert_eq!(read.resolve("main~1999~1000").unwrap(), c[0]);
    assert!(skip_list_reads(&calls()) <= 2 * 2 * 12);
    assert_eq!(skip_lists(&root).len(), N, "nothing derived since");
}

/// Umbrella spec §15's read targets on a chain of 200,000 changesets, measured on `RepoRead`: a
/// `log -n 100` (`log_first_parent(head, 100)`) at a p50 under 10 ms, and a warm `cat` of a 4 KiB
/// file (`read_blob_at`) under 1 ms; `resolve("main~1000")`, a walk of 1,000 narrow lookups, is
/// printed beside them. The chain is built without a sync in the measurements' directory
/// (`measure::base_dir`, on the native filesystem); each figure is the median of 101 runs after
/// one that warms it.
#[test]
#[ignore = "measurement, ~2 min: a 200k chain"]
fn reads_at_200k_meet_the_targets() {
    const N: usize = 200_000;
    let dir = tempfile::tempdir_in(measure::base_dir()).expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let content = vec![b'x'; 4096];
    let c = bulk_chain(&root, N, &[("f", &content)]);
    let head = c[N - 1];
    let repo = LocalRepo::open(&root).unwrap();
    let read = repo.snapshot().unwrap();
    let log = p50(|| assert_eq!(read.log_first_parent(head, 100).unwrap().len(), 100));
    let cat = p50(|| {
        let (_, _, bytes) = read.read_blob_at(head, &path("f")).unwrap().unwrap();
        assert_eq!(bytes.len(), 4096);
    });
    let tilde = p50(|| assert_eq!(read.resolve("main~1000").unwrap(), c[N - 1 - 1_000]));
    eprintln!(
        "a {N}-changeset chain: log -n 100 p50 {log:?}, a warm cat of 4 KiB p50 {cat:?}, \
         resolve main~1000 p50 {tilde:?}"
    );
    assert!(log < Duration::from_millis(10), "log -n 100: p50 {log:?}");
    assert!(cat < Duration::from_millis(1), "cat of 4 KiB: p50 {cat:?}");
}

/// The median time of 101 runs of `f`, after one run that warms it.
fn p50(mut f: impl FnMut()) -> Duration {
    f();
    let mut times: Vec<Duration> = (0..101)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed()
        })
        .collect();
    times.sort_unstable();
    measure::percentile(&times, 50)
}

/// Skip lists placed in a store (whole, their trailers right), and the walks that read them,
/// lifting past 0 generations (the `test-hooks` threshold): see
/// [`a_crafted_skip_list_is_refused_not_walked`].
#[cfg(feature = "test-hooks")]
mod crafted_skip_lists {
    use swarmops_repo::{RepoError, VerifyProblem};
    use swarmops_store::{DerivedSlot, Layout};

    use super::*;

    /// A chain `c` of 41 changesets and a side branch `s` of 3 off `c[3]`, every list derived.
    struct Lists {
        _dir: TempDir,
        root: Utf8PathBuf,
        repo: LocalRepo,
        c: Vec<ChangesetId>,
        s: Vec<ChangesetId>,
    }

    impl Lists {
        fn derived() -> Lists {
            let (dir, root, repo) = unsynced_repo();
            let c = chain_on(&repo, None, 41, "c");
            let s = chain_on(&repo, Some(c[3]), 3, "s");
            for &cs in c.iter().chain(&s) {
                repo.derived::<FirstParentSkipList>(cs, &()).unwrap();
            }
            Lists {
                _dir: dir,
                root,
                repo,
                c,
                s,
            }
        }

        /// The file of `cs`'s list.
        fn file(&self, cs: ChangesetId) -> Utf8PathBuf {
            let slot = DerivedSlot {
                name: FirstParentSkipList::NAME,
                version: FirstParentSkipList::VERSION,
                subject: cs.0,
                key: &[],
            };
            Layout::new(&self.root).derived_path(&slot)
        }

        /// Places a list of `ids` for `cs` in place of its file, as a put writes it; the bytes
        /// of the file it replaced.
        fn place(&self, cs: ChangesetId, ids: &[ChangesetId]) -> Vec<u8> {
            let mut value = vec![u8::try_from(ids.len()).unwrap()];
            for id in ids {
                value.extend(id.0.0);
            }
            let file = self.file(cs);
            let kept = fs::read(&file).unwrap();
            fs::remove_file(&file).unwrap();
            let slot = DerivedSlot {
                name: FirstParentSkipList::NAME,
                version: FirstParentSkipList::VERSION,
                subject: cs.0,
                key: &[],
            };
            assert!(self.repo.derived_store().put(&slot, &value).unwrap());
            kept
        }

        /// The true list of `c[i]`: `c[i - 1]`, `c[i - 2]`, `c[i - 4]`, … while the chain lasts.
        fn list_of(&self, i: usize) -> Vec<ChangesetId> {
            let ups = (0..).map(|level| 1 << level).take_while(|&up| up <= i);
            ups.map(|up| self.c[i - up]).collect()
        }

        /// `c[40]`'s true list with entry 3 (8 steps back) `id`.
        fn entry_3(&self, id: ChangesetId) -> Vec<ChangesetId> {
            let list = self.list_of(40);
            [&list[..3], &[id], &list[4..]].concat()
        }
    }

    /// Asserts that `got` is `DerivedCorrupt` for `cs`'s skip list.
    #[track_caller]
    fn refused(got: Result<(), RepoError>, cs: ChangesetId, what: &str) {
        match got {
            Err(RepoError::DerivedCorrupt {
                name, changeset, ..
            }) => {
                let found = (name.as_str(), changeset);
                assert_eq!(found, ("first_parent_skip_list", cs), "{what}");
            }
            other => panic!("{what}: expected DerivedCorrupt, got {other:?}"),
        }
    }

    /// A list placed for `c[40]` (or `c[2]`) that a walk's checks find wrong is `DerivedCorrupt`,
    /// naming the derivation and the changeset whose list it is, never a walk off the chain:
    /// entry 0 not the first parent; more entries than the generation leaves room for; an entry
    /// the store does not hold; a jump of 8 steps to a changeset 6 generations down, which entry
    /// 2 of the list of its (wrong) entry 2 agrees with; a jump to a side-branch changeset low
    /// enough for it, which entry 2 of the list of entry 2 contradicts; and a list cut short,
    /// which a jump past its end, and a lift (of `is_ancestor` and of `merge_base`) that stops
    /// short, find out. With the true list back, the walks agree with the chain.
    #[test]
    fn a_crafted_skip_list_is_refused_not_walked() {
        let lists = Lists::derived();
        let (c, s) = (&lists.c, &lists.s);
        let unknown = unknown_id();
        let cases: [(ChangesetId, Vec<ChangesetId>, u64, &str); 6] = [
            (
                c[40],
                [&[c[38]][..], &lists.list_of(40)[1..]].concat(),
                8,
                "entry 0 not the parent",
            ),
            (c[2], vec![c[1], c[0], c[0]], 1, "too many for generation 3"),
            (c[40], lists.entry_3(unknown), 8, "no such entry"),
            (
                c[40],
                vec![c[39], c[38], c[38], c[34], c[24], c[8]],
                8,
                "8 steps, 6 generations down, as the list of its entry 2 says",
            ),
            (c[40], lists.entry_3(s[1]), 8, "on a side branch"),
            (c[40], lists.list_of(40)[..3].to_vec(), 8, "cut short"),
        ];
        for (cs, list, n, what) in cases {
            let kept = lists.place(cs, &list);
            let read = lists.repo.snapshot().unwrap();
            refused(read.ancestor_with_skip_after(cs, n, 0).map(drop), cs, what);
            if what == "cut short" {
                let lift = read.is_ancestor_with_skip_after(c[0], cs, 0).map(drop);
                refused(lift, cs, "a lift that stops short");
                let base = read.merge_base_with_skip_after(cs, c[0], 0).map(drop);
                refused(base, cs, "a merge base's lift that stops short");
            }
            fs::write(lists.file(cs), kept).unwrap();
        }
        let read = lists.repo.snapshot().unwrap();
        assert_eq!(
            read.ancestor_with_skip_after(c[40], 8, 0).unwrap(),
            Some(c[32])
        );
        assert!(read.is_ancestor_with_skip_after(c[0], c[40], 0).unwrap());
        let base = read.merge_base_with_skip_after(c[40], s[2], 0).unwrap();
        assert_eq!(base, Some(c[3]));
    }

    /// Two lists made to agree with each other, `c[40]`'s entry 3 on the side branch and entry 2
    /// of the list of its entry 2, which vouches for it, pass a walk's checks, and the walk names
    /// the side branch's changeset: what the checks one level down cannot tell. A deep verify
    /// reports both lists; without them, the walk is right again.
    #[test]
    fn skip_lists_made_to_agree_pass_a_walk_and_fail_verify() {
        let lists = Lists::derived();
        let (c, s) = (&lists.c, &lists.s);
        let head_list = lists.place(c[40], &lists.entry_3(s[1]));
        let thirty_six = lists.list_of(36);
        lists.place(
            c[36],
            &[&thirty_six[..2], &[s[1]], &thirty_six[3..]].concat(),
        );
        let read = lists.repo.snapshot().unwrap();
        assert_eq!(
            read.ancestor_with_skip_after(c[40], 8, 0).unwrap(),
            Some(s[1])
        );
        let mut agreeing = [c[36], c[40]];
        agreeing.sort_unstable();
        let want = agreeing.map(|cs| VerifyProblem::DerivedMismatch {
            name: "first_parent_skip_list".to_owned(),
            changeset: cs,
            key: Vec::new(),
        });
        assert_eq!(lists.repo.verify(true).unwrap().problems, want);

        fs::write(lists.file(c[40]), head_list).unwrap();
        fs::remove_file(lists.file(c[36])).unwrap();
        let read = lists.repo.snapshot().unwrap();
        assert_eq!(
            read.ancestor_with_skip_after(c[40], 8, 0).unwrap(),
            Some(c[32])
        );
        assert_eq!(lists.repo.verify(true).unwrap().problems, Vec::new());
    }
}
