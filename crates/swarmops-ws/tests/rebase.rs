//! `ws rebase`: the workspace moved onto the rebased changeset, only what changed written; a
//! conflict's markers and report; `ws resolve`, `--continue` and `--abort`.

use std::sync::atomic::Ordering;

use swarmops_api::{
    ConflictReport, ReadFileReq, ShowReq, Side, StatusAfter, StoreOps, WsChangeKind, WsCommitReq,
    WsRebaseAction, WsRebaseReq, WsResolveReq, WsState,
};
use swarmops_ws::state::{self, SWARMWS_DIR, State};
use swarmops_ws::status::CONFLICTS_JSON;
use swarmops_ws::{RealFs, pending};

use crate::fixture::{Fixture, Put, Watched, change, changes, file, tree_of, write};

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

/// A `ws rebase` onto a target that gained a name the workspace's volume cannot hold is refused
/// before the store rebases: no operation is recorded, no pending record is left, and the
/// workspace stays on its head, so the next command is not wedged.
#[test]
fn a_rebase_onto_a_reserved_name_leaves_no_record() {
    for reserved in ["src/.SwarmWs/z", ".swarmws/x"] {
        // 1. A workspace on `release/r` with a commit; `release/r` then gains `reserved`.
        let f = Fixture::new();
        let b = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
        f.point("release/r", &b);
        f.create("w", "release/r", &[]);
        let dir = f.beside("w");
        write(&dir, "src/a.txt", "mine\n");
        let head = commit_all(&f, "w", "mine");
        let gained = f.side(Some(&b), &[Put::File(reserved, "z\n")]);
        f.point("release/r", &gained);
        let recorded = f.last_op();

        // 2. The rebase is refused as unportable.
        let mut req = rebase_of("w", WsRebaseAction::Start);
        req.onto = "release/r".to_owned();
        let refused = swarmops_ws::rebase(&f.ctx(), req).unwrap_err();
        assert_eq!(refused.code, "E_WS_UNPORTABLE", "{reserved}: {refused:?}");

        // 3. Nothing recorded: no operation, no pending record, the state as it was.
        assert_eq!(f.last_op(), recorded, "{reserved}: a store frame");
        let pending = dir.join(SWARMWS_DIR).join("journal/pending.json");
        assert!(!pending.exists(), "{reserved}: a pending record");
        let saved = state::read(&RealFs::new(), &dir).unwrap();
        assert_eq!((saved.head, saved.base), (head, b.clone()), "{reserved}");
    }
}

/// The preflight of a rebase reads the head of `--onto`, not the workspace's target: a ref that
/// gained a reserved name refuses it, and no store frame is written.
#[test]
fn a_rebase_preflight_checks_onto() {
    // 1. A workspace on `release/r` with a commit; `release/o` holds a reserved name.
    let f = Fixture::new();
    let b = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.point("release/r", &b);
    f.create("w", "release/r", &[]);
    write(&f.beside("w"), "src/a.txt", "mine\n");
    let head = commit_all(&f, "w", "mine");
    let reserved = f.side(Some(&b), &[Put::File("src/.SwarmWs/z", "z\n")]);
    f.point("release/o", &reserved);
    let recorded = f.last_op();

    // 2. Rebased onto `release/o`: refused, nothing recorded; the target is clean.
    let mut req = rebase_of("w", WsRebaseAction::Start);
    req.onto = "release/o".to_owned();
    let refused = swarmops_ws::rebase(&f.ctx(), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(f.last_op(), recorded, "a store frame");
    let saved = state::read(&RealFs::new(), &f.beside("w")).unwrap();
    assert_eq!(saved.head, head);
}

/// `--continue` onto a target that gained a reserved name since the conflict is refused before
/// the store, and leaves no record.
#[test]
fn a_continue_checks_the_target() {
    // 1. A conflicting rebase, resolved; the target gains a reserved name.
    let f = Fixture::new();
    let (_, main) = conflicting(&f);
    let ctx = f.ctx();
    let dir = f.beside("w");
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    write(&dir, "src/a.txt", "1\nboth\n3\n");
    swarmops_ws::resolve(&ctx, resolve_of("w", &["src/a.txt"], None, false)).unwrap();
    swarmops_ws::resolve(
        &ctx,
        resolve_of("w", &["src/bin"], Some(Side::Theirs), false),
    )
    .unwrap();
    f.commit(Some(&main), &[Put::File("src/.swarmws/x", "z\n")]);
    let recorded = f.last_op();

    // 2. Refused as unportable: no store frame, no record.
    let refused = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Continue)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(f.last_op(), recorded, "a store frame");
    assert!(
        !dir.join(SWARMWS_DIR).join("journal/pending.json").exists(),
        "a record"
    );
}

