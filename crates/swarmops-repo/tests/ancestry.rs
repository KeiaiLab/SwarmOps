//! All-parent ancestry (the M16 design §5): `RepoRead::ancestry` answers whether one changeset
//! is an ancestor of another along every parent, their merge bases (the maximal common
//! ancestors, at most `MAX_MERGE_BASES`), and the changesets `to` reaches and `from` does not,
//! a page at a time, all in generation order, newest first, ties by id.
//!
//! Every test owns a fresh store. The DAGs are raw changesets of one tree in one batch,
//! so that each node is only its parents and its message.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Debug;
use std::fs;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, LeafKind, ObjectKind, Tree, TreeEntry, TreeId};
use swarmops_repo::ancestry::{Ancestry, MAX_MERGE_BASES};
use swarmops_repo::{LocalRepo, RawChangeset, RepoError, StoreError};
use swarmops_store::Layout;
use swarmops_testkit::repo::{blob, new_repo, signature};

/// The nodes of the random DAG.
const NODES: usize = 300;

/// The random pairs the brute-force test asks about: each page re-walks, so a few dozen pairs
/// of some twenty pages each keep it to seconds.
const ROUNDS: usize = 60;

/// The page size the brute-force test reads `entries` at.
const PAGE: usize = 7;

/// A changeset's place in the order every answer keeps: generation descending, then id
/// ascending.
type Key = (Reverse<u64>, ChangesetId);

/// A DAG committed to a store, with what a brute force needs: each node's generation and the
/// nodes it reaches.
struct Dag {
    ids: Vec<ChangesetId>,
    generation: HashMap<ChangesetId, u64>,
    ancestors: HashMap<ChangesetId, BTreeSet<ChangesetId>>,
}

impl Dag {
    /// The changesets `id` reaches along every parent, itself included.
    fn ancestors(&self, id: ChangesetId) -> &BTreeSet<ChangesetId> {
        &self.ancestors[&id]
    }

    /// `ids` in the answers' order.
    fn ordered(&self, ids: impl IntoIterator<Item = ChangesetId>) -> Vec<ChangesetId> {
        let mut keyed: Vec<Key> = ids
            .into_iter()
            .map(|id| (Reverse(self.generation[&id]), id))
            .collect();
        keyed.sort();
        keyed.into_iter().map(|(_, id)| id).collect()
    }
}

/// A small deterministic generator (xorshift64*), so that a failing DAG is the same every run.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
}

/// The tree of one file, stored: a stored root tree is never empty.
fn one_file_tree(repo: &LocalRepo) -> TreeId {
    let entry = TreeEntry::leaf("f", LeafKind::File, blob(b"f"));
    let tree = Tree::new(vec![entry]).unwrap();
    let objects = vec![
        (ObjectKind::Blob, b"f".to_vec()),
        (ObjectKind::Tree, tree.encode()),
    ];
    repo.put_objects(objects).unwrap();
    tree.id()
}

/// Commits one changeset per entry of `shape`, each on the earlier nodes it names by index, in
/// one raw batch; `message` names the node.
fn commit_dag(repo: &LocalRepo, shape: &[Vec<usize>]) -> Dag {
    let root = one_file_tree(repo);
    let mut batch = repo.raw_batch();
    let mut dag = Dag {
        ids: Vec::new(),
        generation: HashMap::new(),
        ancestors: HashMap::new(),
    };
    for (n, on) in shape.iter().enumerate() {
        // 1. The node's parents, by the ids made before it.
        let parents: Vec<ChangesetId> = on.iter().map(|&p| dag.ids[p]).collect();
        let generation = 1 + parents.iter().map(|p| dag.generation[p]).max().unwrap_or(0);

        // 2. The node itself, named by its index.
        let raw = RawChangeset::new(
            parents.clone(),
            root,
            signature(),
            signature(),
            format!("node {n}").into_bytes(),
            Vec::new(),
            Vec::new(),
        );
        let id = batch.push(raw).unwrap();
        // 3. What it reaches: itself, and what its parents reach.
        let mut reach = BTreeSet::from([id]);
        for parent in &parents {
            reach.extend(dag.ancestors[parent].iter().copied());
        }
        dag.ancestors.insert(id, reach);
        dag.ids.push(id);
        dag.generation.insert(id, generation);
    }
    batch.commit().unwrap();
    dag
}

