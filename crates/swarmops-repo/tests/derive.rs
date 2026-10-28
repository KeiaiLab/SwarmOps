//! The derivation engine (storage spec §5): values derived from immutable objects when they are
//! asked for, through an explicit stack, and cached in the derived store (`meta/derived/`), where
//! later calls read them back. A cached value ends a walk; a dependency or a value that `derive`
//! needs and the store lacks is derived first; a `canonical` redirect is followed without storing,
//! except at its memos; a cycle is an error.
//!
//! Every test owns a fresh store (`new_repo()`), so tests are independent and may run in any order
//! or in parallel. The derivations are the tests' own: a first-parent depth that counts its
//! derives, and a few that redirect, read each other's values or form cycles.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{Changeset, ChangesetId, ObjectKind, Tree};
use swarmops_repo::{
    Derivation, DeriveCtx, DeriveError, LocalRepo, Need, Registry, RepoError, StoreError, Subject,
};
use swarmops_store::{
    ChangesetMeta, FsMethod, FsObjectStore, FsOps, Layout, MetaStore, ObjectStore,
};
use swarmops_testkit::fs::{FailingFs, NoSyncFs};
use swarmops_testkit::repo::{new_repo, put, request, signature};

/// How many times a test derivation's `derive` ran: shared between the derivation, which a
/// repository holds once it is registered, and the test.
#[derive(Debug, Clone, Default)]
struct Count(Arc<AtomicU64>);

impl Count {
    fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A changeset's first-parent depth: 0 for a root, its first parent's plus one otherwise. Its one
/// dependency is its first parent (the default would be every parent).
struct Depth(Count);

impl Derivation for Depth {
    const NAME: &'static str = "depth";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn deps(&self, subject: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        subject
            .changeset
            .parents
            .iter()
            .take(1)
            .map(|&p| (p, ()))
            .collect()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        self.0.bump();
        match subject.changeset.parents.first() {
            Some(&parent) => Ok(ctx.value::<Depth>(parent, &())? + 1),
            None => Ok(0),
        }
    }
}

/// `depth`'s second version: the changesets on the first-parent chain, the changeset itself
/// included, so [`Depth`] plus one.
struct DepthV2(Count);

impl Derivation for DepthV2 {
    const NAME: &'static str = "depth";
    const VERSION: u32 = 2;
    type Key = ();
    type Value = u64;

    fn deps(&self, subject: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        subject
            .changeset
            .parents
            .iter()
            .take(1)
            .map(|&p| (p, ()))
            .collect()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        self.0.bump();
        match subject.changeset.parents.first() {
            Some(&parent) => Ok(ctx.value::<DepthV2>(parent, &())? + 1),
            None => Ok(1),
        }
    }
}

/// Twice a changeset's [`Depth`], which it reads through `ctx.value` and does not name as a
/// dependency (it has none): its first `derive` meets `Missing`. Counts its calls, and the calls
/// that met `Missing`.
struct Doubled {
    calls: Count,
    missing: Count,
}

impl Derivation for Doubled {
    const NAME: &'static str = "doubled";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn deps(&self, _: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        Vec::new()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        self.calls.bump();
        match ctx.value::<Depth>(subject.id, &()) {
            Ok(depth) => Ok(2 * depth),
            Err(e) => {
                if let DeriveError::Missing(_) = e {
                    self.missing.bump();
                }
                Err(e)
            }
        }
    }
}

/// The changeset's index on a chain built by [`chain`], which is its message.
fn index(changeset: &Changeset) -> u64 {
    let message = std::str::from_utf8(&changeset.message).expect("an index");
    message.parse().expect("an index")
}

/// On a chain built by [`chain`]: `n / 4 + 1` for the changeset at index `n`. The changesets at a
/// multiple of 4 derive it, from their first parent's value (the root: 1); every other one
/// redirects to its first parent (`canonical`), with a memo every 2 steps. Its dependencies are
/// the default, every parent.
struct Quarter(Count);

impl Derivation for Quarter {
    const NAME: &'static str = "quarter";
    const VERSION: u32 = 1;
    const MEMO_EVERY: Option<u32> = Some(2);
    type Key = ();
    type Value = u64;

    fn canonical(
        &self,
        _: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<Option<(ChangesetId, ())>, DeriveError> {
        if index(subject.changeset).is_multiple_of(4) {
            return Ok(None);
        }
        Ok(subject.changeset.parents.first().map(|&p| (p, ())))
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        self.0.bump();
        match subject.changeset.parents.first() {
            Some(&parent) => Ok(ctx.value::<Quarter>(parent, &())? + 1),
            None => Ok(1),
        }
    }
}

/// Redirects every changeset to itself.
struct Selfish;

impl Derivation for Selfish {
    const NAME: &'static str = "selfish";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = ();

    fn canonical(
        &self,
        _: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<Option<(ChangesetId, ())>, DeriveError> {
        Ok(Some((subject.id, ())))
    }

    fn derive(&self, _: &DeriveCtx<'_>, _: Subject<'_>, (): &()) -> Result<(), DeriveError> {
        Ok(())
    }
}

/// Names every changeset as its own dependency.
struct SelfDep;

impl Derivation for SelfDep {
    const NAME: &'static str = "self_dep";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = ();

    fn deps(&self, subject: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        vec![(subject.id, ())]
    }

    fn derive(&self, _: &DeriveCtx<'_>, _: Subject<'_>, (): &()) -> Result<(), DeriveError> {
        Ok(())
    }
}

/// Needs [`Pong`]'s value of the same changeset, which needs this one's.
struct Ping;

/// Needs [`Ping`]'s value of the same changeset, which needs this one's.
struct Pong;

impl Derivation for Ping {
    const NAME: &'static str = "ping";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn deps(&self, _: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        Vec::new()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        ctx.value::<Pong>(subject.id, &())
    }
}

impl Derivation for Pong {
    const NAME: &'static str = "pong";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn deps(&self, _: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        Vec::new()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        ctx.value::<Ping>(subject.id, &())
    }
}

/// A first-parent chain of `n` changesets committed on `repo`, each putting file `f`; changeset
/// `i`'s message is `i`. Their ids, the root first.
fn chain(repo: &LocalRepo, n: u64) -> Vec<ChangesetId> {
    let mut ids: Vec<ChangesetId> = Vec::new();
    for i in 0..n {
        let parents: Vec<ChangesetId> = ids.last().copied().into_iter().collect();
        let content = i.to_string();
        let req = request(&parents, vec![put("f", content.as_bytes())], &content);
        ids.push(repo.commit(req).expect("commit").id);
    }
    ids
}

