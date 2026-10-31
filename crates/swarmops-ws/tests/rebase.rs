//! `ws rebase`: the workspace moved onto the rebased changeset, only what changed written; a
//! conflict's markers and report; `ws resolve`, `--continue` and `--abort`.

use swarmops_api::{
    ConflictReport, ShowReq, Side, StatusAfter, StoreOps, WsCommitReq, WsRebaseAction, WsRebaseReq,
    WsResolveReq, WsState,
};
use swarmops_ws::RealFs;
use swarmops_ws::state::{self, SWARMWS_DIR, State};
use swarmops_ws::status::CONFLICTS_JSON;

use crate::fixture::{Fixture, Put, changes, file, tree_of, write};

/// The request of `ws rebase` of workspace `name` onto `main`, doing `action`.
fn rebase_of(name: &str, action: WsRebaseAction) -> WsRebaseReq {
    let mut out = WsRebaseReq::new(name.to_owned());
    out.action = action;
    out.status_after = StatusAfter::Page;
    out
}

/// The request resolving `paths` of workspace `name`, taking `take`, forced or not.
fn resolve_of(name: &str, paths: &[&str], take: Option<Side>, force: bool) -> WsResolveReq {
    let mut out = WsResolveReq::new(name.to_owned());
    out.paths = paths.iter().map(|p| (*p).to_owned()).collect();
    out.take = take;
    out.force = force;
    out.status_after = StatusAfter::Page;
    out
}

/// Commits every edit of workspace `name` with `message`; the new head.
fn commit_all(f: &Fixture, name: &str, message: &str) -> String {
    let mut req = WsCommitReq::new(name.to_owned(), message.to_owned());
    req.status_after = StatusAfter::None;
    swarmops_ws::commit(&f.ctx(), req).unwrap().changeset
}

/// The parents of changeset `id`.
fn parents_of(f: &Fixture, id: &str) -> Vec<String> {
    let mut req = ShowReq::new(id);
    req.max_changes = 1;
    f.ops.show(req).unwrap().parents
}

/// A workspace whose head changed `src/a.txt` and `src/bin` one way while `main` changed them
/// another and added `src/c.txt`: the rebase of it conflicts on both. The base and `main`'s head.
fn conflicting(f: &Fixture) -> (String, String) {
    // 1. The base, and a workspace of it with its own commit.
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", "1\n2\n3\n"),
            Put::File("src/b.txt", "b\n"),
            Put::File("src/bin", "x\0a"),
        ],
    );
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "1\nmine\n3\n");
    write(&dir, "src/bin", "x\0b");
    commit_all(f, "w", "mine");

    // 2. Main's own change of the same lines and bytes, and a file of its own.
    let main = f.commit(
        Some(&base),
        &[
            Put::File("src/a.txt", "1\ntheirs\n3\n"),
            Put::File("src/bin", "x\0c"),
            Put::File("src/c.txt", "c\n"),
        ],
    );
    (base, main)
}

