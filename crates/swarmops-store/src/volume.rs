//! Which volumes a store may live on (storage-core design, §2.1 P4).
//!
//! One check per platform answers whether a path is on a volume the store supports: on Linux the
//! path's name, on macOS the type of the volume it is on, and on Windows whether the path is a
//! network share. A path that does not exist yet is judged by the nearest directory above it
//! that does.

use camino::Utf8Path;

/// Whether a store may live at absolute path `path` (storage-core design, §2.1 P4); `path` need
/// not exist yet.
///
/// - Linux: false for a path under `/mnt/`, where WSL mounts Windows drives (drvfs); true
///   otherwise. Path components alone are compared, so `/mnt` itself is true. Detecting NFS or
///   FUSE by `statfs` is M6's.
/// - macOS: true on APFS, and on HFS+ with a journal; false on any other volume (SMB, NFS, AFP,
///   WebDAV, FAT, exFAT, FUSE, devfs), and when the volume cannot be read.
/// - Windows: true until its check (§2.1 P4) lands.
pub fn is_supported_store_fs(path: &Utf8Path) -> bool {
    supported(path)
}

/// Linux: the `/mnt/` rule, by name alone.
#[cfg(not(any(target_os = "macos", windows)))]
fn supported(path: &Utf8Path) -> bool {
    match path.strip_prefix("/mnt") {
        Ok(below) => below.as_str().is_empty(),
        Err(_) => true,
    }
}

/// macOS: the type of the volume that holds `path`, or the nearest directory above it that
/// exists.
#[cfg(target_os = "macos")]
fn supported(path: &Utf8Path) -> bool {
    use rustix::io::Errno;

    // Walk up past the names not made yet; any other failure is no volume this check accepts.
    for dir in path.ancestors() {
        match rustix::fs::statfs(dir.as_std_path()) {
            Ok(stat) => {
                return is_supported_volume(&stat);
            }
            Err(Errno::NOENT | Errno::NOTDIR) => {}
            Err(_) => {
                return false;
            }
        }
    }
    false
}

/// macOS: APFS, or HFS+ with its journal on (`MNT_JOURNALED`); every sync on them is
/// `F_FULLFSYNC` (§2.1 P1, P2).
#[cfg(target_os = "macos")]
fn is_supported_volume(stat: &rustix::fs::StatFs) -> bool {
    const APFS: &[u8] = b"apfs";
    const HFS: &[u8] = b"hfs";

    // `f_fstypename` is a NUL-padded C string, such as "apfs\0\0…".
    let name: Vec<u8> = (stat.f_fstypename.iter())
        .map(|c| c.to_ne_bytes()[0])
        .take_while(|&b| b != 0)
        .collect();
    let journaled = stat.f_flags & libc::MNT_JOURNALED.cast_unsigned() != 0;

    name == APFS || (name == HFS && journaled)
}

/// Windows: no check yet; its own (UNC paths refused) is §2.1 P4's.
#[cfg(windows)]
fn supported(_path: &Utf8Path) -> bool {
    true
}