/// The values of derivation `name` at `version` that the store holds, by file: the subjects'
/// ids, from the files under `meta/derived/<name>/<version>/` (`<2 hex>/<62 hex>`), each followed
/// by its file's key suffix (`.<16 hex>`) where the key is not `()`.
fn stored(root: &Utf8Path, name: &str, version: u32) -> BTreeSet<String> {
    let dir = root.join(format!("meta/derived/{name}/{version}"));
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

/// The ids of `chain`'s changesets at `indices`, as [`stored`] lists them.
fn at(chain: &[ChangesetId], indices: &[usize]) -> BTreeSet<String> {
    indices.iter().map(|&i| chain[i].to_string()).collect()
}

/// The file of derivation `name`'s value of `cs` at `version`, under key `()`.
fn value_file(root: &Utf8Path, name: &str, version: u32, cs: ChangesetId) -> Utf8PathBuf {
    let hex = cs.to_string();
    root.join(format!(
        "meta/derived/{name}/{version}/{}/{}",
        &hex[..2],
        &hex[2..]
    ))
}

/// The entries of `meta/derived/tmp/`, where puts write their temp files; none if it does not
/// exist.
fn temp_files(root: &Utf8Path) -> usize {
    match fs::read_dir(root.join("meta/derived/tmp")) {
        Ok(entries) => entries.count(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => panic!("read meta/derived/tmp: {e}"),
    }
}

/// What `f` returns, run on a thread of its own and given a minute: a hang fails the test rather
/// than stall the run.
fn within_a_minute<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, wait) = mpsc::channel();
    thread::spawn(move || {
        // The test has failed already if it stopped waiting.
        let _ = done.send(f());
    });
    wait.recv_timeout(Duration::from_secs(60))
        .expect("an answer within a minute, not a hang")
}

/// A value is derived once, its dependencies first, and read back from the store from then on: a
/// first-parent depth over a chain of 5 derives the 5 values on its first call, none on the next
/// ones, and leaves one file per changeset in `meta/derived/depth/1/`.
#[test]
fn a_value_is_derived_once_and_read_back() {
    let (_dir, root, mut repo) = new_repo();
    let chain = chain(&repo, 5);
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();

    assert_eq!(repo.derived::<Depth>(chain[4], &()).unwrap(), 4);
    assert_eq!(derives.get(), 5, "each value once, the dependencies first");
    assert_eq!(repo.derived::<Depth>(chain[4], &()).unwrap(), 4);
    for (depth, &cs) in (0_u64..).zip(&chain) {
        assert_eq!(repo.derived::<Depth>(cs, &()).unwrap(), depth);
    }
    assert_eq!(derives.get(), 5, "read back, not derived again");
    assert_eq!(stored(&root, "depth", 1), at(&chain, &[0, 1, 2, 3, 4]));
    assert_eq!(temp_files(&root), 0);
}

/// A chain of 10,000 changesets (built straight through the stores: its changeset objects in
/// bulk, its records through `MetaWrite`, without a sync) is derived on a thread with a 256 KiB
/// stack: a recursive walk would hold a stack frame per level, several MiB at this depth, and
/// overflow it; the engine's stack is on the heap.
#[test]
fn a_long_chain_derives_without_recursion() {
    derives_without_recursion(10_000);
}

/// [`a_long_chain_derives_without_recursion`] over 100,000 changesets.
#[test]
#[ignore = "slow, ~20 s: a 100k-changeset chain"]
fn a_long_chain_derives_without_recursion_at_100k() {
    derives_without_recursion(100_000);
}

/// The first-parent depth of the head of a chain of `n` changesets, derived on a thread with a
/// 256 KiB stack: `n - 1`, after `n` derives, one per changeset.
fn derives_without_recursion(n: u64) {
    let (_dir, root, mut repo) = new_repo();
    let head = bulk_chain(&root, n);
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();
    let depth = thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || repo.derived::<Depth>(head, &()))
        .expect("spawn")
        .join()
        .expect("no panic, no overflow");
    assert_eq!(depth.unwrap(), n - 1);
    assert_eq!(derives.get(), n);
}

/// A first-parent chain of `n` changesets in the store at `root`, each with the empty tree and its
/// index as its message, built in bulk through a filesystem that never syncs (`NoSyncFs`): the
/// objects of up to 8,192 changesets in one batch, then their records in one `MetaWrite`
/// transaction. Its head.
fn bulk_chain(root: &Utf8Path, n: u64) -> ChangesetId {
    const BATCH: u64 = 8_192;
    let fs: Arc<dyn FsOps> = Arc::new(NoSyncFs);
    let objects = FsObjectStore::open_shared(Layout::new(root), Arc::clone(&fs));
    let tree = Tree::empty_root();
    objects.put(ObjectKind::Tree, &tree.encode()).unwrap();
    let meta = MetaStore::open_with_fs(root, fs).unwrap();
    let mut head: Option<ChangesetId> = None;
    for start in (0..n).step_by(usize::try_from(BATCH).unwrap()) {
        let mut items = Vec::new();
        let mut records = Vec::new();
        for i in start..n.min(start + BATCH) {
            let changeset = Changeset::new(
                head.into_iter().collect(),
                tree.id(),
                Vec::new(),
                signature(),
                signature(),
                i.to_string().into_bytes(),
                Vec::new(),
                Vec::new(),
            )
            .unwrap();
            let id = changeset.id().unwrap();
            records.push(ChangesetMeta::new(
                id,
                0,
                0,
                changeset.delta_hash().unwrap(),
                changeset.parents.clone(),
            ));
            items.push((ObjectKind::Changeset, changeset.encode().unwrap()));
            head = Some(id);
        }
        objects.put_batch(items).unwrap();
        objects.flush().unwrap();
        let mut write = meta.write().unwrap();
        for record in records {
            write.insert_changeset(record).unwrap();
        }
        write.commit().unwrap();
    }
    head.expect("a chain of at least one changeset")
}

/// A derivation that reads another's value through `ctx.value`, without naming it as a
/// dependency, meets `Missing` while the store lacks it; the engine derives that value (and its
/// own dependencies) first, then calls `derive` again.
#[test]
fn a_value_another_derivation_needs_is_derived_first() {
    let (_dir, root, mut repo) = new_repo();
    let chain = chain(&repo, 5);
    let depths = Count::default();
    let (calls, missing) = (Count::default(), Count::default());
    repo.register(Depth(depths.clone())).unwrap();
    let doubled = Doubled {
        calls: calls.clone(),
        missing: missing.clone(),
    };
    repo.register(doubled).unwrap();

    assert_eq!(repo.derived::<Doubled>(chain[4], &()).unwrap(), 8);
    assert_eq!(depths.get(), 5, "the depth of the chain, derived first");
    assert_eq!(
        (calls.get(), missing.get()),
        (2, 1),
        "derive met Missing once, then had the depth"
    );
    assert_eq!(stored(&root, "depth", 1), at(&chain, &[0, 1, 2, 3, 4]));
    assert_eq!(stored(&root, "doubled", 1), at(&chain, &[4]));

    // A value the store holds is no Missing: one call, and the depth is read, not derived.
    assert_eq!(repo.derived::<Doubled>(chain[3], &()).unwrap(), 6);
    assert_eq!((calls.get(), missing.get(), depths.get()), (3, 1, 5));
}

