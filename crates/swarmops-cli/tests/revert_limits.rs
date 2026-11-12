//! `revert`'s refusals, pinned above the repository layer (the M18 design §4.2): a reason that is
//! not 1 to 4,096 bytes is `E_INVALID_ARG` with `details.reason_bytes`, checked before anything
//! is resolved; a range of more than 256 changesets is `E_TOO_LARGE` with
//! `details.max_changesets`; a changeset off the target is `E_INVALID_ARG` (`not_on_target`).
//! Each is asserted through the operations layer and through the binary, on the same store.
//! Beside them, what a revert leaves out (§4.1): a path the range's last changeset holds as the
//! range's start does gets no change, and a range of such paths alone is `E_NOTHING_TO_LAND`.

use std::sync::Arc;

use camino::Utf8Path;
use serde_json::{Value, json};
use swarmops_api::{ErrorCode, Identity, OpError, RevertReq, StoreOps};
use swarmops_ops::LocalOps;
use swarmops_repo::{Advance, Change, LocalRepo, MAX_REVERT_CHANGESETS, MAX_REVERT_REASON_BYTES};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::repo::{put, request};

use crate::support::Store;

/// Who makes every change and every revert.
const AGENT: &str = "agent/revert-limits";

/// The ref every revert lands on.
const MAIN: &str = "main";

/// The path a range changes and changes back.
const SAME: &str = "same.txt";

/// The path a range changes.
const MOVED: &str = "moved.txt";

/// What both paths hold before a range.
const OLD: &[u8] = b"old\n";

/// What a range puts at a path.
const NEW: &[u8] = b"new\n";

/// `details.reason` of a revert whose inverse holds no change.
const NO_CHANGES: &str = "no_changes";

/// A store whose `main` is a chain of changesets, beside one changeset off it, and the store's
/// operations opened in this process.
struct Fixture {
    store: Store,
    ops: LocalOps,
    /// `main`'s first-parent chain, oldest first.
    main: Vec<String>,
    /// A changeset on `main`'s head that `main` does not name.
    off: String,
}

impl Fixture {
    /// A fresh store with `n` changesets on `main`, each a file of its own.
    fn new(n: usize) -> Fixture {
        let steps = (0..n).map(|i| vec![put(&format!("f{i}.txt"), b"x\n")]);
        Fixture::chain(steps)
    }

    /// A fresh store whose `main` is one changeset per element of `steps`, written unsynced:
    /// the setup's flushes are not what the tests read, and a long chain would pay for hundreds
    /// of them.
    fn chain(steps: impl IntoIterator<Item = Vec<Change>>) -> Fixture {
        let store = Store::init();
        let root = Utf8Path::new(store.path());

        // 1. The chain, then the changeset off it.
        let repo = LocalRepo::open_with_fs(root, Arc::new(NoSyncFs)).expect("open the store");
        let mut chain = Vec::new();
        for changes in steps {
            let parents: Vec<_> = chain.last().copied().into_iter().collect();
            let mut req = request(&parents, changes, "f");
            req.advance = Some(Advance::new(MAIN, AGENT));
            chain.push(repo.commit(req).expect("commit").id);
        }
        let head = *chain.last().expect("at least one changeset");
        let off = repo.commit(request(&[head], vec![put("off.txt", b"off\n")], "off"));
        let off = off.expect("commit").id.to_string();
        drop(repo);

        // 2. The operations, on what the chain wrote.
        let ops = LocalOps::open(root).expect("open the store");
        let main = chain.iter().map(ToString::to_string).collect();
        Fixture {
            store,
            ops,
            main,
            off,
        }
    }

    /// `main`'s head.
    fn head(&self) -> &str {
        self.main.last().expect("at least one changeset")
    }

    /// `req` through the operations layer: its error.
    fn ops_refusal(&self, req: RevertReq) -> OpError {
        self.ops.revert(&agent(), req).expect_err("a refusal")
    }

    /// `revert` with `args` through the binary, refused with `code`: its exit code and error.
    fn cli_refusal(&self, args: &[&str], code: &str) -> (i32, Value) {
        let args = [&["revert"], args].concat();
        let run = self.store.run_as(AGENT, &args, &[]);
        (run.exit(), run.err(code))
    }

    /// What a refusal must leave as it was: `main`'s head and the store's changeset count.
    fn state(&self) -> (Value, Value) {
        let resolved = self.store.read(&["resolve", MAIN]).ok();
        (
            resolved["id"].clone(),
            self.store.info()["changesets"].clone(),
        )
    }
}

/// The identity of the operations.
fn agent() -> Identity {
    Identity::parse(AGENT).expect("a valid identity")
}

/// A reason of `n` bytes.
fn reason_of(n: usize) -> String {
    "r".repeat(n)
}

/// A reason of 0 or 4,097 bytes is `E_INVALID_ARG` (exit 2) naming its length in
/// `details.reason_bytes`, through the operations layer and the binary alike, and writes
/// nothing; one of 4,096 bytes lands.
#[test]
fn revert_reason_over_the_limit_is_refused() {
    let f = Fixture::new(2);
    let head = f.head();
    let before = f.state();

    // 1. Too short and too long: refused in both layers, nothing written.
    let invalid = ErrorCode::InvalidArg;
    for bytes in [0, MAX_REVERT_REASON_BYTES + 1] {
        let reason = reason_of(bytes);
        let refused = f.ops_refusal(RevertReq::new(head, reason.as_str()));
        assert_eq!(
            refused.code,
            invalid.as_str(),
            "ops, {bytes} bytes: {refused:?}"
        );
        assert_eq!(
            refused.details["reason_bytes"],
            json!(bytes),
            "ops, {bytes} bytes"
        );

        let args = [head, "--reason", &reason];
        let (exit, error) = f.cli_refusal(&args, invalid.as_str());
        assert_eq!(exit, invalid.exit(), "cli, {bytes} bytes");
        assert_eq!(
            error["details"]["reason_bytes"],
            json!(bytes),
            "cli, {bytes} bytes"
        );
        assert_eq!(f.state(), before, "{bytes} bytes: nothing written");
    }

    // 2. At the limit: the revert lands.
    let reason = reason_of(MAX_REVERT_REASON_BYTES);
    let args = ["revert", head, "--reason", &reason];
    let landed = f.store.run_as(AGENT, &args, &[]).ok();
    assert_eq!(landed["reverted"], json!([head]), "{landed}");
}

