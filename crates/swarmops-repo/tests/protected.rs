//! The judge of a ref's move (the U-22 design §2, as its judge's corrections C3 and C4 changed
//! it): `RepoRead::check_ref_move` and `RepoRead::move_class`. `main` and the tracking refs of
//! `main` and `release/<x>` move forward along first parents only, whatever the mode;
//! `release/<x>` moves forward, or anywhere with `MoveMode::Force`, which the answer
//! `MoveClass::NotForward` says; branches, tags and other tracking refs are free here (a tag's
//! move is the store's refusal). A name of no kind is refused, never free. Nothing calls the
//! judge yet beside `RepoRead::check_forward`, which still judges `main` alone.
//!
//! The judge reads names and changesets, never a ref, so every test runs in each format a store
//! is created in, FORMAT 1 and 2 by name; the names only FORMAT 2 and later take are judged in
//! those.

use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_repo::{LocalRepo, MoveClass, MoveMode, RepoError, RepoRead};
use swarmops_store::{FsObjectStore, INIT_FORMATS, StoreFormat};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::repo::{put, request};
use tempfile::TempDir;

const MAIN: &str = "main";
const RELEASE: &str = "release/1.0";
const MODES: [MoveMode; 2] = [MoveMode::Advance, MoveMode::Force];

/// The formats a store is created in: FORMAT 1 and 2 by name, then any other of `INIT_FORMATS`.
fn formats() -> Vec<StoreFormat> {
    let mut all = vec![StoreFormat::V1, StoreFormat::V2];
    for format in INIT_FORMATS {
        if !all.contains(format) {
            all.push(*format);
        }
    }
    all
}

/// [`formats`] without FORMAT 1, which takes only `main` and `release/<x>`.
fn formats_from_2() -> Vec<StoreFormat> {
    let mut all = formats();
    all.retain(|format| *format != StoreFormat::V1);
    all
}

/// A store of `format` holding the history every test judges on.
///
/// ```text
/// c1 ── c2 ── c3 ──┬── first  (parents c3, x)
///                  └── second (parents x, c3)
/// x  (a root of its own)
/// ```
struct History {
    _dir: TempDir,
    repo: LocalRepo,
    c1: ChangesetId,
    c3: ChangesetId,
    x: ChangesetId,
    /// A merge whose first parent is `c3`: forward from `c3`.
    first: ChangesetId,
    /// A merge holding `c3` as its second parent only: not forward from `c3`.
    second: ChangesetId,
}

impl History {
    fn new(format: StoreFormat) -> Self {
        let dir = tempfile::tempdir().expect("create a temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
        let repo = LocalRepo::create_with_fs_as(&root, "test", format, Arc::new(NoSyncFs))
            .expect("create");

        let commit = |parents: &[ChangesetId], message: &str| {
            let req = request(parents, vec![put("f", message.as_bytes())], message);
            repo.commit(req).expect("commit").id
        };
        let c1 = commit(&[], "c1");
        let c2 = commit(&[c1], "c2");
        let c3 = commit(&[c2], "c3");
        let x = commit(&[], "x");
        let first = commit(&[c3, x], "first");
        let second = commit(&[x, c3], "second");

        Self {
            _dir: dir,
            repo,
            c1,
            c3,
            x,
            first,
            second,
        }
    }

    fn read(&self) -> RepoRead<'_, FsObjectStore> {
        self.repo.snapshot().expect("snapshot")
    }
}

/// The id of a changeset no store holds.
fn unknown_id() -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, b"never committed"))
}

/// `got` is the refusal of the move of `name` from `old` to `new`, as `check_forward` builds it:
/// no operation, nothing recovered.
#[track_caller]
fn assert_protected(
    got: Result<MoveClass, RepoError>,
    name: &str,
    old: ChangesetId,
    new: ChangesetId,
) {
    match got {
        Err(RepoError::HistoryProtected {
            name: refused,
            head,
            target,
            op: None,
            recovered,
        }) => {
            assert_eq!((refused.as_str(), head, target), (name, old, new));
            assert_eq!(recovered, Box::default(), "nothing recovered");
        }
        other => panic!("{name} {old} → {new}: expected HistoryProtected, got {other:?}"),
    }
}

/// `got` is the refusal of `name`, a name of no kind.
#[track_caller]
fn assert_no_kind<T: std::fmt::Debug>(got: Result<T, RepoError>, name: &str) {
    match got {
        Err(RepoError::RefName(refused)) => assert_eq!(refused, name),
        other => panic!("{name}: expected RefName, got {other:?}"),
    }
}