/// A rebase onto a moved `main` records `H'` on `main`'s head and moves the workspace there,
/// writing only the files `main` changed: counted by the recording host.
#[test]
#[cfg(feature = "test-hooks")]
fn rebase_moves_the_workspace_to_h_prime_incrementally() {
    use std::sync::Arc;

    use swarmops_ws::RealHost;
    use swarmops_ws::hostfs::{HostMethod, RecordingHost};

    // 1. A tree of 30 files, a workspace of it with one file of its own committed.
    let f = Fixture::new();
    let names: Vec<String> = (0..30).map(|i| format!("src/m{i:02}.rs")).collect();
    let puts: Vec<Put<'_>> = names.iter().map(|n| Put::File(n, "same\n")).collect();
    let base = f.commit(None, &puts);
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    write(&dir, "src/m03.rs", "mine\n");
    let mine = commit_all(&f, "w", "mine");

    // 2. Main moves: one file changed, one added.
    let main = f.commit(
        Some(&base),
        &[
            Put::File("src/m20.rs", "theirs\n"),
            Put::File("src/new.rs", "new\n"),
        ],
    );

    // 3. The rebase, over a recording host.
    let host = Arc::new(RecordingHost::new(RealHost::default()));
    let ctx = f.ctx_on(host.clone());
    let done = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap();
    assert_eq!(done.onto, main);
    assert_ne!(done.changeset, mine);
    assert_eq!(parents_of(&f, &done.changeset), std::slice::from_ref(&main));
    assert_eq!(done.status_after.unwrap().changes, []);

    // 4. Only main's two files were written, nothing removed.
    let mut written: Vec<String> = (host.calls_of(HostMethod::WriteFile).iter())
        .map(|p| p.strip_prefix(&dir).unwrap().to_string())
        .collect();
    written.sort();
    assert_eq!(written, ["src/m20.rs", "src/new.rs"]);
    assert_eq!(host.calls_of(HostMethod::RemoveFile).len(), 0);

    // 5. The workspace stands on H', rebased onto main, its edit kept and main's in.
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(saved.head, done.changeset);
    assert_eq!(saved.materialized, done.changeset);
    assert_eq!(
        (saved.base.as_str(), saved.onto.as_deref()),
        (&*main, Some(&*main))
    );
    assert_eq!(saved.state, State::Clean);
    let tree = tree_of(&dir);
    assert!(tree.contains(&file("src/m03.rs", "mine\n")));
    assert!(tree.contains(&file("src/m20.rs", "theirs\n")));
    assert!(tree.contains(&file("src/new.rs", "new\n")));
    assert_eq!(changes(&f.ctx(), "w", true), []);
}

/// A rebase that conflicts writes the report to `conflicts.json`, diff3 markers into the
/// conflicting text file, leaves the binary one as the head holds it, and moves nothing else:
/// the head, the base and every other file stay; the workspace is rebasing onto main.
#[test]
fn a_conflict_writes_markers_and_conflicts_json_and_moves_nothing_else() {
    // 1. A workspace whose rebase conflicts.
    let f = Fixture::new();
    let (base, main) = conflicting(&f);
    let dir = f.beside("w");
    let before = state::read(&RealFs::new(), &dir).unwrap();

    // 2. The rebase stops on the conflict, with the store's report.
    let ctx = f.ctx();
    let stopped = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    assert_eq!(stopped.code, "E_CONFLICT", "{stopped:?}");
    assert_eq!(stopped.details["report"]["theirs"], main.as_str());

    // 3. The markers in the text file; the binary file and every other one as they were.
    let short = &main[..12];
    let marked = format!(
        "1\n<<<<<<< ours\nmine\n||||||| base\n2\n=======\ntheirs\n>>>>>>> theirs ({short})\n3\n"
    );
    assert_eq!(
        tree_of(&dir),
        [
            file("src/a.txt", &marked),
            file("src/b.txt", "b\n"),
            file("src/bin", "x\0b"),
        ]
    );

    // 4. The report kept; the state rebasing onto main, the head and base unchanged.
    let report: ConflictReport =
        serde_json::from_slice(&std::fs::read(dir.join(SWARMWS_DIR).join(CONFLICTS_JSON)).unwrap())
            .unwrap();
    assert_eq!(report.theirs, main);
    let paths: Vec<&str> = report.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["src/a.txt", "src/bin"]);
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(
        saved.state,
        State::Rebasing {
            onto: main.clone(),
            against: main.clone(),
        }
    );
    assert_eq!((&saved.head, &saved.base), (&before.head, &base));
    assert_eq!(saved.materialized, before.materialized);

    // 5. Status says so; no pending record is left.
    let mut status = swarmops_api::WsStatusReq::new("w".to_owned());
    status.limit = 100;
    let status = swarmops_ws::status(&ctx, status).unwrap();
    assert_eq!(status.state, WsState::Rebasing);
    assert_eq!(status.rebasing.unwrap().conflicts, ["src/a.txt", "src/bin"]);
    assert!(!dir.join(SWARMWS_DIR).join("journal/pending.json").exists());
}

