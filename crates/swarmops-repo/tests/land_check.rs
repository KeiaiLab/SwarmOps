//! The dry run of a land (the M18 design §6.2): `Repo::land_check` plans one change or several,
//! in order, onto a target's head, as `Repo::land` would land them one after another, and
//! writes nothing: no object, no frame, no lock.

use std::collections::BTreeMap;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, Signature};
use swarmops_repo::land::{
    LandCheckRequest, LandRequest, LandStatus, NothingToLandReason, PathChange, PlanItem,
    PlanOutcome,
};
use swarmops_repo::{Advance, Change, LocalRepo, RepoError};
use swarmops_store::RealFs;
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::repo::{delete, path, put, request};
use tempfile::TempDir;

/// Who makes the changes the tests check.
const ACTOR: &str = "test/agent";

/// Who lands.
const LANDER: &str = "test/lander";

/// The seeded pairs `a_check_equals_the_land` checks.
const SEEDS: u64 = 50;

/// The files the seeded changes edit, and the lines each holds.
const FILES: usize = 3;
const LINES: usize = 4;

/// A fresh store, its writes checked against the store's ordering rules.
fn new_store() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", checked(RealFs::new())).unwrap();
    (dir, root, repo)
}

/// The lander's signature: fixed, so that the same land makes the same changeset.
fn lander() -> Signature {
    Signature::new(
        LANDER.as_bytes().to_vec(),
        b"lander@test.swarmops.local".to_vec(),
        1_760_000_100,
        0,
        false,
    )
    .unwrap()
}

/// Commits `changes` on `parents` with `message`, moving `main` to it when `advance` says so.
#[track_caller]
fn commit(
    repo: &LocalRepo,
    parents: &[ChangesetId],
    changes: Vec<Change>,
    message: &str,
    advance: Moves,
) -> ChangesetId {
    let mut req = request(parents, changes, message);
    req.metadata = vec![("agent".to_owned(), ACTOR.as_bytes().to_vec())];
    if advance == Moves::Main {
        req.advance = Some(Advance::new("main", ACTOR));
    }
    repo.commit(req).unwrap().id
}

/// Whether a commit moves `main`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Moves {
    Main,
    Nothing,
}

/// What `main` names.
fn main_head(repo: &LocalRepo) -> ChangesetId {
    repo.get_ref("main").unwrap().expect("main exists").target
}

/// A check of `items` on `main`.
fn check_of(items: Vec<PlanItem>) -> LandCheckRequest {
    LandCheckRequest::new("main", items)
}

/// Every file of the store at `root` below `meta/` and `objects/`, with its bytes.
fn store_files(root: &Utf8Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut dirs = vec![root.join("meta"), root.join("objects")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let at = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
            if entry.file_type().unwrap().is_dir() {
                dirs.push(at);
                continue;
            }
            let name = at.strip_prefix(root).unwrap().to_string();
            found.insert(name, std::fs::read(&at).unwrap());
        }
    }
    found
}

/// A clean change, a conflicting one, one with nothing to land and a keyed replay, checked:
/// the store's files under `meta/` and `objects/` byte for byte as they were.
#[test]
fn a_check_writes_nothing() {
    let (_dir, root, repo) = new_store();
    let t0 = commit(&repo, &[], vec![put("f", b"1\n2\n3\n")], "t0", Moves::Main);
    let keyed = commit(
        &repo,
        &[t0],
        vec![put("k", b"k\n")],
        "keyed",
        Moves::Nothing,
    );
    let mut land = LandRequest::new(keyed, "main", lander());
    land.key = Some("k-1".to_owned());
    repo.land(land).unwrap();
    let theirs = commit(
        &repo,
        &[main_head(&repo)],
        vec![put("f", b"1\nT\n3\n")],
        "t",
        Moves::Main,
    );
    let clean = commit(
        &repo,
        &[t0],
        vec![put("g", b"g\n")],
        "clean",
        Moves::Nothing,
    );
    let conflicting = commit(
        &repo,
        &[t0],
        vec![put("f", b"1\nX\n3\n")],
        "x",
        Moves::Nothing,
    );
    let before = store_files(&root);

    // 1. Each outcome once.
    let mut replay = PlanItem::new(keyed);
    replay.key = Some("k-1".to_owned());
    let items = vec![
        PlanItem::new(clean),
        PlanItem::new(conflicting),
        PlanItem::new(theirs),
        replay,
    ];
    let checked = repo.land_check(check_of(items)).unwrap();
    assert_eq!(checked.onto, theirs);
    let outcomes: Vec<&str> = (checked.items.iter())
        .map(|item| match &item.outcome {
            PlanOutcome::Clean(_) => "clean",
            PlanOutcome::Conflict(_) => "conflict",
            PlanOutcome::NothingToLand(NothingToLandReason::OnTarget) => "on_target",
            PlanOutcome::Replayed(_) => "replayed",
            other => panic!("an outcome the test does not expect: {other:?}"),
        })
        .collect();
    assert_eq!(outcomes, ["clean", "conflict", "on_target", "replayed"]);

    // 2. Nothing written: no object of the clean merge's tree, no frame, no lock file.
    assert_eq!(store_files(&root), before, "the store's files changed");
    assert_eq!(main_head(&repo), theirs);
}

