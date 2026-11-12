//! Tests of the statfs allow-list (the M6 design, section 9).

use camino::Utf8Path;
use swarmops_store::check_supported_fs;
use swarmops_store::fs_kind::{FsClass, classify, classify_macos};

const ALLOWED: [(&str, i64); 7] = [
    ("ext2/3/4", 0xEF53),
    ("xfs", 0x5846_5342),
    ("btrfs", 0x9123_683E),
    ("tmpfs", 0x0102_1994),
    ("zfs", 0x2FC1_2FC1),
    ("f2fs", 0xF2F5_2010),
    ("overlayfs", 0x794C_7630),
];

const REFUSED: [(&str, i64); 7] = [
    ("nfs", 0x6969),
    ("smb", 0x517B),
    ("cifs", 0xFF53_4D42),
    ("smb2", 0xFE53_4D42),
    ("fuse", 0x6573_5546),
    ("9p", 0x0102_1997),
    ("unknown", 0x1234_5678),
];

#[test]
fn every_allow_listed_magic_is_accepted() {
    for (name, magic) in ALLOWED {
        assert_eq!(classify(magic), FsClass::Supported, "{name}");
    }
}

#[test]
fn network_fuse_9p_and_unknown_magics_are_refused() {
    for (name, magic) in REFUSED {
        assert_eq!(classify(magic), FsClass::Unsupported, "{name}");
    }
    assert_eq!(classify(0), FsClass::Unsupported);
    assert_eq!(classify(-1), FsClass::Unsupported);
}

#[test]
fn the_test_temp_dir_is_on_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8Path::from_path(dir.path()).unwrap();
    check_supported_fs(path).unwrap();
}

#[test]
fn a_path_not_yet_created_is_measured_at_its_nearest_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8Path::from_path(dir.path()).unwrap().join("a/b/store");
    check_supported_fs(&path).unwrap();
}

/// `/proc` (magic `0x9FA0`) is on no list, so a store there is refused.
#[cfg(target_os = "linux")]
#[test]
fn a_filesystem_off_the_list_is_refused() {
    let err = check_supported_fs(Utf8Path::new("/proc/self")).unwrap_err();
    assert!(
        matches!(err, swarmops_store::StoreError::UnsupportedFs(_)),
        "{err:?}"
    );
}

/// macOS's list: APFS, and HFS+ only with its journal on. The name is matched whole.
#[test]
fn macos_accepts_apfs_and_journaled_hfs_only() {
    assert_eq!(classify_macos(b"apfs", true), FsClass::Supported);
    assert_eq!(classify_macos(b"apfs", false), FsClass::Supported);
    assert_eq!(classify_macos(b"hfs", true), FsClass::Supported);
    assert_eq!(classify_macos(b"hfs", false), FsClass::Unsupported);
    for name in [
        "smbfs", "nfs", "afpfs", "webdav", "msdos", "exfat", "macfuse", "devfs", "autofs", "apfs2",
        "",
    ] {
        assert_eq!(
            classify_macos(name.as_bytes(), true),
            FsClass::Unsupported,
            "{name}"
        );
    }
}

/// devfs under `/dev` is on no list: a store there is refused at its nearest existing
/// directory, before anything is made.
#[cfg(target_os = "macos")]
#[test]
fn a_macos_volume_off_the_list_is_refused() {
    for path in ["/dev", "/dev/swarmops-store/repo"] {
        let err = check_supported_fs(Utf8Path::new(path)).unwrap_err();
        assert!(
            matches!(err, swarmops_store::StoreError::UnsupportedFs(_)),
            "{path}: {err:?}"
        );
    }
    assert!(!Utf8Path::new("/dev/swarmops-store").exists());
}