/// `ws resolve` refuses a file that still holds a marker line, naming it, unless forced; a
/// resolved path leaves the remaining ones.
#[test]
fn resolve_refuses_markers_without_force() {
    // 1. A workspace stopped on a conflict.
    let f = Fixture::new();
    conflicting(&f);
    let ctx = f.ctx();
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();

    // 2. The markers are still there: refused.
    let refused =
        swarmops_ws::resolve(&ctx, resolve_of("w", &["src/a.txt"], None, false)).unwrap_err();
    assert_eq!(refused.code, "E_MARKERS_PRESENT", "{refused:?}");
    assert_eq!(refused.details["paths"], serde_json::json!(["src/a.txt"]));

    // 3. A path that does not conflict is no path to resolve.
    let stray =
        swarmops_ws::resolve(&ctx, resolve_of("w", &["src/b.txt"], None, false)).unwrap_err();
    assert_eq!(stray.code, "E_INVALID_ARG", "{stray:?}");

    // 4. Forced: resolved; the binary file remains.
    let done = swarmops_ws::resolve(&ctx, resolve_of("w", &["src/a.txt"], None, true)).unwrap();
    assert_eq!(done.resolved, ["src/a.txt"]);
    assert_eq!(done.remaining, ["src/bin"]);
    let rebasing = done.status_after.unwrap().rebasing.unwrap();
    assert_eq!(rebasing.conflicts, ["src/bin"]);
}

/// `ws rebase --continue` refuses while a path is unresolved; once each is resolved, by the
/// file's content or by taking a side, it records `H'` on main's head with those resolutions,
/// and the workspace stands on it, clean.
#[test]
fn continue_with_resolutions_records_h_prime() {
    // 1. A workspace stopped on a conflict.
    let f = Fixture::new();
    let (_, main) = conflicting(&f);
    let ctx = f.ctx();
    let dir = f.beside("w");
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();

    // 2. Nothing resolved yet: refused, naming both.
    let early = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Continue)).unwrap_err();
    assert_eq!(early.code, "E_UNRESOLVED_CONFLICTS", "{early:?}");
    assert_eq!(
        early.details["paths"],
        serde_json::json!(["src/a.txt", "src/bin"])
    );

    // 3. The text file edited and resolved; the binary one resolved by taking main's.
    write(&dir, "src/a.txt", "1\nboth\n3\n");
    swarmops_ws::resolve(&ctx, resolve_of("w", &["src/a.txt"], None, false)).unwrap();
    let took = swarmops_ws::resolve(
        &ctx,
        resolve_of("w", &["src/bin"], Some(Side::Theirs), false),
    )
    .unwrap();
    assert_eq!(took.remaining, Vec::<String>::new());
    assert_eq!(std::fs::read(dir.join("src/bin")).unwrap(), b"x\0c");

    // 4. Continued: H' on main's head.
    let done = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Continue)).unwrap();
    assert_eq!(done.onto, main);
    assert_eq!(parents_of(&f, &done.changeset), std::slice::from_ref(&main));
    assert_eq!(done.status_after.unwrap().changes, []);

    // 5. The workspace stands on it: the resolutions, and main's own file.
    assert_eq!(
        tree_of(&dir),
        [
            file("src/a.txt", "1\nboth\n3\n"),
            file("src/b.txt", "b\n"),
            file("src/bin", "x\0c"),
            file("src/c.txt", "c\n"),
        ]
    );
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(saved.state, State::Clean);
    assert_eq!(
        (&saved.head, &saved.materialized),
        (&done.changeset, &done.changeset)
    );
    assert_eq!(
        (saved.base.as_str(), saved.onto.as_deref()),
        (&*main, Some(&*main))
    );
    assert!(!dir.join(SWARMWS_DIR).join(CONFLICTS_JSON).exists());
    assert_eq!(changes(&ctx, "w", true), []);
}

/// `ws rebase --abort` restores each conflicting path from the head, whatever the markers or a
/// resolution put there, and the workspace is clean again on its head.
#[test]
fn abort_restores_the_conflicting_paths() {
    // 1. A workspace stopped on a conflict, one path resolved by taking main's.
    let f = Fixture::new();
    conflicting(&f);
    let ctx = f.ctx();
    let dir = f.beside("w");
    let before = state::read(&RealFs::new(), &dir).unwrap();
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    swarmops_ws::resolve(
        &ctx,
        resolve_of("w", &["src/bin"], Some(Side::Theirs), false),
    )
    .unwrap();

    // 2. Aborted: the head's files, the head's state.
    let done = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Abort)).unwrap();
    assert_eq!(done.status, None);
    assert_eq!(done.changeset, before.head);
    assert_eq!(
        tree_of(&dir),
        [
            file("src/a.txt", "1\nmine\n3\n"),
            file("src/b.txt", "b\n"),
            file("src/bin", "x\0b"),
        ]
    );
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(saved.state, State::Clean);
    assert_eq!(saved.head, before.head);
    assert!(!dir.join(SWARMWS_DIR).join(CONFLICTS_JSON).exists());
    assert_eq!(changes(&ctx, "w", true), []);

    // 3. Nothing to abort any more.
    let again = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Abort)).unwrap_err();
    assert_eq!(again.code, "E_INVALID_ARG", "{again:?}");
}

