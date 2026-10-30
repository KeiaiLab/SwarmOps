//! `ws status` and `ws discard` without paths: the scan, the index, the racy rule, the ignore
//! files, and how far the target has moved.

use swarmops_api::WsChangeKind::{Added, Deleted, ModeChanged, Modified, TypeChanged};
use swarmops_api::{
    Identity, RefSetReq, StatusAfter, StoreOps, WsDiscardReq, WsState, WsStatusReq,
};
use swarmops_ws::state::SWARMWS_DIR;

use crate::fixture::{AGENT, Fixture, Put, Watched, change, changes, remove, tree_of, write};

/// The index's path in workspace `dir`.
fn index_of(dir: &camino::Utf8Path) -> camino::Utf8PathBuf {
    dir.join(SWARMWS_DIR).join("index.bin")
}

/// The time the index of workspace `dir` was written at: its header's `written_ns`, the eight
/// bytes after `"SWWI" u8 u8 u16 u64`.
fn written_ns(dir: &camino::Utf8Path) -> i64 {
    let bytes = std::fs::read(index_of(dir)).unwrap();
    assert_eq!(&bytes[..4], b"SWWI");
    i64::from_le_bytes(bytes[16..24].try_into().unwrap())
}

/// The status request of workspace `name`'s first page.
fn page_of(name: &str) -> WsStatusReq {
    let mut out = WsStatusReq::new(name.to_owned());
    out.limit = 500;
    out
}

/// Every kind of change is found, by the index and by hashing alike: a file added, one edited,
/// one deleted, a file that became a symlink and a symlink that became a file, and an executable
/// bit set and cleared.
#[cfg(unix)]
#[test]
fn status_finds_added_modified_deleted_type_and_mode_changes() {
    use crate::fixture::{set_exec, symlink};

    // 1. A workspace of a tree holding each kind of entry.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/a.txt", "a\n"),
            Put::File("src/b.txt", "b\n"),
            Put::File("src/c.txt", "c\n"),
            Put::Link("src/l", "a.txt"),
            Put::Exec("src/run.sh", "#!/bin/sh\n"),
            Put::File("src/plain.sh", "#!/bin/sh\n"),
            Put::File("README", "readme\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    let ctx = f.ctx();
    let dir = f.beside("w");
    assert_eq!(changes(&ctx, "w", false), []);

    // 2. One edit of each kind.
    write(&dir, "src/new.txt", "new\n");
    write(&dir, "src/a.txt", "edited\n");
    remove(&dir, "src/b.txt");
    symlink(&dir, "src/c.txt", "a.txt");
    remove(&dir, "src/l");
    write(&dir, "src/l", "a.txt");
    set_exec(&dir, "src/run.sh", false);
    set_exec(&dir, "src/plain.sh", true);

    // 3. Found, in path order, by the index and by hashing every file.
    let expected = [
        change("src/a.txt", Modified),
        change("src/b.txt", Deleted),
        change("src/c.txt", TypeChanged),
        change("src/l", TypeChanged),
        change("src/new.txt", Added),
        change("src/plain.sh", ModeChanged),
        change("src/run.sh", ModeChanged),
    ];
    assert_eq!(changes(&ctx, "w", false), expected);
    assert_eq!(changes(&ctx, "w", true), expected);

    // 4. The page says what the workspace stands on, the sizes of what is there.
    let page = swarmops_ws::status(&ctx, page_of("w")).unwrap();
    assert_eq!((page.base.as_str(), page.head.as_str()), (&*base, &*base));
    assert_eq!((page.main.as_str(), page.behind_by), (&*base, 0));
    assert_eq!(page.state, WsState::Clean);
    assert_eq!(page.rebasing, None);
    let sizes: Vec<Option<u64>> = page.changes.iter().map(|c| c.size).collect();
    assert_eq!(
        sizes,
        [Some(7), None, Some(5), Some(5), Some(4), Some(10), Some(10)]
    );

    // 5. Paged: two at a time, the same changes in the same order.
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let mut req = page_of("w");
        req.limit = 2;
        req.cursor = cursor.take();
        let page = swarmops_ws::status(&ctx, req).unwrap();
        assert!(page.changes.len() <= 2);
        paged.extend(page.changes.into_iter().map(|c| (c.path, c.kind)));
        if !page.truncated {
            break;
        }
        cursor = page.cursor;
    }
    assert_eq!(paged, expected);

    // 6. Only the paths asked for.
    let mut req = page_of("w");
    req.paths = vec!["src/a.txt".to_owned(), "src/new.txt".to_owned()];
    let only: Vec<_> = (swarmops_ws::status(&ctx, req).unwrap().changes.into_iter())
        .map(|c| (c.path, c.kind))
        .collect();
    assert_eq!(only, [expected[0].clone(), expected[4].clone()]);
}

