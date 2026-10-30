//! `ws commit` and `ws diff`: hashing, sending only the blobs the store lacks, inline in the
//! commit, the replay of a crashed commit by its key, and the one diff renderer.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use camino::{Utf8Path, Utf8PathBuf};

use swarmops_api::{
    ChangeOp, ContentInput, DiffFormat, DiffReq, ModeInput, ReadTreeReq, StatusAfter, StoreOps,
    WsCommitReq, WsDiffReq, WsLandReq, WsRebaseReq,
};
use swarmops_ws::hostfs::HostMeta;
use swarmops_ws::state::{self, SWARMWS_DIR, Volume};
use swarmops_ws::{HostFs, RealFs, RealHost};

use crate::fixture::{BARE, Fixture, Put, Watched, changes, remove, write};

/// The request committing every change of workspace `name` with `message`.
fn commit_of(name: &str, message: &str) -> WsCommitReq {
    let mut out = WsCommitReq::new(name.to_owned(), message.to_owned());
    out.status_after = StatusAfter::Page;
    out
}

/// The pending record's path in workspace `dir`.
fn pending_of(dir: &camino::Utf8Path) -> camino::Utf8PathBuf {
    dir.join(SWARMWS_DIR).join("journal/pending.json")
}

/// A commit asks which blobs the store has, carries the one it lacks inline and every other
/// change by blob id, in one commit on the workspace's head; the workspace then stands on the
/// new changeset, clean.
#[test]
fn commit_inlines_only_missing_blobs_and_commits_blob_refs() {
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

    // 3. One question, no upload, one commit on the full head: the held blob by id, the
    //    missing one inline.
    assert!(watched.count("blob_has") >= 1);
    assert_eq!(watched.count("blob_put"), 0);
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
            ("src/fresh.txt", ChangeOp::Put, false),
            ("src/gone.txt", ChangeOp::Delete, false),
        ]
    );

    // 4. The workspace stands on it, its pending record gone; nothing more to commit.
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(
        (saved.head.as_str(), saved.materialized.as_str()),
        (&*done.changeset, &*done.changeset)
    );
    assert!(!pending_of(&dir).exists());
    assert_eq!(changes(&f.ctx(), "w", true), []);
    let empty = swarmops_ws::commit(&f.ctx(), commit_of("w", "nothing")).unwrap_err();
    assert_eq!(empty.code, "E_EMPTY_COMMIT", "{empty:?}");
}

/// A commit of 100 new files sends them in the one commit request: no `blob_put`, all 100
/// written by the commit.
#[test]
fn a_commit_of_many_new_files_makes_no_blob_put() {
    // 1. A workspace with 100 new files.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    for n in 0..100 {
        write(&dir, &format!("src/new{n:03}.txt"), &format!("new {n}\n"));
    }

    // 2. Committed: 100 blobs written, every put inline.
    let watched = Watched::new(&f.ops);
    let done = swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "many")).unwrap();
    assert_eq!((done.files_changed, done.blobs_written), (100, 100));
    let commits = watched.commits();
    let [(req, Some(_))] = commits.as_slice() else {
        panic!("{commits:?}");
    };
    let inline = (req.changes.iter())
        .filter(|change| matches!(change.content, Some(ContentInput::Inline(_))))
        .count();
    assert_eq!(inline, 100);

    // 3. The store asked which it has, then the commit; no blob put.
    let calls = watched.calls();
    let sent: Vec<&str> = (calls.iter().copied())
        .filter(|call| matches!(*call, "blob_has" | "blob_put" | "commit"))
        .collect();
    assert_eq!(sent, ["blob_has", "commit"], "{calls:?}");
    assert_eq!(changes(&f.ctx(), "w", true), []);
}

/// A file rewritten after the commit hashed it, before the commit read it to send, is
/// `E_IO_TRANSIENT`, naming it: nothing put or committed, the file left as it is now.
#[test]
fn a_file_changed_after_hashing_still_fails() {
    // 1. A workspace with a new file, over a host that rewrites it once it was first read.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/fresh.txt", "hashed\n");
    let host = Arc::new(EditAfterRead::new(
        &dir.join("src/fresh.txt"),
        "rewritten\n",
    ));
    let watched = Watched::new(&f.ops);
    let mut ctx = f.ctx_on(host);
    ctx.store = &watched;

    // 2. Refused, naming it; nothing sent that writes.
    let refused = swarmops_ws::commit(&ctx, commit_of("w", "racy")).unwrap_err();
    assert_eq!(refused.code, "E_IO_TRANSIENT", "{refused:?}");
    assert_eq!(refused.details["path"], "src/fresh.txt");
    for op in ["blob_put", "commit"] {
        assert_eq!(watched.count(op), 0, "{op}: {:?}", watched.calls());
    }
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, base);
}

