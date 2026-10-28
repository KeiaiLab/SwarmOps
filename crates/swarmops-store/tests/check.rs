//! Tests of `MetaStore::check` (the M6 design §6) and of what the check's damage does to the
//! index's open.
//!
//! `journal2.ckpt` is `u64le offset | u64le seq | u32le crc32(the 16 bytes before)`.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{
    ChangesetMeta, Index, Journal, Layout, MetaLock, MetaStore, RealFs, StoreFormat,
};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::store::new_root;
use tempfile::TempDir;

/// A fresh store of `format` with changesets 0 to `n - 1`, one transaction each.
fn store_of(format: StoreFormat, n: u64) -> (TempDir, Layout, MetaStore) {
    let (dir, layout) = new_root();
    let layout = Layout::with_format(layout.root(), format);
    let store = MetaStore::create_in(layout.clone(), Arc::new(NoSyncFs)).unwrap();
    for n in 0..n {
        commit(&store, n);
    }
    (dir, layout, store)
}

/// The changeset `n`, a root, as `insert_changeset` takes it.
fn changeset(n: u64) -> ChangesetMeta {
    let mut id = [0; 32];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id[31] = 0xc5;
    ChangesetMeta::new(
        ChangesetId::new(ObjectId::new(id)),
        0,
        0,
        [n.to_le_bytes()[0]; 32],
        Vec::new(),
    )
}

/// Commits changeset `n` in a transaction of its own.
fn commit(store: &MetaStore, n: u64) {
    let mut write = store.write().unwrap();
    write.insert_changeset(changeset(n)).unwrap();
    write.commit().unwrap();
}

/// Compacts the index of `layout` under the writer lock.
fn compact(layout: &Layout) -> u64 {
    let _lock = MetaLock::acquire(layout, Duration::from_secs(10)).unwrap();
    Index::open(layout, Arc::new(NoSyncFs))
        .unwrap()
        .compact()
        .unwrap()
}

/// The offsets of the journal's frames.
fn frame_offsets(layout: &Layout) -> Vec<u64> {
    let journal = Journal::open(layout, Arc::new(NoSyncFs)).unwrap();
    let frames = journal.frames_from(0, 100).unwrap();
    frames.iter().map(|frame| frame.offset).collect()
}

/// A `journal2.ckpt` of `offset` and `seq`, its CRC right.
fn ckpt2(offset: u64, seq: u64) -> Vec<u8> {
    let mut bytes = offset.to_le_bytes().to_vec();
    bytes.extend(seq.to_le_bytes());
    bytes.extend(crc32fast::hash(&bytes).to_le_bytes());
    bytes
}

/// Recomputes and writes the header CRC (`hcrc`, bytes `20..24`) of the FORMAT 2 frame at
/// `offset` in `log`.
fn fix_header_crc(log: &mut [u8], offset: usize) {
    let hcrc = crc32fast::hash(&log[offset..offset + 20]);
    log[offset + 20..offset + 24].copy_from_slice(&hcrc.to_le_bytes());
}

/// Swaps entries `a` and `b` (each `width` bytes) of file `path`.
fn swap_entries(path: &Utf8Path, width: usize, a: usize, b: usize) {
    let mut bytes = fs::read(path).unwrap();
    let first = bytes[a * width..(a + 1) * width].to_vec();
    let second = bytes[b * width..(b + 1) * width].to_vec();
    bytes[a * width..(a + 1) * width].copy_from_slice(&second);
    bytes[b * width..(b + 1) * width].copy_from_slice(&first);
    fs::write(path, bytes).unwrap();
}

/// The index past the checkpoint counts the records the journal vouches for; with a FORMAT 2
/// checkpoint its entry disagrees with, the journal vouches for nothing, and the records are
/// counted by their lengths, as for a FORMAT 1 checkpoint that does not read: the damage is the
/// journal's, which the check reports.
#[test]
fn index_counts_by_length_past_a_format_2_checkpoint_its_entry_disagrees_with() {
    let (_dir, layout, store) = store_of(StoreFormat::V2, 3);
    store.flush().unwrap();
    commit(&store, 3);
    let frames = frame_offsets(&layout);
    fs::write(layout.journal_ckpt(), ckpt2(frames[1], 2)).unwrap();

    let index = Index::open(&layout, Arc::new(NoSyncFs)).unwrap();
    assert_eq!(index.len(), 4);
}