/// A small generator of the seeded changes: splitmix64.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

/// File `at`'s lines in `tree`, joined: what the seeded changes write.
fn text(lines: &[String]) -> Vec<u8> {
    lines.concat().into_bytes()
}

/// One or two seeded edits of `files`, each a line set to one of three values (so that two
/// changes sometimes agree, sometimes conflict), or now and then a file deleted.
fn edits(rng: &mut Seeded, files: &[Vec<String>]) -> Vec<Change> {
    let mut edited: BTreeMap<usize, Option<Vec<String>>> = BTreeMap::new();
    for _ in 0..=rng.below(2) {
        let file = rng.below(FILES);
        if rng.below(10) == 0 {
            edited.insert(file, None);
            continue;
        }
        let mut lines = match edited.get(&file) {
            Some(Some(lines)) => lines.clone(),
            _ => files[file].clone(),
        };
        lines[rng.below(LINES)] = format!("v{}\n", rng.below(3));
        edited.insert(file, Some(lines));
    }
    (edited.into_iter())
        .map(|(file, lines)| match lines {
            Some(lines) => put(&format!("f{file}"), &text(&lines)),
            None => delete(&format!("f{file}")),
        })
        .collect()
}

/// For 50 seeded pairs of changes made on the same head of `main`: the check of both, in
/// order, gives each the outcome its land then gets, one after the other, and the root of each
/// clean one is the root of the changeset its land makes. The second is planned by the
/// staged re-merge onto the first's result, the land by the whole merge onto the landed one.
#[test]
fn a_check_equals_the_land() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let repo = LocalRepo::create_with_fs(&root, "test", Arc::new(NoSyncFs)).unwrap();
    let mut rng = Seeded(0x5eed);
    let mut agreed = BTreeMap::<&str, usize>::new();
    for seed in 0..SEEDS {
        // 1. A fresh tree on main, and two changes made on it.
        let files: Vec<Vec<String>> = (0..FILES)
            .map(|_| (0..LINES).map(|line| format!("l{line}\n")).collect())
            .collect();
        let fresh = (0..FILES)
            .map(|file| put(&format!("f{file}"), &text(&files[file])))
            .collect();
        let parents: Vec<ChangesetId> = repo
            .get_ref("main")
            .unwrap()
            .map(|r| r.target)
            .into_iter()
            .collect();
        let t = commit(&repo, &parents, fresh, &format!("t{seed}"), Moves::Main);
        let a = commit(
            &repo,
            &[t],
            edits(&mut rng, &files),
            &format!("a{seed}"),
            Moves::Nothing,
        );
        let b = commit(
            &repo,
            &[t],
            edits(&mut rng, &files),
            &format!("b{seed}"),
            Moves::Nothing,
        );

        // 2. The check, then each land in the same order.
        let checked = repo.land_check(check_of(vec![PlanItem::new(a), PlanItem::new(b)]));
        let checked = checked.unwrap();
        assert_eq!(checked.onto, t, "seed {seed}");
        for (planned, head) in checked.items.iter().zip([a, b]) {
            assert_eq!(planned.head, head);
            let landed = repo.land(LandRequest::new(head, "main", lander()));
            let what = match (&planned.outcome, landed) {
                (PlanOutcome::Clean(clean), Ok(outcome)) => {
                    assert_eq!(outcome.status, LandStatus::Landed);
                    let made = repo.changeset(outcome.landed).unwrap().unwrap();
                    assert_eq!(clean.root, made.changeset.root, "seed {seed}: the root");
                    "clean"
                }
                (PlanOutcome::Conflict(_), Err(RepoError::Conflict(_))) => "conflict",
                (PlanOutcome::NothingToLand(planned), Err(RepoError::NothingToLand(got))) => {
                    assert_eq!(*planned, got, "seed {seed}");
                    "nothing"
                }
                (planned, landed) => panic!("seed {seed}: checked {planned:?}, landed {landed:?}"),
            };
            *agreed.entry(what).or_default() += 1;
        }
    }

    // 3. The seeds reach every outcome a pair can have.
    assert!(
        agreed.len() >= 3,
        "the seeds reach too few outcomes: {agreed:?}"
    );
}

