//! `ws commit` and `ws diff`: hashing, uploading only the blobs the store lacks, the replay of a
//! crashed commit by its key, and the one diff renderer.

use std::sync::atomic::Ordering;

use swarmops_api::{
    ChangeOp, ContentInput, DiffFormat, DiffReq, StatusAfter, StoreOps, WsCommitReq, WsDiffReq,
};
use swarmops_ws::RealFs;
use swarmops_ws::state::{self, SWARMWS_DIR};

use crate::fixture::{Fixture, Put, Watched, changes, remove, write};

/// The request committing every change of workspace `name` with `message`.
fn commit_of(name: &str, message: &str) -> WsCommitReq {
    WsCommitReq {
        workspace: name.to_owned(),
        message: message.to_owned(),
        paths: Vec::new(),
        modes: std::collections::BTreeMap::new(),
        status_after: StatusAfter::Page,
    }
}

/// The pending record's path in workspace `dir`.
fn pending_of(dir: &camino::Utf8Path) -> camino::Utf8PathBuf {
    dir.join(SWARMWS_DIR).join("journal/pending.json")
}

/// A commit asks which blobs the store has, uploads only the one it lacks, and commits every
/// change by blob id on the workspace's head; the workspace then stands on the new changeset,
/// clean.
#[test]
fn commit_uploads_only_missing_blobs_and_commits_blob_refs() {
    // 1. A workspace, a file added with content the store holds, one with content it lacks,
    //    one deleted.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", "shared\n"),
            Put::File("src/gone.txt", "gone\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/copy.txt", "shared\n");
    write(&dir, "src/fresh.txt", "fresh\n");
    remove(&dir, "src/gone.txt");

    // 2. Committed through a store that records its calls.
    let watched = Watched::new(&f.ops);
    let ctx = f.ctx_over(&watched);
    let done = swarmops_ws::commit(&ctx, commit_of("w", "add two")).unwrap();
    assert_eq!(done.parent, base);
    assert_eq!((done.files_changed, done.blobs_written), (3, 1));
    assert!(!done.replayed);
    assert_eq!(done.status_after.unwrap().changes, []);

    // 3. One question, one upload, one commit, every put by blob id, on the full head.
    assert!(watched.count("blob_has") >= 1);
    assert_eq!(watched.count("blob_put"), 1);
    let commits = watched.commits();
    let [(req, Some(made))] = commits.as_slice() else {
        panic!("{commits:?}");
    };
    assert_eq!(made, &done.changeset);
    assert_eq!(req.parent.as_ref().map(ToString::to_string), Some(base));
    assert!(req.idempotency_key.is_some());
    let mut ops: Vec<(&str, ChangeOp, bool)> = (req.changes.iter())
        .map(|change| {
            let by_id = matches!(change.content, Some(ContentInput::Blob(_)));
            (change.path.as_str(), change.op, by_id)
        })
        .collect();
    ops.sort_by_key(|op| op.0);
    assert_eq!(
        ops,
        [
            ("src/copy.txt", ChangeOp::Put, true),
            ("src/fresh.txt", ChangeOp::Put, true),
            ("src/gone.txt", ChangeOp::Delete, false),
        ]
    );

    // 4. The workspace stands on it, its pending record gone; nothing more to commit.
    let saved = state::read(&RealFs, &dir).unwrap();
    assert_eq!(
        (saved.head.as_str(), saved.materialized.as_str()),
        (&*done.changeset, &*done.changeset)
    );
    assert!(!pending_of(&dir).exists());
    assert_eq!(changes(&f.ctx(), "w", true), []);
    let empty = swarmops_ws::commit(&f.ctx(), commit_of("w", "nothing")).unwrap_err();
    assert_eq!(empty.code, "E_EMPTY_COMMIT", "{empty:?}");
}

/// A commit killed after the store recorded it, before `workspace.json` names it, leaves its
/// key in the pending record: the next commit replays the same changeset by that key.
#[test]
fn a_crash_between_store_commit_and_workspace_update_replays_the_same_changeset() {
    // 1. A workspace with an edit.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "edited\n");

    // 2. The store records the commit; the process dies before the workspace learns of it.
    let watched = Watched::new(&f.ops);
    watched.crash_after_commit.store(true, Ordering::SeqCst);
    let ctx = f.ctx_over(&watched);
    swarmops_ws::commit(&ctx, commit_of("w", "edit")).unwrap_err();
    let first = watched.commits()[0].1.clone().unwrap();
    assert_eq!(state::read(&RealFs, &dir).unwrap().head, base);
    assert!(pending_of(&dir).exists());

    // 3. Run again: the same key, the same changeset, replayed.
    let done = swarmops_ws::commit(&ctx, commit_of("w", "edit")).unwrap();
    assert!(done.replayed, "{done:?}");
    assert_eq!(done.changeset, first);
    let commits = watched.commits();
    assert_eq!(commits[0].0.idempotency_key, commits[1].0.idempotency_key);
    assert_eq!(state::read(&RealFs, &dir).unwrap().head, first);
    assert!(!pending_of(&dir).exists());
    assert_eq!(changes(&f.ctx(), "w", true), []);
}

