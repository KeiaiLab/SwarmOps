//! A stale workspace (the M19 design §3): its base was put on its own target, and the target's
//! head no longer reaches it. `ws status`, `ws diff`, `ws commit`, `ws materialize`, `ws land`
//! onto any target and `ws rebase --continue` answer `E_WS_STALE` and change nothing; a base
//! made off the target never is.

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use swarmops_api::{
    GitImportReq, Identity, StatusAfter, StoreOps, UndoReq, WsCommitReq, WsDiffReq, WsLandReq,
    WsMaterializeReq, WsRebaseAction, WsRebaseReq, WsResolveReq, WsStatusReq,
};
use swarmops_testkit::git;
use swarmops_ws::RealFs;
use swarmops_ws::state::{self, SWARMWS_DIR};

use crate::fixture::{AGENT, Fixture, Put, Watched, tree_of, write};

/// The most changesets and operations the old search read (`status::BEHIND_CAP`, `OP_CAP`).
const CAP: u64 = 10_000;

/// What a refused command must leave as it was: the files, every record of `.swarmws/` that a
/// command writes, and the journal's end.
#[derive(Debug, PartialEq, Eq)]
struct Left {
    /// The workspace's files.
    files: Vec<(String, char, String)>,
    /// The bytes of `workspace.json`, the pending record, the conflict report and the
    /// resolutions, each when there.
    records: Vec<Option<Vec<u8>>>,
    /// The newest operation's number.
    op: u64,
}

impl Left {
    /// Workspace `name`'s files and records, and the journal's end.
    fn of(f: &Fixture, name: &str) -> Left {
        let dir = f.beside(name);
        let records = [
            "workspace.json",
            "journal/pending.json",
            "conflicts.json",
            "resolved.json",
        ];
        Left {
            files: tree_of(&dir),
            records: (records.iter())
                .map(|rel| std::fs::read(dir.join(SWARMWS_DIR).join(rel)).ok())
                .collect(),
            op: f.last_op(),
        }
    }
}

/// The identity the fixture acts as.
fn who() -> Identity {
    Identity::parse(AGENT).unwrap()
}

/// A workspace `w` on `release/r` at `b`, one commit above `p`, with an edit of `src/a.txt`;
/// `release/r` then taken back to `p` by an undo of the move to `b`. `(p, b, base_op,
/// moved_op)`: the move to `b`, and the undo.
fn undone(f: &Fixture, prefixes: &[&str]) -> (String, String, u64, u64) {
    // 1. `p`, `b` on it, `release/r` moved to `b`; the workspace of it, with an edit.
    let p = f.commit(
        None,
        &[
            Put::File("src/a.txt", "a\n"),
            Put::File("docs/d.txt", "d\n"),
        ],
    );
    f.point("release/r", &p);
    let b = f.side(Some(&p), &[Put::File("src/b.txt", "b\n")]);
    f.point("release/r", &b);
    let (base_op, id) = f.op_moving("release/r", &b);
    f.create("w", "release/r", prefixes);
    write(&f.beside("w"), "src/a.txt", "mine\n");

    // 2. Another process undoes the move.
    f.ops.undo(&who(), UndoReq::new(id)).unwrap();
    let (moved_op, _) = f.op_moving("release/r", &p);
    (p, b, base_op, moved_op)
}

/// `ws status` of workspace `name`.
fn status_of(f: &Fixture, name: &str) -> Result<swarmops_api::WsStatus, swarmops_api::OpError> {
    swarmops_ws::status(&f.ctx(), WsStatusReq::new(name))
}

/// `ws commit` of every edit of workspace `name`.
fn commit_of(ctx: &swarmops_ws::Ctx<'_>, name: &str) -> Result<String, swarmops_api::OpError> {
    let mut req = WsCommitReq::new(name, "mine");
    req.status_after = StatusAfter::None;
    swarmops_ws::commit(ctx, req).map(|done| done.changeset)
}

