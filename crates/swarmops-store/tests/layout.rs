//! Store layout: where each piece of a store lives, and the native-filesystem check.

use camino::Utf8Path;
use swarmops_core::{ObjectKind, hash_object};
use swarmops_store::{Layout, is_native_linux_fs};

#[test]
fn layout_paths_follow_the_store_spec() {
    let layout = Layout::new("/srv/store");
    assert_eq!(layout.root(), "/srv/store");
    assert_eq!(layout.format_file(), "/srv/store/FORMAT");
    assert_eq!(layout.repo_file(), "/srv/store/REPO");
    assert_eq!(layout.meta_dir(), "/srv/store/meta");
    assert_eq!(layout.meta_path("index/v1"), "/srv/store/meta/index/v1");
    assert_eq!(layout.meta_lock(), "/srv/store/meta/lock");
    assert_eq!(layout.journal_log(), "/srv/store/meta/journal.log");
    assert_eq!(layout.journal_off(), "/srv/store/meta/journal.off");
    assert_eq!(layout.journal_ckpt(), "/srv/store/meta/journal.ckpt");
    assert_eq!(layout.objects_dir(), "/srv/store/objects");
    assert_eq!(layout.objects_tmp(), "/srv/store/objects/tmp");

    let id = hash_object(ObjectKind::Blob, b"x");
    let hex = id.to_hex();
    let expected = format!("/srv/store/objects/{}/{}", &hex[..2], &hex[2..]);
    assert_eq!(layout.object_path(id), expected.as_str());
}

#[test]
fn mnt_paths_are_not_native_linux_fs() {
    for windows_drive in ["/mnt/c", "/mnt/c/Users/me/store", "/mnt/d/repos"] {
        assert!(
            !is_native_linux_fs(Utf8Path::new(windows_drive)),
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
        assert!(is_native_linux_fs(Utf8Path::new(native)), "{native}");
    }
}
