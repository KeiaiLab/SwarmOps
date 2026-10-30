//! Materialisation: what an update writes, what it never overwrites, and the volume's rules.

use swarmops_api::{StatusAfter, WsCommitReq, WsCreateReq, WsDiscardReq, WsMaterializeReq};
use swarmops_ws::state::{self, SWARMWS_DIR, Volume};
use swarmops_ws::{RealFs, lock, materialize};

use crate::fixture::{BARE, Fixture, Put, exec, file, link, tree_of, write};

/// A volume as this host's, which holds all three.
const LINUX: Volume = Volume::new(false, true, true);

/// The request of `ws materialize` adding `paths` to workspace `name`.
fn expand(name: &str, paths: &[&str]) -> WsMaterializeReq {
    let mut out = WsMaterializeReq::new(
        name.to_owned(),
        paths.iter().map(|p| (*p).to_owned()).collect(),
    );
    out.status_after = StatusAfter::None;
    out
}

/// Moves workspace `name` to changeset `to`, under its lock, as a rebase or a land does.
fn update(ctx: &swarmops_ws::Ctx<'_>, name: &str, to: &str) -> Result<(), String> {
    let mut ws = Fixture::open(ctx, name);
    let _held = lock::acquire(&ws.dir).unwrap();
    materialize::update(ctx, &mut ws, to)
        .map(|_| ())
        .map_err(|e| format!("{} {}", e.code, serde_json::Value::Object(e.details)))
}

/// A create writes the tree of its base under its prefixes, and nothing outside them: no other
/// top-level directory, no file of a sibling directory, its own state aside.
#[test]
fn create_materialises_the_prefixes_and_nothing_else() {
    // 1. A tree with three top-level directories and a file at the root.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/lib.rs", "lib\n"),
            Put::Exec("src/bin/run.sh", "#!/bin/sh\n"),
            Put::Link("src/current", "lib.rs"),
            Put::File("docs/a.md", "a\n"),
            Put::File("docs-old/b.md", "b\n"),
            Put::File("README", "readme\n"),
        ],
    );

    // 2. A workspace of `src` and `docs`.
    let info = f.create("w", &base, &["src", "docs/"]);
    assert!(info.created);
    assert_eq!((info.base.as_str(), info.head.as_str()), (&*base, &*base));
    assert_eq!(info.prefixes, ["docs", "src"]);

    // 3. Exactly their files, each of its kind.
    let dir = f.beside("w");
    assert_eq!(
        tree_of(&dir),
        [
            file("docs/a.md", "a\n"),
            exec("src/bin/run.sh", "#!/bin/sh\n"),
            link("src/current", "lib.rs"),
            file("src/lib.rs", "lib\n"),
        ]
    );

    // 4. Its state names the base as what the files stand for, and no intent is left.
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(saved.materialized, base);
    assert_eq!(saved.target, "main");
    assert!(
        !dir.join(SWARMWS_DIR)
            .join("journal/materialize.json")
            .exists()
    );

    // 5. The same request again is the same workspace.
    let again = f.create("w", &base, &["src", "docs/"]);
    assert!(!again.created);
}

/// An update from one changeset to the next writes the files that changed and removes the ones
/// that went, and touches nothing else: counted by the recording host.
#[test]
#[cfg(feature = "test-hooks")]
fn an_update_writes_only_what_changed() {
    use std::sync::Arc;

    use swarmops_ws::RealHost;
    use swarmops_ws::hostfs::{HostMethod, RecordingHost};

    // 1. A tree of 50 files, a workspace of all of it.
    let f = Fixture::new();
    let names: Vec<String> = (0..50).map(|i| format!("src/m{i:02}.rs")).collect();
    let puts: Vec<Put<'_>> = names.iter().map(|n| Put::File(n, "same\n")).collect();
    let base = f.commit(None, &puts);
    f.create("w", &base, &[]);

    // 2. The next changeset: one file changed, one added, one removed.
    let next = f.commit(
        Some(&base),
        &[
            Put::File("src/m07.rs", "changed\n"),
            Put::File("src/new/added.rs", "new\n"),
            Put::Gone("src/m31.rs"),
        ],
    );

    // 3. The update over a recording host.
    let host = Arc::new(RecordingHost::new(RealHost::default()));
    let ctx = f.ctx_on(host.clone());
    update(&ctx, "w", &next).unwrap();

    // 4. Two writes and one removal, of exactly those paths.
    let dir = f.beside("w");
    let mut written: Vec<String> = host
        .calls_of(HostMethod::WriteFile)
        .iter()
        .map(|p| p.strip_prefix(&dir).unwrap().to_string())
        .collect();
    written.sort();
    assert_eq!(written, ["src/m07.rs", "src/new/added.rs"]);
    let removed: Vec<String> = host
        .calls_of(HostMethod::RemoveFile)
        .iter()
        .map(|p| p.strip_prefix(&dir).unwrap().to_string())
        .collect();
    assert_eq!(removed, ["src/m31.rs"]);

    // 5. The tree is the new changeset's, and the state says so.
    let tree = tree_of(&dir);
    assert_eq!(tree.len(), 50);
    assert!(tree.contains(&file("src/m07.rs", "changed\n")));
    assert!(tree.contains(&file("src/new/added.rs", "new\n")));
    assert_eq!(
        state::read(&RealFs::new(), &dir).unwrap().materialized,
        next
    );
}

