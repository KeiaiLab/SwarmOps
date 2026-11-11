//! FORMAT 3 feature lists never panic when read, and whatever reads is canonical (the FORMAT 3
//! design §2.6, §6): the input, as a FORMAT 3 store's `meta/features`, opens the store, is
//! refused as no list (`InvalidData`), or names features this build lacks (`TooNew`, all of
//! them: the release build supports none), and then the list of exactly those names in order is
//! the input's bytes.
#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use swarmops_store::{Layout, MetaStore, RealFs, StoreError, StoreFormat};

/// What `meta/features` starts with.
const HEADER: &[u8] = b"swarmops-features 1\n";

/// One FORMAT 3 store for every input, and its directory.
fn store() -> &'static (tempfile::TempDir, Layout) {
    static STORE: OnceLock<(tempfile::TempDir, Layout)> = OnceLock::new();
    STORE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("store");
        let root = root.to_str().expect("a UTF-8 temp dir").to_owned();
        let layout = Layout::with_format(root, StoreFormat::V3);
        MetaStore::create_in(layout.clone(), Arc::new(RealFs::new())).expect("a store");
        (dir, layout)
    })
}

fuzz_target!(|data: &[u8]| {
    // 1. The input as the store's list, read by the store's open.
    let (_, layout) = store();
    std::fs::write(layout.features_file(), data).expect("the list written");
    let opened = MetaStore::open_in(layout.clone(), Arc::new(RealFs::new()));

    // 2. An open list is empty; a too-new one is exactly its names, in order.
    let names = match opened {
        Ok(_) => Vec::new(),
        Err(StoreError::TooNew { format, features }) => {
            assert_eq!(format, 3, "a feature's refusal names FORMAT 3");
            features
        }
        Err(StoreError::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
            return;
        }
        Err(other) => panic!("a list reads, or is refused as one: {other}"),
    };
    let mut list = HEADER.to_vec();
    for name in &names {
        list.extend_from_slice(name.as_bytes());
        list.push(b'\n');
    }
    assert_eq!(list, data, "a list that reads is canonical");
});