/// `main` moves forward along first parents and nowhere else, with `Force` as without it: a
/// forward move is plain in both modes, and a move back, sideways or onto a merge that holds the
/// head as its second parent is refused in both.
#[test]
fn main_is_forward_only_and_never_forced() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();

        for mode in MODES {
            // 1. Forward: plain.
            let forward = [(h.c1, h.c3), (h.c3, h.first)];
            for (old, new) in forward {
                let class = read.check_ref_move(MAIN, Some(old), Some(new), mode);
                assert_eq!(class.unwrap(), MoveClass::Plain, "{format:?} {mode:?}");
            }

            // 2. Any other move: refused.
            let off_chain = [(h.c3, h.c1), (h.c1, h.x), (h.c3, h.second)];
            for (old, new) in off_chain {
                let refused = read.check_ref_move(MAIN, Some(old), Some(new), mode);
                assert_protected(refused, MAIN, old, new);
            }
        }
    }
}

/// A `release/<x>` ref does not move back or sideways by an `Advance`.
#[test]
fn release_back_is_refused_without_force() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();

        for (old, new) in [(h.c3, h.c1), (h.c1, h.x)] {
            let refused = read.check_ref_move(RELEASE, Some(old), Some(new), MoveMode::Advance);
            assert_protected(refused, RELEASE, old, new);
        }
    }
}

/// The same moves pass with `Force`, and the answer says they were not forward.
#[test]
fn release_back_is_forced_with_force() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();

        for (old, new) in [(h.c3, h.c1), (h.c1, h.x)] {
            let class = read.check_ref_move(RELEASE, Some(old), Some(new), MoveMode::Force);
            assert_eq!(class.unwrap(), MoveClass::NotForward, "{format:?}");
        }
    }
}

/// `Force` on a forward move of `release/<x>` changes nothing: the move is plain.
#[test]
fn release_forward_is_plain_even_with_force() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();

        for mode in MODES {
            for (old, new) in [(h.c1, h.c3), (h.c3, h.first)] {
                let class = read.check_ref_move(RELEASE, Some(old), Some(new), mode);
                assert_eq!(class.unwrap(), MoveClass::Plain, "{format:?} {mode:?}");
            }
        }
    }
}

/// Forward is first parents: a merge that holds the head as its second parent only descends
/// from it, and is still not forward.
#[test]
fn release_to_a_second_parent_merge_is_not_forward() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();
        let (old, new) = (Some(h.c3), Some(h.second));

        let refused = read.check_ref_move(RELEASE, old, new, MoveMode::Advance);
        assert_protected(refused, RELEASE, h.c3, h.second);

        let forced = read.check_ref_move(RELEASE, old, new, MoveMode::Force);
        assert_eq!(forced.unwrap(), MoveClass::NotForward);
        assert_eq!(
            read.move_class(RELEASE, old, new).unwrap(),
            MoveClass::NotForward
        );
    }
}

/// A creation, a deletion, a request with neither end and a move to the same target are no
/// moves: plain for `main` and `release/<x>` in both modes, and no changeset is read (the ids
/// are ones the store does not hold).
#[test]
fn creation_deletion_and_no_move_are_not_judged() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();
        let id = Some(unknown_id());
        let no_moves = [(None, id), (id, None), (None, None), (id, id)];

        for name in [MAIN, RELEASE] {
            for (old, new) in no_moves {
                for mode in MODES {
                    let class = read.check_ref_move(name, old, new, mode);
                    assert_eq!(class.unwrap(), MoveClass::Plain, "{name} {mode:?}");
                }
                assert_eq!(read.move_class(name, old, new).unwrap(), MoveClass::Plain);
            }
        }
    }
}

/// A free-form branch and a tag move anywhere as far as this judge goes (the store refuses a
/// tag's move before it), and such a move is plain.
#[test]
fn a_branch_and_a_tag_are_free_here() {
    for format in formats_from_2() {
        let h = History::new(format);
        let read = h.read();
        let (old, new) = (Some(h.c3), Some(h.c1));

        for name in ["feature/x", "tags/v1"] {
            for mode in MODES {
                let class = read.check_ref_move(name, old, new, mode);
                assert_eq!(class.unwrap(), MoveClass::Plain, "{name} {mode:?}");
            }
            assert_eq!(read.move_class(name, old, new).unwrap(), MoveClass::Plain);
        }
    }
}