/// The real host, but the file at `path` is rewritten with `text` once it was first read: an
/// edit between the commit's hash and its send.
struct EditAfterRead {
    /// The host under it.
    inner: RealHost,
    /// The file rewritten.
    path: Utf8PathBuf,
    /// What it holds after.
    text: &'static str,
    /// Whether it was rewritten yet.
    done: AtomicBool,
}

impl EditAfterRead {
    /// A host rewriting `path` with `text` after its first read.
    fn new(path: &Utf8Path, text: &'static str) -> EditAfterRead {
        EditAfterRead {
            inner: RealHost::default(),
            path: path.to_owned(),
            text,
            done: AtomicBool::new(false),
        }
    }
}

impl HostFs for EditAfterRead {
    fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_file(path, bytes)
    }

    fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
        self.inner.symlink(target, path)
    }

    fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
        self.inner.symlink_metadata(path)
    }

    fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
        // The bytes as read; the file rewritten after its first read.
        let bytes = self.inner.read_file(path)?;
        if path == self.path && !self.done.swap(true, Ordering::SeqCst) {
            std::fs::write(path, self.text)?;
        }
        Ok(bytes)
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        self.inner.read_dir(path)
    }

    fn remove_file(&self, path: &Utf8Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool> {
        self.inner.remove_dir_if_empty(path)
    }

    fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }

    fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
        self.inner.read_link(path)
    }

    fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()> {
        self.inner.set_mode(path, mode)
    }
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
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, base);
    assert!(pending_of(&dir).exists());

    // 3. Run again: the same key, the same changeset, replayed.
    let done = swarmops_ws::commit(&ctx, commit_of("w", "edit")).unwrap();
    assert!(done.replayed, "{done:?}");
    assert_eq!(done.changeset, first);
    let commits = watched.commits();
    assert_eq!(commits[0].0.idempotency_key, commits[1].0.idempotency_key);
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, first);
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
        let mut req = WsDiffReq::new("w".to_owned());
        req.format = format;
        req.context = 3;
        req.limit = limit;
        req.cursor = cursor;
        req.max_bytes = 32_768;
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
        let mut req = DiffReq::new(base.as_str(), done.changeset.as_str());
        req.format = format;
        req.context = 3;
        req.limit = limit;
        req.cursor = cursor;
        req.max_bytes = 32_768;
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

/// The head and prefixes of workspace `dir`'s state, which a refusal leaves as they were.
fn standing(dir: &Utf8Path) -> (String, Vec<String>) {
    let saved = state::read(&RealFs::new(), dir).unwrap();
    (saved.head, saved.prefixes)
}

/// The paths of a refusal's `details.paths`.
fn refused_paths(refused: &swarmops_api::OpError) -> Vec<&str> {
    (refused.details["paths"].as_array().unwrap().iter())
        .map(|path| path.as_str().unwrap())
        .collect()
}

/// A commit naming a path outside a sparse workspace's prefixes is `E_WS_OUTSIDE_PREFIXES`
/// naming it: the prefixes are not widened, and the store commits nothing.
#[test]
fn a_named_path_outside_the_prefixes_is_refused() {
    // 1. A workspace of `src`; a file written outside it.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "docs/a", "doc\n");

    // 2. A commit naming it: refused, naming it, with the hint that widens the prefixes.
    let watched = Watched::new(&f.ops);
    let mut req = commit_of("w", "outside");
    req.paths = vec!["docs/a".to_owned()];
    let refused = swarmops_ws::commit(&f.ctx_over(&watched), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused_paths(&refused), ["docs/a"]);
    assert_eq!(
        refused.hint.as_deref(),
        Some("swarmops ws materialize docs/a")
    );

    // 3. Nothing changed: head and prefixes, no store commit, no pending record.
    assert_eq!(standing(&dir), (base, vec!["src".to_owned()]));
    assert_eq!(watched.count("commit"), 0, "{:?}", watched.calls());
    assert!(!pending_of(&dir).exists());
}

/// A commit naming no path meets the edits outside the prefixes too: the topmost entry of each,
/// a directory not entered, refused in path order; the edit inside is not committed alone.
#[test]
fn an_unnamed_edit_outside_the_prefixes_is_refused() {
    // 1. A workspace of `src`: an edit inside, a directory and a file outside.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/x", "x\n");
    write(&dir, "docs/a", "doc\n");
    write(&dir, "README", "readme\n");

    // 2. Refused, naming `README` and `docs`.
    let watched = Watched::new(&f.ops);
    let refused =
        swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "unnamed")).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused_paths(&refused), ["README", "docs"]);

    // 3. Nothing changed.
    assert_eq!(standing(&dir), (base, vec!["src".to_owned()]));
    assert_eq!(watched.count("commit"), 0, "{:?}", watched.calls());
    assert!(!pending_of(&dir).exists());
}