/// An update that would overwrite or remove a file the agent edited, or replace a file the agent
/// added, is `E_WS_DIRTY` naming the paths, and writes nothing.
#[test]
fn an_update_never_overwrites_an_edit() {
    // 1. A workspace, and a next changeset that changes `a`, removes `b` and adds `c`.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("a", "a\n"),
            Put::File("b", "b\n"),
            Put::File("k", "k\n"),
        ],
    );
    f.create("w", &base, &[]);
    let next = f.commit(
        Some(&base),
        &[
            Put::File("a", "a2\n"),
            Put::Gone("b"),
            Put::File("c", "c\n"),
        ],
    );

    // 2. The agent edits `a` and `b`, and adds its own `c`.
    let dir = f.beside("w");
    std::fs::write(dir.join("a"), "mine\n").unwrap();
    std::fs::write(dir.join("b"), "mine too\n").unwrap();
    std::fs::write(dir.join("c"), "my own\n").unwrap();
    let before = tree_of(&dir);

    // 3. Refused, naming the three, and nothing changed on disk or in the state.
    let refused = update(&f.ctx(), "w", &next).unwrap_err();
    assert!(refused.starts_with("E_WS_DIRTY"), "{refused}");
    for path in ["\"a\"", "\"b\"", "\"c\""] {
        assert!(refused.contains(path), "{refused}");
    }
    assert!(!refused.contains("\"k\""), "{refused}");
    assert_eq!(tree_of(&dir), before);
    assert_eq!(
        state::read(&RealFs::new(), &dir).unwrap().materialized,
        base
    );
    assert!(
        !dir.join(SWARMWS_DIR)
            .join("journal/materialize.json")
            .exists()
    );
}

