//! M4 Task 6 (operations spec §9 item 1): a land's incremental re-merge against a whole merge
//! from its base. A land that lost its race merges again what it merged before, `L`, onto what
//! the target gained since its first attempt, over `L`'s delta paths alone (`Repo::merge_trees`
//! of the head `L` was made on, `L` and the target's head now); the naive retry merges the whole
//! change again, from its base (`Repo::merge` of the base, the change's head and the target's
//! head now). Both read only the trees on the change's paths, so the bench shows what the
//! incremental form saves, if anything, as the change grows and as the target gains more.
//!
//! For a change of 1, 10 and 100 files, out of a base of 1,000 files in 100 directories, a store
//! is built outside the timed loops, in a fresh temporary directory under `SWARMOPS_BENCH_DIR`
//! (default `~/.local/state/swarmops-bench`), through a filesystem that skips every sync
//! (`NoSyncFs`): the base on `main`; the change, one commit on it; the target's head at the
//! land's first attempt, one changeset more elsewhere; `L`, the change on that head; and 100
//! changesets the target gained after it, elsewhere, each changing one file. The variants
//! `incremental/<files>x<gained>` and `full/<files>x<gained>` time the two merges against the
//! target after 1, 10 and 100 of those, the objects in the page cache as a hot store's are.
//! `docs/perf.md` ("M4 — a land's incremental re-merge") records them; the gate only builds it.

use std::hint::black_box;
use std::sync::Arc;

use camino::Utf8PathBuf;
use criterion::{Criterion, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, RepoPath, TreeId};
use swarmops_repo::{Advance, Change, CommitRequest, LocalRepo};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::measure::base_dir;
use swarmops_testkit::repo::{put, request};

/// The change sizes measured, in files.
const FILES: [usize; 3] = [1, 10, 100];

/// How many changesets the target gained after the land's first attempt, for each variant.
const GAINED: [usize; 3] = [1, 10, 100];

/// The base's directories, and the files in each.
const DIRS: usize = 100;
const PER_DIR: usize = 10;

/// Who commits.
const ACTOR: &str = "bench/remerge";

/// The `i`-th file of the base: 100 directories of 10 files.
fn file(i: usize) -> String {
    format!("src/d{:02}/f{}.rs", i % DIRS, i / DIRS)
}

/// The content of a file at a version: a few lines, one of which names the version.
fn text(i: usize, version: &str) -> Vec<u8> {
    format!("// file {i}\nfn f{i}() {{}}\n// {version}\nfn g{i}() {{}}\n").into_bytes()
}

/// Commits `changes` on `parents`; with `advance`, moves `main` to it too.
fn commit(
    repo: &LocalRepo,
    parents: &[ChangesetId],
    changes: Vec<Change>,
    advance: bool,
) -> ChangesetId {
    let mut req: CommitRequest = request(parents, changes, "a bench commit");
    if advance {
        req.advance = Some(Advance::new("main", ACTOR));
    }
    repo.commit(req).expect("commit").id
}

/// The root of changeset `id`.
fn root(repo: &LocalRepo, id: ChangesetId) -> TreeId {
    repo.changeset(id)
        .expect("read")
        .expect("the changeset")
        .changeset
        .root
}

/// One store of the bench: what the two merges take, for a change of `files` files.
struct Fixture {
    /// The store's directory, removed when the fixture is dropped.
    _dir: tempfile::TempDir,
    repo: LocalRepo,
    base: ChangesetId,
    head: ChangesetId,
    /// The root of the target's head at the land's first attempt, which `L` was made on.
    onto: TreeId,
    /// `L`'s root, and the paths its delta changes.
    landed: (TreeId, Vec<RepoPath>),
    /// The target's head after each changeset it gained since: `gained[n - 1]` after `n`.
    gained: Vec<ChangesetId>,
}

impl Fixture {
    /// Builds the store for a change of `files` files.
    fn build(files: usize) -> Fixture {
        let dir = tempfile::tempdir_in(base_dir()).expect("a temp dir");
        let at = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
        let repo = LocalRepo::create_with_fs(&at, "bench", Arc::new(NoSyncFs)).expect("create");
        let all = (0..DIRS * PER_DIR)
            .map(|i| put(&file(i), &text(i, "base")))
            .collect();
        let base = commit(&repo, &[], all, true);
        let edits: Vec<Change> = (0..files)
            .map(|i| put(&file(i * 7 % 1_000), &text(i, "ours")))
            .collect();
        let head = commit(&repo, &[base], edits.clone(), false);
        let first = commit(
            &repo,
            &[base],
            vec![put("other/first.rs", b"first\n")],
            true,
        );
        let l = commit(&repo, &[first], edits, false);
        let paths = (repo.changeset(l).expect("read").expect("L").changeset.delta)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        let mut gained = Vec::new();
        let mut at_head = first;
        for n in 0..GAINED[GAINED.len() - 1] {
            let other = put(
                &format!("other/g{n}.rs"),
                format!("gained {n}\n").as_bytes(),
            );
            at_head = commit(&repo, &[at_head], vec![other], true);
            gained.push(at_head);
        }
        Fixture {
            onto: root(&repo, first),
            landed: (root(&repo, l), paths),
            _dir: dir,
            repo,
            base,
            head,
            gained,
        }
    }
}

/// Every variant, a store per change size.
fn remerge(c: &mut Criterion) {
    let mut group = c.benchmark_group("remerge");
    for files in FILES {
        let fixture = Fixture::build(files);
        let (l_root, l_paths) = &fixture.landed;
        for gained in GAINED {
            let actual = fixture.gained[gained - 1];
            let actual_root = root(&fixture.repo, actual);
            group.bench_function(format!("incremental/{files}x{gained}"), |b| {
                b.iter(|| {
                    let merged = fixture
                        .repo
                        .merge_trees(fixture.onto, *l_root, actual_root, l_paths)
                        .expect("merge");
                    assert_eq!(merged.conflicts, Vec::new());
                    black_box(merged)
                });
            });
            group.bench_function(format!("full/{files}x{gained}"), |b| {
                b.iter(|| {
                    let merged = fixture
                        .repo
                        .merge(fixture.base, fixture.head, actual)
                        .expect("merge");
                    assert_eq!(merged.conflicts, Vec::new());
                    black_box(merged)
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, remerge);
criterion_main!(benches);
