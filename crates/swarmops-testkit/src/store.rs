//! Fresh store roots and the file helpers the store's tests use on them.

use std::fs::{self as stdfs, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_store::{FsOps, Layout, RealFs};
use tempfile::TempDir;

/// A fresh, empty directory and its path; the directory lives as long as the `TempDir`.
pub fn temp_dir() -> (TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");
    (dir, path)
}

/// A fresh, empty store root and its layout; the directory lives as long as the `TempDir`.
pub fn new_root() -> (TempDir, Layout) {
    let (dir, root) = temp_dir();
    (dir, Layout::new(root))
}

/// The real filesystem, as the stores take it.
pub fn real_fs() -> Arc<dyn FsOps> {
    Arc::new(RealFs)
}

/// The length of the file at `path`, which must exist.
pub fn file_len(path: impl AsRef<Path>) -> u64 {
    stdfs::metadata(path).unwrap().len()
}

/// Appends `bytes` to the existing file at `path`, syncing nothing.
pub fn append_bytes(path: impl AsRef<Path>, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

/// Cuts (or grows, with zeros) the existing file at `path` to `len` bytes, syncing nothing.
pub fn set_len(path: impl AsRef<Path>, len: u64) {
    let file = OpenOptions::new().write(true).open(path).unwrap();
    file.set_len(len).unwrap();
}