/// A cycle is an error, not a hang, whatever forms it: a `canonical` redirect to the node itself,
/// a dependency on itself, or two derivations that each need the other's value of a changeset.
/// The error names the derivation whose node the walk met again, and nothing is stored.
#[test]
fn a_cycle_is_an_error_not_a_hang() {
    let (_dir, root, mut repo) = new_repo();
    let cs = chain(&repo, 2)[1];
    repo.register(Selfish).unwrap();
    repo.register(SelfDep).unwrap();
    repo.register(Ping).unwrap();
    repo.register(Pong).unwrap();

    let (repo, results) = within_a_minute(move || {
        let results = [
            repo.derived::<Selfish>(cs, &()),
            repo.derived::<SelfDep>(cs, &()),
            repo.derived::<Ping>(cs, &()).map(|_| ()),
        ];
        (repo, results)
    });
    for (result, name) in results.into_iter().zip(["selfish", "self_dep", "ping"]) {
        match result {
            Err(RepoError::Derived { name: found, why }) => {
                assert_eq!(found, name, "{why}");
                assert!(why.contains("cycle"), "{why}");
            }
            other => panic!("{name}: expected a cycle error, got {other:?}"),
        }
    }
    for name in ["selfish", "self_dep", "ping", "pong"] {
        assert_eq!(stored(&root, name, 1), BTreeSet::new(), "{name}");
    }
    drop(repo);
}

/// A `canonical` redirect stores nothing, except the memos `MEMO_EVERY` asks for: a chain of
/// redirects longer than its `n` steps stores its end's value at its origin and at every `n`-th
/// node from there, and a chain of `n` steps or fewer stores none. On a 12-chain where only the
/// changesets at a multiple of 4 derive (`Quarter`, `MEMO_EVERY = 2`), the head's value walks
/// three chains of 3 steps: 11 → 8, whose dependency 7 walks 7 → 4, whose dependency 3 walks
/// 3 → 0. Each stores its end (8, 4, 0), as a derived value, and memos at its origin (11, 7, 3)
/// and 2 steps on (9, 5, 1); 10, 6 and 2 store nothing.
#[test]
fn canonical_redirects_store_only_their_memos() {
    let (_dir, root, mut repo) = new_repo();
    let mut chain = chain(&repo, 12);
    let derives = Count::default();
    repo.register(Quarter(derives.clone())).unwrap();

    assert_eq!(repo.derived::<Quarter>(chain[11], &()).unwrap(), 3);
    assert_eq!(derives.get(), 3, "0, 4 and 8, once each");
    let want = at(&chain, &[0, 1, 3, 4, 5, 7, 8, 9, 11]);
    assert_eq!(stored(&root, "quarter", 1), want);
    // Each memo holds its chain's end value: read back, nothing derived.
    for (i, value) in [
        (1, 1),
        (3, 1),
        (5, 2),
        (7, 2),
        (9, 3),
        (11, 3),
        (10, 3),
        (6, 2),
    ] {
        assert_eq!(
            repo.derived::<Quarter>(chain[i], &()).unwrap(),
            value,
            "{i}"
        );
    }
    assert_eq!(derives.get(), 3);
    assert_eq!(
        stored(&root, "quarter", 1),
        want,
        "a 1-step chain stores no memo"
    );

    // A chain of exactly 2 steps (14 → 12) stores its end alone: no memo at 14.
    for i in 12..15_u64 {
        let content = i.to_string();
        let req = request(
            &chain[chain.len() - 1..],
            vec![put("f", content.as_bytes())],
            &content,
        );
        chain.push(repo.commit(req).unwrap().id);
    }
    assert_eq!(repo.derived::<Quarter>(chain[14], &()).unwrap(), 4);
    assert_eq!(derives.get(), 4, "12, on 11's memo");
    let want = at(&chain, &[0, 1, 3, 4, 5, 7, 8, 9, 11, 12]);
    assert_eq!(stored(&root, "quarter", 1), want);
    assert_eq!(repo.derived::<Quarter>(chain[13], &()).unwrap(), 4);
    assert_eq!(derives.get(), 4);
    assert_eq!(stored(&root, "quarter", 1), want);
}

/// A new version of a derivation derives everything again, into a directory of its own, and never
/// reads an older version's values: `depth` at versions 1 and 2, registered in two repositories
/// on one store, each derive the chain once and read their own values back.
#[test]
fn a_new_version_derives_again() {
    let (_dir, root, mut v1) = new_repo();
    let chain = chain(&v1, 3);
    let (derives_v1, derives_v2) = (Count::default(), Count::default());
    v1.register(Depth(derives_v1.clone())).unwrap();
    assert_eq!(v1.derived::<Depth>(chain[2], &()).unwrap(), 2);

    let mut v2 = LocalRepo::open(&root).unwrap();
    v2.register(DepthV2(derives_v2.clone())).unwrap();
    assert_eq!(v2.derived::<DepthV2>(chain[2], &()).unwrap(), 3);
    assert_eq!(
        (derives_v1.get(), derives_v2.get()),
        (3, 3),
        "version 2 derived it all again"
    );
    assert_eq!(stored(&root, "depth", 1), at(&chain, &[0, 1, 2]));
    assert_eq!(stored(&root, "depth", 2), at(&chain, &[0, 1, 2]));

    for (i, &cs) in (0_u64..).zip(&chain) {
        assert_eq!(v1.derived::<Depth>(cs, &()).unwrap(), i);
        assert_eq!(v2.derived::<DepthV2>(cs, &()).unwrap(), i + 1);
    }
    assert_eq!((derives_v1.get(), derives_v2.get()), (3, 3));
}

/// A put that fails is dropped, and the value is returned all the same: with every link into
/// `meta/derived/` failing, the depth of a chain of 5 is right and no value file exists (nor a
/// temp file); the next call derives it all again, as nothing was stored.
#[test]
fn a_failed_put_still_returns_the_value() {
    let (_dir, root, repo) = new_repo();
    let chain = chain(&repo, 5);
    drop(repo);
    let derived_dir = root.join("meta/derived").into_std_path_buf();
    let failing = FailingFs::new(move |method, path: &Path| {
        method == FsMethod::LinkNoclobber && path.starts_with(&derived_dir)
    });
    let mut repo = LocalRepo::open_with_fs(&root, Arc::new(failing)).unwrap();
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();

    assert_eq!(repo.derived::<Depth>(chain[4], &()).unwrap(), 4);
    assert_eq!(derives.get(), 5);
    assert_eq!(stored(&root, "depth", 1), BTreeSet::new());
    assert_eq!(temp_files(&root), 0, "a failed put removes its temp file");
    assert_eq!(repo.derived::<Depth>(chain[4], &()).unwrap(), 4);
    assert_eq!(derives.get(), 10, "nothing was stored, so all of it again");
}