/// A file rewritten in place with as many bytes, its mtime put back where it was, at or after
/// the time the index was written: its stat matches the index's row, and the racy rule hashes
/// it all the same.
#[test]
fn a_same_size_edit_within_the_racy_window_is_found() {
    use std::time::{Duration, SystemTime};

    // 1. A workspace whose file has an mtime ahead of any index written now.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "aaaa\n")]);
    f.create("w", &base, &["src"]);
    let ctx = f.ctx();
    let dir = f.beside("w");
    let ahead = SystemTime::now() + Duration::from_secs(3600);
    let set_mtime = |at: SystemTime| {
        let file = std::fs::File::options()
            .write(true)
            .open(dir.join("src/a.txt"))
            .unwrap();
        file.set_modified(at).unwrap();
    };
    set_mtime(ahead);

    // 2. A status hashes it and records its stat in the index, written before that mtime.
    assert_eq!(changes(&ctx, "w", false), []);
    let index_time = written_ns(&dir);
    let ahead_ns = ahead.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    assert!(i128::from(index_time) <= i128::try_from(ahead_ns.as_nanos()).unwrap());

    // 3. The same size written in place (the same inode), the mtime put back: found.
    std::fs::write(dir.join("src/a.txt"), "bbbb\n").unwrap();
    set_mtime(ahead);
    assert_eq!(changes(&ctx, "w", false), [change("src/a.txt", Modified)]);
    assert_eq!(changes(&ctx, "w", true), [change("src/a.txt", Modified)]);
}

/// A file whose bytes a status confirmed in the tick its index was written, then rewritten in
/// place with as many bytes in that same tick, is found by every later status: an index written
/// later, by a status that refreshed another row, does not turn its racy row into a trusted one.
#[test]
fn a_racy_row_stays_racy_in_a_later_index() {
    use std::time::{Duration, SystemTime};

    // 1. A workspace of two files; `src/e.txt` stat'd as written in each scan's starting tick.
    let f = Fixture::new();
    let puts = [
        Put::File("src/a.txt", "0123\n"),
        Put::File("src/e.txt", "0123\n"),
    ];
    let base = f.commit(None, &puts);
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    let host = std::sync::Arc::new(same_tick::SameTick::new(&dir, "src/e.txt"));
    let ctx = f.ctx_on(host.clone());

    // 2. A status confirms its bytes and writes an index at that very tick: a racy row.
    assert_eq!(changes(&ctx, "w", false), []);
    let tick = host.pinned().unwrap();
    assert_eq!(written_ns(&dir), tick);

    // 3. The same size written in place within the tick (the same stat), then the clock past it.
    std::fs::write(dir.join("src/e.txt"), "1032\n").unwrap();
    same_tick::wait_past(&dir, tick);

    // 4. A status that refreshes another row writes a later index; the edit is still found.
    let file = std::fs::File::options()
        .write(true)
        .open(dir.join("src/a.txt"))
        .unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    assert_eq!(changes(&ctx, "w", false), [change("src/e.txt", Modified)]);
    assert!(written_ns(&dir) > tick);
    assert_eq!(changes(&ctx, "w", false), [change("src/e.txt", Modified)]);
    assert_eq!(changes(&ctx, "w", true), [change("src/e.txt", Modified)]);

    // 5. A status of another path alone writes a later index: the edit is still found.
    std::fs::write(dir.join("src/e.txt"), "0123\n").unwrap();
    host.unpin();
    assert_eq!(changes(&ctx, "w", false), []);
    let tick = host.pinned().unwrap();
    assert_eq!(written_ns(&dir), tick);
    std::fs::write(dir.join("src/e.txt"), "1032\n").unwrap();
    same_tick::wait_past(&dir, tick);
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2))
        .unwrap();
    let mut req = page_of("w");
    req.paths = vec!["src/a.txt".to_owned()];
    assert_eq!(swarmops_ws::status(&ctx, req).unwrap().changes, []);
    assert!(written_ns(&dir) > tick);
    assert_eq!(changes(&ctx, "w", false), [change("src/e.txt", Modified)]);
}