/// A range one past [`MAX_REVERT_CHANGESETS`] is `E_TOO_LARGE` (exit 1) with the bound in
/// `details.max_changesets` and the range's last changeset in `details.changeset`; a changeset
/// off `main` is `E_INVALID_ARG` (exit 2) with `details.reason` `not_on_target`. Both layers
/// answer the same, and write nothing.
#[test]
fn revert_maps_to_its_codes_in_ops_and_cli() {
    // `main`: the range's exclusive start, then one changeset past the bound.
    let f = Fixture::new(MAX_REVERT_CHANGESETS + 2);
    let (since, head) = (f.main[0].as_str(), f.head());
    let before = f.state();

    // 1. The range (since, head], one past the bound.
    let too_large = ErrorCode::TooLarge;
    let mut req = RevertReq::new(head, "too long");
    req.since = Some(since.into());
    let refused = f.ops_refusal(req);
    assert_eq!(refused.code, too_large.as_str(), "{refused:?}");
    assert_eq!(refused.exit, too_large.exit());
    assert_eq!(
        refused.details["max_changesets"],
        json!(MAX_REVERT_CHANGESETS)
    );
    assert_eq!(refused.details["changeset"], json!(head));

    let args = [head, "--since", since, "--reason", "too long"];
    let (exit, error) = f.cli_refusal(&args, too_large.as_str());
    assert_eq!(exit, too_large.exit());
    assert_eq!(
        error["details"]["max_changesets"],
        json!(MAX_REVERT_CHANGESETS)
    );
    assert_eq!(error["details"]["changeset"], json!(head));

    // 2. A changeset off main.
    let invalid = ErrorCode::InvalidArg;
    let refused = f.ops_refusal(RevertReq::new(f.off.as_str(), "off"));
    assert_eq!(refused.code, invalid.as_str(), "{refused:?}");
    assert_eq!(refused.exit, invalid.exit());
    assert_eq!(refused.details["reason"], "not_on_target");

    let args = [f.off.as_str(), "--reason", "off"];
    let (exit, error) = f.cli_refusal(&args, invalid.as_str());
    assert_eq!(exit, invalid.exit());
    assert_eq!(error["details"]["reason"], "not_on_target");
    assert_eq!(f.state(), before, "nothing written");
}

/// A path the range changed and changed back gets no change in the inverse (the M18 design
/// §4.1: a leaf at the range's start is put "unless `X` holds the same"). Beside a path the
/// range did change, the revert's commit names that path alone; alone in its range, the revert
/// is `E_NOTHING_TO_LAND` (exit 0, `details.reason` `no_changes`) in both layers, and writes
/// nothing.
#[test]
fn revert_skips_a_leaf_already_at_its_old_value() {
    // `main`: `same.txt` and `moved.txt`; `same.txt` changed; `same.txt` back, `moved.txt`
    // changed; `same.txt` changed; `same.txt` back.
    let f = Fixture::chain([
        vec![put(SAME, OLD), put(MOVED, OLD)],
        vec![put(SAME, NEW)],
        vec![put(SAME, OLD), put(MOVED, NEW)],
        vec![put(SAME, NEW)],
        vec![put(SAME, OLD)],
    ]);
    let (since, head) = (f.main[2].as_str(), f.head());

    // 1. A range of the returned path alone: nothing to land, nothing written.
    let before = f.state();
    let nothing = ErrorCode::NothingToLand;
    let mut req = RevertReq::new(head, "back already");
    req.since = Some(since.into());
    let refused = f.ops_refusal(req);
    assert_eq!(refused.code, nothing.as_str(), "{refused:?}");
    assert_eq!(refused.details["reason"], NO_CHANGES);

    let args = [head, "--since", since, "--reason", "back already"];
    let (exit, error) = f.cli_refusal(&args, nothing.as_str());
    assert_eq!(exit, nothing.exit());
    assert_eq!(error["details"]["reason"], NO_CHANGES);
    assert_eq!(f.state(), before, "nothing written");

    // 2. The range of both paths, to the changeset that returned the first: the inverse sets
    //    back the path that moved, and does not name the one that returned.
    let (since, to) = (f.main[0].as_str(), f.main[2].as_str());
    let args = ["revert", to, "--since", since, "--reason", "moved"];
    let landed = f.store.run_as(AGENT, &args, &[]).ok();
    assert_eq!(
        landed["reverted"],
        json!([f.main[1], f.main[2]]),
        "{landed}"
    );
    assert_eq!(landed["land"]["files_changed"], 1, "{landed}");

    let inverse = landed["land"]["squashed_from"][0].as_str().expect("an id");
    let shown = f.store.read(&["show", inverse]).ok();
    let paths: Vec<&Value> = (shown["changes"].as_array().expect("its changes").iter())
        .map(|entry| &entry["path"])
        .collect();
    assert_eq!(paths, [MOVED], "{shown}");
}