/// A rebase with edits to settle onto a ref with a reserved name is refused before the edits
/// are committed: no store frame, the head as it was.
#[test]
fn a_refused_rebase_settles_nothing() {
    // 1. A workspace with a commit and an unsettled edit; `release/o` holds a reserved name.
    let f = Fixture::new();
    let b = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.point("release/r", &b);
    f.create("w", "release/r", &[]);
    write(&f.beside("w"), "src/a.txt", "mine\n");
    let head = commit_all(&f, "w", "mine");
    write(&f.beside("w"), "src/a.txt", "unsettled\n");
    let reserved = f.side(Some(&b), &[Put::File("src/.SwarmWs/z", "z\n")]);
    f.point("release/o", &reserved);
    let recorded = f.last_op();

    // 2. Refused as unportable before `-m` commits the edit.
    let mut req = rebase_of("w", WsRebaseAction::Start);
    req.onto = "release/o".to_owned();
    req.message = Some("settle".to_owned());
    let refused = swarmops_ws::rebase(&f.ctx(), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(f.last_op(), recorded, "a store frame");
    assert_eq!(
        state::read(&RealFs::new(), &f.beside("w")).unwrap().head,
        head
    );
}

/// The text changeset `id` holds at `path`.
fn text_at(f: &Fixture, id: &str, path: &str) -> String {
    let mut req = ReadFileReq::new(id, path);
    req.max_bytes = 1024;
    f.ops.read_file(req).unwrap().content
}

/// The number of operations the store has recorded.
fn ops_of(f: &Fixture) -> u64 {
    f.last_op()
}

/// A workspace `w` of the whole tree at a base of three files, `main` then moved on two of them;
/// the base.
fn moved_under_edits(f: &Fixture) -> String {
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", "a\n"),
            Put::File("src/b.txt", "b\n"),
            Put::File("src/c.txt", "1\n2\n3\n4\n5\n"),
        ],
    );
    f.create("w", &base, &[]);
    f.commit(
        Some(&base),
        &[
            Put::File("src/b.txt", "b2\n"),
            Put::File("src/c.txt", "1\n2\n3\n4\nfive\n"),
        ],
    );
    base
}

/// An update with edits that apply to the new head snapshots them first and carries them: the
/// files are the new head plus the edits (a text file both changed merged), the status lists
/// them against it, and the snapshot holds the old head plus the edits.
#[test]
fn an_update_carries_clean_edits() {
    // 1. Edits of a file main left and of one main changed elsewhere in it.
    let f = Fixture::new();
    let base = moved_under_edits(&f);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "mine\n");
    write(&dir, "src/c.txt", "one\n2\n3\n4\n5\n");

    // 2. The update succeeds, its snapshot named, nothing dropped.
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let snapshot = done.snapshot.clone().expect("a snapshot");
    assert_eq!((done.kept.as_deref(), done.dropped.len()), (None, 0));
    assert_eq!(parents_of(&f, &snapshot), [base]);
    assert_eq!(text_at(&f, &snapshot, "src/a.txt"), "mine\n");
    assert_eq!(text_at(&f, &snapshot, "src/b.txt"), "b\n");

    // 3. The files: the new head plus the edits; the status lists the edits against it.
    assert_eq!(
        tree_of(&dir),
        [
            file("src/a.txt", "mine\n"),
            file("src/b.txt", "b2\n"),
            file("src/c.txt", "one\n2\n3\n4\nfive\n"),
        ]
    );
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(
        (saved.head.as_str(), saved.materialized.as_str()),
        (&*done.changeset, &*done.changeset)
    );
    let edited = [
        change("src/a.txt", WsChangeKind::Modified),
        change("src/c.txt", WsChangeKind::Modified),
    ];
    assert_eq!(changes(&f.ctx(), "w", false), edited);
    assert_eq!(changes(&f.ctx(), "w", true), edited);
    assert!(
        pending::read_pending(&RealFs::new(), &dir)
            .unwrap()
            .is_none()
    );
}