/// An outside path an ignore file on disk names is no edit: build output outside the prefixes
/// does not stop a commit, and is not committed.
#[test]
fn an_ignored_outside_path_is_not_refused() {
    // 1. A workspace of `src` and the root `.gitignore`, which names `target/`.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", "a\n"),
            Put::File(".gitignore", "target/\n"),
        ],
    );
    f.create("w", &base, &["src", ".gitignore"]);
    let dir = f.beside("w");
    write(&dir, "target/debug/out", "built\n");
    write(&dir, "src/a.txt", "edited\n");

    // 2. Committed: the edit inside, nothing under `target/`.
    let watched = Watched::new(&f.ops);
    swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "build")).unwrap();
    let commits = watched.commits();
    let [(req, Some(_))] = commits.as_slice() else {
        panic!("{commits:?}");
    };
    let paths: Vec<&str> = req.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["src/a.txt"]);
}

/// A workspace made with neither prefixes nor `all` holds the whole tree, and so does one an
/// older build wrote as `all` false: a named commit leaves no untouched file listed, the chain
/// lands as it is, a mode commit goes through, and the prefixes stay empty.
#[test]
fn an_empty_prefix_workspace_commits_a_named_path() {
    for how in BARE {
        // 1. A workspace with no prefixes, the CLI's default, on a volume without the
        //    executable bit.
        let f = Fixture::new();
        let base = f.commit(
            None,
            &[
                Put::File("README", "readme\n"),
                Put::File("bin/run", "#!/bin/sh\n"),
                Put::File("src/a.txt", "a\n"),
            ],
        );
        let dir = f.create_bare("w", &base, how);
        f.simulate("w", Volume::new(false, false, true));

        // 2. A commit naming an edited path: committed, nothing else left listed.
        write(&dir, "src/a.txt", "edited\n");
        let mut named = commit_of("w", "named");
        named.paths = vec!["src/a.txt".to_owned()];
        let first = swarmops_ws::commit(&f.ctx(), named).unwrap();
        assert_eq!(first.files_changed, 1, "{how:?}");
        assert_eq!(changes(&f.ctx(), "w", true), [], "{how:?}");

        // 3. The chain lands as it is.
        let mut land = WsLandReq::new("w".to_owned());
        land.status_after = StatusAfter::None;
        let landed = swarmops_ws::land(&f.ctx(), land).unwrap();
        assert_eq!(landed.land.rebased_onto, base, "{how:?}");

        // 4. A commit of a mode alone: committed; the prefixes are still empty.
        let mut moded = commit_of("w", "mode");
        moded.modes = BTreeMap::from([("bin/run".to_owned(), ModeInput::Exec)]);
        let second = swarmops_ws::commit(&f.ctx(), moded).unwrap();
        assert_eq!(second.files_changed, 1, "{how:?}");
        assert_eq!(standing(&dir), (second.changeset, Vec::new()), "{how:?}");
    }
}

/// A deleted file is committed in a workspace with neither prefixes nor `all`, as in one of the
/// whole tree.
#[test]
fn an_empty_prefix_workspace_commits_a_deletion() {
    for how in BARE {
        // 1. A workspace with no prefixes: one file removed, one edited.
        let f = Fixture::new();
        let base = f.commit(
            None,
            &[
                Put::File("README", "readme\n"),
                Put::File("src/a.txt", "a\n"),
            ],
        );
        let dir = f.create_bare("w", &base, how);
        remove(&dir, "src/a.txt");
        write(&dir, "README", "edited\n");

        // 2. Committed: the deletion and the edit, nothing left listed.
        let watched = Watched::new(&f.ops);
        swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "both")).unwrap();
        let commits = watched.commits();
        let [(req, Some(_))] = commits.as_slice() else {
            panic!("{how:?} {commits:?}");
        };
        let sent: Vec<(&str, bool)> = (req.changes.iter())
            .map(|c| (c.path.as_str(), c.op == ChangeOp::Delete))
            .collect();
        assert_eq!(sent, [("README", false), ("src/a.txt", true)], "{how:?}");
        assert_eq!(changes(&f.ctx(), "w", true), [], "{how:?}");
    }
}