/// A symlink the agent put where the tree has a directory is never written through: the update
/// stops with `E_WS_DIRTY` before writing, and the symlink's target is untouched. A symlink the
/// tree itself had there, which the new tree replaces by a directory, is removed first.
#[test]
#[cfg(unix)]
fn no_write_follows_a_symlink_the_agent_made() {
    // 1. A workspace of `src`; a directory outside it that a link could lead to.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a/x", "x\n"), Put::Link("src/b", "a")],
    );
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    let outside = f.beside("outside");
    std::fs::create_dir_all(&outside).unwrap();

    // 2. The agent replaces `src/a` with a symlink to the outside directory.
    std::fs::remove_dir_all(dir.join("src/a")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("src/a")).unwrap();

    // 3. An update that writes below `src/a`: refused, nothing written through the link.
    let next = f.commit(Some(&base), &[Put::File("src/a/y", "y\n")]);
    let refused = update(&f.ctx(), "w", &next).unwrap_err();
    assert!(refused.starts_with("E_WS_DIRTY"), "{refused}");
    assert!(refused.contains("src/a"), "{refused}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);

    // 4. The tree's own symlink `src/b`, replaced by a directory: removed, then written.
    std::fs::remove_file(dir.join("src/a")).unwrap();
    std::fs::create_dir(dir.join("src/a")).unwrap();
    std::fs::write(dir.join("src/a/x"), "x\n").unwrap();
    let third = f.commit(
        Some(&next),
        &[Put::Gone("src/b"), Put::File("src/b/z", "z\n")],
    );
    update(&f.ctx(), "w", &third).unwrap();
    assert_eq!(
        tree_of(&dir),
        [
            file("src/a/x", "x\n"),
            file("src/a/y", "y\n"),
            file("src/b/z", "z\n"),
        ]
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// An update that removes a file below a symlink the agent made does not remove through it: it
/// stops with `E_WS_DIRTY` naming the link, and the file the link leads to is kept.
#[test]
#[cfg(unix)]
fn no_removal_follows_a_symlink_the_agent_made() {
    // 1. A workspace of `src`; outside it, a file of the same name the link will lead to.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a/x", "x\n")]);
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    let outside = f.beside("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x"), "x\n").unwrap();

    // 2. The agent replaces `src/a` with a symlink to the outside directory.
    std::fs::remove_dir_all(dir.join("src/a")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("src/a")).unwrap();

    // 3. An update that removes `src/a/x`: refused, the outside file kept.
    let next = f.commit(Some(&base), &[Put::Gone("src/a/x")]);
    let refused = update(&f.ctx(), "w", &next).unwrap_err();
    assert!(refused.starts_with("E_WS_DIRTY"), "{refused}");
    assert!(refused.contains("src/a"), "{refused}");
    assert_eq!(std::fs::read_to_string(outside.join("x")).unwrap(), "x\n");
}

/// On a case-insensitive volume (its record simulated on this host) a tree with two paths that
/// differ only in case is refused before anything is written, naming both: as the prefixes a
/// `ws materialize` adds, and as an update adds them.
#[test]
fn a_case_collision_is_refused_on_a_case_insensitive_volume_naming_both_paths() {
    // 1. A workspace of `src`, on a volume that folds case.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/readme", "r\n"),
            Put::File("lib/Makefile", "a\n"),
            Put::File("lib/makefile", "b\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    let mut volume = LINUX;
    volume.case_insensitive = true;
    f.simulate("w", volume);
    let dir = f.beside("w");
    let before = tree_of(&dir);

    // 2. Adding `lib`: refused, naming both, nothing written, the prefixes unchanged.
    let refused = swarmops_ws::materialize(&f.ctx(), expand("w", &["lib"])).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(refused.details["reason"], "case_collision");
    assert_eq!(
        refused.details["paths"],
        serde_json::json!([["lib/Makefile", "lib/makefile"]])
    );
    assert_eq!(tree_of(&dir), before);
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().prefixes, ["src"]);

    // 3. An update that adds `src/README` beside `src/readme`: refused the same way.
    let next = f.commit(Some(&base), &[Put::File("src/README", "R\n")]);
    let refused = update(&f.ctx(), "w", &next).unwrap_err();
    assert!(refused.starts_with("E_WS_UNPORTABLE"), "{refused}");
    assert!(
        refused.contains("src/README") && refused.contains("src/readme"),
        "{refused}"
    );
    assert_eq!(tree_of(&dir), before);
}

/// On Windows a name the platform cannot hold is refused before anything is written.
#[test]
#[cfg(windows)]
fn a_windows_name_is_refused_before_anything_is_written() {
    // 1. A tree with a device name below a prefix.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/ok", "ok\n"), Put::File("src/aux.txt", "x\n")],
    );

    // 2. Refused at create, naming it, and no file written.
    let refused = f.create_in(&f.ctx(), "w", &base, &["src"]).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(refused.details["reason"], "windows_name");
    assert_eq!(refused.details["paths"], serde_json::json!(["src/aux.txt"]));
    assert!(!f.beside("w").join("src").exists());
}

/// Where symlinks cannot be made (Windows by default), a symlink is a plain file holding its
/// target.
#[test]
#[cfg(windows)]
fn a_symlink_is_a_plain_file_holding_its_target_where_links_cannot_be_made() {
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a", "a\n"), Put::Link("l", "a")]);
    f.create("w", &base, &[]);
    assert_eq!(tree_of(&f.beside("w")), [file("a", "a\n"), file("l", "a")]);
}

/// The same rule on a volume record without symlinks, simulated on this host: the symlink a
/// `ws materialize` adds is a plain file holding its target, and an update over it is no edit.
#[test]
fn a_symlink_is_a_plain_file_on_a_volume_without_links() {
    // 1. A workspace of `src`, on a volume without symlinks.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("src/a", "a\n"), Put::Link("lib/l", "../src/a")],
    );
    f.create("w", &base, &["src"]);
    let mut volume = LINUX;
    volume.symlinks = false;
    f.simulate("w", volume);

    // 2. `lib` added: its link is a file holding the target.
    swarmops_ws::materialize(&f.ctx(), expand("w", &["lib"])).unwrap();
    let dir = f.beside("w");
    assert_eq!(
        tree_of(&dir),
        [file("lib/l", "../src/a"), file("src/a", "a\n")]
    );

    // 3. The link retargeted: the file is the tree's, so the update rewrites it.
    let next = f.commit(Some(&base), &[Put::Link("lib/l", "../src")]);
    update(&f.ctx(), "w", &next).unwrap();
    assert!(tree_of(&dir).contains(&file("lib/l", "../src")));
}