/// A random DAG of [`NODES`] nodes: mostly one parent, a merge of two or three now and then, and
/// a few more roots, so that some pairs share no ancestor.
fn random_shape(seed: u64) -> Vec<Vec<usize>> {
    let mut rng = Seeded(seed);
    let mut shape: Vec<Vec<usize>> = vec![Vec::new()];
    for n in 1..NODES {
        // 1. A root one time in forty.
        if rng.below(40) == 0 {
            shape.push(Vec::new());
            continue;
        }

        // 2. One to three distinct parents, the first one near.
        let count = match rng.below(8) {
            0 => 3,
            1 | 2 => 2,
            _ => 1,
        };
        let mut on = vec![n - 1 - rng.below(n.min(4))];
        while on.len() < count.min(n) {
            let parent = rng.below(n);
            if !on.contains(&parent) {
                on.push(parent);
            }
        }
        shape.push(on);
    }
    shape
}

/// Reads every page of `entries` of `from..to` at `limit`, checking each page's other answers
/// against the first's.
fn all_entries(
    repo: &LocalRepo,
    from: ChangesetId,
    to: ChangesetId,
    limit: usize,
) -> (Ancestry, Vec<ChangesetId>) {
    let read = repo.snapshot().unwrap();
    let first = read.ancestry(from, to, limit, None).unwrap();
    let mut entries: Vec<ChangesetId> = first.entries.iter().map(|e| e.id).collect();
    let mut more = first.more;
    while more {
        // A full page ends where the next starts.
        let last = entries.last().copied().unwrap();
        let generation = read.changeset(last).unwrap().unwrap().generation;
        let page = read
            .ancestry(from, to, limit, Some((generation, last)))
            .unwrap();
        assert_eq!(page.is_ancestor, first.is_ancestor);
        assert_eq!(page.merge_bases, first.merge_bases);
        assert!(page.entries.len() <= limit);
        entries.extend(page.entries.iter().map(|e| e.id));
        more = page.more;
    }
    (first, entries)
}

#[test]
fn ancestry_matches_a_brute_force_walk() {
    let (_dir, _root, repo) = new_repo();
    let dag = commit_dag(&repo, &random_shape(0x5eed_a11c_e570_0001));
    let mut rng = Seeded(0x0bad_cafe);
    let mut seen = BTreeSet::new();

    for round in 0..ROUNDS {
        // 1. A random pair, now and then the same node twice.
        let from = dag.ids[rng.below(NODES)];
        let to = if round % 10 == 0 {
            from
        } else {
            dag.ids[rng.below(NODES)]
        };

        // 2. The answers by set arithmetic.
        let of_from = dag.ancestors(from);
        let of_to = dag.ancestors(to);
        let common: BTreeSet<ChangesetId> = of_from.intersection(of_to).copied().collect();
        let maximal = common.iter().copied().filter(|&c| {
            !common
                .iter()
                .any(|&d| d != c && dag.ancestors(d).contains(&c))
        });
        let bases = dag.ordered(maximal);
        let between = dag.ordered(of_to.difference(of_from).copied());

        // 3. The walk's, paged.
        let (got, entries) = all_entries(&repo, from, to, PAGE);
        let what = format!("round {round}: {from}..{to}");
        assert_eq!(got.is_ancestor, of_to.contains(&from), "{what}");
        assert_eq!(
            got.merge_bases_truncated,
            bases.len() > MAX_MERGE_BASES,
            "{what}"
        );
        let capped: Vec<ChangesetId> = bases.iter().copied().take(MAX_MERGE_BASES).collect();
        assert_eq!(got.merge_bases, capped, "{what}");
        assert_eq!(entries, between, "{what}");

        // 4. A page of 0 gives the two answers alone.
        let read = repo.snapshot().unwrap();
        let bare = read.ancestry(from, to, 0, None).unwrap();
        assert_eq!(
            (bare.is_ancestor, &bare.merge_bases),
            (got.is_ancestor, &got.merge_bases)
        );
        assert!(bare.entries.is_empty() && !bare.more, "{what}");

        // 5. Which cases the pair covered.
        let cases = [
            (got.is_ancestor && from != to, "an ancestor"),
            (bases.is_empty(), "no common ancestor"),
            (bases.len() > 1, "more than one merge base"),
            (between.len() > PAGE, "more than one page"),
        ];
        seen.extend(
            cases
                .into_iter()
                .filter(|(hit, _)| *hit)
                .map(|(_, case)| case),
        );
    }
    assert_eq!(seen.len(), 4, "the seeds cover every case: {seen:?}");
}