/// Three changes made on the same head (the M18 design §6.2, D13): the third edits the line
/// the first does, the second another file. Each alone lands cleanly; in order, only the third
/// conflicts, with the first's result, and the report names its path.
#[test]
fn an_ordered_check_reports_only_the_third_conflict() {
    let (_dir, _root, repo) = new_store();
    let t = commit(&repo, &[], vec![put("f", b"1\n2\n3\n")], "t", Moves::Main);
    let first = commit(
        &repo,
        &[t],
        vec![put("f", b"1\nA\n3\n")],
        "first",
        Moves::Nothing,
    );
    let second = commit(
        &repo,
        &[t],
        vec![put("g/h", b"h\n")],
        "second",
        Moves::Nothing,
    );
    let third = commit(
        &repo,
        &[t],
        vec![put("f", b"1\nC\n3\n")],
        "third",
        Moves::Nothing,
    );

    // 1. Each alone is clean.
    for head in [first, second, third] {
        let alone = repo
            .land_check(check_of(vec![PlanItem::new(head)]))
            .unwrap();
        assert!(matches!(alone.items[0].outcome, PlanOutcome::Clean(_)));
    }

    // 2. In order: clean, clean, then the third's conflict.
    let items = vec![
        PlanItem::new(first),
        PlanItem::new(second),
        PlanItem::new(third),
    ];
    let checked = repo.land_check(check_of(items)).unwrap();
    let PlanOutcome::Clean(one) = &checked.items[0].outcome else {
        panic!("the first conflicts: {:?}", checked.items[0]);
    };
    assert_eq!(one.paths, [(path("f"), PathChange::Modified)]);
    let PlanOutcome::Clean(two) = &checked.items[1].outcome else {
        panic!("the second conflicts: {:?}", checked.items[1]);
    };
    assert_eq!(two.paths, [(path("g/h"), PathChange::Added)]);
    let PlanOutcome::Conflict(conflicts) = &checked.items[2].outcome else {
        panic!("the third is clean: {:?}", checked.items[2]);
    };
    let paths: Vec<&str> = (conflicts.conflicts.iter())
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["f"]);
    assert_eq!((conflicts.base, conflicts.ours), (t, third));
}

/// A change landed with a key, checked again with it: a replay naming the changeset that
/// landed it; a change after it is planned on the target's head as it is.
#[test]
fn a_check_of_a_landed_key_replays() {
    let (_dir, _root, repo) = new_store();
    let t = commit(&repo, &[], vec![put("f", b"f\n")], "t", Moves::Main);
    let a = commit(&repo, &[t], vec![put("a", b"a\n")], "a", Moves::Nothing);
    let b = commit(&repo, &[t], vec![put("b", b"b\n")], "b", Moves::Nothing);
    let mut land = LandRequest::new(a, "main", lander());
    land.key = Some("key-a".to_owned());
    let landed = repo.land(land).unwrap().landed;

    // 1. The replay, then b on the target's head.
    let mut again = PlanItem::new(a);
    again.key = Some("key-a".to_owned());
    let checked = repo
        .land_check(check_of(vec![again, PlanItem::new(b)]))
        .unwrap();
    assert_eq!(checked.onto, landed);
    assert_eq!(checked.items[0].outcome, PlanOutcome::Replayed(landed));
    let PlanOutcome::Clean(clean) = &checked.items[1].outcome else {
        panic!("b does not plan cleanly: {:?}", checked.items[1]);
    };
    assert_eq!(clean.paths, [(path("b"), PathChange::Added)]);

    // 2. The same key for another change fails the whole check.
    let mut reused = PlanItem::new(b);
    reused.key = Some("key-a".to_owned());
    let failed = repo.land_check(check_of(vec![PlanItem::new(b), reused]));
    assert!(
        matches!(failed, Err(RepoError::KeyReused { .. })),
        "{failed:?}"
    );
}
