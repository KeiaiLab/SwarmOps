//! Every move is reversible (A59, the M18 design §9): `op undo` takes back each kind of move a
//! release ref makes — a `ref set`, a land, a revert, a `ref txn` — and an undo of that undo
//! makes it again; once the ref has moved on, the undo is `E_CAS_MISMATCH` and writes nothing. A
//! sync moves `main` alone, which `op undo` never takes back, so it is not a kind here.

use serde_json::{Map, Value, json};

use crate::support::Store;

/// Who makes every move.
const AGENT: &str = "agent/undo-kinds";

/// The release ref every kind moves.
const RELEASE: &str = "release/x";

/// Every ref of `store`, by name, with its target.
fn refs(store: &Store) -> Value {
    let listed = store.read(&["ref", "list"]).ok();
    let refs = (listed["refs"].as_array().expect("a ref list").iter())
        .map(|r| (r["name"].as_str().unwrap().to_owned(), r["target"].clone()))
        .collect::<Map<_, _>>();
    Value::Object(refs)
}

/// The refs of `store` and how many operations its log holds: what a refusal must leave as it
/// was.
fn state(store: &Store) -> (Value, usize) {
    let log = store.read(&["op", "log", "-n", "1000"]).ok();
    (
        refs(store),
        log["ops"].as_array().expect("an op list").len(),
    )
}

/// The id of the newest operation of `store`, as `op undo` takes it.
fn newest_op(store: &Store) -> String {
    let log = store.read(&["op", "log", "-n", "1"]).ok();
    log["ops"][0]["id"].as_str().expect("an op id").to_owned()
}

/// What `release/x` names.
fn release(store: &Store) -> String {
    refs(store)[RELEASE]
        .as_str()
        .expect("release/x exists")
        .to_owned()
}

/// Moves `release/x` from `from` to a new changeset on it: a later move, which any undo of an
/// operation before it meets.
fn move_on(store: &Store, from: &str, content: &str) -> String {
    let next = store.commit(AGENT, "later.txt", content, Some(from));
    let set = ["ref", "set", RELEASE, &next, "--expect", from];
    store.run_as(AGENT, &set, &[]).ok();
    next
}

/// Undoes operation `op`, which took the refs from `before` to `after`: the undo restores
/// `before`, an undo of the undo restores `after`, and once `release/x` has moved on, the undo of
/// `op` again is `E_CAS_MISMATCH` (exit 4), with refs and op log as they were. `what` names the
/// kind in messages. The new head of `release/x`.
#[track_caller]
fn undo_cycle(store: &Store, what: &str, op: &str, before: &Value, after: &Value) -> String {
    // 1. Undone: every ref as before the operation.
    let undone = store.run_as(AGENT, &["op", "undo", op], &[]).ok();
    assert_eq!(undone["already_applied"], false, "{what}: {undone}");
    assert_eq!(&refs(store), before, "{what}: the undo");

    // 2. The undo undone: every ref as the operation left it.
    let again = undone["id"].as_str().expect("the undo's id").to_owned();
    let redone = store.run_as(AGENT, &["op", "undo", &again], &[]).ok();
    assert_eq!(redone["already_applied"], false, "{what}: {redone}");
    assert_eq!(&refs(store), after, "{what}: the undo of the undo");

    // 3. A later move, then the undo again: refused, nothing written.
    let head = move_on(store, &release(store), &format!("after {what}\n"));
    let moved = state(store);
    let blocked = store.run_as(AGENT, &["op", "undo", op], &[]);
    assert_eq!(blocked.exit(), 4, "{what}: {blocked}");
    let error = blocked.err("E_CAS_MISMATCH");
    assert_eq!(error["details"]["name"], RELEASE, "{what}: {error}");
    assert_eq!(error["details"]["actual"], head.as_str(), "{what}: {error}");
    assert_eq!(state(store), moved, "{what}: the refused undo wrote");
    head
}

/// A59 per kind, each on `release/x`: a `ref set` move, a land, a revert and a `ref txn` are each
/// undone, the undo undone, and the undo refused once the ref moved on, writing nothing.
#[test]
fn undo_restores_each_kind_and_undo_again() {
    // 1. main and release/x at c0.
    let store = Store::init();
    let c0 = store.commit(AGENT, "a.txt", "a\n", None);
    for (name, create) in [("main", "--create"), (RELEASE, "--create")] {
        store
            .run_as(AGENT, &["ref", "set", name, &c0, create], &[])
            .ok();
    }

    // 2. A `ref set` move: release/x from c0 to c1.
    let c1 = store.commit(AGENT, "a.txt", "b\n", Some(&c0));
    let before = refs(&store);
    let set = ["ref", "set", RELEASE, &c1, "--expect", &c0];
    store.run_as(AGENT, &set, &[]).ok();
    let after = refs(&store);
    undo_cycle(&store, "ref set", &newest_op(&store), &before, &after);

    // 3. A land onto release/x of a change made on c0.
    let change = store.commit(AGENT, "b.txt", "b\n", Some(&c0));
    let before = refs(&store);
    let landed = store.run_as(AGENT, &["land", &change, "--target", RELEASE], &[]);
    let landed = landed.ok()["landed"].as_str().unwrap().to_owned();
    let after = refs(&store);
    assert_eq!(after[RELEASE], landed.as_str());
    let op = newest_op(&store);
    let head = undo_cycle(&store, "land", &op, &before, &after);

    // 4. A revert onto release/x of that land.
    let before = refs(&store);
    let revert = [
        "revert",
        &landed,
        "--onto",
        RELEASE,
        "--reason",
        "undo kinds",
    ];
    let reverted = store.run_as(AGENT, &revert, &[]).ok();
    assert_eq!(reverted["land"]["rebased_onto"], head.as_str());
    let after = refs(&store);
    let op = newest_op(&store);
    let head = undo_cycle(&store, "revert", &op, &before, &after);

    // 5. A `ref txn`: release/x back to c0 and release/y made, in one operation.
    let before = refs(&store);
    let x = format!("{RELEASE}={head}:{c0}");
    let y = format!("release/y=new:{c1}");
    let txn = ["ref", "txn", "--move", &x, "--move", &y];
    store.run_as(AGENT, &txn, &[]).ok();
    let after = refs(&store);
    assert_eq!(
        (&after[RELEASE], &after["release/y"]),
        (&json!(c0), &json!(c1))
    );
    let op = newest_op(&store);
    undo_cycle(&store, "ref txn", &op, &before, &after);

    // 6. The store, after every take-back.
    store.assert_clean("after every kind's undo");
}
