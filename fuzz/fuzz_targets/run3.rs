//! FORMAT 3 run files never panic a reader, and never make it answer wrong (the FORMAT 3 design
//! §2.4, §6): the input, as the run the manifest names, either reads, and then every lookup of
//! the store's three changesets finds each at its record, or is refused as damage
//! (`IndexCorrupt`). The store's records are those of the golden run `run_c`, the seed.
#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use swarmops_store::{ChangesetMeta, Layout, MetaStore, RealFs, StoreError, StoreFormat};

/// What a manifest of the one run `[0, 3)` of changeset records holds, its CRC last.
fn manifest() -> Vec<u8> {
    let mut bytes = b"SWMF\x01\0\0\0".to_vec();
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&3u64.to_le_bytes());
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// The records of the golden run: (id byte, delta-hash byte) at recs 0, 1, 2.
const RECORDS: [(u8, u8); 3] = [(2, 6), (3, 4), (1, 6)];

/// One FORMAT 3 store for every input: three changesets the checkpoint passed, and a manifest
/// naming their run.
fn store() -> &'static (tempfile::TempDir, Layout) {
    static STORE: OnceLock<(tempfile::TempDir, Layout)> = OnceLock::new();
    STORE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("store");
        let root = root.to_str().expect("a UTF-8 temp dir").to_owned();
        let layout = Layout::with_format(root, StoreFormat::V3);
        let store = MetaStore::create_in(layout.clone(), Arc::new(RealFs::new())).expect("a store");
        let mut write = store.write().expect("a write");
        for (id, dh) in RECORDS {
            let id = swarmops_core::ChangesetId::new(swarmops_core::ObjectId::new([id; 32]));
            let meta = ChangesetMeta::new(id, 0, 0, [dh; 32], Vec::new());
            write.insert_changeset(meta).expect("an insert");
        }
        write.commit().expect("a commit");
        store.flush().expect("the checkpoint past them");
        std::fs::write(layout.views_dir().join("MANIFEST"), manifest()).expect("the manifest");
        (dir, layout)
    })
}

fuzz_target!(|data: &[u8]| {
    // 1. The input as the run, read by a snapshot.
    let (_, layout) = store();
    let run = layout
        .views_dir()
        .join("c-0000000000000000-0000000000000003.run");
    std::fs::write(run, data).expect("the run written");
    let store = MetaStore::open_in(layout.clone(), Arc::new(RealFs::new())).expect("an open");
    let read = match store.read() {
        Ok(read) => read,
        Err(StoreError::IndexCorrupt { .. }) => return,
        Err(other) => panic!("a run reads, or is refused as damage: {other}"),
    };

    // 2. Every lookup answers right, or refuses.
    for (rec, (id, dh)) in (0..).zip(RECORDS) {
        let id = swarmops_core::ChangesetId::new(swarmops_core::ObjectId::new([id; 32]));
        match read.changeset(id) {
            Ok(found) => assert_eq!(found.map(|meta| meta.seq), Some(rec)),
            Err(StoreError::IndexCorrupt { .. }) => {}
            Err(other) => panic!("a lookup by id: {other}"),
        }
        match read.by_delta_hash(&[dh; 32]) {
            Ok(ids) => assert!(ids.contains(&id), "{ids:?}"),
            Err(StoreError::IndexCorrupt { .. }) => {}
            Err(other) => panic!("a lookup by delta hash: {other}"),
        }
    }
});
