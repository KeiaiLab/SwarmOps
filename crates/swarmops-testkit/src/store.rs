//! Fresh store roots and the file helpers the store's tests use on them; and, with the
//! `test-hooks` feature, the store-level error and crash sweeps' harness (`sweep`).

#[cfg(feature = "test-hooks")]
pub mod sweep;

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

/// Makes `file`, the bytes of an object file, a zstd object whose header and frame both record
/// `size`, around a frame holding one empty raw block: a lie that agrees with itself, so that the
/// header check passes and only decoding finds it out (storage spec §2 rule 1). The frame asks
/// for a 1 KiB window.
pub fn lying_zstd(file: &mut Vec<u8>, size: u64) {
    file.truncate(16); // "SOB1" | kind u8 | enc u8 | u16 rsvd | u64le ulen
    file[5] = 1; // zstd
    file[8..16].copy_from_slice(&size.to_le_bytes());
    file.extend([0x28, 0xB5, 0x2F, 0xFD, 0xC0, 0x00]); // magic, FHD (8-byte FCS), window 1 KiB
    file.extend(size.to_le_bytes()); // frame content size
    file.extend([0x01, 0x00, 0x00]); // last block, raw, 0 bytes
}

/// One frame of zstd's legacy v0.7 format (2016), which records its content size and holds
/// `payload` in one raw block: a format that no SwarmOps store writes, and that the store's zstd,
/// built without the legacy decoders, does not read (storage spec §2 rule 1). A zstd built with
/// its `legacy` feature decodes it to `payload`. Its block header records the size in 16 bits, so
/// `payload` holds less than 64 KiB.
pub fn zstd_v07_frame(payload: &[u8]) -> Vec<u8> {
    let len = u16::try_from(payload.len()).expect("a payload of less than 64 KiB");
    let mut frame = vec![0x27, 0xB5, 0x2F, 0xFD, 0xA0]; // v0.7 magic, FHD (single segment, 4-byte FCS)
    frame.extend(u32::from(len).to_le_bytes()); // frame content size
    frame.push(0x40); // a raw block, of
    frame.extend(len.to_be_bytes()); // `len` bytes
    frame.extend_from_slice(payload);
    frame.extend([0xC0, 0x00, 0x00]); // the end block
    frame
}

/// Makes `file`, the bytes of an object file, a zstd object of `size` payload bytes whose body is
/// `body`, its kind kept: the header check passes when the frames of `body` record `size` bytes
/// between them.
pub fn zstd_object(file: &mut Vec<u8>, size: u64, body: &[u8]) {
    file.truncate(16); // "SOB1" | kind u8 | enc u8 | u16 rsvd | u64le ulen
    file[5] = 1; // zstd
    file[8..16].copy_from_slice(&size.to_le_bytes());
    file.extend_from_slice(body);
}