#[test]
fn identical_revs_are_their_own_base() {
    let (_dir, _root, repo) = new_repo();
    let dag = commit_dag(&repo, &[vec![], vec![0], vec![1]]);
    let c = dag.ids[1];

    let got = repo.snapshot().unwrap().ancestry(c, c, 50, None).unwrap();
    assert!(got.is_ancestor);
    assert_eq!(got.merge_bases, [c]);
    assert!(!got.merge_bases_truncated);
    assert!(got.entries.is_empty() && !got.more);
}

#[test]
fn no_common_ancestor_has_no_base() {
    let (_dir, _root, repo) = new_repo();
    // Two histories: 0 <- 1, and 2 <- 3.
    let dag = commit_dag(&repo, &[vec![], vec![0], vec![], vec![2]]);
    let [a0, a1, b0, b1] = dag.ids[..] else {
        unreachable!("four nodes")
    };

    let got = repo.snapshot().unwrap().ancestry(a1, b1, 50, None).unwrap();
    assert!(!got.is_ancestor);
    assert_eq!(got.merge_bases, Vec::<ChangesetId>::new());
    let entries: Vec<ChangesetId> = got.entries.iter().map(|e| e.id).collect();
    assert_eq!(entries, [b1, b0]);
    assert!(!got.more);

    let back = repo.snapshot().unwrap().ancestry(b1, a1, 50, None).unwrap();
    let entries: Vec<ChangesetId> = back.entries.iter().map(|e| e.id).collect();
    assert_eq!(entries, [a1, a0]);
}

#[test]
fn sixty_five_bases_are_truncated() {
    let (_dir, _root, repo) = new_repo();
    // 65 roots, and two merges of all of them: every root is a maximal common ancestor.
    let roots = MAX_MERGE_BASES + 1;
    let mut shape: Vec<Vec<usize>> = vec![Vec::new(); roots];
    shape.push((0..roots).collect());
    shape.push((0..roots).rev().collect());
    let dag = commit_dag(&repo, &shape);
    let (a, b) = (dag.ids[roots], dag.ids[roots + 1]);

    let got = repo.snapshot().unwrap().ancestry(a, b, 50, None).unwrap();
    assert!(!got.is_ancestor);
    assert!(got.merge_bases_truncated);
    let mut lowest: Vec<ChangesetId> = dag.ids[..roots].to_vec();
    lowest.sort();
    lowest.truncate(MAX_MERGE_BASES);
    assert_eq!(
        got.merge_bases, lowest,
        "the 64 smallest ids, all of generation 1"
    );
    let entries: Vec<ChangesetId> = got.entries.iter().map(|e| e.id).collect();
    assert_eq!(entries, [b]);
}

#[test]
fn a_bad_generation_is_corrupt() {
    let (_dir, root, repo) = new_repo();
    // 0 <- 1 <- 2, checkpointed, then record 1's generation 2 made 5.
    let dag = commit_dag(&repo, &[vec![], vec![0], vec![1]]);
    repo.flush_metadata().unwrap();
    set_generation(&root, 1, 2, 5);
    let repo = LocalRepo::open(&root).unwrap();

    let read = repo.snapshot().unwrap();
    assert_index_corrupt(read.ancestry(dag.ids[0], dag.ids[2], 50, None), "0..2");
    assert_index_corrupt(read.ancestry(dag.ids[2], dag.ids[0], 50, None), "2..0");
}

/// Writes generation `now` over record `record`'s in `idx/gen.col`, after checking that it is
/// `was`.
fn set_generation(root: &Utf8Path, record: usize, was: u64, now: u64) {
    let file = Layout::new(root).meta_path("idx/gen.col");
    let mut content = fs::read(&file).unwrap();
    let place = 8 * record..8 * record + 8;
    assert_eq!(
        content[place.clone()],
        was.to_le_bytes(),
        "gen.col at {record}"
    );
    content[place].copy_from_slice(&now.to_le_bytes());
    fs::write(&file, content).unwrap();
}

#[track_caller]
fn assert_index_corrupt<T: Debug>(got: Result<T, RepoError>, what: &str) {
    match got {
        Err(RepoError::Store(StoreError::IndexCorrupt { .. })) => {}
        other => panic!("{what}: expected IndexCorrupt, got {other:?}"),
    }
}