/// The tracking refs of a remote's `main` and `release/<x>` move forward only, as `main` does:
/// `Force` never passes another move.
#[test]
fn a_tracking_ref_of_main_or_release_is_forward_only() {
    for format in formats_from_2() {
        let h = History::new(format);
        let read = h.read();

        for name in ["remote/origin/main", "remote/origin/release/1.0"] {
            for mode in MODES {
                let forward = read.check_ref_move(name, Some(h.c1), Some(h.c3), mode);
                assert_eq!(forward.unwrap(), MoveClass::Plain, "{name} {mode:?}");

                for (old, new) in [(h.c3, h.c1), (h.c3, h.second)] {
                    let refused = read.check_ref_move(name, Some(old), Some(new), mode);
                    assert_protected(refused, name, old, new);
                }
            }
        }
    }
}

/// Every other tracking ref is free: of a branch, of a tag, and of a path that only starts like
/// a release.
#[test]
fn a_tracking_ref_of_a_branch_is_free() {
    for format in formats_from_2() {
        let h = History::new(format);
        let read = h.read();
        let (old, new) = (Some(h.c3), Some(h.c1));
        let free = [
            "remote/origin/feature/x",
            "remote/origin/topic",
            "remote/origin/tags/v1",
            "remote/origin/release/a/b",
            "remote/origin/main/x",
        ];

        for name in free {
            for mode in MODES {
                let class = read.check_ref_move(name, old, new, mode);
                assert_eq!(class.unwrap(), MoveClass::Plain, "{name} {mode:?}");
            }
        }
    }
}

/// `move_class` marks a move of `release/<x>` off the new target's first-parent chain and
/// nothing else: the same pair on `main`, on a tracking ref and on a branch reads plain, since
/// `main`'s only moves off its chain are no force.
#[test]
fn move_class_marks_only_a_release_move_back() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();
        let class = |name: &str, old, new| read.move_class(name, Some(old), Some(new)).unwrap();

        // 1. A release: back and sideways are marked, forward is not.
        assert_eq!(class(RELEASE, h.c3, h.c1), MoveClass::NotForward);
        assert_eq!(class(RELEASE, h.c1, h.x), MoveClass::NotForward);
        assert_eq!(class(RELEASE, h.c1, h.c3), MoveClass::Plain);
        assert_eq!(class(RELEASE, h.c3, h.first), MoveClass::Plain);

        // 2. The same pair back on every other kind: plain.
        let mut others = vec![MAIN];
        if format != StoreFormat::V1 {
            others.extend([
                "remote/origin/main",
                "remote/origin/release/1.0",
                "feature/x",
            ]);
        }
        for name in others {
            assert_eq!(class(name, h.c3, h.c1), MoveClass::Plain, "{name}");
        }
    }
}

/// A name of no kind is refused by both, whatever the pair and the mode: it never reads as a
/// free ref.
#[test]
fn a_name_of_no_kind_is_refused_not_free() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();
        let pairs = [(Some(h.c3), Some(h.c1)), (None, Some(h.c1))];

        for name in ["release/a/b", "main/x"] {
            for (old, new) in pairs {
                for mode in MODES {
                    assert_no_kind(read.check_ref_move(name, old, new, mode), name);
                }
                assert_no_kind(read.move_class(name, old, new), name);
            }
        }
    }
}

/// `check_forward` is what it was: it refuses a move of `main` off its first-parent chain, and
/// passes every other name, a release moved back and a name of no kind among them.
#[test]
fn check_forward_still_judges_main_only() {
    for format in formats() {
        let h = History::new(format);
        let read = h.read();
        let (old, new) = (Some(h.c3), Some(h.c1));

        // 1. main: refused off its chain, passed along it and when it is no move.
        match read.check_forward(MAIN, old, new) {
            Err(RepoError::HistoryProtected {
                name,
                head,
                target,
                op: None,
                ..
            }) => assert_eq!((name.as_str(), head, target), (MAIN, h.c3, h.c1)),
            other => panic!("expected HistoryProtected, got {other:?}"),
        }
        read.check_forward(MAIN, Some(h.c1), Some(h.c3)).unwrap();
        read.check_forward(MAIN, None, new).unwrap();
        read.check_forward(MAIN, old, None).unwrap();

        // 2. Every other name: not judged.
        for name in [RELEASE, "feature/x", "remote/origin/main", "release/a/b"] {
            read.check_forward(name, old, new).unwrap();
        }
    }
}