/// `ws land -m` of workspace `name` onto `target`, or its own.
fn land_of(name: &str, target: Option<&str>) -> WsLandReq {
    let mut req = WsLandReq::new(name);
    req.message = Some("land".to_owned());
    req.target = target.map(str::to_owned);
    req.status_after = StatusAfter::None;
    req
}

/// The `details` of a refusal of workspace `w` on `release/r`, based on `base`.
fn details(base: &str, head: &str, base_op: u64, moved_op: Option<u64>) -> Value {
    json!({
        "target": "release/r",
        "base": base,
        "head": head,
        "base_op": base_op,
        "moved_op": moved_op,
    })
}

/// The bytes of workspace `name`'s `workspace.json`.
fn state_bytes(dir: &Utf8Path) -> Vec<u8> {
    std::fs::read(state::path_in(dir)).unwrap()
}

/// `ws status` and `ws diff` of a workspace whose base an undo took off its target are
/// `E_WS_STALE`, naming the move to the base and the undo; every file's bytes stay.
#[test]
fn status_of_a_stale_workspace_is_refused() {
    // 1. The workspace on `b`, `release/r` taken back to `p`.
    let f = Fixture::new();
    let (p, b, base_op, moved_op) = undone(&f, &[]);
    let before = Left::of(&f, "w");

    // 2. Status refused, with the two operations.
    let refused = status_of(&f, "w").unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        Value::Object(refused.details.clone()),
        details(&b, &p, base_op, Some(moved_op))
    );
    assert_eq!(refused.hint.as_deref(), Some("swarmops ws rebase"));

    // 3. Diff too; nothing changed.
    let refused = swarmops_ws::diff(&f.ctx(), WsDiffReq::new("w")).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(Left::of(&f, "w"), before);
}

/// `ws commit` and `ws materialize` of a stale workspace write nothing: the journal's length,
/// `workspace.json`'s bytes and the files stay.
#[test]
fn commit_of_a_stale_workspace_writes_nothing() {
    // 1. A sparse workspace of `src` on `b`, `release/r` taken back.
    let f = Fixture::new();
    undone(&f, &["src"]);
    let dir = f.beside("w");
    let before = Left::of(&f, "w");
    let bytes = state_bytes(&dir);

    // 2. The commit refused, nothing written.
    let refused = commit_of(&f.ctx(), "w").unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(state_bytes(&dir), bytes);
    assert_eq!(Left::of(&f, "w"), before);

    // 3. The materialisation of `docs` refused, nothing written.
    let mut req = WsMaterializeReq::new("w", vec!["docs".to_owned()]);
    req.status_after = StatusAfter::None;
    let refused = swarmops_ws::materialize(&f.ctx(), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(state_bytes(&dir), bytes);
    assert_eq!(Left::of(&f, "w"), before);
}

/// A land onto another target than the workspace's own is judged against its own: the base it
/// came from was taken back there, and the land would bring that work back.
#[test]
fn a_land_onto_another_target_is_stale() {
    // 1. The workspace on `b`, `release/r` taken back; `release/x` at `b`, never moved off it.
    let f = Fixture::new();
    let (p, b, base_op, moved_op) = undone(&f, &[]);
    f.point("release/x", &b);
    let before = Left::of(&f, "w");

    // 2. Refused, naming its own target, nothing written.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", Some("release/x"))).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        Value::Object(refused.details.clone()),
        details(&b, &p, base_op, Some(moved_op))
    );
    assert_eq!(Left::of(&f, "w"), before);
    let head = f
        .ops
        .resolve_rev(swarmops_api::ResolveRevReq::new("release/x"));
    assert_eq!(head.unwrap().id, b);
}

