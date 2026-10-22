//! `ws status`: the scan, the index, the racy rule, the ignore
//! files, and how far the target has moved.

use swarmops_api::WsChangeKind::{Added, Deleted, ModeChanged, Modified, TypeChanged};
use swarmops_api::{WsState, WsStatusReq};
use swarmops_ws::state::SWARMWS_DIR;

use crate::fixture::{Fixture, Put, Watched, change, changes, remove, write};

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
    WsStatusReq {
        workspace: name.to_owned(),
        paths: Vec::new(),
        full: false,
        limit: 500,
        cursor: None,
    }
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
        let req = WsStatusReq {
            limit: 2,
            cursor: cursor.take(),
            ..page_of("w")
        };
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
    let req = WsStatusReq {
        paths: vec!["src/a.txt".to_owned(), "src/new.txt".to_owned()],
        ..page_of("w")
    };
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
/// changesets a page until the base, and never past 10,000 changesets.
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

    // 3. A history of 250 changesets without the base: three pages, all counted.
    let mut long = Watched::new(&f.ops);
    long.fake_history = Some(250);
    let page = swarmops_ws::status(&f.ctx_over(&long), page_of("w")).unwrap();
    assert_eq!(page.behind_by, 250);
    assert_eq!((long.count("resolve_rev"), long.count("log")), (1, 3));

    // 4. One of 20,000: the cap, after 100 pages.
    let mut longer = Watched::new(&f.ops);
    longer.fake_history = Some(20_000);
    let page = swarmops_ws::status(&f.ctx_over(&longer), page_of("w")).unwrap();
    assert_eq!(page.behind_by, 10_000);
    assert_eq!((longer.count("resolve_rev"), longer.count("log")), (1, 100));
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