/// An edit that does not apply to the new head is not written as markers: its path takes the new
/// head's bytes, and the snapshot, returned as `kept`, holds it.
#[test]
fn an_update_keeps_conflicting_edits_in_a_changeset() {
    // 1. An edit of the line main changed, and a clean one.
    let f = Fixture::new();
    moved_under_edits(&f);
    let dir = f.beside("w");
    write(&dir, "src/c.txt", "1\n2\n3\n4\nmine\n");
    write(&dir, "src/a.txt", "mine\n");

    // 2. Kept by id, the path dropped.
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let snapshot = done.snapshot.clone().expect("a snapshot");
    assert_eq!(done.kept.as_deref(), Some(&*snapshot));
    assert_eq!(done.dropped, ["src/c.txt"]);
    assert_eq!(text_at(&f, &snapshot, "src/c.txt"), "1\n2\n3\n4\nmine\n");

    // 3. The files: the new head's at the dropped path, the clean edit carried.
    assert_eq!(
        tree_of(&dir),
        [
            file("src/a.txt", "mine\n"),
            file("src/b.txt", "b2\n"),
            file("src/c.txt", "1\n2\n3\n4\nfive\n"),
        ]
    );
    assert_eq!(
        changes(&f.ctx(), "w", true),
        [change("src/a.txt", WsChangeKind::Modified)]
    );
}

/// An update cut short after its snapshot was recorded makes the same snapshot again on the next
/// run, by its key: the store replays it, and the two runs record what one uncut run does.
#[test]
fn an_update_with_the_same_edits_replays_its_snapshot() {
    // 1. What an uncut update records.
    let uncut = Fixture::new();
    moved_under_edits(&uncut);
    write(&uncut.beside("w"), "src/a.txt", "mine\n");
    let before = ops_of(&uncut);
    swarmops_ws::rebase(&uncut.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let recorded = ops_of(&uncut) - before;

    // 2. The same update, cut right after the store recorded the snapshot.
    let f = Fixture::new();
    moved_under_edits(&f);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "mine\n");
    let before = ops_of(&f);
    let watched = Watched::new(&f.ops);
    watched.crash_after_commit.store(true, Ordering::SeqCst);
    let cut = swarmops_ws::rebase(&f.ctx_over(&watched), rebase_of("w", WsRebaseAction::Start));
    assert_eq!(cut.unwrap_err().code, "E_IO");
    let made = watched.commits()[0]
        .1
        .clone()
        .expect("the snapshot recorded");

    // 3. Run again: the same snapshot, and as many operations as the uncut run.
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    assert_eq!(done.snapshot.as_deref(), Some(&*made));
    assert_eq!(ops_of(&f) - before, recorded);
    assert_eq!(tree_of(&dir)[0], file("src/a.txt", "mine\n"));
}

/// A file the agent added, which no ignore file names, is snapshotted and stays on disk as an
/// addition against the new head.
#[test]
fn an_untracked_file_is_snapshotted_and_carried() {
    let f = Fixture::new();
    moved_under_edits(&f);
    let dir = f.beside("w");
    write(&dir, "src/new.txt", "new\n");
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let snapshot = done.snapshot.expect("a snapshot");
    assert_eq!(text_at(&f, &snapshot, "src/new.txt"), "new\n");
    assert!(tree_of(&dir).contains(&file("src/new.txt", "new\n")));
    assert_eq!(
        changes(&f.ctx(), "w", true),
        [change("src/new.txt", WsChangeKind::Added)]
    );
}

/// A file an ignore file names is no edit: the snapshot does not hold it, and it stays on disk.
#[test]
fn an_ignored_file_is_not_snapshotted() {
    // 1. A base that ignores `*.log`; an edit and a log beside it.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File(".gitignore", "*.log\n"),
            Put::File("a.txt", "a\n"),
        ],
    );
    f.create("w", &base, &[]);
    f.commit(Some(&base), &[Put::File("b.txt", "b\n")]);
    let dir = f.beside("w");
    write(&dir, "a.txt", "mine\n");
    write(&dir, "build.log", "noise\n");

    // 2. The snapshot holds the edit, not the log, which stays.
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let snapshot = done.snapshot.expect("a snapshot");
    assert_eq!(text_at(&f, &snapshot, "a.txt"), "mine\n");
    assert!(!crate::fixture::tree_holds(&f, &snapshot, "build.log"));
    assert!(tree_of(&dir).contains(&file("build.log", "noise\n")));
}

