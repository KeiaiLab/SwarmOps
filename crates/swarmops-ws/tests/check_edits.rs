//! A workspace's edits, read for a plan of its checks (`check::edits`): its committed chain's
//! ends and the paths of its edits, with no lock, refused as a run of its checks refuses it.

use swarmops_api::{StatusAfter, WsCommitReq, WsRebaseAction, WsRebaseReq};
use swarmops_ws::check::edits;
use swarmops_ws::lock;
use swarmops_ws::workspace::Locate;

use crate::fixture::{Fixture, Put, remove, write};

/// The workspace `w`, by name.
fn w() -> Locate {
    Locate::Name("w".to_owned())
}

/// A clean workspace has no edit: its name, target, base and head are told, and its directory.
#[test]
fn a_clean_workspace_has_no_edit() {
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a.txt", "a\n")]);
    f.create("w", "main", &[]);
    let ops_before = f.last_op();
    let read = edits(&f.ctx(), &w(), 10).unwrap();
    assert_eq!((read.name.as_str(), read.target.as_str()), ("w", "main"));
    assert_eq!(
        (read.base.as_str(), read.head.as_str()),
        (base.as_str(), base.as_str())
    );
    assert_eq!(read.dir, f.beside("w"));
    assert_eq!(read.paths, Vec::<String>::new());
    assert!(!read.truncated && !read.outside);
    assert_eq!(
        f.last_op(),
        ops_before,
        "reading edits writes nothing to the store"
    );
}

/// Edits are what this is for: an edited, an added and a deleted file are its paths, in path
/// order, against the head its commits made. No lock is taken: it reads while another holds
/// the workspace's lock.
#[test]
fn a_workspace_s_edits_are_its_paths() {
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("a.txt", "a\n"),
            Put::File("gone.txt", "g\n"),
            Put::File("kept.txt", "k\n"),
        ],
    );
    f.create("w", "main", &[]);
    let dir = f.beside("w");
    write(&dir, "committed.txt", "c\n");
    let mut commit = WsCommitReq::new("w".to_owned(), "mine".to_owned());
    commit.status_after = StatusAfter::None;
    let head = swarmops_ws::commit(&f.ctx(), commit).unwrap().changeset;

    write(&dir, "a.txt", "edited\n");
    write(&dir, "sub/new.txt", "new\n");
    remove(&dir, "gone.txt");
    let locked = lock::acquire(&dir).unwrap();
    let read = edits(&f.ctx(), &w(), 10).unwrap();
    drop(locked);
    assert_eq!(
        (read.base.as_str(), read.head.as_str()),
        (base.as_str(), head.as_str())
    );
    assert_eq!(read.paths, ["a.txt", "gone.txt", "sub/new.txt"]);
    assert!(!read.truncated && !read.outside);

    // More edits than the limit: the first of them, and `truncated`.
    let cut = edits(&f.ctx(), &w(), 2).unwrap();
    assert_eq!(cut.paths, ["a.txt", "gone.txt"]);
    assert!(cut.truncated);
    let exact = edits(&f.ctx(), &w(), 3).unwrap();
    assert_eq!((exact.paths.len(), exact.truncated), (3, false));
}

/// A rebasing workspace is refused as a run of its checks refuses it.
#[test]
fn a_rebasing_workspace_is_refused() {
    // 1. A workspace whose rebase conflicts, stopped on the conflict.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a.txt", "1\n2\n3\n")]);
    f.create("w", &base, &[]);
    write(&f.beside("w"), "a.txt", "1\nmine\n3\n");
    let mut commit = WsCommitReq::new("w".to_owned(), "mine".to_owned());
    commit.status_after = StatusAfter::None;
    swarmops_ws::commit(&f.ctx(), commit).unwrap();
    f.commit(Some(&base), &[Put::File("a.txt", "1\ntheirs\n3\n")]);
    let mut rebase = WsRebaseReq::new("w".to_owned());
    rebase.action = WsRebaseAction::Start;
    let stopped = swarmops_ws::rebase(&f.ctx(), rebase).unwrap_err();
    assert_eq!(stopped.code, "E_CONFLICT", "{stopped:?}");

    // 2. Refused, with the code and the hint of `check run`'s refusal.
    let refused = edits(&f.ctx(), &w(), 10).unwrap_err();
    assert_eq!(refused.code, "E_UNRESOLVED_CONFLICTS", "{refused:?}");
    assert!(refused.hint.unwrap().contains("ws rebase --continue"));
    let held = swarmops_ws::check::hold_for_run(&f.ctx(), &w()).unwrap_err();
    assert_eq!(held.code, refused.code);
    assert_eq!(held.message, refused.message);
}

/// Files that stand for another changeset than the head are refused, `materialized`, as a run
/// refuses them.
#[test]
fn files_of_another_changeset_are_refused() {
    let f = Fixture::new();
    let c1 = f.commit(None, &[Put::File("a.txt", "a\n")]);
    f.commit(Some(&c1), &[Put::File("a.txt", "b\n")]);
    f.create("w", "main", &[]);
    let file = f.beside("w").join(".swarmws/workspace.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    state["materialized"] = c1.as_str().into();
    std::fs::write(&file, state.to_string()).unwrap();
    let refused = edits(&f.ctx(), &w(), 10).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert_eq!(refused.details["reason"], "materialized");
}

/// A sparse workspace that holds an entry off its prefixes says so: what is there is in no
/// list of its edits, so its changed paths are not all known.
#[test]
fn an_entry_off_a_sparse_workspace_s_prefixes_is_told() {
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a.txt", "a\n"), Put::File("docs/d.md", "d\n")],
    );
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/x", "x\n");
    let inside = edits(&f.ctx(), &w(), 10).unwrap();
    assert_eq!(inside.paths, ["src/x"]);
    assert!(!inside.outside, "an edit on the prefixes alone");

    write(&dir, "README", "readme\n");
    let off = edits(&f.ctx(), &w(), 10).unwrap();
    assert_eq!(off.paths, ["src/x"]);
    assert!(off.outside);
}