/// A value depends on objects and other values only: once derived it stays, whatever happens to
/// refs and the journal after. Moving `main` and committing on stores new frames, and the value
/// reads back without a derive; the new changeset derives its own value alone.
#[test]
fn derived_values_ignore_refs_and_the_journal() {
    let (_dir, _root, mut repo) = new_repo();
    let chain = chain(&repo, 3);
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();
    assert_eq!(repo.derived::<Depth>(chain[2], &()).unwrap(), 2);
    assert_eq!(derives.get(), 3);

    repo.cas_ref("main", None, Some(chain[2]), "test/agent")
        .unwrap();
    let req = request(&chain[2..], vec![put("g", b"g")], "on top");
    let next = repo.commit(req).unwrap().id;
    repo.cas_ref("main", Some(chain[2]), Some(next), "test/agent")
        .unwrap();
    repo.cas_ref("main", Some(next), Some(chain[0]), "test/agent")
        .unwrap();

    assert_eq!(repo.derived::<Depth>(chain[2], &()).unwrap(), 2);
    assert_eq!(derives.get(), 3, "the same value, read back");
    assert_eq!(repo.derived::<Depth>(next, &()).unwrap(), 3);
    assert_eq!(derives.get(), 4, "the new changeset's, on its parent's");
}

/// A value file that is damaged reads as missing, and the value is derived again and stored anew:
/// a zero-filled depth file costs the one derive of that value, its parent's value read back.
#[test]
fn a_damaged_value_is_derived_again() {
    let (_dir, root, mut repo) = new_repo();
    let chain = chain(&repo, 5);
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();
    assert_eq!(repo.derived::<Depth>(chain[4], &()).unwrap(), 4);

    let file = value_file(&root, "depth", 1, chain[2]);
    let len = usize::try_from(fs::metadata(&file).unwrap().len()).unwrap();
    fs::write(&file, vec![0; len]).unwrap();
    assert_eq!(repo.derived::<Depth>(chain[4], &()).unwrap(), 4);
    assert_eq!(
        derives.get(),
        5,
        "the head's value never reads its parent's"
    );
    assert_eq!(repo.derived::<Depth>(chain[2], &()).unwrap(), 2);
    assert_eq!(derives.get(), 6, "that one value again, on its parent's");
    assert_ne!(fs::read(&file).unwrap(), vec![0; len], "stored anew");
    assert_eq!(repo.derived::<Depth>(chain[2], &()).unwrap(), 2);
    assert_eq!(derives.get(), 6);
}

/// Derivations whose key and value are `()` and that derive nothing, to register: the macro's
/// arguments are each one's type, `NAME` and `VERSION`.
macro_rules! plain {
    ($($ty:ident = $name:expr, $version:expr;)*) => {$(
        struct $ty;

        impl Derivation for $ty {
            const NAME: &'static str = $name;
            const VERSION: u32 = $version;
            type Key = ();
            type Value = ();

            fn derive(&self, _: &DeriveCtx<'_>, _: Subject<'_>, (): &()) -> Result<(), DeriveError> {
                Ok(())
            }
        }
    )*};
}

plain! {
    Unnamed = "", 1;
    Upper = "A", 1;
    Dash = "a-b", 1;
    Tmp = "tmp", 1;
    TooLong = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 1;
    Longest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 1;
    VersionZero = "version_zero", 0;
    Again = "longest_again", 1;
    Unregistered = "unregistered", 1;
}

/// `MEMO_EVERY = Some(0)`: a memo every 0 steps.
struct MemoZero;

impl Derivation for MemoZero {
    const NAME: &'static str = "memo_zero";
    const VERSION: u32 = 1;
    const MEMO_EVERY: Option<u32> = Some(0);
    type Key = ();
    type Value = ();

    fn derive(&self, _: &DeriveCtx<'_>, _: Subject<'_>, (): &()) -> Result<(), DeriveError> {
        Ok(())
    }
}

/// `EAGER`, with a key that is not `()`: the eager worker derives one value per changeset.
struct EagerKeyed;

impl Derivation for EagerKeyed {
    const NAME: &'static str = "eager_keyed";
    const VERSION: u32 = 1;
    const EAGER: bool = true;
    type Key = u64;
    type Value = ();

    fn derive(&self, _: &DeriveCtx<'_>, _: Subject<'_>, _: &u64) -> Result<(), DeriveError> {
        Ok(())
    }
}

/// Another derivation under [`Longest`]'s name.
struct Impostor;

impl Derivation for Impostor {
    const NAME: &'static str = Longest::NAME;
    const VERSION: u32 = 2;
    type Key = ();
    type Value = ();

    fn derive(&self, _: &DeriveCtx<'_>, _: Subject<'_>, (): &()) -> Result<(), DeriveError> {
        Ok(())
    }
}

