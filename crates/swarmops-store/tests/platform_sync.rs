//! The syncs of a file and of a directory, where the platforms differ (storage-core design,
//! §2.1 P1, P2).

use std::fs;
use std::io::Write;

use swarmops_store::{FsOps, RealFs};

/// The directory probe (P2): a sync of a directory just made, with an entry just made in it,
/// returns. On macOS it is `F_FULLFSYNC` on a directory descriptor (assumption AP1).
#[test]
fn fsync_dir_of_a_new_directory_succeeds() {
    let temp = tempfile::tempdir().expect("create a temporary directory");
    let dir = temp.path().join("new");

    // 1. A new directory, and a new file in it.
    fs::create_dir(&dir).expect("create a directory");
    fs::write(dir.join("entry"), b"x").expect("write a file");

    // 2. The entry's directory, then the directory's parent, synced.
    RealFs.fsync_dir(&dir).expect("sync the new directory");
    RealFs.fsync_dir(temp.path()).expect("sync its parent");
}

/// The sync by path (P1): a file another handle wrote and closed is synced through a handle of
/// its own, opened with write access as Windows's flush needs. Nothing is created or cut.
#[test]
fn sync_data_of_a_file_another_handle_wrote() {
    let temp = tempfile::tempdir().expect("create a temporary directory");
    let path = temp.path().join("written");

    // 1. Written and closed by another handle.
    let mut file = fs::File::create(&path).expect("create a file");
    file.write_all(b"the bytes").expect("write the file");
    drop(file);

    // 2. Synced by path: the content stays as written.
    RealFs.sync_data(&path).expect("sync the file");
    assert_eq!(fs::read(&path).expect("read the file"), b"the bytes");

    // 3. A missing file is not made by the sync.
    let missing = temp.path().join("missing");
    RealFs.sync_data(&missing).expect_err("no file to sync");
    assert!(!missing.exists(), "the sync made a file");
}