/// A host that reports one file as last written in the tick the first scan that stats it
/// started: the filesystem's coarse clock, made certain.
mod same_tick {
    use std::io;
    use std::sync::{Arc, Mutex};

    use camino::{Utf8Path, Utf8PathBuf};
    use swarmops_ws::hostfs::HostMeta;
    use swarmops_ws::state::SWARMWS_DIR;
    use swarmops_ws::{HostFs, RealHost};

    /// The real host, but for the mtime of `path`.
    pub struct SameTick {
        /// The host under it.
        inner: Arc<dyn HostFs>,
        /// The workspace's root.
        dir: Utf8PathBuf,
        /// The file whose mtime is pinned.
        path: Utf8PathBuf,
        /// The pinned mtime, once a scan stat'd the file.
        pinned: Mutex<Option<i64>>,
    }

    impl SameTick {
        /// A host pinning the mtime of `path` in the workspace at `dir`.
        pub fn new(dir: &Utf8Path, path: &str) -> SameTick {
            SameTick {
                inner: Arc::new(RealHost::default()),
                dir: dir.to_owned(),
                path: dir.join(path),
                pinned: Mutex::new(None),
            }
        }

        /// Pins the file's mtime again, at the tick of the next scan that stats it.
        pub fn unpin(&self) {
            *self.pinned.lock().unwrap() = None;
        }

        /// The pinned mtime, if a scan stat'd the file yet.
        pub fn pinned(&self) -> Option<i64> {
            *self.pinned.lock().unwrap()
        }
    }

    /// The mtime of the scan's temp index in `.swarmws/`: the tick the scan started at.
    fn scan_tick(dir: &Utf8Path) -> i64 {
        let found = std::fs::read_dir(dir.join(SWARMWS_DIR))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_name().to_str().is_some_and(is_temp))
            .map(|entry| mtime_ns(&entry.metadata().unwrap()))
            .max();
        found.unwrap()
    }

    /// Whether `name` is a scan's temp index.
    fn is_temp(name: &str) -> bool {
        name.starts_with("index.") && name != "index.bin"
    }

    /// The mtime of `meta`, in Unix nanoseconds.
    fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
        let since = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        i64::try_from(since.as_nanos()).unwrap()
    }

    /// Returns once a file made in workspace `dir`'s `.swarmws/` has an mtime past `tick`.
    pub fn wait_past(dir: &Utf8Path, tick: i64) {
        loop {
            let probe = tempfile::NamedTempFile::new_in(dir.join(SWARMWS_DIR)).unwrap();
            if mtime_ns(&probe.as_file().metadata().unwrap()) > tick {
                return;
            }
            std::thread::yield_now();
        }
    }

    impl HostFs for SameTick {
        fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
            self.inner.write_file(path, bytes)
        }

        fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
            self.inner.symlink(target, path)
        }

        fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
            // The real stat; the pinned file's mtime the first scan's tick from then on.
            let meta = self.inner.symlink_metadata(path)?;
            if path != self.path {
                return Ok(meta);
            }
            let mut pinned = self.pinned.lock().unwrap();
            let tick = *pinned.get_or_insert_with(|| scan_tick(&self.dir));
            Ok(meta.map(|mut meta| {
                meta.mtime_ns = tick;
                meta
            }))
        }

        fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
            self.inner.read_file(path)
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
}