/// Registering refuses a derivation the engine cannot keep, with `RepoError::Derived` naming it
/// and before the registry changes: a name that is not 1 to 64 bytes of `[a-z0-9_]`, or is `tmp`
/// (the directory of the derived store's temp files); version 0; `MEMO_EVERY` of 0; `EAGER` with
/// a key other than `()`; and a name registered already. A derivation not registered is refused
/// when its value is asked for.
#[test]
fn register_refuses_what_the_engine_cannot_keep() {
    let (_dir, _root, mut repo) = new_repo();
    let cs = chain(&repo, 1)[0];
    assert_eq!((TooLong::NAME.len(), Longest::NAME.len()), (65, 64));
    let refused = |result: Result<(), RepoError>, name: &str| match result {
        Err(RepoError::Derived { name: found, .. }) => assert_eq!(found, name),
        other => panic!("{name:?}: expected a refusal, got {other:?}"),
    };
    refused(repo.register(Unnamed), "");
    refused(repo.register(Upper), "A");
    refused(repo.register(Dash), "a-b");
    refused(repo.register(Tmp), "tmp");
    refused(repo.register(TooLong), TooLong::NAME);
    refused(repo.register(VersionZero), "version_zero");
    refused(repo.register(MemoZero), "memo_zero");
    refused(repo.register(EagerKeyed), "eager_keyed");

    repo.register(Longest).unwrap();
    refused(repo.register(Longest), Longest::NAME);
    // Another type under a name registered already.
    refused(repo.register(Impostor), Longest::NAME);
    repo.derived::<Longest>(cs, &()).unwrap();
    match repo.derived::<Impostor>(cs, &()) {
        Err(RepoError::Derived { name, .. }) => assert_eq!(name, Longest::NAME),
        other => panic!("expected a refusal, got {other:?}"),
    }
    match repo.derived::<Unregistered>(cs, &()) {
        Err(RepoError::Derived { name, why }) => {
            assert_eq!(name, "unregistered");
            assert!(why.contains("not registered"), "{why}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // A registry of its own takes the same rules.
    let mut registry = Registry::new();
    refused(registry.register(Tmp), "tmp");
    registry.register(Again).unwrap();
    refused(registry.register(Again), "longest_again");
}

/// An I/O error reading a value is the error, not a miss: the engine does not derive over a store
/// it cannot read, whether the read is of the value asked for (the root's, which has no
/// dependency to read) or of a dependency's.
#[test]
fn a_failed_read_of_a_value_is_the_error() {
    let (_dir, root, repo) = new_repo();
    let chain = chain(&repo, 2);
    drop(repo);
    let derived_dir = root.join("meta/derived").into_std_path_buf();
    let failing = FailingFs::new(move |method, path: &Path| {
        method == FsMethod::Read && path.starts_with(&derived_dir)
    });
    let mut repo = LocalRepo::open_with_fs(&root, Arc::new(failing)).unwrap();
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();
    for cs in [chain[0], chain[1]] {
        match repo.derived::<Depth>(cs, &()) {
            Err(RepoError::Store(StoreError::Io(failure))) => {
                assert_eq!(failure.op.as_str(), "read", "{failure}");
            }
            other => panic!("expected the read's failure, got {other:?}"),
        }
    }
    assert_eq!(derives.get(), 0);
}

/// 1 plus the sum of its parents' values: the changesets on every path down to a root, each counted
/// once per path. Its dependencies are the default, every parent.
struct Tally(Count);

impl Derivation for Tally {
    const NAME: &'static str = "tally";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        self.0.bump();
        let mut tally = 1;
        for &parent in &subject.changeset.parents {
            tally += ctx.value::<Tally>(parent, &())?;
        }
        Ok(tally)
    }
}

/// A value depends on every parent's by default (`Derivation::deps`), and each is derived once per
/// run: over a merge `m` of `a2` (on `a`, on the root `r`) and `b` (on `r`), the root is derived
/// once though both sides need it, and the merge's value counts both sides (6, where its first
/// parent alone would give 4). So it is with every put failing: the run keeps what it could not
/// store, rather than derive it again for the second side.
#[test]
fn every_parent_is_a_dependency_by_default() {
    let (_dir, root, repo) = new_repo();
    let commit = |parents: &[ChangesetId], name: &str| {
        let req = request(parents, vec![put("f", name.as_bytes())], name);
        repo.commit(req).unwrap().id
    };
    let r = commit(&[], "r");
    let a = commit(&[r], "a");
    let a2 = commit(&[a], "a2");
    let b = commit(&[r], "b");
    let m = commit(&[a2, b], "m");
    drop(repo);

    let derived_dir = root.join("meta/derived").into_std_path_buf();
    let failing = FailingFs::new(move |method, path: &Path| {
        method == FsMethod::LinkNoclobber && path.starts_with(&derived_dir)
    });
    let mut unstored = LocalRepo::open_with_fs(&root, Arc::new(failing)).unwrap();
    let derives = Count::default();
    unstored.register(Tally(derives.clone())).unwrap();
    assert_eq!(unstored.derived::<Tally>(m, &()).unwrap(), 6);
    assert_eq!(
        derives.get(),
        5,
        "each of the five once, with every put failing"
    );
    assert_eq!(stored(&root, "tally", 1), BTreeSet::new());

    let mut repo = LocalRepo::open(&root).unwrap();
    let derives = Count::default();
    repo.register(Tally(derives.clone())).unwrap();
    assert_eq!(repo.derived::<Tally>(m, &()).unwrap(), 6);
    assert_eq!(derives.get(), 5, "each of the five once");
    let want: BTreeSet<String> = [r, a, a2, b, m].iter().map(ToString::to_string).collect();
    assert_eq!(stored(&root, "tally", 1), want);
    for (cs, value) in [(r, 1), (a, 2), (a2, 3), (b, 2), (m, 6)] {
        assert_eq!(repo.derived::<Tally>(cs, &()).unwrap(), value);
    }
    assert_eq!(derives.get(), 5, "read back");
}

/// A value whose put stores nothing because a damaged file it cannot remove holds the slot is
/// kept for the rest of the run, as a failed put's is (storage spec §5: once per run): over the
/// same merge, with the root's file zero-filled and every remove under `meta/derived/` failing,
/// the root is derived once though both sides need it, and its file stays damaged.
#[test]
fn a_value_an_unremovable_file_blocks_is_kept() {
    let (_dir, root, repo) = new_repo();
    let commit = |parents: &[ChangesetId], name: &str| {
        let req = request(parents, vec![put("f", name.as_bytes())], name);
        repo.commit(req).unwrap().id
    };
    let r = commit(&[], "r");
    let a = commit(&[r], "a");
    let a2 = commit(&[a], "a2");
    let b = commit(&[r], "b");
    let m = commit(&[a2, b], "m");
    drop(repo);

    // 1. The root's value stored, then zero-filled.
    let mut repo = LocalRepo::open(&root).unwrap();
    repo.register(Tally(Count::default())).unwrap();
    assert_eq!(repo.derived::<Tally>(r, &()).unwrap(), 1);
    drop(repo);
    let file = value_file(&root, "tally", 1, r);
    let len = usize::try_from(fs::metadata(&file).unwrap().len()).unwrap();
    fs::write(&file, vec![0; len]).unwrap();

    // 2. A run that can remove nothing under `meta/derived/`.
    let derived_dir = root.join("meta/derived").into_std_path_buf();
    let failing = FailingFs::new(move |method, path: &Path| {
        method == FsMethod::Remove && path.starts_with(&derived_dir)
    });
    let mut stuck = LocalRepo::open_with_fs(&root, Arc::new(failing)).unwrap();
    let derives = Count::default();
    stuck.register(Tally(derives.clone())).unwrap();
    assert_eq!(stuck.derived::<Tally>(m, &()).unwrap(), 6);
    assert_eq!(derives.get(), 5, "each of the five once, the root included");
    assert_eq!(fs::read(&file).unwrap(), vec![0; len], "still damaged");
}

/// What another process does while a `derive` waits for a value: given the changeset.
type Meanwhile = Box<dyn Fn(ChangesetId) + Send + Sync>;

/// Twice a changeset's [`Depth`], which it reads through `ctx.value` (it has no dependency): when
/// that is `Missing`, it lets `meanwhile` run before it returns, as another process might store
/// the value in that moment.
struct Racer {
    meanwhile: Meanwhile,
}

impl Derivation for Racer {
    const NAME: &'static str = "racer";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn deps(&self, _: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        Vec::new()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        match ctx.value::<Depth>(subject.id, &()) {
            Ok(depth) => Ok(2 * depth),
            Err(DeriveError::Missing(need)) => {
                (self.meanwhile)(subject.id);
                Err(DeriveError::Missing(need))
            }
            Err(e) => Err(e),
        }
    }
}

/// A value a `derive` waits for that another process stores in the meantime is read, not derived
/// again: the engine looks for a value in the store before it derives it. Here a second
/// repository on the store derives the depths while `racer`'s first call returns.
#[test]
fn a_value_stored_meanwhile_is_read_not_derived() {
    let (_dir, root, mut repo) = new_repo();
    let chain = chain(&repo, 3);
    let (ours, theirs) = (Count::default(), Count::default());
    repo.register(Depth(ours.clone())).unwrap();
    let other = root.clone();
    let meanwhile = move |cs| {
        let mut repo = LocalRepo::open(&other).unwrap();
        repo.register(Depth(theirs.clone())).unwrap();
        assert_eq!(repo.derived::<Depth>(cs, &()).unwrap(), 2);
        assert_eq!(theirs.get(), 3, "the other process derived the chain");
    };
    let meanwhile: Meanwhile = Box::new(meanwhile);
    repo.register(Racer { meanwhile }).unwrap();

    assert_eq!(repo.derived::<Racer>(chain[2], &()).unwrap(), 4);
    assert_eq!(ours.get(), 0, "read from the store, not derived again");
    assert_eq!(stored(&root, "racer", 1), at(&chain, &[2]));
}