#[test]
fn meta_check_reports_damaged_metadata_files_with_verify_text() {
    for format in [StoreFormat::V1, StoreFormat::V2] {
        let (_dir, layout, store) = store_of(format, 3);
        let mut write = store.write().unwrap();
        write
            .cas_ref("main", None, Some(changeset(2).id), "test/agent")
            .unwrap();
        write.commit().unwrap();
        assert_eq!(compact(&layout), 3);
        let checked = store.check().unwrap();
        assert_eq!(checked.problems, Vec::<String>::new(), "{format:?}");
        assert_eq!(checked.changesets.len(), 3, "{format:?}");
    }

    let (_dir, layout, store) = store_of(StoreFormat::V1, 2);
    fs::remove_file(layout.meta_lock()).unwrap();
    fs::create_dir(layout.meta_lock()).unwrap();
    assert_eq!(
        store.check().unwrap().problems,
        [
            "damaged metadata: meta/lock is not a regular file: every write fails on it until it \
             is removed, and the next write makes a new one"
        ]
    );

    let (_dir, layout, store) = store_of(StoreFormat::V1, 2);
    fs::write(layout.journal_ckpt(), b"bad").unwrap();
    assert_eq!(
        store.check().unwrap().problems,
        [
            "damaged metadata: meta/journal.ckpt does not hold a u64le offset: the records the \
             checkpoint passed are not checked against the index"
        ]
    );

    let (_dir, layout, store) = store_of(StoreFormat::V1, 2);
    let offsets = frame_offsets(&layout);
    let bogus = offsets[1] + 1;
    Journal::open(&layout, Arc::new(NoSyncFs))
        .unwrap()
        .set_ckpt(bogus)
        .unwrap();
    assert_eq!(
        store.check().unwrap().problems,
        [format!(
            "damaged metadata: meta/journal.ckpt holds {bogus}, which is not a frame boundary: \
             reads and writes fail on it, and the records the checkpoint passed are not checked \
             against the index"
        )]
    );

    for file in ["journal.log", "journal.off"] {
        let (_dir, layout, store) = store_of(StoreFormat::V1, 2);
        fs::remove_file(layout.meta_path(file)).unwrap();
        assert_eq!(
            store.check().unwrap().problems,
            [format!(
                "damaged metadata: meta/{file} is missing: the journal is not checked, and the \
                 index is checked as it is"
            )]
        );
    }

    let (_dir, layout, store) = store_of(StoreFormat::V1, 2);
    compact(&layout);
    let ids_rec = layout.meta_path("sorted/ids.rec");
    let bytes = fs::read(&ids_rec).unwrap();
    fs::write(&ids_rec, &bytes[..bytes.len() - 1]).unwrap();
    assert_eq!(
        store.check().unwrap().problems,
        [
            "damaged metadata: the changeset index does not open (a sorted view file ends in a \
             partial entry): it is not checked, and the journal's changesets are"
        ]
    );

    fs::remove_file(layout.journal_log()).unwrap();
    assert_eq!(
        store.check().unwrap().problems,
        [
            "damaged metadata: meta/journal.log is missing: the journal is not checked",
            "damaged metadata: the changeset index does not open (a sorted view file ends in a \
             partial entry): it is not checked; with no journal either, no changeset is known, \
             so neither changesets nor the targets of refs are checked"
        ]
    );
}

#[test]
fn meta_check_reports_journal_index_view_and_ref_mismatches_with_verify_text() {
    let (_dir, layout, store) = store_of(StoreFormat::V1, 3);
    store.flush().unwrap();
    assert_eq!(compact(&layout), 3);
    let offsets = frame_offsets(&layout);
    let gens = layout.meta_path("idx/gen.col");
    let mut col = fs::read(&gens).unwrap();
    col[8..16].copy_from_slice(&99_u64.to_le_bytes());
    fs::write(&gens, col).unwrap();
    swap_entries(&layout.meta_path("sorted/ids.key"), 32, 0, 1);
    swap_entries(&layout.meta_path("sorted/ids.rec"), 8, 0, 1);
    let refs = layout.meta_path("refs/release");
    fs::create_dir_all(&refs).unwrap();
    let nowhere = ChangesetId::new(ObjectId::new([0xee; 32]));
    fs::write(refs.join("broken"), b"not a ref file\n").unwrap();
    fs::write(refs.join("gone"), format!("{nowhere}\n1\n")).unwrap();
    let mut log = fs::read(layout.journal_log()).unwrap();
    let corrupt_at = offsets[2];
    log[usize::try_from(corrupt_at).unwrap() + 10] ^= 0xff;
    fs::write(layout.journal_log(), log).unwrap();

    let index = Index::open(&layout, Arc::new(RealFs::new())).unwrap();
    let swapped_rec = index.ids_view().rec_at(1).unwrap();
    assert_eq!(
        store.check().unwrap().problems,
        [
            format!("the journal is corrupt at offset {corrupt_at}"),
            format!(
                "changeset record {swapped_rec}: a sorted view disagrees with the index columns; \
                 rebuild the sorted views from the columns"
            ),
            "changeset record 1: the index disagrees with the journal; rebuild the index from \
             the journal"
                .to_owned(),
            "the file of ref release/broken is not a ref file".to_owned(),
            "ref release/gone names no changeset in the store".to_owned(),
        ]
    );
}

