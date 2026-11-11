//! FORMAT 3 manifests never panic a reader, and never make it answer wrong (the FORMAT 3 design
//! §2.5, §6): the input, as `meta/sorted3/MANIFEST`, either reads, and then every lookup of the
//! store's three changesets finds each at its record, or is refused as damage (`IndexCorrupt`).
//! The store holds the golden run `run_c` of its three records; the seed is the manifest naming
//! it.
#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use swarmops_store::{ChangesetMeta, Layout, MetaStore, RealFs, StoreError, StoreFormat};

/// The golden run `run_c` (the store's `tests/golden3/run_c.hex`): records `[0, 3)` below.
const RUN_C: &str = concat!(
    "5357524e010000000000000000000000030000000000000003000000000000000300000000000000d50a2c02",
    "0101010101010101010101010101010101010101010101010101010101010101020202020202020202020202",
    "0202020202020202020202020202020202020202030303030303030303030303030303030303030303030303",
    "0303030303030303020000000000000000000000000000000100000000000000040404040404040404040404",
    "0404040404040404040404040404040404040404060606060606060606060606060606060606060606060606",
    "0606060606060606060606060606060606060606060606060606060606060606060606060606060601000000",
    "000000000000000000000000020000000000000042f76a4d",
);

/// The bytes `hex` spells.
fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
        .collect()
}

/// The records of the golden run: (id byte, delta-hash byte) at recs 0, 1, 2.
const RECORDS: [(u8, u8); 3] = [(2, 6), (3, 4), (1, 6)];

/// One FORMAT 3 store for every input: three changesets the checkpoint passed, and their run.
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
        let run = layout
            .views_dir()
            .join("c-0000000000000000-0000000000000003.run");
        std::fs::write(run, unhex(RUN_C)).expect("the run");
        (dir, layout)
    })
}

fuzz_target!(|data: &[u8]| {
    // 1. The input as the manifest, read by a snapshot.
    let (_, layout) = store();
    let manifest = layout.views_dir().join("MANIFEST");
    std::fs::write(manifest, data).expect("the manifest written");
    let store = MetaStore::open_in(layout.clone(), Arc::new(RealFs::new())).expect("an open");
    let read = match store.read() {
        Ok(read) => read,
        Err(StoreError::IndexCorrupt { .. }) => return,
        Err(other) => panic!("a manifest reads, or is refused as damage: {other}"),
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