/// A `ws rebase --continue` whose `onto` an undo took off the target is `E_WS_STALE`: it would
/// record a changeset on work the target dropped. Its hint leaves the rebase first.
#[test]
fn continue_after_onto_was_undone_is_stale() {
    // 1. A workspace on `p` with a commit; `release/r` moved to a change of the same line.
    let f = Fixture::new();
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.point("release/r", &p);
    f.create("w", "release/r", &[]);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "mine\n");
    commit_of(&f.ctx(), "w").unwrap();
    let theirs = f.side(Some(&p), &[Put::File("src/a.txt", "theirs\n")]);
    f.point("release/r", &theirs);
    let (onto_op, id) = f.op_moving("release/r", &theirs);

    // 2. The rebase conflicts; the conflict resolved; the move to `theirs` undone.
    let mut req = WsRebaseReq::new("w");
    req.action = WsRebaseAction::Start;
    "release/r".clone_into(&mut req.onto);
    req.status_after = StatusAfter::None;
    let conflict = swarmops_ws::rebase(&f.ctx(), req.clone()).unwrap_err();
    assert_eq!(conflict.code, "E_CONFLICT", "{conflict:?}");
    write(&dir, "src/a.txt", "both\n");
    let mut resolve = WsResolveReq::new("w");
    resolve.paths = vec!["src/a.txt".to_owned()];
    resolve.status_after = StatusAfter::None;
    swarmops_ws::resolve(&f.ctx(), resolve).unwrap();
    f.ops.undo(&who(), UndoReq::new(id)).unwrap();
    let (moved_op, _) = f.op_moving("release/r", &p);
    let before = Left::of(&f, "w");

    // 3. The continue refused, nothing written; the hint aborts, then updates.
    req.action = WsRebaseAction::Continue;
    let refused = swarmops_ws::rebase(&f.ctx(), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        Value::Object(refused.details.clone()),
        details(&theirs, &p, onto_op, Some(moved_op))
    );
    assert_eq!(
        refused.hint.as_deref(),
        Some("swarmops ws rebase --abort, then swarmops ws rebase")
    );
    assert_eq!(Left::of(&f, "w"), before);
}

/// A workspace made off a side changeset (`ws create --base <side>`) is never stale, however
/// its target moves: status, commit and land go through.
#[test]
fn a_side_base_is_never_stale() {
    // 1. A workspace of a side changeset; `main` moves on without it.
    let f = Fixture::new();
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let side = f.side(Some(&p), &[Put::File("src/s.txt", "s\n")]);
    f.create("w", &side, &[]);
    f.commit(Some(&p), &[Put::File("src/m.txt", "m\n")]);
    write(&f.beside("w"), "src/a.txt", "mine\n");

    // 2. Status, commit and land go through.
    status_of(&f, "w").unwrap();
    commit_of(&f.ctx(), "w").unwrap();
    write(&f.beside("w"), "src/a.txt", "again\n");
    swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap();
}

/// A base off a target's history longer than the old search's cap is stale: the target's
/// first-parent history from its head reads 10,001 changesets without the base (a double), so
/// a walk of it stopped at the cap, not knowing; `ancestry` knows.
#[test]
fn a_base_past_ten_thousand_commits_is_stale() {
    // 1. The workspace on `b`, `release/r` taken back to `p`, whose history reads 10,001 long.
    let f = Fixture::new();
    let (p, b, base_op, moved_op) = undone(&f, &[]);
    let mut watched = Watched::new(&f.ops);
    watched.fake_history_of = Some((p.clone(), CAP + 1));
    let before = Left::of(&f, "w");

    // 2. The land refused, nothing written.
    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        Value::Object(refused.details.clone()),
        details(&b, &p, base_op, Some(moved_op))
    );
    assert_eq!(Left::of(&f, "w"), before);
}

/// A base whose move lies past 10,001 newer operations is still found: the create recorded it.
#[test]
fn a_base_op_past_ten_thousand_operations_is_found() {
    // 1. The workspace on `b`, `release/r` taken back; 10,001 operations above (a double).
    let f = Fixture::new();
    let (p, b, base_op, _) = undone(&f, &[]);
    let mut watched = Watched::new(&f.ops);
    watched.fake_ops = Some(CAP + 1);
    let before = Left::of(&f, "w");

    // 2. The land refused, naming the recorded move; the move back lies past the cap.
    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        Value::Object(refused.details.clone()),
        details(&b, &p, base_op, None)
    );
    assert_eq!(Left::of(&f, "w"), before);
}