#[test]
fn meta_check_reads_index_and_sorted_views_in_4096_entry_blocks() {
    // 5,000 changesets span 2 blocks of 4,096 (0..4096 and 4096..5000).
    let (dir, layout) = new_root();
    let store = MetaStore::create_with_fs(layout.root(), Arc::new(NoSyncFs)).unwrap();
    let mut write = store.write().unwrap();
    for n in 0..5_000_u64 {
        write.insert_changeset(changeset(n)).unwrap();
    }
    write.commit().unwrap();
    store.flush().unwrap();
    assert_eq!(compact(&layout), 5_000);

    let checked = store.check().unwrap();
    assert_eq!(checked.problems, Vec::<String>::new());
    assert_eq!(checked.changesets.len(), 5_000);
    // 2 blocks × (4 index columns + 2 views × (1 key read + 1 rec read)) = 16 reads.
    assert_eq!(
        checked.index_reads(),
        16,
        "5,000 records must be read in 2 blocks of 4,096, not per record"
    );
    drop(dir);
}

#[test]
fn meta_check_reports_format_2_seq_prev_crc_entry_crc_and_ckpt_damage() {
    // 1. FORMAT 2 frame seq damage (header CRC recomputed so only the seq check catches it).
    {
        let (_dir, layout, store) = store_of(StoreFormat::V2, 3);
        let offsets = frame_offsets(&layout);
        let at = usize::try_from(offsets[1]).unwrap();
        let mut log = fs::read(layout.journal_log()).unwrap();
        log[at + 4..at + 12].copy_from_slice(&99_u64.to_le_bytes());
        fix_header_crc(&mut log, at);
        fs::write(layout.journal_log(), log).unwrap();

        let checked = store.check().unwrap();
        assert_eq!(
            checked.problems,
            [format!("the journal is corrupt at offset {}", offsets[1])]
        );
        assert_eq!(checked.journal_corrupt, Some(offsets[1]));
    }

    // 2. FORMAT 2 frame prev_crc damage (header CRC recomputed so only prev_crc catches it).
    {
        let (_dir, layout, store) = store_of(StoreFormat::V2, 3);
        let offsets = frame_offsets(&layout);
        let at = usize::try_from(offsets[1]).unwrap();
        let mut log = fs::read(layout.journal_log()).unwrap();
        log[at + 12] ^= 0x01;
        fix_header_crc(&mut log, at);
        fs::write(layout.journal_log(), log).unwrap();

        let checked = store.check().unwrap();
        assert_eq!(
            checked.problems,
            [format!("the journal is corrupt at offset {}", offsets[1])]
        );
        assert_eq!(checked.journal_corrupt, Some(offsets[1]));
    }

    // 3. FORMAT 2 journal2.off entry checksum (ecrc) damage.
    {
        let (_dir, layout, store) = store_of(StoreFormat::V2, 3);
        let offsets = frame_offsets(&layout);
        let mut off = fs::read(layout.journal_off()).unwrap();
        // Entry 1 is bytes 16..32; its ecrc is bytes 28..32.
        off[28] ^= 0x01;
        fs::write(layout.journal_off(), off).unwrap();

        let checked = store.check().unwrap();
        assert_eq!(
            checked.problems,
            [format!("the journal is corrupt at offset {}", offsets[1])]
        );
        assert_eq!(checked.journal_corrupt, Some(offsets[1]));
    }

    // 4. FORMAT 2 journal2.ckpt disagreed seq/offset and bad CRC are reported, not raised, by
    // the file's own name.
    {
        let (_dir, layout, store) = store_of(StoreFormat::V2, 3);
        let offsets = frame_offsets(&layout);
        fs::write(layout.journal_ckpt(), ckpt2(offsets[1], 2)).unwrap();
        assert_eq!(
            store.check().unwrap().problems,
            [format!(
                "damaged metadata: meta/journal2.ckpt holds {}, which is not a frame boundary: \
                 reads and writes fail on it, and the records the checkpoint passed are not \
                 checked against the index",
                offsets[1]
            )]
        );

        let mut bad_crc = ckpt2(offsets[1], 1);
        *bad_crc.last_mut().unwrap() ^= 0x01;
        fs::write(layout.journal_ckpt(), bad_crc).unwrap();
        assert_eq!(
            store.check().unwrap().problems,
            [
                "damaged metadata: meta/journal2.ckpt does not hold an offset, a seq and their \
                 CRC: the records the checkpoint passed are not checked against the index"
            ]
        );
    }
}