/// A chain that conflicts leaves the workspace rebasing with its snapshot recorded and the edits
/// on disk; `--continue` carries them onto the rebased head.
#[test]
fn continue_carries_the_edits_after_a_conflict() {
    // 1. A conflicting chain and an edit of a path the conflict leaves alone.
    let f = Fixture::new();
    conflicting(&f);
    let ctx = f.ctx();
    let dir = f.beside("w");
    write(&dir, "src/b.txt", "edited\n");
    let refused = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    assert_eq!(refused.code, "E_CONFLICT", "{refused:?}");
    let record = pending::read_pending(&RealFs::new(), &dir)
        .unwrap()
        .expect("a record");
    let snapshot = record.snapshot.clone().expect("the snapshot recorded");
    assert_eq!(text_at(&f, &snapshot, "src/b.txt"), "edited\n");

    // 2. Resolved and continued: the edit carried.
    write(&dir, "src/a.txt", "1\nboth\n3\n");
    swarmops_ws::resolve(&ctx, resolve_of("w", &["src/a.txt"], None, false)).unwrap();
    swarmops_ws::resolve(
        &ctx,
        resolve_of("w", &["src/bin"], Some(Side::Theirs), false),
    )
    .unwrap();
    let done = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Continue)).unwrap();
    assert_eq!(done.snapshot.as_deref(), Some(&*snapshot));
    assert!(tree_of(&dir).contains(&file("src/b.txt", "edited\n")));
    assert_eq!(
        changes(&ctx, "w", true),
        [change("src/b.txt", WsChangeKind::Modified)]
    );
}

/// `--abort` after a conflict keeps the edits on disk, and the update again takes the same
/// snapshot: the same edits on the same head make one changeset.
#[test]
fn abort_keeps_the_edits() {
    // 1. A conflicting chain, an edit beside it, the update stopped on the conflict.
    let f = Fixture::new();
    conflicting(&f);
    let ctx = f.ctx();
    let dir = f.beside("w");
    write(&dir, "src/b.txt", "edited\n");
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    let first = pending::read_pending(&RealFs::new(), &dir)
        .unwrap()
        .unwrap()
        .snapshot;

    // 2. Aborted: the edit stays, listed, and the response names the snapshot that holds it.
    let aborted = swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Abort)).unwrap();
    assert_eq!(aborted.snapshot, first);
    assert!(tree_of(&dir).contains(&file("src/b.txt", "edited\n")));
    assert_eq!(
        changes(&ctx, "w", true),
        [change("src/b.txt", WsChangeKind::Modified)]
    );

    // 3. The update again: the same snapshot, no new one.
    let before = ops_of(&f);
    swarmops_ws::rebase(&ctx, rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    let again = pending::read_pending(&RealFs::new(), &dir)
        .unwrap()
        .unwrap()
        .snapshot;
    assert_eq!(again, first);
    assert!(first.is_some());
    assert_eq!(ops_of(&f), before, "a store operation");
}

/// An update cut after `workspace.json` named the new head and before its intent went: the next
/// open takes the state's `materialized`, which the intent stands for, as done and only removes
/// the intent, so an edit the agent made since to a file the update wrote is no obstacle.
#[test]
fn an_open_after_the_state_names_the_update_drops_its_intent() {
    // 1. An update that carried an edit; the snapshot of the carried edits on the new head.
    let f = Fixture::new();
    moved_under_edits(&f);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "mine\n");
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    let latest = std::fs::read(dir.join(SWARMWS_DIR).join("snapshot.json")).unwrap();
    let latest: serde_json::Value = serde_json::from_slice(&latest).unwrap();
    let carried = latest["snapshot"].as_str().unwrap().to_owned();
    assert_ne!(carried, done.changeset);

    // 2. The intent the cut leaves, and an edit of the carried file since.
    let intent = serde_json::json!({
        "from": done.snapshot.unwrap(),
        "to": carried,
        "prefixes": [],
        "stands_for": done.changeset,
    });
    let journal = dir.join(SWARMWS_DIR).join("journal");
    std::fs::write(journal.join("materialize.json"), intent.to_string()).unwrap();
    write(&dir, "src/b.txt", "mine since\n");

    // 3. Opened: the intent gone, the edit kept, the workspace on the new head.
    assert_eq!(
        changes(&f.ctx(), "w", true),
        [
            change("src/a.txt", WsChangeKind::Modified),
            change("src/b.txt", WsChangeKind::Modified),
        ]
    );
    assert!(!journal.join("materialize.json").exists());
    assert!(tree_of(&dir).contains(&file("src/b.txt", "mine since\n")));
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(saved.materialized, done.changeset);
}