/// The untracked walk reads the tree's `.gitignore` and `.swarmignore` files with no `.git`
/// anywhere, lists dotfiles the agent adds, never lists a `.git` directory's files, and reads
/// nothing above the workspace's root and no `.ignore` file.
#[test]
fn status_honours_gitignore_and_swarmignore_and_skips_nested_git() {
    // 1. A tree with its ignore files.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File(".gitignore", "*.log\n/target/\n"),
            Put::File("src/.swarmignore", "gen/\n"),
            Put::File("src/main.rs", "fn main() {}\n"),
        ],
    );
    f.create("w", &base, &[]);
    let ctx = f.ctx();
    let dir = f.beside("w");

    // 2. Ignore files that must change nothing: one above the root, one `.ignore` in the tree.
    write(dir.parent().unwrap(), ".gitignore", "*\n");
    write(&dir, ".ignore", "*\n");

    // 3. What the agent and its build add.
    write(&dir, "x.log", "log\n");
    write(&dir, "target/debug/out", "bin\n");
    write(&dir, "src/gen/g.rs", "gen\n");
    write(&dir, "src/sub/trace.log", "log\n");
    write(&dir, ".github/w.yml", "on: push\n");
    write(&dir, ".env", "KEY=1\n");
    write(&dir, "vendor/.git/config", "[core]\n");
    write(&dir, "vendor/lib.rs", "lib\n");

    // 4. Listed: the dotfiles, the `.ignore`, the nested repository's files but its `.git`.
    let expected = [
        change(".env", Added),
        change(".github/w.yml", Added),
        change(".ignore", Added),
        change("vendor/lib.rs", Added),
    ];
    assert_eq!(changes(&ctx, "w", false), expected);
    assert_eq!(changes(&ctx, "w", true), expected);
}

/// A tracked file is never ignored: a `.gitignore` that matches it leaves it clean, and an edit
/// of it is found, by the index and by hashing alike.
#[test]
fn a_tracked_file_matching_gitignore_is_neither_deleted_nor_ignored() {
    // 1. A tree holding a file its own `.gitignore` matches.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File(".gitignore", "*.gen\n"),
            Put::File("src/a.gen", "generated\n"),
        ],
    );
    f.create("w", &base, &[]);
    let ctx = f.ctx();
    let dir = f.beside("w");

    // 2. Clean: neither deleted nor listed.
    assert_eq!(changes(&ctx, "w", false), []);
    assert_eq!(changes(&ctx, "w", true), []);

    // 3. Edited: modified; a new file it matches is still ignored.
    write(&dir, "src/a.gen", "regenerated\n");
    write(&dir, "src/b.gen", "new\n");
    assert_eq!(changes(&ctx, "w", false), [change("src/a.gen", Modified)]);
    assert_eq!(changes(&ctx, "w", true), [change("src/a.gen", Modified)]);
}

/// An index with one byte flipped fails its checksum: status rebuilds it by hashing against the
/// head's tree, and finds what hashing every file finds.
#[test]
fn a_corrupt_index_is_rebuilt_by_hashing() {
    // 1. A workspace with an index, then an edit.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a.txt", "a\n"), Put::File("src/b.txt", "b\n")],
    );
    f.create("w", &base, &["src"]);
    let ctx = f.ctx();
    let dir = f.beside("w");
    assert_eq!(changes(&ctx, "w", false), []);
    write(&dir, "src/b.txt", "edited\n");

    // 2. Each byte of the index flipped in turn: the status equals the full one.
    let good = std::fs::read(index_of(&dir)).unwrap();
    for at in [0, 5, 20, good.len() / 2, good.len() - 1] {
        let mut bad = good.clone();
        bad[at] ^= 0x40;
        std::fs::write(index_of(&dir), &bad).unwrap();
        assert_eq!(
            changes(&ctx, "w", false),
            [change("src/b.txt", Modified)],
            "byte {at} flipped"
        );
        assert_eq!(changes(&ctx, "w", true), [change("src/b.txt", Modified)]);
    }

    // 3. No index at all: the same.
    std::fs::remove_file(index_of(&dir)).unwrap();
    assert_eq!(changes(&ctx, "w", false), [change("src/b.txt", Modified)]);
}