/// Returns from every call after its first the `Missing` its first call met, without reading
/// again: a derivation that ignores the value it was given.
struct Stubborn(Mutex<Option<Need>>);

impl Derivation for Stubborn {
    const NAME: &'static str = "stubborn";
    const VERSION: u32 = 1;
    type Key = ();
    type Value = u64;

    fn deps(&self, _: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
        Vec::new()
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        (): &(),
    ) -> Result<u64, DeriveError> {
        let mut first = self.0.lock().unwrap();
        if let Some(need) = &*first {
            return Err(DeriveError::Missing(need.clone()));
        }
        let missing = ctx.value::<Depth>(subject.id, &());
        if let Err(DeriveError::Missing(need)) = &missing {
            *first = Some(need.clone());
        }
        missing
    }
}

/// A `Missing` for a value the engine has given a derivation already is an error, not a loop: the
/// value is derived once, and the call that waits for it again is refused, naming its derivation.
#[test]
fn a_missing_for_a_value_it_was_given_is_an_error_not_a_hang() {
    let (_dir, _root, mut repo) = new_repo();
    let chain = chain(&repo, 2);
    let derives = Count::default();
    repo.register(Depth(derives.clone())).unwrap();
    repo.register(Stubborn(Mutex::new(None))).unwrap();

    let (repo, result) = within_a_minute(move || {
        let result = repo.derived::<Stubborn>(chain[1], &());
        (repo, result)
    });
    match result {
        Err(RepoError::Derived { name, why }) => {
            assert_eq!(name, "stubborn", "{why}");
            assert!(why.contains("Missing"), "{why}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
    assert_eq!(derives.get(), 2, "the depth it waited for, once");
    drop(repo);
}

/// The index, on a chain built by [`chain`], of the changeset `k` first-parent steps back (or the
/// root, if the chain is shorter): a `canonical` redirect to the first parent under `k - 1`, down
/// to `k = 0`, which derives the changeset's own index. Keyed by `k`, so its key changes along a
/// redirect chain; a memo every 2 steps.
struct Back(Count);

impl Derivation for Back {
    const NAME: &'static str = "back";
    const VERSION: u32 = 1;
    const MEMO_EVERY: Option<u32> = Some(2);
    type Key = u32;
    type Value = u64;

    fn deps(&self, _: Subject<'_>, _: &u32) -> Vec<(ChangesetId, u32)> {
        Vec::new()
    }

    fn canonical(
        &self,
        _: &DeriveCtx<'_>,
        subject: Subject<'_>,
        &k: &u32,
    ) -> Result<Option<(ChangesetId, u32)>, DeriveError> {
        let parent = subject.changeset.parents.first();
        Ok(parent.filter(|_| k > 0).map(|&parent| (parent, k - 1)))
    }

    fn derive(&self, _: &DeriveCtx<'_>, subject: Subject<'_>, _: &u32) -> Result<u64, DeriveError> {
        self.0.bump();
        Ok(index(subject.changeset))
    }
}

/// Keys tell a changeset's values apart, and a redirect may change the key: each value is in a
/// slot of its own key (its file name ends in 16 hex digits of the key's hash), and a memo is
/// stored under the key of its node. On a 6-chain, `back` of 5 under key 3 walks (5, 3) → (4, 2)
/// → (3, 1) → (2, 0), which derives 2 and stores it with memos at (5, 3) and (3, 1); under key 1,
/// (5, 1) → (4, 0) derives 4, a chain too short for a memo.
#[test]
fn keys_tell_values_apart_and_a_redirect_may_change_the_key() {
    let (_dir, root, mut repo) = new_repo();
    let chain = chain(&repo, 6);
    let derives = Count::default();
    repo.register(Back(derives.clone())).unwrap();

    assert_eq!(repo.derived::<Back>(chain[5], &3).unwrap(), 2);
    assert_eq!(derives.get(), 1, "only (2, 0) derives");
    assert_eq!(repo.derived::<Back>(chain[5], &1).unwrap(), 4);
    assert_eq!(derives.get(), 2, "only (4, 0) derives");
    assert_eq!(repo.derived::<Back>(chain[3], &1).unwrap(), 2);
    assert_eq!(repo.derived::<Back>(chain[5], &3).unwrap(), 2);
    assert_eq!(derives.get(), 2, "the memos, read back");

    let files = stored(&root, "back", 1);
    let mut subjects = BTreeSet::new();
    for file in &files {
        let (subject, key) = file.split_once('.').expect("a key suffix: no key is ()");
        assert_eq!(key.len(), 16, "{file}");
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()), "{file}");
        subjects.insert(subject.to_owned());
    }
    assert_eq!(
        files.len(),
        4,
        "(2, 0), (5, 3), (3, 1) and (4, 0): {files:?}"
    );
    assert_eq!(subjects, at(&chain, &[2, 3, 4, 5]));
    assert_eq!(repo.derived::<Back>(chain[0], &7).unwrap(), 0, "the root");
}

/// The first-parent skip list, the first built-in derivation (storage spec §5): each changeset's
/// first-parent ancestors 1, 2, 4, … steps back, nearest first.
mod skip_list {
    use swarmops_repo::{FirstParentSkipList, SkipList};
    use swarmops_store::{DerivedSlot, DerivedStore, RealFs};

    use super::*;

    /// A changeset's skip list, through the repository's built-in.
    fn list(repo: &LocalRepo, cs: ChangesetId) -> Vec<ChangesetId> {
        let list: SkipList = repo.derived::<FirstParentSkipList>(cs, &()).unwrap();
        list.into_vec()
    }

    /// The changesets of `chain` 1, 2, 4, … steps back from the one at `k`, as far as the chain
    /// goes, by a walk: what that changeset's skip list holds.
    fn walked(chain: &[ChangesetId], k: usize) -> Vec<ChangesetId> {
        (0..usize::BITS)
            .map(|i| 1_usize << i)
            .take_while(|&steps| steps <= k)
            .map(|steps| chain[k - steps])
            .collect()
    }

    /// The value files of the skip list's version 1, as [`stored`] lists them, for `ids`.
    fn files_of(ids: &[ChangesetId]) -> BTreeSet<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    /// A changeset's skip list holds its first-parent ancestors 1, 2, 4, … steps back, nearest
    /// first, as far as its chain goes, and a root's is empty. On a chain c0 … c32: c8's list is
    /// [c7, c6, c4, c0] (8 steps reach the root exactly), and deriving it derives the lists of c0
    /// … c8 and nothing past c8; c32's is [c31, c30, c28, c24, c16, c0]; and every changeset's
    /// list is what a walk along its first parents gives.
    #[test]
    fn skip_list_values_are_powers_of_two_back() {
        let (_dir, root, repo) = new_repo();
        let c = chain(&repo, 33);

        assert_eq!(list(&repo, c[8]), [c[7], c[6], c[4], c[0]]);
        assert_eq!(
            stored(&root, FirstParentSkipList::NAME, 1),
            files_of(&c[..=8]),
            "c8's chain, nothing past it"
        );
        assert!(list(&repo, c[0]).is_empty(), "a root's list is empty");
        let head = repo.derived::<FirstParentSkipList>(c[32], &()).unwrap();
        assert_eq!(
            *head,
            [c[31], c[30], c[28], c[24], c[16], c[0]],
            "read as a slice"
        );
        assert_eq!(stored(&root, FirstParentSkipList::NAME, 1), files_of(&c));
        for (k, &cs) in c.iter().enumerate() {
            assert_eq!(list(&repo, cs), walked(&c, k), "c{k}");
        }
    }

    /// A merge's skip list follows its first parent, never the second, and so does the list of a
    /// changeset whose first-parent chain passes through a merge. Over a root r, a first side a1 …
    /// a4 on it and a second side b1 on it: the merge m of a4 and b1 has the list [a4, a3, a1],
    /// and c, on m, [m, a4, a2]; deriving them derives the lists of their first-parent chain
    /// alone, not b1's. The merge n of b1 and a4, b1 first, has [b1, r].
    #[test]
    fn a_skip_list_over_a_second_parent_follows_the_first() {
        let (_dir, root, repo) = new_repo();
        let commit = |parents: &[ChangesetId], name: &str| {
            let req = request(parents, vec![put("f", name.as_bytes())], name);
            repo.commit(req).unwrap().id
        };
        let r = commit(&[], "r");
        let a1 = commit(&[r], "a1");
        let a2 = commit(&[a1], "a2");
        let a3 = commit(&[a2], "a3");
        let a4 = commit(&[a3], "a4");
        let b1 = commit(&[r], "b1");
        let m = commit(&[a4, b1], "m");
        let c = commit(&[m], "c");
        let n = commit(&[b1, a4], "n");

        assert_eq!(list(&repo, m), [a4, a3, a1]);
        assert_eq!(list(&repo, c), [m, a4, a2]);
        assert_eq!(
            stored(&root, FirstParentSkipList::NAME, 1),
            files_of(&[r, a1, a2, a3, a4, m, c]),
            "the first parents' lists alone"
        );
        assert_eq!(list(&repo, n), [b1, r]);
        assert_eq!(
            stored(&root, FirstParentSkipList::NAME, 1),
            files_of(&[r, a1, a2, a3, a4, m, c, b1, n])
        );
    }

    /// The skip list is built in: a repository derives it with no registration, created or
    /// opened, and refuses to register it again, while a registry of its own (`Registry::new`)
    /// starts without it. It is `first_parent_skip_list`, version 1, eager (`swarmops derive`
    /// derives it for each new changeset), with no memos.
    #[test]
    fn the_skip_list_is_built_in() {
        assert_eq!(
            (
                FirstParentSkipList::NAME,
                FirstParentSkipList::VERSION,
                FirstParentSkipList::EAGER,
                FirstParentSkipList::MEMO_EVERY,
            ),
            ("first_parent_skip_list", 1, true, None)
        );
        let (_dir, root, mut repo) = new_repo();
        let c = chain(&repo, 3);
        assert_eq!(list(&repo, c[2]), [c[1], c[0]]);
        match repo.register(FirstParentSkipList::default()) {
            Err(RepoError::Derived { name, why }) => {
                assert_eq!(name, "first_parent_skip_list", "{why}");
                assert!(why.contains("registered already"), "{why}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let reopened = LocalRepo::open(&root).unwrap();
        assert_eq!(list(&reopened, c[1]), [c[0]]);
        assert_eq!(stored(&root, "first_parent_skip_list", 1), files_of(&c));

        let mut registry = Registry::new();
        registry.register(FirstParentSkipList::default()).unwrap();
    }

    /// A skip list is stored as postcard encodes it: the number of ids, then each id's 32 bytes,
    /// nearest first; a root's is the one byte 0. This is version 1's layout: another takes a new
    /// `VERSION`.
    #[test]
    fn a_skip_list_is_stored_as_its_ids_bytes() {
        let (_dir, root, repo) = new_repo();
        let c = chain(&repo, 5);
        assert_eq!(list(&repo, c[4]), [c[3], c[2], c[0]]);

        let store = DerivedStore::open(Layout::new(&root), Arc::new(RealFs::new()));
        let value = |cs: ChangesetId| {
            let slot = DerivedSlot::new("first_parent_skip_list", 1, cs.0, &[]);
            store.get(&slot).unwrap().expect("a stored value")
        };
        let mut want = vec![3_u8];
        for id in [c[3], c[2], c[0]] {
            want.extend_from_slice(&id.0.0);
        }
        assert_eq!(value(c[4]), want);
        assert_eq!(value(c[0]), [0]);
    }

    /// An ancestor's list that the store lost (derived values are never synced, so a power loss
    /// may take any back) is derived again on the way: with the files of c4 and c8 of a 9-chain
    /// removed, c8's list reads c4's, finds it missing, has the engine derive it, and is right;
    /// both files are back.
    #[test]
    fn a_lost_ancestor_list_is_derived_again_on_the_way() {
        let (_dir, root, repo) = new_repo();
        let c = chain(&repo, 9);
        assert_eq!(list(&repo, c[8]), [c[7], c[6], c[4], c[0]]);

        for k in [4, 8] {
            fs::remove_file(value_file(&root, FirstParentSkipList::NAME, 1, c[k])).unwrap();
        }
        assert_eq!(list(&repo, c[8]), [c[7], c[6], c[4], c[0]]);
        assert_eq!(stored(&root, FirstParentSkipList::NAME, 1), files_of(&c));
        assert_eq!(list(&repo, c[4]), [c[3], c[2], c[0]]);
    }
}

/// Eager derivations (storage spec §5): `Repo::derive_eager`, which `swarmops derive` runs,
/// derives the value of every eager derivation for each changeset the journal records after a
/// frame, at most `limit` frames at a time, so that readers find those values stored.
mod eager {
    use swarmops_repo::derive::DeriveRun;
    use swarmops_repo::{CasResult, FirstParentSkipList};

    use super::*;

    /// A changeset's first-parent depth, as [`Depth`] makes it, but eager: `swarmops derive`
    /// derives it for each new changeset. Its derives are counted.
    struct EagerDepth(Count);

    impl Derivation for EagerDepth {
        const NAME: &'static str = "eager_depth";
        const VERSION: u32 = 1;
        const EAGER: bool = true;
        type Key = ();
        type Value = u64;

        fn deps(&self, subject: Subject<'_>, (): &()) -> Vec<(ChangesetId, ())> {
            let first = subject.changeset.parents.first();
            first.map(|&parent| (parent, ())).into_iter().collect()
        }

        fn derive(
            &self,
            ctx: &DeriveCtx<'_>,
            subject: Subject<'_>,
            (): &(),
        ) -> Result<u64, DeriveError> {
            self.0.bump();
            match subject.changeset.parents.first() {
                Some(&parent) => Ok(ctx.value::<EagerDepth>(parent, &())? + 1),
                None => Ok(0),
            }
        }
    }

    /// What a call reports: the values it derived, those it found stored, and the frame it
    /// leaves the journal at.
    fn run(derived: u64, cached: u64, last_frame: Option<u64>) -> DeriveRun {
        let mut run = DeriveRun::default();
        run.derived = derived;
        run.cached = cached;
        run.last_frame = last_frame;
        run
    }

    /// The changesets whose skip lists the store holds, as [`stored`] lists them.
    fn lists(root: &Utf8Path) -> BTreeSet<String> {
        stored(
            root,
            FirstParentSkipList::NAME,
            FirstParentSkipList::VERSION,
        )
    }

    /// Commits a put of `content` at `f` on `parent`; the new changeset's id.
    fn commit_on(repo: &LocalRepo, parent: ChangesetId, content: &str) -> ChangesetId {
        let req = request(&[parent], vec![put("f", content.as_bytes())], content);
        repo.commit(req).expect("commit").id
    }

    /// A call derives the value of every eager derivation, the built-in skip list and a test's
    /// own depth, for each changeset of the frames it reads: over three commits (frames 0 to 2),
    /// 6 values, each derived once (3 depths derived), and it leaves the journal at frame 2. A
    /// derivation that is not eager is left to readers. The same call again from there reads no
    /// frame and derives nothing; from the first frame, it finds the 6 values stored and derives
    /// none of them again.
    #[test]
    fn derive_eager_derives_every_new_changeset_once() {
        let (_dir, root, mut repo) = new_repo();
        let count = Count::default();
        repo.register(EagerDepth(count.clone())).unwrap();
        let lazy = Count::default();
        repo.register(Depth(lazy.clone())).unwrap();
        assert_eq!(repo.derive_eager(None, 1000).unwrap(), run(0, 0, None));
        let c = chain(&repo, 3);

        assert_eq!(repo.derive_eager(None, 1000).unwrap(), run(6, 0, Some(2)));
        assert_eq!(count.get(), 3, "each depth derived once");
        assert_eq!(
            lazy.get(),
            0,
            "a derivation that is not eager waits for a reader"
        );
        assert_eq!(stored(&root, "depth", 1), BTreeSet::new());
        assert_eq!(lists(&root), at(&c, &[0, 1, 2]));
        assert_eq!(stored(&root, "eager_depth", 1), at(&c, &[0, 1, 2]));
        assert_eq!(repo.derived::<EagerDepth>(c[2], &()).unwrap(), 2);
        let list = repo.derived::<FirstParentSkipList>(c[2], &()).unwrap();
        assert_eq!(list.into_vec(), [c[1], c[0]]);

        assert_eq!(repo.derive_eager(Some(2), 1000).unwrap(), run(0, 0, None));
        assert_eq!(repo.derive_eager(None, 1000).unwrap(), run(0, 6, Some(2)));
        assert_eq!(count.get(), 3, "nothing derived again");
    }

    /// A call goes on after the frame it is given and reads at most `limit` frames; a frame that
    /// records no changeset (a ref's move) is read and passed over, and frames before the
    /// checkpoint read as those past it do. After c0 and c1 (frames 0, 1) are derived: c2 and
    /// c3 (frames 2, 3, then behind the checkpoint), a move of `main` (frame 4) and c4 (frame 5).
    /// Two frames after frame 1 derive c2's and c3's lists; the next two, c4's; after frame 5
    /// nothing is left.
    #[test]
    fn derive_eager_resumes_after_its_frame() {
        let (_dir, root, repo) = new_repo();
        let c = chain(&repo, 2);
        assert_eq!(repo.derive_eager(None, 1000).unwrap(), run(2, 0, Some(1)));

        let c2 = commit_on(&repo, c[1], "2");
        let c3 = commit_on(&repo, c2, "3");
        MetaStore::open(&root).unwrap().flush().unwrap();
        let moved = repo.cas_ref("main", None, Some(c3), "test/mover").unwrap();
        assert!(
            matches!(moved.result, CasResult::Applied { .. }),
            "{moved:?}"
        );
        let c4 = commit_on(&repo, c3, "4");

        assert_eq!(repo.derive_eager(Some(1), 2).unwrap(), run(2, 0, Some(3)));
        assert_eq!(lists(&root), at(&[c[0], c[1], c2, c3], &[0, 1, 2, 3]));
        assert_eq!(repo.derive_eager(Some(3), 2).unwrap(), run(1, 0, Some(5)));
        let list = repo.derived::<FirstParentSkipList>(c4, &()).unwrap();
        assert_eq!(list.into_vec(), [c3, c2, c[0]]);
        assert_eq!(repo.derive_eager(Some(5), 2).unwrap(), run(0, 0, None));
        assert_eq!(repo.derive_eager(None, 1000).unwrap(), run(0, 5, Some(5)));
    }

    /// A `limit` of 0 reads no frame and derives nothing, wherever `after` is: it gives the
    /// journal's last frame, where a follower that leaves what is recorded so far starts, and
    /// `None` while the journal holds no frame.
    #[test]
    fn a_limit_of_0_gives_the_journals_end_and_derives_nothing() {
        let (_dir, root, repo) = new_repo();
        assert_eq!(repo.derive_eager(None, 0).unwrap(), run(0, 0, None));
        chain(&repo, 3);
        assert_eq!(repo.derive_eager(None, 0).unwrap(), run(0, 0, Some(2)));
        assert_eq!(repo.derive_eager(Some(0), 0).unwrap(), run(0, 0, Some(2)));
        assert_eq!(repo.derive_eager(Some(7), 0).unwrap(), run(0, 0, Some(2)));
        assert_eq!(lists(&root), BTreeSet::new(), "nothing derived");
    }

    /// Values a call needs on the way and the store lacks are derived, as `Repo::derived`
    /// derives them, and not counted: a call after frame 2 of a chain c0 … c3 that nothing has
    /// derived (a follower that started at the journal's end) is asked for c3's list alone, so
    /// it counts 1, and derives c0's, c1's and c2's on the way.
    #[test]
    fn values_needed_on_the_way_are_derived_and_not_counted() {
        let (_dir, root, repo) = new_repo();
        let c = chain(&repo, 4);
        assert_eq!(
            repo.derive_eager(Some(2), 1000).unwrap(),
            run(1, 0, Some(3))
        );
        assert_eq!(lists(&root), at(&c, &[0, 1, 2, 3]));
        assert_eq!(repo.derive_eager(None, 1000).unwrap(), run(0, 4, Some(3)));
    }
}