/// On a volume without the executable bit (simulated), an executable is written as a plain file:
/// its mode lives in the tree the workspace records, not on the host, so the host's mode is no
/// edit and an update over it goes through.
#[test]
#[cfg(unix)]
fn exec_mode_lives_in_the_index_on_a_volume_without_the_bit() {
    // 1. A workspace of `src`, on a volume without the bit.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/a", "a\n"),
            Put::Exec("tools/run", "#!/bin/sh\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    let mut volume = LINUX;
    volume.exec_bit = false;
    f.simulate("w", volume);

    // 2. `tools` added: the executable is a plain file there.
    swarmops_ws::materialize(&f.ctx(), expand("w", &["tools"])).unwrap();
    let dir = f.beside("w");
    assert!(tree_of(&dir).contains(&file("tools/run", "#!/bin/sh\n")));

    // 3. Its content changed, its mode kept: the update writes it, still without the bit.
    let next = f.commit(
        Some(&base),
        &[Put::Exec("tools/run", "#!/bin/sh\nexit 0\n")],
    );
    update(&f.ctx(), "w", &next).unwrap();
    assert!(tree_of(&dir).contains(&file("tools/run", "#!/bin/sh\nexit 0\n")));

    // 4. On this host's own volume the bit is written.
    f.create("x", &next, &["tools"]);
    assert_eq!(
        tree_of(&f.beside("x")),
        [exec("tools/run", "#!/bin/sh\nexit 0\n")]
    );
}

/// The three trees the reserved name must refuse: at the top, below a directory, and in another
/// ASCII case, each as its path to the reserved directory and the file it holds.
const RESERVED_TREES: [(&str, &str); 3] = [
    (".SWARMWS", ".SWARMWS/x"),
    ("src/.swarmws", "src/.swarmws/y"),
    ("src/.SwarmWs", "src/.SwarmWs/z"),
];

/// A tree holding `.swarmws` as a component, at any depth and in any ASCII case, is refused on
/// every platform, before anything is written; a name that only resembles it is an ordinary
/// directory.
#[test]
fn the_swarmws_name_is_reserved_at_any_depth_and_case() {
    // 1. Each reserved tree alone, in a store of its own: refused, naming the path, nothing
    //    written.
    for (_, path) in RESERVED_TREES {
        let f = Fixture::new();
        let base = f.commit(None, &[Put::File(path, "x\n"), Put::File("lib/a", "a\n")]);
        let name = "w";
        let refused = f.create_in(&f.ctx(), name, &base, &[]).unwrap_err();
        assert_eq!(refused.code, "E_WS_UNPORTABLE", "{path}: {refused:?}");
        assert_eq!(refused.details["reason"], "reserved_name", "{path}");
        assert_eq!(
            refused.details["paths"],
            serde_json::json!([path]),
            "{path}"
        );
        assert!(!f.beside(name).join("lib").exists(), "{path}");
    }

    // 2. Names that only resemble it are written.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("src/swarmws/y", "y\n"),
            Put::File("src/.swarmws.bak/y", "b\n"),
        ],
    );
    f.create("ok", &base, &[]);
    assert_eq!(
        tree_of(&f.beside("ok")),
        [
            file("src/.swarmws.bak/y", "b\n"),
            file("src/swarmws/y", "y\n"),
        ]
    );
}

