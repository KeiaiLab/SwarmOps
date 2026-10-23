//! Which filesystems a store may live on (the M6 design §9): on Linux, an allow-list of the
//! local filesystems whose fsync the store's durability rules rely on, read with `statfs`.
//! A filesystem not on the list (NFS, SMB, FUSE, 9p among them) is refused, fail-closed.

use camino::Utf8Path;

use crate::error::StoreError;

/// `f_type` magics of the local filesystems the store supports (`statfs(2)`).
const ALLOWED_MAGICS: [i64; 7] = [
    0xEF53,      // ext2, ext3, ext4
    0x5846_5342, // xfs
    0x9123_683E, // btrfs
    0x0102_1994, // tmpfs
    0x2FC1_2FC1, // zfs
    0xF2F5_2010, // f2fs
    0x794C_7630, // overlayfs
];

/// `f_type` is a 32-bit magic; the mask drops the sign extension of a 32-bit `f_type`.
#[cfg(target_os = "linux")]
const MAGIC_MASK: i64 = 0xFFFF_FFFF;

/// Whether the store supports a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FsClass {
    /// On the allow-list.
    Supported,
    /// Not on the allow-list: refused.
    Unsupported,
}

/// Classifies a `statfs` `f_type`: only the allow-list is [`FsClass::Supported`].
pub fn classify(f_type: i64) -> FsClass {
    if ALLOWED_MAGICS.contains(&f_type) {
        return FsClass::Supported;
    }

    FsClass::Unsupported
}

/// Refuses, with [`StoreError::UnsupportedFs`], a store at `path` on a filesystem the store
/// does not support. `path` is the store's root on open, and the nearest existing ancestor of
/// the directory a create makes (a path not yet there is measured at its nearest ancestor).
///
/// Other platforms keep their own checks, and every path passes here.
#[cfg(target_os = "linux")]
pub fn check_supported_fs(path: &Utf8Path) -> Result<(), StoreError> {
    // 1. The nearest ancestor that exists carries the filesystem the path will be on.
    let mut probe = path;
    let stat = loop {
        match rustix::fs::statfs(probe.as_std_path()) {
            Ok(stat) => break stat,
            Err(rustix::io::Errno::NOENT) => {}
            // Unreadable: not provably supported, so refused.
            Err(_) => return Err(StoreError::UnsupportedFs(path.to_path_buf())),
        }

        match probe.parent() {
            Some(parent) if !parent.as_str().is_empty() => probe = parent,
            _ => return Err(StoreError::UnsupportedFs(path.to_path_buf())),
        }
    };

    // 2. Only the allow-list passes.
    let magic = stat.f_type as i64 & MAGIC_MASK;
    match classify(magic) {
        FsClass::Supported => Ok(()),
        FsClass::Unsupported => Err(StoreError::UnsupportedFs(path.to_path_buf())),
    }
}

/// Other platforms keep their own checks, and every path passes here.
#[cfg(not(target_os = "linux"))]
pub fn check_supported_fs(path: &Utf8Path) -> Result<(), StoreError> {
    let _ = path;
    Ok(())
}