/// A file past the 512 MiB an object holds is `E_TOO_LARGE`, naming it, before the store is
/// asked anything that writes (a sparse file: nothing is read).
#[cfg(not(windows))]
#[test]
fn a_file_over_512_mib_is_refused_before_anything_is_sent() {
    // 1. A workspace with a file one byte past the cap.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/small.txt", "small\n");
    let big = std::fs::File::create(dir.join("src/big.bin")).unwrap();
    big.set_len(swarmops_store::MAX_OBJECT_LEN + 1).unwrap();

    // 2. Refused, naming it; no blob asked about, sent, or committed.
    let watched = Watched::new(&f.ops);
    let refused = swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "big")).unwrap_err();
    assert_eq!(refused.code, "E_TOO_LARGE", "{refused:?}");
    assert_eq!(refused.details["path"], "src/big.bin");
    for op in ["blob_has", "blob_put", "commit"] {
        assert_eq!(watched.count(op), 0, "{op}: {:?}", watched.calls());
    }
    assert!(!pending_of(&dir).exists());
}

/// `ws diff` of a workspace's edits is the store's diff of the changeset that commits them, file
/// for file in each format and page, but for its `to`: one renderer.
#[cfg(unix)]
#[test]
fn ws_diff_matches_store_diff_after_commit() {
    use crate::fixture::set_exec;

    // 1. A workspace with text, binary, kind and mode edits.
    let f = Fixture::new();
    let lines: Vec<String> = (0..40).map(|n| format!("line {n}")).collect();
    let lines = lines.join("\n") + "\n";
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", &lines),
            Put::File("src/b.txt", "b\n"),
            Put::File("src/c.txt", "c\n"),
            Put::File("src/run.sh", "#!/bin/sh\n"),
            Put::Link("src/l", "a.txt"),
        ],
    );
    f.create("w", &base, &["src"]);
    let ctx = f.ctx();
    let dir = f.beside("w");
    write(
        &dir,
        "src/a.txt",
        &lines
            .replace("line 3\n", "three\n")
            .replace("line 30\n", ""),
    );
    write(&dir, "src/new.txt", "new\n");
    std::fs::write(dir.join("src/bin.dat"), [0u8, 1, 2, 3]).unwrap();
    remove(&dir, "src/b.txt");
    set_exec(&dir, "src/run.sh", true);
    remove(&dir, "src/l");
    write(&dir, "src/l", "a.txt");

    // 2. The workspace's diffs, in each format, one page and pages of two files.
    let ws_diff = |format, limit, cursor| {
        let req = WsDiffReq {
            workspace: "w".to_owned(),
            paths: Vec::new(),
            format,
            context: 3,
            limit,
            cursor,
            max_bytes: 32_768,
        };
        swarmops_ws::diff(&ctx, req).unwrap()
    };
    let formats = [DiffFormat::Names, DiffFormat::Stat, DiffFormat::Unified];
    let before: Vec<_> = formats.iter().map(|&fmt| ws_diff(fmt, 100, None)).collect();
    let first = ws_diff(DiffFormat::Unified, 2, None);
    let second = ws_diff(DiffFormat::Unified, 2, first.cursor.clone());

    // 3. The commit of the same edits.
    let done = swarmops_ws::commit(&ctx, commit_of("w", "edits")).unwrap();

    // 4. The store's diff of it says the same of each file, page for page.
    let store_diff = |format, limit, cursor| {
        let req = DiffReq {
            from: base.as_str().into(),
            to: done.changeset.as_str().into(),
            paths: Vec::new(),
            format,
            context: 3,
            limit,
            cursor,
            max_bytes: 32_768,
        };
        f.ops.diff(req).unwrap()
    };
    for (fmt, ws) in formats.iter().zip(&before) {
        let store = store_diff(*fmt, 100, None);
        assert_eq!((ws.from.as_str(), ws.to.as_str()), (&*base, "ws:w"));
        assert_eq!(ws.files, store.files, "{fmt:?}");
        assert_eq!((ws.truncated, &ws.cursor), (store.truncated, &store.cursor));
    }
    let store_first = store_diff(DiffFormat::Unified, 2, None);
    let store_second = store_diff(DiffFormat::Unified, 2, store_first.cursor.clone());
    assert_eq!(first.files, store_first.files);
    assert_eq!(first.cursor, store_first.cursor);
    assert_eq!(second.files, store_second.files);
    assert_eq!(before[0].files.len(), 6);
}
