//! Landing order (B15, the M18 design §9): changes made on one base land onto `main` to the same
//! tree in whatever order they come. A seeded generator makes 4 to 6 changes of one file over one
//! base — far edits, edits of touching lines, inserts at one position, and edits inside a block
//! another change deletes — each with a file of its own too; every permutation of them lands, in
//! a fresh store, one change after another. Either every order lands them all, to one root, or
//! every order stops on a conflict over the same paths.
//!
//! Every change edits the one shared file, so a conflict can only be over that file: with two
//! contested files, two orders could each stop on the conflict they meet first, a difference of
//! the order, not of the merge.

use std::collections::BTreeSet;
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, ObjectKind, TreeId, hash_object};
use swarmops_repo::land::LandRequest;
use swarmops_repo::{Advance, Change, LocalRepo, RepoError};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::repo::{put, request, signature};

/// Who makes the base and the changes.
const ACTOR: &str = "test/agent";

/// The file every change edits.
const SHARED: &str = "f";

/// How many lines the shared file's base holds.
const LINES: usize = 40;

/// The first line of the far edits' slots: change `i` edits line `FAR + 3 * i` alone.
const FAR: usize = 1;

/// The touching lines: a change edits one of them.
const TOUCHING: [usize; 3] = [24, 25, 26];

/// Where every insert goes: before this line.
const INSERT_AT: usize = 30;

/// The block that a change deletes, and inside which another edits a line.
const BLOCK: std::ops::Range<usize> = 34..38;

/// The seeds the property runs: each makes `4 + seed % 3` changes.
const SEEDS: std::ops::Range<u64> = 0..12;

/// SplitMix64: a small generator, so that a seed names its changes on every host.
struct Rng(u64);

impl Rng {
    /// The next value.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
}

/// One change's edit of the shared file, by its base's line numbers.
#[derive(Debug, Clone)]
enum Edit {
    /// Line `line` replaced by `text`.
    Replace { line: usize, text: String },
    /// `text` inserted before line `at`.
    Insert { at: usize, text: String },
    /// The lines of `lines` deleted.
    Delete { lines: std::ops::Range<usize> },
}

/// The text a change writes: its own, or one every change that picks it shares (identical edits
/// merge).
fn text_of(rng: &mut Rng, change: usize) -> String {
    match rng.below(2) {
        0 => "shared".to_owned(),
        _ => format!("change {change}"),
    }
}

/// The edits of seed `seed`: `4 + seed % 3` of them, each of a kind the seed picks.
fn edits_of(seed: u64) -> Vec<Edit> {
    let mut rng = Rng(seed);
    let count = 4 + usize::try_from(seed % 3).unwrap();
    (0..count)
        .map(|change| match rng.below(5) {
            0 => Edit::Replace {
                line: FAR + 3 * change,
                text: format!("far {change}"),
            },
            1 => Edit::Replace {
                line: TOUCHING[rng.below(TOUCHING.len())],
                text: text_of(&mut rng, change),
            },
            2 => Edit::Insert {
                at: INSERT_AT,
                text: text_of(&mut rng, change),
            },
            3 => Edit::Delete { lines: BLOCK },
            _ => Edit::Replace {
                line: BLOCK.start + 1 + rng.below(BLOCK.len() - 2),
                text: format!("inside {change}"),
            },
        })
        .collect()
}

/// The shared file's base line `n`.
fn base_line(n: usize) -> String {
    format!("line {n:02}\n")
}

/// The shared file after `edit`, as bytes.
fn content_of(edit: Option<&Edit>) -> Vec<u8> {
    let mut out = String::new();
    for n in 0..LINES {
        // 1. An insert goes before its line.
        if let Some(Edit::Insert { at, text }) = edit
            && *at == n
        {
            out.push_str(text);
            out.push('\n');
        }

        // 2. The line itself: deleted, replaced, or kept.
        match edit {
            Some(Edit::Delete { lines }) if lines.contains(&n) => {}
            Some(Edit::Replace { line, text }) if *line == n => {
                out.push_str(text);
                out.push('\n');
            }
            _ => out.push_str(&base_line(n)),
        }
    }
    out.into_bytes()
}

/// How an order of lands ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Every change landed; `main`'s tree at the end.
    Landed(TreeId),
    /// A land conflicted over these paths; nothing after it ran.
    Conflict(BTreeSet<String>),
}