/// `behind_by` reads the target's head once, no history when it is the base, history 100
/// changesets a page until the base, and never past 10,000 changesets; a history without the
/// base, then the base's own, a page here.
#[test]
fn behind_by_reads_pages_until_base_and_stops_at_the_cap() {
    // 1. A workspace on the target's head: one ref read, no log.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &["src"]);
    let watched = Watched::new(&f.ops);
    let page = swarmops_ws::status(&f.ctx_over(&watched), page_of("w")).unwrap();
    assert_eq!((page.main.as_str(), page.behind_by), (&*base, 0));
    assert_eq!((watched.count("resolve_rev"), watched.count("log")), (1, 0));

    // 2. The target three changesets ahead: one page finds the base.
    let mut head = base.clone();
    for n in 0..3 {
        head = f.commit(Some(&head), &[Put::File("src/a.txt", &format!("{n}\n"))]);
    }
    watched.clear();
    let page = swarmops_ws::status(&f.ctx_over(&watched), page_of("w")).unwrap();
    assert_eq!((page.main.as_str(), page.behind_by), (&*head, 3));
    assert_eq!((watched.count("resolve_rev"), watched.count("log")), (1, 1));

    // 3. A history of 250 changesets without the base: three pages, all counted, then the
    //    base's page, which shares none of them.
    let mut long = Watched::new(&f.ops);
    long.fake_history = Some(250);
    let page = swarmops_ws::status(&f.ctx_over(&long), page_of("w")).unwrap();
    assert_eq!(page.behind_by, 250);
    assert_eq!((long.count("resolve_rev"), long.count("log")), (1, 4));

    // 4. One of 20,000: the cap, after 100 pages and the base's.
    let mut longer = Watched::new(&f.ops);
    longer.fake_history = Some(20_000);
    let page = swarmops_ws::status(&f.ctx_over(&longer), page_of("w")).unwrap();
    assert_eq!(page.behind_by, 10_000);
    assert_eq!((longer.count("resolve_rev"), longer.count("log")), (1, 101));
}

/// Moves `main` from `from` back or aside to `to`, as `sync` or `op undo` can.
fn move_main(f: &Fixture, from: &str, to: &str) {
    let mut req = RefSetReq::new("main", to);
    req.expected_old = Some(from.into());
    f.ops
        .ref_set(&Identity::parse(AGENT).unwrap(), req)
        .unwrap();
}

/// A base the target's history lost to a squash-like replacement (X <- A in the workspace, the
/// target moved to X <- B): `behind_by` counts B alone, from both histories' first page.
#[test]
fn behind_by_counts_past_a_replaced_base() {
    // 1. A workspace on A, on X.
    let f = Fixture::new();
    let x = f.commit(None, &[Put::File("src/a.txt", "x\n")]);
    let a = f.commit(Some(&x), &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &a, &["src"]);

    // 2. The target moved back to X, then on to B in A's place.
    move_main(&f, &a, &x);
    let b = f.commit(Some(&x), &[Put::File("src/a.txt", "b\n")]);

    // 3. One changeset behind: a page of the target's history, then one of the base's.
    let watched = Watched::new(&f.ops);
    let page = swarmops_ws::status(&f.ctx_over(&watched), page_of("w")).unwrap();
    assert_eq!((page.main.as_str(), page.behind_by), (&*b, 1));
    assert_eq!((watched.count("resolve_rev"), watched.count("log")), (1, 2));
}

/// A target moved back to an ancestor of the base (X <- A in the workspace, the target at X):
/// `behind_by` is 0.
#[test]
fn behind_by_is_zero_for_a_target_moved_back() {
    // 1. A workspace on A, on X; the target moved back to X.
    let f = Fixture::new();
    let x = f.commit(None, &[Put::File("src/a.txt", "x\n")]);
    let a = f.commit(Some(&x), &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &a, &["src"]);
    move_main(&f, &a, &x);

    // 2. Nothing to catch up.
    let page = swarmops_ws::status(&f.ctx(), page_of("w")).unwrap();
    assert_eq!((page.main.as_str(), page.behind_by), (&*x, 0));
}