/// Under a nested prefix, a file in a directory on the path to it is outside too: with prefix
/// `src/a`, a stray `src/stray` is refused by its own path, the edit under `src/a` not committed.
#[test]
fn a_stray_beside_a_nested_prefix_is_refused() {
    // 1. A workspace of `src/a`; an edit under it and a file written beside it, in `src`.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a/f", "f\n"), Put::File("src/b", "b\n")],
    );
    f.create("w", &base, &["src/a"]);
    let dir = f.beside("w");
    write(&dir, "src/a/f", "edited\n");
    write(&dir, "src/stray", "stray\n");

    // 2. Refused, naming `src/stray` alone.
    let watched = Watched::new(&f.ops);
    let refused = swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "nested")).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused_paths(&refused), ["src/stray"]);

    // 3. Nothing changed.
    assert_eq!(standing(&dir), (base, vec!["src/a".to_owned()]));
    assert_eq!(watched.count("commit"), 0, "{:?}", watched.calls());
}

/// `ws rebase -m` commits the edits under the prefixes and leaves one outside them where it is
/// (the M15 design §5 leaves `ws rebase` as it was): not refused, not committed, not removed.
#[test]
fn rebase_m_leaves_an_outside_edit() {
    // 1. A workspace of `src` with an edit inside and a stray outside; main moved since.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/a", "mine\n");
    write(&dir, "docs/a", "stray\n");
    let main = f.commit(Some(&base), &[Put::File("src/b", "b\n")]);

    // 2. Rebased with a message: the edit inside committed and moved onto main.
    let mut req = WsRebaseReq::new("w".to_owned());
    req.message = Some("mine".to_owned());
    req.status_after = StatusAfter::None;
    let done = swarmops_ws::rebase(&f.ctx(), req).unwrap();
    assert_eq!(done.onto, main);

    // 3. The rebased tree holds `src` alone; the stray is still on disk.
    let listing = f
        .ops
        .read_tree(ReadTreeReq::new(done.changeset.as_str()))
        .unwrap();
    let names: Vec<&str> = (listing.entries.iter())
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["src"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/a")).unwrap(),
        "stray\n"
    );
}

/// A refusal names at most 20 paths, the first in path order, and its hint widens to the first.
#[test]
fn a_refusal_names_the_first_twenty_paths() {
    // 1. A workspace of `src`; 25 files outside it, written last to first.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    for n in (0..25).rev() {
        write(&dir, &format!("o{n:02}"), "stray\n");
    }

    // 2. Refused: `o00` to `o19` in the details, the hint naming `o00`.
    let refused = swarmops_ws::commit(&f.ctx(), commit_of("w", "many")).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    let first: Vec<String> = (0..20).map(|n| format!("o{n:02}")).collect();
    assert_eq!(refused_paths(&refused), first);
    assert_eq!(refused.hint.as_deref(), Some("swarmops ws materialize o00"));
}

/// A mode set on a path outside the prefixes is refused as a named path is, before the scan:
/// the prefixes not widened, nothing committed.
#[test]
fn a_mode_outside_the_prefixes_is_refused() {
    // 1. A workspace of `src`, on a volume without the executable bit; `docs/run` in the tree
    //    outside it.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("docs/run", "#!/bin/sh\n"),
            Put::File("src/a.txt", "a\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    f.simulate("w", Volume::new(false, false, true));
    let dir = f.beside("w");

    // 2. A commit setting its mode: refused, naming it.
    let watched = Watched::new(&f.ops);
    let mut req = commit_of("w", "mode");
    req.modes = BTreeMap::from([("docs/run".to_owned(), ModeInput::Exec)]);
    let refused = swarmops_ws::commit(&f.ctx_over(&watched), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused_paths(&refused), ["docs/run"]);

    // 3. Nothing changed.
    assert_eq!(standing(&dir), (base, vec!["src".to_owned()]));
    assert_eq!(watched.count("commit"), 0, "{:?}", watched.calls());
}

/// A `.git` directory at the root of a sparse workspace is skipped, as every `.git` is: no edit
/// outside the prefixes, and not committed.
#[test]
fn a_root_dot_git_is_not_refused() {
    // 1. A workspace of `src` with an edit, and a `.git` directory at its root.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, ".git/HEAD", "ref: refs/heads/main\n");
    write(&dir, "src/a.txt", "edited\n");

    // 2. Committed: the edit alone.
    let watched = Watched::new(&f.ops);
    swarmops_ws::commit(&f.ctx_over(&watched), commit_of("w", "git")).unwrap();
    let commits = watched.commits();
    let [(req, Some(_))] = commits.as_slice() else {
        panic!("{commits:?}");
    };
    let paths: Vec<&str> = req.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["src/a.txt"]);
}