/// A file with two conflicting places and a clean change of main's between them gets one block
/// of markers per hunk of the report, in the report's order, the clean change merged in.
#[test]
fn markers_follow_the_reports_hunk_order() {
    // 1. Twelve lines; the workspace changes lines 2 and 10, main changes 2, 6 and 10.
    let f = Fixture::new();
    let lines: Vec<String> = (1..=12).map(|n| format!("l{n}\n")).collect();
    let text = lines.concat();
    let base = f.commit(None, &[Put::File("a.txt", &text)]);
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    let ours = text.replace("l2\n", "o2\n").replace("l10\n", "o10\n");
    write(&dir, "a.txt", &ours);
    commit_all(&f, "w", "mine");
    let theirs = (text.replace("l2\n", "t2\n"))
        .replace("l6\n", "t6\n")
        .replace("l10\n", "t10\n");
    let main = f.commit(Some(&base), &[Put::File("a.txt", &theirs)]);

    // 2. The rebase conflicts; the report holds two hunks.
    let ctx = f.ctx();
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    let report: ConflictReport =
        serde_json::from_slice(&std::fs::read(dir.join(SWARMWS_DIR).join(CONFLICTS_JSON)).unwrap())
            .unwrap();
    let hunks = &report.files[0].hunks;
    let ours_texts: Vec<Option<&str>> = hunks.iter().map(|h| h.ours.text.as_deref()).collect();
    assert_eq!(ours_texts, [Some("o2\n"), Some("o10\n")]);

    // 3. One block per hunk, in that order, main's clean change between them.
    let short = &main[..12];
    let block = |o: &str, b: &str, t: &str| {
        format!("<<<<<<< ours\n{o}||||||| base\n{b}=======\n{t}>>>>>>> theirs ({short})\n")
    };
    let expected = [
        "l1\n".to_owned(),
        block("o2\n", "l2\n", "t2\n"),
        "l3\nl4\nl5\nt6\nl7\nl8\nl9\n".to_owned(),
        block("o10\n", "l10\n", "t10\n"),
        "l11\nl12\n".to_owned(),
    ]
    .concat();
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        expected
    );
}

/// `ws rebase` is the way out of `E_WS_STALE`: it moves the workspace onto the target's head,
/// and the next land goes through.
#[test]
fn ws_rebase_updates_a_stale_workspace() {
    // 1. A workspace on `release/r` at `b` with a commit; the target moved back to `p`.
    let f = Fixture::new();
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let b = f.side(Some(&p), &[Put::File("src/b.txt", "b\n")]);
    f.point("release/r", &b);
    f.create("w", "release/r", &[]);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "mine\n");
    commit_all(&f, "w", "mine");
    f.point("release/r", &p);
    let mut land = swarmops_api::WsLandReq::new("w".to_owned());
    land.status_after = StatusAfter::None;
    let refused = swarmops_ws::land(&f.ctx(), land.clone()).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");

    // 2. Rebased onto `release/r`: the base is its head.
    let mut req = rebase_of("w", WsRebaseAction::Start);
    req.onto = "release/r".to_owned();
    let done = swarmops_ws::rebase(&f.ctx(), req).unwrap();
    assert_eq!(done.onto, p);
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(saved.base, p);
    assert_eq!(saved.target, "release/r");

    // 3. The land proceeds and moves `release/r` to what it landed.
    let landed = swarmops_ws::land(&f.ctx(), land).unwrap();
    let head = f
        .ops
        .resolve_rev(swarmops_api::ResolveRevReq::new("release/r"));
    assert_eq!(head.unwrap().id, landed.land.landed);
    assert_eq!(tree_of(&dir), [file("src/a.txt", "mine\n")]);
}
