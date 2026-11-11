//! The hold record (the M21 design §10.4): `<store>/workspaces/hold/<name>.json`, written by
//! each mutation that succeeded, unsynced, under the workspace's lock; removed by `ws delete`.
//! These tests read the record's JSON by its path.

use std::io;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use swarmops_api::{
    Side, StatusAfter, WsCommitReq, WsDeleteReq, WsDiffReq, WsDiscardReq, WsInfoReq, WsLandReq,
    WsListReq, WsMaterializeReq, WsRebaseAction, WsRebaseReq, WsResolveReq, WsSnapshotReq,
    WsStatusReq,
};
use swarmops_ws::hostfs::HostMeta;
use swarmops_ws::registry::Registry;
use swarmops_ws::state;
use swarmops_ws::{HostFs, RealFs, RealHost};

use crate::fixture::{AGENT, Fixture, Put, write};

/// The lease of a hold, in milliseconds.
const LEASE_MS: u64 = swarmops_ws::WS_LEASE_S * 1_000;

/// The environment variable that makes a test the child its parent runs.
const CHILD: &str = "SWARMOPS_TEST_HOLD_CHILD";

/// The hold record of workspace `name` of the store at `root`.
fn hold_path(root: &Utf8Path, name: &str) -> Utf8PathBuf {
    root.join("workspaces/hold").join(format!("{name}.json"))
}

/// The hold record of workspace `name`, parsed.
fn hold(f: &Fixture, name: &str) -> Value {
    let bytes = std::fs::read(hold_path(&f.root, name))
        .unwrap_or_else(|e| panic!("no hold record of {name}: {e}"));
    serde_json::from_slice(&bytes).unwrap()
}

/// The time now, in Unix milliseconds.
fn now_ms() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(since.as_millis()).unwrap()
}

/// Commits every edit of workspace `name`; the new head.
fn commit_all(f: &Fixture, name: &str, message: &str) -> String {
    let mut req = WsCommitReq::new(name.to_owned(), message.to_owned());
    req.status_after = StatusAfter::None;
    swarmops_ws::commit(&f.ctx(), req).unwrap().changeset
}

/// Workspace `name` on a base holding `src/a.txt`, with `commits` commits of its own.
fn worked(f: &Fixture, name: &str, commits: u32) -> String {
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create(name, &base, &[]);
    for i in 0..commits {
        write(&f.beside(name), "src/a.txt", &format!("mine {i}\n"));
        commit_all(f, name, &format!("mine {i}"));
    }
    base
}

/// The request of `ws rebase` of workspace `name` onto `main`, doing `action`.
fn rebase_of(name: &str, action: WsRebaseAction) -> WsRebaseReq {
    let mut out = WsRebaseReq::new(name.to_owned());
    out.action = action;
    out.status_after = StatusAfter::None;
    out
}

/// The request of `ws land` of workspace `name`.
fn land_of(name: &str) -> WsLandReq {
    let mut out = WsLandReq::new(name.to_owned());
    out.status_after = StatusAfter::None;
    out
}

/// A host that refuses every write below the hold directory, and forwards all else.
struct NoHoldHost(RealHost);

impl NoHoldHost {
    /// Whether a write names the hold directory: `<store>/workspaces`, then `hold/…`.
    fn refuses(root: &Utf8Path, rel: &str) -> bool {
        root.file_name() == Some("workspaces") && rel.starts_with("hold/")
    }
}

impl HostFs for NoHoldHost {
    fn write_file(&self, root: &Utf8Path, rel: &str, bytes: &[u8]) -> io::Result<()> {
        if NoHoldHost::refuses(root, rel) {
            return Err(io::Error::other("refused by the test"));
        }
        self.0.write_file(root, rel, bytes)
    }

    fn symlink(&self, root: &Utf8Path, rel: &str, target: &str) -> io::Result<()> {
        self.0.symlink(root, rel, target)
    }

    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
        self.0.symlink_metadata(path)
    }

    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
        self.0.read_file(path)
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        self.0.read_dir(path)
    }

    fn remove_file(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        self.0.remove_file(root, rel)
    }

    fn remove_dir_if_empty(&self, root: &Utf8Path, rel: &str) -> io::Result<bool> {
        self.0.remove_dir_if_empty(root, rel)
    }

    fn create_dir_all(&self, root: &Utf8Path, rel: &str) -> io::Result<()> {
        self.0.create_dir_all(root, rel)
    }

    fn rename(&self, root: &Utf8Path, from: &str, to: &str) -> io::Result<()> {
        self.0.rename(root, from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        self.0.read_link(path)
    }

    fn set_mode(&self, root: &Utf8Path, rel: &str, mode: u32) -> io::Result<()> {
        self.0.set_mode(root, rel, mode)
    }
}

