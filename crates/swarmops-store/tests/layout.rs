//! Store layout: where each piece of a store lives, and the native-filesystem check.

use camino::Utf8Path;
use swarmops_core::{ObjectKind, hash_object};
use swarmops_store::{Layout, StoreFormat, is_native_linux_fs};

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

/// The files that differ between the formats are named by the layout's format: FORMAT 1's are
/// as they always were, and `Layout::new` is FORMAT 1; FORMAT 2's have names of their own.
#[test]
fn layout_names_files_per_format() {
    let v1 = Layout::new("/srv/store");
    assert_eq!(v1.format(), StoreFormat::V1);
    assert_eq!(v1, Layout::with_format("/srv/store", StoreFormat::V1));
    assert_eq!(v1.journal_log(), "/srv/store/meta/journal.log");
    assert_eq!(v1.journal_off(), "/srv/store/meta/journal.off");
    assert_eq!(v1.journal_ckpt(), "/srv/store/meta/journal.ckpt");
    assert_eq!(v1.refs_dir(), "/srv/store/meta/refs");

    let v2 = Layout::with_format("/srv/store", StoreFormat::V2);
    assert_eq!(v2.format(), StoreFormat::V2);
    assert_eq!(v2.journal_log(), "/srv/store/meta/journal2.log");
    assert_eq!(v2.journal_off(), "/srv/store/meta/journal2.off");
    assert_eq!(v2.journal_ckpt(), "/srv/store/meta/journal2.ckpt");
    assert_eq!(v2.refs_dir(), "/srv/store/meta/refs2");

    // Everything else is where FORMAT 1 has it.
    for (a, b) in [
        (v1.format_file(), v2.format_file()),
        (v1.meta_lock(), v2.meta_lock()),
        (v1.derived_dir(), v2.derived_dir()),
        (v1.external_ids_dir(), v2.external_ids_dir()),
        (v1.objects_tmp(), v2.objects_tmp()),
    ] {
        assert_eq!(a, b);
    }
}
