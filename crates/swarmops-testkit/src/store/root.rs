//! Fresh store roots and the file helpers the store's tests use on them.

use std::fs::{self as stdfs, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_store::{FsOps, Layout, RealFs, StoreFormat};
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

/// The layout of the store rooted at `root`, in the format its `FORMAT` names; FORMAT 1 when
/// there is no `FORMAT` (a metadata store made on its own, as the store's tests make, opens so).
/// Panics on a `FORMAT` it cannot read, or that names no format this build reads: a layout
/// guessed for such a store would have the test look at another format's files.
pub fn layout_of(root: impl Into<Utf8PathBuf>) -> Layout {
    let root = root.into();
    let format = match stdfs::read(root.join("FORMAT")) {
        Ok(marker) => StoreFormat::from_marker(&marker).unwrap_or_else(|| {
            let marker = String::from_utf8_lossy(&marker);
            panic!("{root}/FORMAT names no format this build reads: {marker:?}")
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => StoreFormat::V1,
        Err(e) => panic!("read {root}/FORMAT: {e}"),
    };
    Layout::with_format(root, format)
}

/// The real filesystem, as the stores take it.
pub fn real_fs() -> Arc<dyn FsOps> {
    Arc::new(RealFs::new())
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

#[cfg(test)]
mod tests {
    use std::panic;

    use swarmops_store::INIT_FORMATS;

    use super::*;

    /// A root without `FORMAT` lays out as FORMAT 1, and one with a marker as the format it
    /// names. A `FORMAT` that names no format, or cannot be read (a directory), is no layout:
    /// the helper panics, naming the file, where it once fell back to FORMAT 1.
    #[test]
    fn layout_of_guesses_no_format() {
        let (_dir, root) = temp_dir();
        assert_eq!(layout_of(root.clone()).format(), StoreFormat::V1);
        for &format in INIT_FORMATS {
            stdfs::write(root.join("FORMAT"), format.marker()).unwrap();
            assert_eq!(layout_of(root.clone()).format(), format);
        }

        let panics = |what: &str| {
            let caught = panic::catch_unwind(|| layout_of(root.clone()));
            let payload = caught.expect_err(what);
            let message = payload.downcast_ref::<String>().expect("a message");
            assert!(message.contains("/FORMAT"), "{what}: {message}");
            message.clone()
        };
        stdfs::write(root.join("FORMAT"), b"swarmops-store 9\n").unwrap();
        let unknown = panics("a marker of no format");
        assert!(unknown.contains("names no format"), "{unknown}");
        stdfs::remove_file(root.join("FORMAT")).unwrap();
        stdfs::create_dir(root.join("FORMAT")).unwrap();
        let unread = panics("a FORMAT that is a directory");
        assert!(unread.contains("read "), "{unread}");
    }
}