/// A commit records its holder, the time, the lease and what the workspace stands on; the
/// workspace's one commit is unlanded.
#[test]
fn a_commit_writes_the_holder_and_the_time() {
    // 1. A workspace with one commit, timed.
    let f = Fixture::new();
    let before = now_ms();
    worked(&f, "w", 1);
    let after = now_ms();

    // 2. The record: format, name, holder, the operation and its time, the lease.
    let held = hold(&f, "w");
    assert_eq!(held["format"], swarmops_ws::HOLD_FORMAT, "{held}");
    assert_eq!(held["name"], "w");
    assert_eq!(held["holder"], AGENT);
    assert_eq!(held["last_op"], "commit");
    let at = held["last_op_at"].as_u64().unwrap();
    assert!((before..=after).contains(&at), "{held}");
    assert_eq!(held["lease_until"].as_u64(), Some(at + LEASE_MS));
    let since = held["since"].as_u64().unwrap();
    assert!((before..=at).contains(&since), "{held}");

    // 3. What the workspace stands on, as workspace.json says; one commit unlanded.
    let file = state::read(&RealFs::new(), &f.beside("w")).unwrap();
    assert_eq!(held["target"], file.target.as_str());
    assert_eq!(held["base"], file.base.as_str());
    assert_eq!(held["head"], file.head.as_str());
    assert_eq!(held["base_op"].as_u64(), file.base_op);
    assert_eq!(held["unlanded"], 1);
}

/// A land leaves nothing unlanded and moves the base to what it landed.
#[test]
fn a_land_resets_unlanded_and_moves_the_base() {
    // 1. Two commits unlanded.
    let f = Fixture::new();
    worked(&f, "w", 2);
    assert_eq!(hold(&f, "w")["unlanded"], 2);

    // 2. Landed: none unlanded, the base and the head what landed.
    let done = swarmops_ws::land(&f.ctx(), land_of("w")).unwrap();
    let held = hold(&f, "w");
    assert_eq!(held["last_op"], "land");
    assert_eq!(held["unlanded"], 0);
    assert_eq!(held["base"], done.land.landed.as_str());
    assert_eq!(held["head"], done.land.landed.as_str());
}

/// A rebase onto a moved target keeps the count of commits not landed.
#[test]
fn a_rebase_keeps_unlanded() {
    // 1. Two commits unlanded; main moves.
    let f = Fixture::new();
    let base = worked(&f, "w", 2);
    let main = f.commit(Some(&base), &[Put::File("src/b.txt", "b\n")]);

    // 2. Rebased: the base is main's head, the count kept.
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let held = hold(&f, "w");
    assert_eq!(held["last_op"], "rebase");
    assert_eq!(held["unlanded"], 2);
    assert_eq!(held["base"], main.as_str());
    assert_eq!(held["head"], done.changeset.as_str());
}

