//! A workspace held for a run of its checks (`check::hold_for_run`): clean, settled and locked,
//! its files the tree of the changeset they stand for.

use swarmops_api::{ShowReq, StoreOps};
use swarmops_ws::check::hold_for_run;
use swarmops_ws::lock;
use swarmops_ws::workspace::Locate;

use crate::fixture::{Fixture, Put, write};

/// A clean workspace is held: its changeset and root tree named, its lock taken until the hold
/// is dropped; nothing is written to the store.
#[test]
fn a_clean_workspace_is_held_under_its_lock() {
    // 1. A workspace on main.
    let f = Fixture::new();
    let c1 = f.commit(None, &[Put::File("a.txt", "a\n")]);
    f.create("w", "main", &[]);
    let ctx = f.ctx();
    let ops_before = f.last_op();

    // 2. Held, by name and by its directory: the changeset its files stand for and its tree.
    let held = hold_for_run(&ctx, &Locate::Name("w".to_owned())).unwrap();
    let mut show = ShowReq::new(c1.as_str());
    show.max_changes = 1;
    let root = f.ops.show(show).unwrap().root;
    assert_eq!(
        (held.changeset.as_str(), held.tree.as_str()),
        (c1.as_str(), root.as_str())
    );
    assert_eq!((held.name.as_str(), &held.dir), ("w", &f.beside("w")));

    // 3. The lock is held: another taker waits it out, then gets it once the hold is dropped.
    let locked = lock::acquire(&held.dir).unwrap_err();
    assert_eq!(locked.code, "E_WS_LOCKED", "{locked:?}");
    drop(held);
    drop(lock::acquire(&f.beside("w")).unwrap());
    let again = hold_for_run(&ctx, &Locate::Path(f.beside("w"))).unwrap();
    assert_eq!(again.changeset, c1);
    assert_eq!(
        f.last_op(),
        ops_before,
        "a hold writes nothing to the store"
    );
}

/// An edit on disk is `E_WS_DIRTY`, naming it, with a hint at `ws commit` and at the directory
/// form; nothing is held.
#[test]
fn an_edited_workspace_is_not_held() {
    let f = Fixture::new();
    f.commit(None, &[Put::File("a.txt", "a\n")]);
    f.create("w", "main", &[]);
    write(&f.beside("w"), "a.txt", "edited\n");
    write(&f.beside("w"), "new.txt", "new\n");
    let refused = hold_for_run(&f.ctx(), &Locate::Name("w".to_owned())).unwrap_err();
    assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
    assert_eq!(
        refused.details["paths"],
        serde_json::json!(["a.txt", "new.txt"])
    );
    let hint = refused.hint.unwrap();
    assert!(
        hint.contains("ws commit") && hint.contains("check run --dir"),
        "{hint}"
    );
    drop(lock::acquire(&f.beside("w")).unwrap());
}