/// A workspace moved onto a tree holding the reserved name, by a rebase's update or by
/// `ws materialize` of the directory, is refused with its files and its state unchanged.
#[test]
fn moving_onto_a_reserved_tree_is_refused() {
    // Each tree in a store of its own: `main` moves along with every commit.
    for (i, (dir_path, path)) in RESERVED_TREES.iter().enumerate() {
        // 1. A workspace of the whole clean base, to be moved onto a tree that holds the reserved
        //    name.
        let f = Fixture::new();
        let clean = f.commit(None, &[Put::File("lib/a", "a\n")]);
        let reserved = f.commit(Some(&clean), &[Put::File(path, "x\n")]);
        let name = format!("u{i}");
        f.create(&name, &clean, &[]);
        let dir = f.beside(&name);
        let files = tree_of(&dir);
        let saved = state::read(&RealFs::new(), &dir).unwrap();

        // 2. An update onto it, as a rebase does: refused, nothing changed.
        let err = update(&f.ctx(), &name, &reserved).unwrap_err();
        assert!(err.starts_with("E_WS_UNPORTABLE"), "{path}: {err}");
        assert!(err.contains("reserved_name"), "{path}: {err}");
        assert_eq!(tree_of(&dir), files, "{path}");
        assert_eq!(state::read(&RealFs::new(), &dir).unwrap(), saved, "{path}");

        // 3. A workspace of the clean prefix on that tree, widened onto the reserved directory.
        let wide = format!("m{i}");
        f.create(&wide, &reserved, &["lib"]);
        let dir = f.beside(&wide);
        let files = tree_of(&dir);
        let saved = state::read(&RealFs::new(), &dir).unwrap();
        let err = swarmops_ws::materialize(&f.ctx(), expand(&wide, &[dir_path])).unwrap_err();
        assert_eq!(err.code, "E_WS_UNPORTABLE", "{path}: {err:?}");
        assert_eq!(err.details["reason"], "reserved_name", "{path}");
        assert_eq!(tree_of(&dir), files, "{path}");
        assert_eq!(state::read(&RealFs::new(), &dir).unwrap(), saved, "{path}");
    }
}

/// A materialisation killed at any call of the host or of the durable files' layer is finished
/// by the next open (or, killed before the workspace's state exists, by the same create again):
/// afterwards the tree is the base's, whole, and no intent is left.
#[test]
#[cfg(feature = "test-hooks")]
#[ignore = "slow, ~30 s: a child process per host and fs call of a 100-file create"]
fn a_killed_materialisation_finishes_on_the_next_open() {
    let test = swarmops_testkit::test_name!("a_killed_materialisation_finishes_on_the_next_open");
    killed::sweep(&test, 1);
}

/// The same sweep at every call of the durable files' layer and every 25th of the host's (and
/// its last): the share of it the gate runs.
#[test]
#[cfg(feature = "test-hooks")]
fn a_killed_materialisation_finishes_on_the_next_open_sampled() {
    let test =
        swarmops_testkit::test_name!("a_killed_materialisation_finishes_on_the_next_open_sampled");
    killed::sweep(&test, 25);
}

/// The crash sweep of a create, run by the two tests above.
#[cfg(feature = "test-hooks")]
mod killed {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use camino::Utf8Path;
    use swarmops_api::{Identity, WsCreateReq};
    use swarmops_ops::LocalOps;
    use swarmops_store::{CrashAtFs, FsOps};
    use swarmops_testkit::child;
    use swarmops_ws::hostfs::CrashAtHost;
    use swarmops_ws::workspace::{Locate, Workspace};
    use swarmops_ws::{Ctx, HostFs, RealHost};

    use super::*;
    use crate::fixture::AGENT;

    /// The variables that make a run of a sweep's test the child that crashes.
    const STORE: &str = "SWARMOPS_WS_TEST_STORE";
    const CUT: &str = "SWARMOPS_WS_TEST_CUT";

    /// How many children run at once.
    const PARALLEL: usize = 8;

    /// A tree row of [`tree_of`].
    type Row = (String, char, String);

