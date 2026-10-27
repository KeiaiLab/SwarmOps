//! FORMAT 2 through the binary (the M6 design §10, acceptance): a FORMAT 2 store holds a
//! free-form branch and a tag, a tag never moves, a FORMAT 1 store asks for the upgrade a
//! FORMAT 2 name needs, a land targets a branch and never a tag, and gc and repair find
//! nothing on a clean store. Golden snapshots pin the envelopes of the three codes M6 raises
//! first: `E_FORMAT_REQUIRED`, `E_TAG_IMMUTABLE`, `E_MAINTENANCE_BUSY`, without their `meta`
//! (the temp dir, times and resolved ids change from run to run).

use serde_json::{Map, Value};
use swarmops_store::MaintLease;

use crate::support::{Run, Store, agent, run};

/// The branch the acceptance makes.
const BRANCH: &str = "feature/login";

/// The tag the acceptance makes.
const TAG: &str = "tags/v1";

/// The operations that take the maintenance lease.
const MAINTENANCE: [&str; 3] = ["gc", "repair", "upgrade"];

/// The design §10 acceptance, in its order: `init` makes FORMAT 2; a branch and a tag are
/// made and listed with their kinds; the tag's move is `E_TAG_IMMUTABLE`; on a FORMAT 1
/// store the branch is `E_FORMAT_REQUIRED` until `upgrade`; a land on the branch lands and
/// one on the tag is `E_REF_NAME_INVALID`; `gc --dry-run` and `repair --dry-run` are clean.
#[test]
fn a_format_2_store_holds_a_branch_and_a_tag() {
    let me = agent(1);

    // 1. init without --format: FORMAT 2.
    let store = Store::empty();
    let made = run(&["init", store.path()], &[]).ok();
    assert_eq!(made["store_version"], 2, "{made}");

    // 2. A branch and a tag on one changeset, listed with their kinds.
    let c1 = store.commit(&me, "a.txt", "a\n", None);
    for name in [BRANCH, TAG] {
        let set = store.run_as(&me, &["ref", "set", name, &c1, "--create"], &[]);
        assert_eq!(set.ok()["name"], name, "{set}");
    }
    let listed = store.read(&["ref", "list"]).ok();
    let kinds: Vec<(&str, &str)> = (listed["refs"].as_array().expect("a ref list").iter())
        .map(|r| (r["name"].as_str().unwrap(), r["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(kinds, [(BRANCH, "branch"), (TAG, "tag")], "{listed}");

    // 3. The tag never moves.
    let c2 = store.commit(&me, "b.txt", "b\n", Some(&c1));
    let moved = store.run_as(&me, &["ref", "set", TAG, &c2, "--expect", &c1], &[]);
    moved.err("E_TAG_IMMUTABLE");
    insta::assert_json_snapshot!("tag_immutable", golden(&moved));

    // 4. A FORMAT 1 store refuses the branch's name until it is upgraded.
    let old = Store::empty();
    let made = run(&["init", "--format", "1", old.path()], &[]).ok();
    assert_eq!(made["store_version"], 1, "{made}");
    let o1 = old.commit(&me, "a.txt", "a\n", None);
    let branch = ["ref", "set", BRANCH, &o1, "--create"];
    let refused = old.run_as(&me, &branch, &[]);
    refused.err("E_FORMAT_REQUIRED");
    insta::assert_json_snapshot!("format_required", golden(&refused));
    let upgraded = old.run_as(&me, &["upgrade"], &[]).ok();
    assert_eq!(
        (&upgraded["from"], &upgraded["to"], &upgraded["upgraded"]),
        (&1.into(), &2.into(), &true.into()),
        "{upgraded}"
    );
    assert_eq!(old.run_as(&me, &branch, &[]).ok()["name"], BRANCH);

    // 5. A land onto the branch lands; onto the tag it is refused for what the name is.
    let landed = store
        .run_as(&me, &["land", &c2, "--target", BRANCH], &[])
        .ok();
    assert_eq!(
        (&landed["status"], &landed["target"]),
        (&"landed".into(), &BRANCH.into()),
        "{landed}"
    );
    let on_tag = store.run_as(&me, &["land", &c2, "--target", TAG], &[]);
    assert_eq!(on_tag.err("E_REF_NAME_INVALID")["details"]["reason"], "tag");

    // 6. gc and repair find nothing to do on the clean store.
    let gc = store.run_as(&me, &["gc", "--dry-run"], &[]).ok();
    assert_eq!(gc["removed"], Value::Array(vec![]), "{gc}");
    assert_eq!(gc["orphan_objects"], 0, "{gc}");
    let repair = store.run_as(&me, &["repair", "--dry-run"], &[]).ok();
    assert_eq!(repair["actions"], Value::Array(vec![]), "{repair}");
    assert_eq!(repair["after"]["found"], 0, "{repair}");
    store.assert_clean("after the acceptance");
}

/// While another process holds the maintenance lease, gc, repair and upgrade each fail at once
/// with `E_MAINTENANCE_BUSY` (exit 4, retryable), and run once it is released. The FORMAT 1
/// store makes `upgrade` one that has work to do.
#[test]
fn a_held_maintenance_lease_is_busy_through_the_binary() {
    let me = agent(1);
    let store = Store::empty();
    run(&["init", "--format", "1", store.path()], &[]).ok();

    // 1. The lease held here: each of the three refused, its envelope pinned.
    let held = MaintLease::try_acquire(&store.layout()).expect("take the lease");
    let mut busy = Map::new();
    for op in MAINTENANCE {
        let refused = store.run_as(&me, &[op], &[]);
        assert_eq!(refused.exit(), 4, "{refused}");
        assert_eq!(refused.err("E_MAINTENANCE_BUSY")["retryable"], true);
        busy.insert(op.to_owned(), golden(&refused));
    }
    insta::assert_json_snapshot!("maintenance_busy", busy);

    // 2. Released: each runs.
    drop(held);
    for op in MAINTENANCE {
        store.run_as(&me, &[op], &[]).ok();
    }
}

/// `run`'s envelope without its `meta`: what a golden snapshot pins.
fn golden(run: &Run) -> Value {
    let mut envelope = run.envelope();
    let meta = envelope.as_object_mut().expect("an object").remove("meta");
    assert!(meta.is_some(), "{run}");
    envelope
}
