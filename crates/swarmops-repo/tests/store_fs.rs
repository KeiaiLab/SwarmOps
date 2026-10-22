//! `Repo::store_fs`: the store's own filesystem handle, for the store files that live beside its
//! objects and metadata (M13's `remotes.toml`).

use std::sync::Arc;

use swarmops_repo::LocalRepo;
use swarmops_testkit::fs::RecordingFs;

/// A write through `store_fs` goes through the `FsOps` the repository was opened with, so the
/// test hooks and the fault doubles see it as they see the store's own writes.
#[test]
fn store_fs_writes_through_the_stores_fs() {
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    drop(LocalRepo::create(&root, "store-fs").unwrap());

    // 1. Opened over a recording filesystem, which names each call by its path in the store.
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(root.clone().into_std_path_buf());
    let repo = LocalRepo::open_with_fs(&root, Arc::new(fs)).unwrap();
    calls();

    // 2. An atomic replace of a file at the store's root, through the handle.
    let file = root.join("remotes.toml");
    repo.store_fs()
        .atomic_replace(file.as_std_path(), b"version = 1\n")
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"version = 1\n");

    // 3. The recording filesystem saw it.
    let calls = calls();
    assert_eq!(calls, ["atomic_replace remotes.toml"]);
}