    /// Kills a create of 100 files at each call (every `stride`-th of the host's, and its last)
    /// in a child process, a run of `test`, then finishes it; in that child, makes the create
    /// and dies.
    pub(super) fn sweep(test: &str, stride: u64) {
        // 1. In the child: the create, over a layer that aborts at call k.
        if let (Ok(root), Ok(cut)) = (std::env::var(STORE), std::env::var(CUT)) {
            crash(&root, &cut);
            return;
        }

        // 2. A store with 100 files in 10 directories, and the tree a workspace of it holds.
        let f = Fixture::new();
        let names: Vec<String> = (0..100).map(|i| format!("d{}/f{i:03}", i % 10)).collect();
        let puts: Vec<Put<'_>> = names.iter().map(|n| Put::File(n, n)).collect();
        let base = f.commit(None, &puts);
        f.create("probe", &base, &[]);
        let expected = tree_of(&f.beside("probe"));

        // 3. How many calls a create makes on each layer, counted by the doubles with no cut.
        let counting_host = Arc::new(CrashAtHost::new(RealHost::default(), 0));
        let counting_fs = Arc::new(CrashAtFs::new(RealFs::new(), 0));
        let ctx = f.ctx_with(counting_host.clone(), counting_fs.clone());
        swarmops_ws::create(&ctx, request(f.root.as_str(), "count")).unwrap();
        let (host_calls, fs_calls) = (counting_host.calls(), counting_fs.calls());
        assert!(host_calls > 100 && fs_calls > 4, "{host_calls} {fs_calls}");

        // 4. A child killed at each cut, then the workspace finished: eight at a time, each
        //    thread with its own handle on the store.
        let cuts: Vec<(&str, u64)> = (1..=host_calls)
            .filter(|k| (k - 1) % stride == 0 || *k == host_calls)
            .map(|k| ("host", k))
            .chain((1..=fs_calls).map(|k| ("fs", k)))
            .collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..PARALLEL {
                scope.spawn(|| {
                    let ops = LocalOps::open(&f.root).unwrap();
                    let ctx = ctx_on(
                        &ops,
                        f.root.as_str(),
                        Arc::new(RealHost::default()),
                        Arc::new(RealFs::new()),
                    );
                    while let Some(&(layer, k)) = cuts.get(next.fetch_add(1, Ordering::SeqCst)) {
                        cut_and_recover(test, &f, &ctx, (layer, k), &expected);
                    }
                });
            }
        });
    }

    /// The child's part: the create of the workspace `cut` (`<layer>:<k>:<name>`) names, on the
    /// store at `root`, over `layer` aborting at its call `k`.
    fn crash(root: &str, cut: &str) {
        // 1. The cut.
        let mut parts = cut.splitn(3, ':');
        let layer = parts.next().unwrap();
        let k = parts.next().unwrap().parse().unwrap();
        let name = parts.next().unwrap();

        // 2. The create over the layer that aborts.
        let ops = LocalOps::open(Utf8Path::new(root)).unwrap();
        let (host, fs): (Arc<dyn HostFs>, Arc<dyn FsOps>) = match layer {
            "host" => (
                Arc::new(CrashAtHost::new(RealHost::default(), k)),
                Arc::new(RealFs::new()),
            ),
            _ => (
                Arc::new(RealHost::default()),
                Arc::new(CrashAtFs::new(RealFs::new(), k)),
            ),
        };
        let ctx = ctx_on(&ops, root, host, fs);
        let _ = swarmops_ws::create(&ctx, request(root, name));
    }

    /// Kills a child (a run of `test`) creating workspace `<layer>-<k>` at call `k` of `layer`,
    /// then opens the workspace (or creates it again) through `ctx` and finds the tree whole.
    fn cut_and_recover(test: &str, f: &Fixture, ctx: &Ctx<'_>, cut: (&str, u64), expected: &[Row]) {
        // 1. The child, killed.
        let (layer, k) = cut;
        let name = format!("{layer}-{k}");
        let out = child::run(
            child::this_test(test)
                .env(STORE, f.root.as_str())
                .env(CUT, format!("{layer}:{k}:{name}")),
        );
        assert!(!out.status.success(), "{name}: the child was not killed");

        // 2. The next open finishes it; before its state exists, the same create does.
        let dir = f.beside(&name);
        if state::path_in(&dir).exists() {
            Workspace::open(ctx, Locate::Name(name.clone())).unwrap();
        } else {
            swarmops_ws::create(ctx, request(f.root.as_str(), &name)).unwrap();
        }
        assert_eq!(tree_of(&dir), expected, "{name}");
        let intent = dir.join(SWARMWS_DIR).join("journal/materialize.json");
        assert!(!intent.exists(), "{name}");
    }

    /// A context on the store `ops` at `root`, over `host` and `fs`.
    fn ctx_on<'a>(
        ops: &'a LocalOps,
        root: &str,
        host: Arc<dyn HostFs>,
        fs: Arc<dyn FsOps>,
    ) -> Ctx<'a> {
        Ctx::new(
            ops,
            root.into(),
            Some(Identity::parse(AGENT).unwrap()),
            fs,
            host,
        )
    }

    /// The create of workspace `name` beside the store at `root`, of the whole of `main`.
    fn request(root: &str, name: &str) -> WsCreateReq {
        let dir = Utf8Path::new(root).parent().unwrap().join(name);
        let mut out = WsCreateReq::new(name.to_owned());
        out.path = Some(dir.to_string());
        out.base = "main".into();
        out.all = true;
        out.status_after = StatusAfter::None;
        out
    }
}

