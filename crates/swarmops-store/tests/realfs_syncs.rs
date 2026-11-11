//! What `RealFs` flushes, watched where it flushes (`fs_ops::watch_syncs`): each sync that
//! `FsOps` documents is made, and a `RealFs` that dropped any one of them fails here. The order
//! checker and the power-loss images take these syncs on trust; this test is what backs them.

use std::fs;
use std::path::{Path, PathBuf};

use swarmops_store::fs_ops::{FsOps, RealFs, SyncCall, SyncKind, watch_syncs};

/// A sync the documentation of one `FsOps` method promises: its kind and what it flushes.
#[derive(Debug)]
enum Want {
    /// This very file or directory.
    At(SyncKind, PathBuf),
    /// A temp file in this directory, which is not the target (`atomic_replace`'s).
    TempIn(SyncKind, PathBuf),
}

impl Want {
    /// Whether `call` is the sync this promises.
    fn matches(&self, call: &SyncCall) -> bool {
        match self {
            Want::At(kind, path) => call.kind == *kind && call.path == *path,
            Want::TempIn(kind, dir) => call.kind == *kind && call.path.parent() == Some(dir),
        }
    }
}

/// One `FsOps` method run on `RealFs` in `dir`, and the syncs it promises, in order.
type Case = fn(&Path) -> Vec<Want>;

/// Every mutating method of `FsOps`, each with its documented syncs (none for those that
/// promise none).
const CASES: [(&str, Case); 11] = [
    ("create_dir_all", create_dir_all),
    ("write_durable_temp", write_durable_temp),
    ("link_noclobber", link_noclobber),
    ("fsync_dir", fsync_dir),
    ("remove", remove),
    ("append_durable", append_durable),
    ("truncate", truncate),
    ("atomic_replace", atomic_replace),
    ("sync_data", sync_data),
    ("append", append),
    ("rename_durable", rename_durable),
];

/// The syncs `FsOps` documents across [`CASES`]: one planted drop each.
const DOCUMENTED_SYNCS: usize = 8;

fn create_dir_all(dir: &Path) -> Vec<Want> {
    RealFs::new().create_dir_all(&dir.join("a/b")).unwrap();
    vec![]
}

/// Syncs the temp file it returns (`sync_all`), not its directory entry.
fn write_durable_temp(dir: &Path) -> Vec<Want> {
    let temp = RealFs::new().write_durable_temp(dir, b"bytes").unwrap();
    vec![Want::At(SyncKind::File, temp)]
}

/// Syncs nothing: the new name is durable only after `fsync_dir`.
fn link_noclobber(dir: &Path) -> Vec<Want> {
    let from = dir.join("from");
    fs::write(&from, b"bytes").unwrap();
    RealFs::new()
        .link_noclobber(&from, &dir.join("to"))
        .unwrap();
    vec![]
}

fn fsync_dir(dir: &Path) -> Vec<Want> {
    RealFs::new().fsync_dir(dir).unwrap();
    vec![Want::At(SyncKind::Dir, dir.to_path_buf())]
}

fn remove(dir: &Path) -> Vec<Want> {
    let file = dir.join("file");
    fs::write(&file, b"bytes").unwrap();
    RealFs::new().remove(&file).unwrap();
    vec![]
}

fn append_durable(dir: &Path) -> Vec<Want> {
    let file = dir.join("log");
    RealFs::new().append_durable(&file, b"bytes").unwrap();
    vec![Want::At(SyncKind::FileData, file)]
}

fn truncate(dir: &Path) -> Vec<Want> {
    let file = dir.join("log");
    fs::write(&file, b"bytes").unwrap();
    RealFs::new().truncate(&file, 2).unwrap();
    vec![Want::At(SyncKind::FileData, file)]
}

/// Syncs its temp file before the rename, the directory after it.
fn atomic_replace(dir: &Path) -> Vec<Want> {
    let file = dir.join("ref");
    fs::write(&file, b"old").unwrap();
    RealFs::new().atomic_replace(&file, b"new").unwrap();
    vec![
        Want::TempIn(SyncKind::File, dir.to_path_buf()),
        Want::At(SyncKind::Dir, dir.to_path_buf()),
    ]
}

fn sync_data(dir: &Path) -> Vec<Want> {
    let file = dir.join("log");
    fs::write(&file, b"bytes").unwrap();
    RealFs::new().sync_data(&file).unwrap();
    vec![Want::At(SyncKind::FileData, file)]
}

/// Syncs nothing: a later `sync_data` does.
fn append(dir: &Path) -> Vec<Want> {
    RealFs::new().append(&dir.join("log"), b"bytes").unwrap();
    vec![]
}

/// Syncs the target's directory.
fn rename_durable(dir: &Path) -> Vec<Want> {
    let sub = dir.join("sub");
    fs::create_dir(&sub).unwrap();
    let from = dir.join("from");
    fs::write(&from, b"bytes").unwrap();
    RealFs::new()
        .rename_durable(&from, &sub.join("to"))
        .unwrap();
    vec![Want::At(SyncKind::Dir, sub)]
}

/// Runs `case` in a fresh directory with the `drop`-th sync dropped (none for `None`), and
/// whether `RealFs` made exactly the syncs the method documents, in order. Returns how many
/// it documents.
fn check(name: &str, case: Case, drop: Option<usize>) -> Result<usize, String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (wants, calls) = watch_syncs(drop, || case(&root));

    // Each documented sync, in order, and nothing else.
    let same = wants.len() == calls.len() && wants.iter().zip(&calls).all(|(w, c)| w.matches(c));
    if !same {
        return Err(format!(
            "{name}: RealFs synced {calls:?}, its documentation promises {wants:?}"
        ));
    }
    Ok(wants.len())
}

/// The KL entry "No test watches what a `RealFs` call does inside": every documented sync is
/// made, and dropping any one of them, one at a time, fails the check.
#[test]
fn realfs_dropping_a_sync_fails_a_test() {
    let mut planted = 0;
    for (name, case) in CASES {
        // 1. As built, each method makes its documented syncs.
        let documented = check(name, case, None).unwrap_or_else(|e| panic!("{e}"));

        // 2. With any one of them dropped, the check fails.
        for k in 0..documented {
            assert!(
                check(name, case, Some(k)).is_err(),
                "{name}: dropping sync {k} went unnoticed"
            );
            planted += 1;
        }
    }
    assert_eq!(
        planted, DOCUMENTED_SYNCS,
        "one planted drop per documented sync"
    );
}