/// `ws discard` without paths restores every change status finds: edits, deletions, kinds and
/// modes back as the head holds them, additions removed; ignored files stay.
#[cfg(unix)]
#[test]
fn discard_without_paths_restores_every_change() {
    use crate::fixture::{set_exec, symlink};

    // 1. A workspace, as created.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File(".gitignore", "*.log\n"),
            Put::File("src/a.txt", "a\n"),
            Put::File("src/b.txt", "b\n"),
            Put::File("src/c.txt", "c\n"),
            Put::Exec("src/run.sh", "#!/bin/sh\n"),
            Put::Link("src/l", "a.txt"),
        ],
    );
    f.create("w", &base, &[]);
    let ctx = f.ctx();
    let dir = f.beside("w");
    let created = tree_of(&dir);

    // 2. One change of each kind, and an ignored file.
    write(&dir, "src/new/n.txt", "new\n");
    write(&dir, "src/a.txt", "edited\n");
    remove(&dir, "src/b.txt");
    symlink(&dir, "src/c.txt", "a.txt");
    set_exec(&dir, "src/run.sh", false);
    remove(&dir, "src/l");
    write(&dir, "build.log", "kept\n");

    // 3. Discarded, each named; the tree is as created but for the ignored file.
    let mut req = WsDiscardReq::new("w".to_owned());
    req.status_after = StatusAfter::Page;
    let done = swarmops_ws::discard(&ctx, req).unwrap();
    assert_eq!(
        done.discarded,
        [
            "src/a.txt",
            "src/b.txt",
            "src/c.txt",
            "src/l",
            "src/new/n.txt",
            "src/run.sh"
        ]
    );
    assert_eq!(done.status_after.unwrap().changes, []);
    let mut expected = created;
    expected.push(crate::fixture::file("build.log", "kept\n"));
    expected.sort();
    assert_eq!(tree_of(&dir), expected);
    assert_eq!(changes(&ctx, "w", true), []);
}

/// After any sequence of edits, renames over a file, same-size writes and deletions, the status
/// the index gives is the status hashing every file gives (the operations spec §9, risk 3).
mod random_edits {
    use proptest::prelude::*;

    use crate::fixture::{Fixture, Put, changes, remove, write};

    /// The tracked files the edits act on.
    const FILES: [&str; 4] = ["src/a.txt", "src/b.txt", "src/d/c.txt", "src/d/e.txt"];

    /// One edit of the workspace.
    #[derive(Debug, Clone)]
    enum Edit {
        /// New content of another size.
        Rewrite(usize, u8),
        /// File `.0` renamed over file `.1`.
        RenameOver(usize, usize),
        /// As many bytes as there are, other ones.
        SameSize(usize),
        /// The file removed.
        Delete(usize),
    }

    fn edit() -> impl Strategy<Value = Edit> {
        prop_oneof![
            (0..FILES.len(), 1..20u8).prop_map(|(i, n)| Edit::Rewrite(i, n)),
            (0..FILES.len(), 0..FILES.len()).prop_map(|(i, j)| Edit::RenameOver(i, j)),
            (0..FILES.len()).prop_map(Edit::SameSize),
            (0..FILES.len()).prop_map(Edit::Delete),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn status_equals_full_status_under_random_edits(
            edits in proptest::collection::vec(edit(), 1..10),
        ) {
            // 1. A workspace of the four files.
            let f = Fixture::new();
            let puts: Vec<Put<'_>> = FILES.iter().map(|path| Put::File(path, "0123\n")).collect();
            let base = f.commit(None, &puts);
            f.create("w", &base, &["src"]);
            let ctx = f.ctx();
            let dir = f.beside("w");

            // 2. Each edit, where its files are there; then both statuses agree.
            for edit in edits {
                let there = |i: usize| dir.join(FILES[i]).exists();
                match edit {
                    Edit::Rewrite(i, n) => {
                        write(&dir, FILES[i], &"x".repeat(usize::from(n)));
                    }
                    Edit::RenameOver(i, j) if i != j && there(i) => {
                        std::fs::rename(dir.join(FILES[i]), dir.join(FILES[j])).unwrap();
                    }
                    Edit::SameSize(i) if there(i) => {
                        let old = std::fs::read(dir.join(FILES[i])).unwrap();
                        let new: Vec<u8> = old.iter().map(|b| b ^ 0x01).collect();
                        std::fs::write(dir.join(FILES[i]), new).unwrap();
                    }
                    Edit::Delete(i) if there(i) => remove(&dir, FILES[i]),
                    _ => {}
                }
                let by_index = changes(&ctx, "w", false);
                let by_hash = changes(&ctx, "w", true);
                prop_assert_eq!(by_index, by_hash);
            }
        }
    }
}