/// A base the target's head reaches only through a merge's second parent is on its history:
/// not stale, though its first parents never meet it.
#[test]
fn a_base_through_a_merge_is_not_stale() {
    // 1. The `merges` fixture imported: `release/x` at the commit its first merge reaches only
    //    through its second parent, `release/m` at the last merge.
    let f = Fixture::new();
    let merges = git::fixture("merges");
    let source = merges.repo.as_deref().unwrap().to_str().unwrap();
    for (git_ref, target) in [
        ("refs/tags/light", "release/x"),
        ("refs/heads/main", "release/m"),
    ] {
        let mut req = GitImportReq::new(source);
        req.git_ref = git_ref.to_owned();
        req.target_ref = target.to_owned();
        f.ops.git_import(&who(), req).unwrap();
    }

    // 2. A workspace of `release/x`; `release/x` moved to the last merge.
    f.create("w", "release/x", &[]);
    let merge = f
        .ops
        .resolve_rev(swarmops_api::ResolveRevReq::new("release/m"));
    f.point("release/x", &merge.unwrap().id);
    write(&f.beside("w"), "base.txt", "mine\n");

    // 3. Status, commit and land go through.
    status_of(&f, "w").unwrap();
    commit_of(&f.ctx(), "w").unwrap();
    swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap();
}

/// A `workspace.json` written without `base_op` (an older binary's) is checked as before: the
/// journal's newest-first search finds the move to the base, and a side base stays not stale.
#[test]
fn a_workspace_without_base_op_is_checked_as_before() {
    // 1. The workspace on `b`, `release/r` taken back; its state without `base_op`.
    let f = Fixture::new();
    let (p, b, base_op, moved_op) = undone(&f, &[]);
    let dir = f.beside("w");
    drop_base_op(&dir);

    // 2. The land refused, with the operations the journal holds.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        Value::Object(refused.details.clone()),
        details(&b, &p, base_op, Some(moved_op))
    );

    // 3. A side base without it lands.
    let side = f.side(Some(&p), &[Put::File("src/s.txt", "s\n")]);
    f.create("v", &side, &[]);
    let other = f.beside("v");
    drop_base_op(&other);
    write(&other, "src/a.txt", "v\n");
    swarmops_ws::land(&f.ctx(), land_of("v", None)).unwrap();
}

/// Rewrites the `workspace.json` at `dir` without `base_op`, as an older binary writes it.
fn drop_base_op(dir: &Utf8PathBuf) {
    let path = state::path_in(dir);
    let mut wire: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    wire.as_object_mut().unwrap().remove("base_op");
    std::fs::write(&path, wire.to_string()).unwrap();
    state::read(&RealFs::new(), dir).unwrap();
}

/// `workspace.json` records the operation that put the target on the base each time a command
/// sets the base: `ws create`, a `ws rebase` that finishes, `ws land`.
#[test]
fn create_rebase_and_land_record_the_base_op() {
    // 1. Created on `release/r` at `p`: the move to `p`.
    let f = Fixture::new();
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.point("release/r", &p);
    f.create("w", "release/r", &[]);
    let dir = f.beside("w");
    let recorded = || state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(recorded().base_op, Some(f.op_moving("release/r", &p).0));

    // 2. Rebased onto `release/r` at `q`: the move to `q`.
    let q = f.side(Some(&p), &[Put::File("src/q.txt", "q\n")]);
    f.point("release/r", &q);
    let mut req = WsRebaseReq::new("w");
    req.action = WsRebaseAction::Start;
    "release/r".clone_into(&mut req.onto);
    req.status_after = StatusAfter::None;
    swarmops_ws::rebase(&f.ctx(), req).unwrap();
    assert_eq!(recorded().base, q);
    assert_eq!(recorded().base_op, Some(f.op_moving("release/r", &q).0));

    // 3. Landed: the land's move.
    write(&dir, "src/a.txt", "mine\n");
    let done = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap();
    let landed = done.land.landed;
    assert_eq!(recorded().base, landed);
    assert_eq!(
        recorded().base_op,
        Some(f.op_moving("release/r", &landed).0)
    );
}