/// A base that ignores `build/`, with `a.txt`; workspace `w` of the whole tree on it; `main` then
/// adding `build/out.txt` and `gen/x.txt`.
fn adds_under_ignores(f: &Fixture) {
    let base = f.commit(
        None,
        &[
            Put::File(".gitignore", "build/\n"),
            Put::File("a.txt", "a\n"),
        ],
    );
    f.create("w", &base, &[]);
    f.commit(
        Some(&base),
        &[
            Put::File("build/out.txt", "theirs\n"),
            Put::File("gen/x.txt", "x\n"),
        ],
    );
}

/// What a refused update must leave as it was: the newest operation, `workspace.json`'s bytes,
/// whether a pending record is there, and the files of workspace `w`.
fn untouched(f: &Fixture) -> (u64, String, bool, Vec<(String, char, String)>) {
    let dir = f.beside("w");
    let swarmws = dir.join(SWARMWS_DIR);
    (
        f.last_op(),
        std::fs::read_to_string(swarmws.join("workspace.json")).unwrap(),
        swarmws.join("journal/pending.json").exists(),
        tree_of(&dir),
    )
}

/// An ignored file at a path `main` adds is no edit the update snapshots: the update refuses it
/// before any store call (neither the snapshot nor the rebase is recorded), writes nothing, and
/// its hint names the way out.
#[test]
fn an_ignored_obstacle_is_refused_before_the_store_rebase() {
    // 1. An edit, which the update would snapshot, and an ignored file where `main` adds one.
    let f = Fixture::new();
    adds_under_ignores(&f);
    let dir = f.beside("w");
    write(&dir, "a.txt", "mine\n");
    write(&dir, "build/out.txt", "mine\n");
    let before = untouched(&f);

    // 2. Refused, naming the ignored file; nothing recorded or written.
    let refused = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
    assert_eq!(
        refused.details["paths"],
        serde_json::json!(["build/out.txt"])
    );
    let hint = refused.hint.clone().unwrap_or_default();
    assert!(hint.contains("move the file aside, or delete it"), "{hint}");
    assert_eq!(untouched(&f), before);
}

/// Every obstacle is named in one refusal: an ignored file at a path `main` adds, and a file the
/// agent added where `main` adds a directory, which no carry can put beside it.
#[test]
fn an_ignored_and_an_untracked_obstacle_are_both_listed() {
    let f = Fixture::new();
    adds_under_ignores(&f);
    let dir = f.beside("w");
    write(&dir, "build/out.txt", "mine\n");
    write(&dir, "gen", "mine\n");
    let before = untouched(&f);
    let refused = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap_err();
    assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
    assert_eq!(
        refused.details["paths"],
        serde_json::json!(["build/out.txt", "gen"])
    );
    assert_eq!(untouched(&f), before);
}

/// An ignored file that holds the bytes `main` adds at its path is no obstacle: the update goes
/// through, the edit carried, the file as it was.
#[test]
fn a_file_holding_the_target_bytes_passes() {
    let f = Fixture::new();
    adds_under_ignores(&f);
    let dir = f.beside("w");
    write(&dir, "a.txt", "mine\n");
    write(&dir, "build/out.txt", "theirs\n");
    let done = swarmops_ws::rebase(&f.ctx(), rebase_of("w", WsRebaseAction::Start)).unwrap();
    assert!(done.snapshot.is_some());
    let files = tree_of(&dir);
    for held in [
        file("a.txt", "mine\n"),
        file("build/out.txt", "theirs\n"),
        file("gen/x.txt", "x\n"),
    ] {
        assert!(files.contains(&held), "{held:?} in {files:?}");
    }
}

/// The edits `-m` commits are no obstacle at a path `main` changed too: the early check leaves
/// them to the rebase, which merges them.
#[test]
fn a_message_commits_edits_main_changed_too() {
    let f = Fixture::new();
    moved_under_edits(&f);
    let dir = f.beside("w");
    write(&dir, "src/c.txt", "one\n2\n3\n4\n5\n");
    let mut req = rebase_of("w", WsRebaseAction::Start);
    req.message = Some("mine".to_owned());
    let done = swarmops_ws::rebase(&f.ctx(), req).unwrap();
    assert_eq!(done.snapshot, None);
    assert_eq!(
        text_at(&f, &done.changeset, "src/c.txt"),
        "one\n2\n3\n4\nfive\n"
    );
    assert!(tree_of(&dir).contains(&file("src/c.txt", "one\n2\n3\n4\nfive\n")));
}
