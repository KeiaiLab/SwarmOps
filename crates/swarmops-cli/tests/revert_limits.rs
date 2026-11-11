//! `revert`'s refusals, pinned above the repository layer (the M18 design §4.2): a reason that is
//! not 1 to 4,096 bytes is `E_INVALID_ARG` with `details.reason_bytes`, checked before anything
//! is resolved; a range of more than 256 changesets is `E_TOO_LARGE` with
//! `details.max_changesets`; a changeset off the target is `E_INVALID_ARG` (`not_on_target`).
//! Each is asserted through the operations layer and through the binary, on the same store.

use std::sync::Arc;

use camino::Utf8Path;
use serde_json::{Value, json};
use swarmops_api::{ErrorCode, Identity, OpError, RevertReq, StoreOps};
use swarmops_ops::LocalOps;
use swarmops_repo::{Advance, LocalRepo, MAX_REVERT_CHANGESETS, MAX_REVERT_REASON_BYTES};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::repo::{put, request};

use crate::support::Store;

/// Who makes every change and every revert.
const AGENT: &str = "agent/revert-limits";

/// The ref every revert lands on.
const MAIN: &str = "main";

/// A store whose `main` is a chain of changesets, beside one changeset off it, and the store's
/// operations opened in this process.
struct Fixture {
    store: Store,
    ops: LocalOps,
    /// `main`'s first-parent chain, oldest first, each changeset a file of its own.
    main: Vec<String>,
    /// A changeset on `main`'s head that `main` does not name.
    off: String,
}

impl Fixture {
    /// A fresh store with `n` changesets on `main`, written unsynced: the setup's flushes are
    /// not what the tests read, and a long chain would pay for hundreds of them.
    fn new(n: usize) -> Fixture {
        let store = Store::init();
        let root = Utf8Path::new(store.path());

        // 1. The chain, then the changeset off it.
        let repo = LocalRepo::open_with_fs(root, Arc::new(NoSyncFs)).expect("open the store");
        let mut chain = Vec::with_capacity(n);
        for i in 0..n {
            let parents: Vec<_> = chain.last().copied().into_iter().collect();
            let mut req = request(&parents, vec![put(&format!("f{i}.txt"), b"x\n")], "f");
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
