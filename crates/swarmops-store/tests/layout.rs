//! Store layout: where each piece of a store lives, and the native-filesystem check.

use camino::Utf8Path;
use swarmops_core::{ObjectKind, hash_object};
use swarmops_store::Layout;
#[cfg(target_os = "linux")]
use swarmops_store::is_supported_store_fs;

#[test]
fn layout_paths_follow_the_store_spec() {
    let root = Utf8Path::new("/srv/store");
    let meta = root.join("meta");
    let layout = Layout::new(root);
    assert_eq!(layout.root(), root);
    assert_eq!(layout.format_file(), root.join("FORMAT"));
    assert_eq!(layout.repo_file(), root.join("REPO"));
    assert_eq!(layout.meta_dir(), meta);
    assert_eq!(layout.meta_path("index/v1"), meta.join("index/v1"));
    assert_eq!(layout.meta_lock(), meta.join("lock"));
    assert_eq!(layout.journal_log(), meta.join("journal.log"));
    assert_eq!(layout.journal_off(), meta.join("journal.off"));
    assert_eq!(layout.journal_ckpt(), meta.join("journal.ckpt"));
    assert_eq!(layout.objects_dir(), root.join("objects"));
    assert_eq!(layout.objects_tmp(), root.join("objects").join("tmp"));

    let id = hash_object(ObjectKind::Blob, b"x");
    let hex = id.to_hex();
    let expected = root.join("objects").join(&hex[..2]).join(&hex[2..]);
    assert_eq!(layout.object_path(id), expected);
}

// The `/mnt/` rule is Linux's: WSL mounts Windows drives there (drvfs). Elsewhere a volume is
// judged by its file system (the storage-core design, §2.1 P4).
#[cfg(target_os = "linux")]
#[test]
fn mnt_paths_are_not_native_linux_fs() {
    for windows_drive in ["/mnt/c", "/mnt/c/Users/me/store", "/mnt/d/repos"] {
        assert!(
            !is_supported_store_fs(Utf8Path::new(windows_drive)),
            "{windows_drive}"
        );
    }
    for native in [
        "/home/me/.local/state/swarmops/repos/r",
        "/tmp/store",
        "/mnt",
        "/mntx/store",
        "/srv/mnt/c",
    ] {
        assert!(is_supported_store_fs(Utf8Path::new(native)), "{native}");
    }
}