/// Lands `edits` in `order` onto `main` in a fresh store, one after another, each made on the
/// base: how that ended.
fn land_in(edits: &[Edit], order: &[usize]) -> Outcome {
    // 1. A fresh store, main at the base.
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).unwrap();
    let mut base = request(&[], vec![put(SHARED, &content_of(None))], "base");
    base.advance = Some(Advance::new("main", ACTOR));
    let t0 = repo.commit(base).unwrap().id;

    // 2. Each change on the base: the shared file and a file of its own. The same seed makes the
    //    same changesets in every store.
    let heads: Vec<ChangesetId> = (edits.iter().enumerate())
        .map(|(i, edit)| {
            let changes: Vec<Change> = vec![
                put(SHARED, &content_of(Some(edit))),
                put(&format!("own/{i}"), format!("{i}\n").as_bytes()),
            ];
            let made = request(&[t0], changes, &format!("change {i}"));
            repo.commit(made).unwrap().id
        })
        .collect();

    // 3. The lands, in order, until one conflicts.
    for &i in order {
        match repo.land(LandRequest::new(heads[i], "main", signature())) {
            Ok(_) => {}
            Err(RepoError::Conflict(conflicts)) => {
                let paths = (conflicts.conflicts.iter())
                    .map(|conflict| conflict.path.as_str().to_owned())
                    .collect();
                return Outcome::Conflict(paths);
            }
            Err(other) => panic!("order {order:?}: land {i} failed: {other:?}"),
        }
    }

    // 4. main's tree.
    let head = repo.get_ref("main").unwrap().expect("main exists").target;
    Outcome::Landed(repo.changeset(head).unwrap().unwrap().changeset.root)
}

/// Every permutation of `0..n`, in lexicographic order.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..n).collect();
    let mut all = vec![order.clone()];
    loop {
        // 1. The rightmost rise; none left means the last permutation.
        let Some(i) = (0..n.saturating_sub(1))
            .rev()
            .find(|&i| order[i] < order[i + 1])
        else {
            return all;
        };

        // 2. Swap it with the smallest larger value to its right, then reverse the tail.
        let j = (i + 1..n).rev().find(|&j| order[j] > order[i]).unwrap();
        order.swap(i, j);
        order[i + 1..].reverse();
        all.push(order.clone());
    }
}

/// The first order whose outcome differs from the first order's, with both outcomes: `None`
/// when every order ended alike.
fn order_dependence(outcomes: &[(Vec<usize>, Outcome)]) -> Option<String> {
    let (first_order, first) = outcomes.first()?;
    let (order, other) = outcomes.iter().find(|(_, outcome)| outcome != first)?;
    Some(format!(
        "order {first_order:?} ended {first:?}, order {order:?} ended {other:?}"
    ))
}

/// B15: for each seed, every permutation of its changes lands them all to one root, or every
/// permutation stops on a conflict over the same paths. A counterexample names its seed and its
/// orders. The seeds make both kinds of case, so that neither half of the property passes
/// unexercised.
#[test]
#[ignore = "slow, ~280 s: 12 seeds, up to 720 orders each, a store per order; load 90-160"]
fn landing_order_does_not_change_the_tree() {
    let (mut landed, mut conflicted) = (0, 0);
    for seed in SEEDS {
        // 1. Every permutation of the seed's changes, each in a fresh store.
        let edits = edits_of(seed);
        let outcomes: Vec<(Vec<usize>, Outcome)> = (permutations(edits.len()).into_iter())
            .map(|order| {
                let outcome = land_in(&edits, &order);
                (order, outcome)
            })
            .collect();

        // 2. One outcome for every order.
        if let Some(found) = order_dependence(&outcomes) {
            panic!("seed {seed}, edits {edits:?}: {found}");
        }

        // 3. Counted by kind, for the self-check below.
        match &outcomes[0].1 {
            Outcome::Landed(_) => landed += 1,
            Outcome::Conflict(paths) => {
                assert_eq!(paths, &BTreeSet::from([SHARED.to_owned()]), "seed {seed}");
                conflicted += 1;
            }
        }
    }
    eprintln!("{landed} seeds landed in every order, {conflicted} conflicted in every order");
    assert!(
        landed > 0 && conflicted > 0,
        "the seeds made {landed} clean and {conflicted} conflicting cases: both must occur"
    );
}

/// The check itself: it finds an order that ended otherwise, a conflict or another root, and
/// passes outcomes that are all alike; and the permutations are all `n!` of them, each once.
#[test]
fn the_order_check_finds_a_differing_order() {
    // 1. Two roots, and a root against a conflict.
    let one = Outcome::Landed(TreeId::new(hash_object(ObjectKind::Tree, b"1")));
    let two = Outcome::Landed(TreeId::new(hash_object(ObjectKind::Tree, b"2")));
    let conflict = Outcome::Conflict(BTreeSet::from([SHARED.to_owned()]));
    let alike = [(vec![0, 1], one.clone()), (vec![1, 0], one.clone())];
    assert_eq!(order_dependence(&alike), None);
    for other in [two, conflict] {
        let differ = [(vec![0, 1], one.clone()), (vec![1, 0], other)];
        let found = order_dependence(&differ).expect("a differing order");
        assert!(found.contains("order [1, 0]"), "{found}");
    }

    // 2. Every permutation, once.
    let all = permutations(4);
    let distinct: BTreeSet<&Vec<usize>> = all.iter().collect();
    assert_eq!((all.len(), distinct.len()), (24, 24));
    assert_eq!(permutations(6).len(), 720);
}