/// `status`, `diff`, `info` and `list` leave the record byte for byte as it was.
#[test]
fn a_read_renews_nothing() {
    // 1. A record, and an edit for the reads to see.
    let f = Fixture::new();
    worked(&f, "w", 1);
    write(&f.beside("w"), "src/a.txt", "edited\n");
    let path = hold_path(&f.root, "w");
    let before = std::fs::read(&path).unwrap();

    // 2. Each read, then the record.
    let ctx = f.ctx();
    swarmops_ws::status(&ctx, WsStatusReq::new("w")).unwrap();
    swarmops_ws::diff(&ctx, WsDiffReq::new("w")).unwrap();
    swarmops_ws::info(&ctx, WsInfoReq::new("w")).unwrap();
    swarmops_ws::list(&ctx, WsListReq::default()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// A hold write that fails fails no mutation: the commit succeeds and one `warning:` line goes
/// to stderr. Run as a child, whose stderr the parent reads.
#[test]
fn a_failed_hold_write_fails_no_mutation() {
    // 1. The child: a commit over a host that refuses the hold write.
    if std::env::var_os(CHILD).is_some() {
        let f = Fixture::new();
        let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
        let ctx = f.ctx_on(Arc::new(NoHoldHost(RealHost::default())));
        f.create_in(&ctx, "w", &base, &[]).unwrap();
        write(&f.beside("w"), "src/a.txt", "mine\n");
        let mut req = WsCommitReq::new("w".to_owned(), "mine".to_owned());
        req.status_after = StatusAfter::None;
        let head = swarmops_ws::commit(&ctx, req).unwrap().changeset;
        let file = state::read(&RealFs::new(), &f.beside("w")).unwrap();
        assert_eq!(file.head, head);
        assert!(!hold_path(&f.root, "w").exists());
        return;
    }

    // 2. The parent: the child passed, and wrote one warning per mutation, the create's and
    //    the commit's, none naming a host path.
    let test = swarmops_testkit::test_name!("a_failed_hold_write_fails_no_mutation");
    let mut child = swarmops_testkit::child::this_test(&test);
    child.env(CHILD, "1");
    let out = swarmops_testkit::child::run(&mut child);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let warnings: Vec<&str> = (stderr.lines())
        .filter(|line| line.starts_with("warning:"))
        .collect();
    assert_eq!(warnings.len(), 2, "{stderr}");
    assert!(warnings.iter().all(|w| !w.contains('/')), "{stderr}");
}

/// The hold write makes no call of the durable files' layer: a commit's `FsOps` calls are the
/// same whether the hold is written or refused.
#[test]
fn a_hold_write_makes_no_sync() {
    // 1. The `FsOps` calls of a commit, the hold written or refused.
    let trace = |host: Arc<dyn HostFs>| {
        let f = Fixture::new();
        let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
        let (recorder, calls) = swarmops_testkit::fs::RecordingFs::new();
        let tmp = f.root.parent().unwrap().to_owned();
        let recorder = recorder.naming_paths_in(tmp.into_std_path_buf());
        let ctx = f.ctx_with(host, Arc::new(recorder));
        f.create_in(&ctx, "w", &base, &[]).unwrap();
        write(&f.beside("w"), "src/a.txt", "mine\n");
        calls();
        let mut req = WsCommitReq::new("w".to_owned(), "mine".to_owned());
        req.status_after = StatusAfter::None;
        swarmops_ws::commit(&ctx, req).unwrap();
        (calls(), hold_path(&f.root, "w").exists())
    };
    let (written, held) = trace(Arc::new(RealHost::default()));
    let (refused, none) = trace(Arc::new(NoHoldHost(RealHost::default())));

    // 2. Written in one, not in the other; the same calls in both.
    assert!(held && !none);
    assert_eq!(written, refused);
}

/// `ws delete` removes the record.
#[test]
fn delete_removes_the_hold() {
    let f = Fixture::new();
    worked(&f, "w", 0);
    assert!(hold_path(&f.root, "w").exists());
    swarmops_ws::delete(&f.ctx(), WsDeleteReq::new("w")).unwrap();
    assert!(!hold_path(&f.root, "w").exists());
}

/// Each of the nine mutations leaves the record: the eight write it naming themselves, the
/// ninth, `delete`, removes it.
#[test]
fn each_of_the_nine_mutations_renews_the_hold() {
    // 1. A base with `src/` and `doc/`; a workspace of `src/` only, created.
    let f = Fixture::new();
    let ctx = f.ctx();
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", "1\n2\n3\n"),
            Put::File("doc/d.txt", "d\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    let last = |op: &str| {
        let held = hold(&f, "w");
        assert_eq!(held["last_op"], op, "{held}");
    };
    last("create");

    // 2. A commit; main changes the same line; the rebase stops on the conflict, which
    //    renews nothing.
    write(&dir, "src/a.txt", "1\nmine\n3\n");
    commit_all(&f, "w", "mine");
    last("commit");
    f.commit(Some(&base), &[Put::File("src/a.txt", "1\ntheirs\n3\n")]);
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    last("commit");

    // 3. Resolved, then the rebase continued.
    let mut resolve = WsResolveReq::new("w".to_owned());
    resolve.paths = vec!["src/a.txt".to_owned()];
    resolve.take = Some(Side::Ours);
    resolve.status_after = StatusAfter::None;
    swarmops_ws::resolve(&ctx, resolve).unwrap();
    last("resolve");
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Continue)).unwrap();
    last("rebase");

    // 4. `doc/` materialised; an edit snapshotted, then discarded.
    let mut materialize = WsMaterializeReq::new("w".to_owned(), vec!["doc".to_owned()]);
    materialize.status_after = StatusAfter::None;
    swarmops_ws::materialize(&ctx, materialize).unwrap();
    last("materialize");
    write(&dir, "src/a.txt", "1\nlater\n3\n");
    swarmops_ws::snapshot(&ctx, WsSnapshotReq::new("w")).unwrap();
    last("snapshot");
    swarmops_ws::discard(&ctx, WsDiscardReq::new("w")).unwrap();
    last("discard");

    // 5. Landed, then deleted: the record gone.
    swarmops_ws::land(&ctx, land_of("w")).unwrap();
    last("land");
    swarmops_ws::delete(&ctx, WsDeleteReq::new("w")).unwrap();
    assert!(!hold_path(&f.root, "w").exists());
}

/// An older build's registry listing passes over `workspaces/hold/`: the same entries with it.
#[test]
fn an_older_build_lists_past_the_hold_directory() {
    // 1. Two workspaces, each with its record.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("v", &base, &[]);
    f.create("w", &base, &[]);
    assert!(f.root.join("workspaces/hold").is_dir());

    // 2. The listing: the two entries, nothing of the hold directory.
    let registry = Registry::open(&f.root, Arc::new(RealFs::new()));
    let names: Vec<String> = (registry.list().unwrap().into_iter())
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["v", "w"]);
}