/// While one mutation holds a workspace's lock, another waits its 5 s and fails with
/// `E_WS_LOCKED`, retryable, having changed nothing.
#[test]
fn the_lock_refuses_a_second_mutation_with_ws_locked() {
    // 1. A workspace with an edit, its lock held.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("a", "a\n")]);
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    std::fs::write(dir.join("a"), "edit\n").unwrap();
    let held = lock::acquire(&dir).unwrap();

    // 2. A discard meanwhile: locked out, the edit kept.
    let started = std::time::Instant::now();
    let mut req = WsDiscardReq::new("w".to_owned());
    req.paths = vec!["a".to_owned()];
    req.status_after = StatusAfter::None;
    let refused = swarmops_ws::discard(&f.ctx(), req.clone()).unwrap_err();
    assert_eq!(refused.code, "E_WS_LOCKED", "{refused:?}");
    assert!(refused.retryable);
    assert!(started.elapsed() >= lock::LOCK_WAIT);
    assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "edit\n");

    // 3. Once the lock is let go, the same discard goes through.
    drop(held);
    swarmops_ws::discard(&f.ctx(), req).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "a\n");
}

/// `ws discard <path>` restores a file from the head: an edit, a removal, a mode change and a
/// file put where a directory was; a file the agent added and names is removed; other paths
/// are left as they are.
#[test]
#[cfg(unix)]
fn discard_restores_a_path_from_head() {
    use std::os::unix::fs::PermissionsExt;

    // 1. A workspace, and the agent's changes.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("a", "a\n"),
            Put::File("b", "b\n"),
            Put::Exec("x", "x\n"),
            Put::File("d/e", "e\n"),
            Put::File("keep", "k\n"),
        ],
    );
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    std::fs::write(dir.join("a"), "edit\n").unwrap();
    std::fs::remove_file(dir.join("b")).unwrap();
    std::fs::set_permissions(dir.join("x"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_dir_all(dir.join("d")).unwrap();
    std::fs::write(dir.join("d"), "a file now\n").unwrap();
    std::fs::write(dir.join("new"), "added\n").unwrap();
    std::fs::write(dir.join("keep"), "kept edit\n").unwrap();

    // 2. Discarded by name: each restored, `keep` not named and left.
    let mut req = WsDiscardReq::new("w".to_owned());
    req.paths = ["a", "b", "x", "d", "new"].map(str::to_owned).to_vec();
    req.status_after = StatusAfter::None;
    let resp = swarmops_ws::discard(&f.ctx(), req).unwrap();
    assert_eq!(resp.discarded, ["a", "b", "d", "d/e", "new", "x"]);
    assert_eq!(
        tree_of(&dir),
        [
            file("a", "a\n"),
            file("b", "b\n"),
            file("d/e", "e\n"),
            file("keep", "kept edit\n"),
            exec("x", "x\n"),
        ]
    );

    // 3. Nothing to restore: nothing discarded.
    let mut req = WsDiscardReq::new("w".to_owned());
    req.paths = vec!["a".to_owned()];
    req.status_after = StatusAfter::None;
    let again = swarmops_ws::discard(&f.ctx(), req).unwrap();
    assert_eq!(again.discarded, Vec::<String>::new());
}

/// `ws discard <path>` of a file the head lacks, below a symlink the agent made, does not
/// remove through it: `E_WS_DIRTY` naming the link, and the file the link leads to is kept.
#[test]
#[cfg(unix)]
fn discard_follows_no_symlink_the_agent_made() {
    // 1. A workspace of `src`; outside it, a file the link will lead to.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a/x", "x\n")]);
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    let outside = f.beside("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("precious"), "p\n").unwrap();

    // 2. The agent replaces `src/a` with a symlink to the outside directory.
    std::fs::remove_dir_all(dir.join("src/a")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("src/a")).unwrap();

    // 3. A discard of `src/a/precious`: refused, the outside file kept.
    let mut req = WsDiscardReq::new("w".to_owned());
    req.paths = vec!["src/a/precious".to_owned()];
    req.status_after = StatusAfter::None;
    let refused = swarmops_ws::discard(&f.ctx(), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
    assert_eq!(refused.details["paths"], serde_json::json!(["src/a"]));
    assert_eq!(
        std::fs::read_to_string(outside.join("precious")).unwrap(),
        "p\n"
    );
}

/// A tree deeper than one listing reaches (64 levels below a prefix) is listed again from the
/// deepest directory, so a create writes its deepest file, and an update reaches it too.
#[test]
fn an_update_lists_a_tree_deeper_than_64_levels() {
    // 1. A file 70 directories down.
    let f = Fixture::new();
    let dirs: Vec<String> = (0..70).map(|i| format!("d{i}")).collect();
    let deep = format!("{}/leaf", dirs.join("/"));
    let base = f.commit(None, &[Put::File(&deep, "deep\n"), Put::File("top", "t\n")]);

    // 2. A create of the whole tree writes it.
    f.create("w", &base, &[]);
    let dir = f.beside("w");
    assert_eq!(tree_of(&dir), [file(&deep, "deep\n"), file("top", "t\n")]);

    // 3. So does an update that changes it, and a create below the first directory.
    let next = f.commit(Some(&base), &[Put::File(&deep, "deeper\n")]);
    update(&f.ctx(), "w", &next).unwrap();
    assert!(tree_of(&dir).contains(&file(&deep, "deeper\n")));
    f.create("v", &next, &["d0"]);
    assert_eq!(tree_of(&f.beside("v")), [file(&deep, "deeper\n")]);
}

/// A host path the workspace crate is handed that is not UTF-8 is `E_INVALID_ARG`, at its
/// boundary; a relative one too.
#[test]
#[cfg(unix)]
fn a_non_utf8_path_is_invalid_arg() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let raw = std::path::Path::new(OsStr::from_bytes(b"/tmp/ws-\xff"));
    let refused = swarmops_ws::workspace::host_path(raw).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    let relative = swarmops_ws::workspace::host_path(std::path::Path::new("ws")).unwrap_err();
    assert_eq!(relative.code, "E_INVALID_ARG", "{relative:?}");
    let fine = swarmops_ws::workspace::host_path(std::path::Path::new("/tmp/ws")).unwrap();
    assert_eq!(fine, "/tmp/ws");
}

/// A workspace with neither prefixes nor `all` asked for holds the whole tree, as `ws create`
/// wrote it, and so does one an older build wrote as `all` false: it reads as `all`, the same
/// create again is the same workspace, `ws materialize` adds nothing to it, `ws discard` takes
/// any path, a commit meets no outside edit, and the state it writes back says `all`.
#[test]
fn a_bare_workspace_holds_the_whole_tree() {
    for how in BARE {
        // 1. A workspace with no prefixes: read as the whole tree.
        let f = Fixture::new();
        let base = f.commit(
            None,
            &[
                Put::File("README", "readme\n"),
                Put::File("docs/d.md", "d\n"),
                Put::File("src/a.txt", "a\n"),
            ],
        );
        let dir = f.create_bare("w", &base, how);
        let saved = state::read(&RealFs::new(), &dir).unwrap();
        assert_eq!((saved.all, saved.prefixes.len()), (true, 0), "{how:?}");

        // 2. The same create again: the same workspace.
        let mut again = WsCreateReq::new("w".to_owned());
        again.path = Some(dir.to_string());
        again.base = base.as_str().into();
        again.status_after = StatusAfter::None;
        let again = swarmops_ws::create(&f.ctx(), again).unwrap();
        assert_eq!((again.created, again.all), (false, true), "{how:?}");

        // 3. `ws materialize src`: nothing added, the prefixes still none.
        let done = swarmops_ws::materialize(&f.ctx(), expand("w", &["src"])).unwrap();
        assert_eq!(done.materialized, Vec::<String>::new(), "{how:?}");
        let saved = state::read(&RealFs::new(), &dir).unwrap();
        assert_eq!((saved.all, saved.prefixes.len()), (true, 0), "{how:?}");

        // 4. `ws discard README`: restored.
        write(&dir, "README", "edited\n");
        let mut discard = WsDiscardReq::new("w".to_owned());
        discard.paths = vec!["README".to_owned()];
        discard.status_after = StatusAfter::None;
        let done = swarmops_ws::discard(&f.ctx(), discard).unwrap();
        assert_eq!(done.discarded, ["README"], "{how:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("README")).unwrap(),
            "readme\n"
        );

        // 5. An edit beside `src` commits, and the state written back says `all`.
        write(&dir, "docs/d.md", "edited\n");
        let mut commit = WsCommitReq::new("w".to_owned(), "docs".to_owned());
        commit.status_after = StatusAfter::None;
        let done = swarmops_ws::commit(&f.ctx(), commit).unwrap();
        assert_eq!(done.files_changed, 1, "{how:?}");
        let wire: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state::path_in(&dir)).unwrap()).unwrap();
        assert_eq!(wire["all"], true, "{how:?}");
    }
}
