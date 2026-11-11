//! FORMAT 3's journal (the FORMAT 3 design §2.3, §2.7): its checkpoint `journal3.ckpt`, which
//! keeps the base seq, and the nonce every frame from the base on ends in, exactly once, one per
//! frame of a batch too. A frame past the base without its nonce, with a second or misplaced one,
//! with only a nonce, or with a `Compacted`, is damage, listed or not.
//!
//! The stores are made with `MetaStore::create_in` of a FORMAT 3 layout: no `init` makes one yet.

use std::fs;

use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, Frame, Journal, Layout, MetaStore, Record, StoreError, StoreFormat,
};
use swarmops_testkit::store::{new_root, real_fs};
use tempfile::TempDir;

const ACTOR: &str = "test/1";

/// Changeset `n`'s id.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n`, a root, as a caller hands it to `insert_changeset`.
fn changeset(n: u64) -> ChangesetMeta {
    let delta_hash = hash_object(ObjectKind::Blob, &n.to_le_bytes()).0;
    ChangesetMeta::new(id(n), 0, 0, delta_hash, Vec::new())
}

/// The nonce record's kind.
const NONCE_KIND: u8 = 11;

/// A FORMAT 3 checkpoint's length.
const CKPT3_LEN: usize = 44;

/// A fresh store of `format`.
fn store_of(format: StoreFormat) -> (TempDir, Layout, MetaStore) {
    let (dir, layout) = new_root();
    let layout = Layout::with_format(layout.root(), format);
    let mut store = MetaStore::create_in(layout.clone(), real_fs()).unwrap();
    store.set_parallelism(1);
    (dir, layout, store)
}

/// Commits changeset `n`, and `main` at it, in a transaction of its own.
fn commit(store: &MetaStore, n: u64) {
    let mut write = store.write().unwrap();
    write.insert_changeset(changeset(n)).unwrap();
    let expected = write.get_ref("main").unwrap();
    write.cas_ref("main", expected, Some(id(n)), ACTOR).unwrap();
    write.commit().unwrap();
}

/// Every frame the journal of `layout` lists.
fn frames(layout: &Layout) -> Vec<Frame> {
    Journal::open(layout, real_fs())
        .unwrap()
        .frames_from(0, usize::MAX)
        .unwrap()
}

/// The fields of the FORMAT 3 checkpoint of `layout`: offset, seq, base seq, records, fact rows.
fn ckpt3(layout: &Layout) -> [u64; 5] {
    let bytes = fs::read(layout.journal_ckpt()).unwrap();
    assert_eq!(bytes.len(), CKPT3_LEN);
    assert_eq!(
        crc32fast::hash(&bytes[..40]).to_le_bytes(),
        bytes[40..],
        "its CRC"
    );
    let at = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
    [at(0), at(1), at(2), at(3), at(4)]
}

/// The bytes of a FORMAT 3 checkpoint of `fields`.
fn ckpt3_bytes(fields: [u64; 5]) -> Vec<u8> {
    let mut bytes: Vec<u8> = fields.iter().flat_map(|f| f.to_le_bytes()).collect();
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// What a frame past the base may hold, record by record, as a test writes it by hand.
enum Part {
    Record(Record),
    Nonce(u64),
}

/// The bytes of a FORMAT 2 frame header and `parts` as its payload, at seq `seq` after a frame
/// of payload CRC `prev_crc`.
fn frame_bytes(parts: &[Part], seq: u64, prev_crc: u32) -> Vec<u8> {
    let mut payload = u16::try_from(parts.len()).unwrap().to_le_bytes().to_vec();
    for part in parts {
        match part {
            Part::Record(record) => record.encode(&mut payload).unwrap(),
            Part::Nonce(nonce) => {
                payload.push(NONCE_KIND);
                payload.extend_from_slice(&nonce.to_le_bytes());
            }
        }
    }
    let mut frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
    frame.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(&prev_crc.to_le_bytes());
    let hcrc = crc32fast::hash(&frame);
    frame.extend_from_slice(&hcrc.to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// A ref move of `main` to changeset 1, from nothing.
fn move_main() -> Record {
    Record::RefMove {
        name: "main".to_owned(),
        old: None,
        new: Some(id(1)),
        actor: ACTOR.to_owned(),
        ts: 1_700_000_000,
    }
}

/// A FORMAT 3 store of one committed frame, then `parts` as a frame of their own appended to
/// `journal2.log` by hand, unlisted: reads and writes report it as damage at its offset, and
/// write nothing.
#[track_caller]
fn corrupt_past_the_base(parts: &[Part]) {
    let (_dir, layout, store) = store_of(StoreFormat::V3);
    commit(&store, 1);
    let last = frames(&layout).pop().unwrap();
    let at = fs::metadata(layout.journal_log()).unwrap().len();
    let mut log = fs::read(layout.journal_log()).unwrap();
    log.extend(frame_bytes(parts, last.seq + 1, last.crc));
    fs::write(layout.journal_log(), &log).unwrap();
    let off = fs::read(layout.journal_off()).unwrap();

    let read = store.read().map(drop);
    assert!(
        matches!(read, Err(StoreError::JournalCorrupt { offset }) if offset == at),
        "read: {read:?}"
    );
    let write = store.write().map(drop);
    assert!(
        matches!(write, Err(StoreError::JournalCorrupt { offset }) if offset == at),
        "write: {write:?}"
    );
    assert_eq!(fs::read(layout.journal_log()).unwrap(), log, "nothing cut");
    assert_eq!(
        fs::read(layout.journal_off()).unwrap(),
        off,
        "nothing listed"
    );
}

/// The checkpoint keeps the base seq through every move ([`Journal::set_ckpt`] and the
/// write's): frames before it are FORMAT 2's, without a nonce, and those from it on end in one.
/// A new store's is all zero but for its CRC.
#[test]
fn ckpt3_keeps_base_through_set_ckpt() {
    // 1. A new store's checkpoint is all zero.
    let (_dir, v3, store) = store_of(StoreFormat::V3);
    assert_eq!(ckpt3(&v3), [0; 5]);
    drop(store);

    // 2. A FORMAT 2 store of three frames, turned FORMAT 3 by hand as an upgrade does: the base
    // seq is the frame count then.
    let (_dir, v2, store) = store_of(StoreFormat::V2);
    for n in 1..=3 {
        commit(&store, n);
    }
    drop(store);
    let layout = Layout::with_format(v2.root(), StoreFormat::V3);
    fs::write(layout.journal_ckpt(), ckpt3_bytes([0, 0, 3, 0, 0])).unwrap();
    // And its views: the new store's manifest of no run.
    fs::create_dir_all(layout.views_dir()).unwrap();
    fs::copy(
        v3.views_dir().join("MANIFEST"),
        layout.views_dir().join("MANIFEST"),
    )
    .unwrap();
    // And its fact rows: none, as an upgrade writes them.
    fs::write(layout.facts_col(), b"").unwrap();

    // 3. Two writes and a flush: the checkpoint moves to the end, the base stays.
    let store = MetaStore::open_in(layout.clone(), real_fs()).unwrap();
    commit(&store, 4);
    commit(&store, 5);
    store.flush().unwrap();
    let end = fs::metadata(layout.journal_log()).unwrap().len();
    assert_eq!(ckpt3(&layout), [end, 5, 3, 5, 0]);

    // 4. Moved back to 0 by the journal's own call: the base stays.
    let journal = Journal::open(&layout, real_fs()).unwrap();
    journal.set_ckpt(0).unwrap();
    assert_eq!(ckpt3(&layout), [0, 0, 3, 0, 0]);
    journal.set_ckpt(end).unwrap();
    assert_eq!(ckpt3(&layout)[..3], [end, 5, 3]);

    // 5. The frames before the base hold no nonce; those after one each, all different.
    let frames = frames(&layout);
    assert_eq!(frames.len(), 5);
    assert!(frames[..3].iter().all(|frame| frame.nonce.is_none()));
    let (a, b) = (frames[3].nonce, frames[4].nonce);
    assert!(a.is_some() && b.is_some() && a != b, "{a:?} {b:?}");

    // 6. And the store reads them all.
    let read = store.read().unwrap();
    assert_eq!(read.count_changesets().unwrap(), 5);
}

/// A frame past the base without its nonce is damage.
#[test]
fn a_v3_frame_without_its_nonce_is_corrupt() {
    corrupt_past_the_base(&[Part::Record(move_main())]);
}

/// A second nonce, or one that is not the frame's last record, is damage.
#[test]
fn a_second_or_misplaced_nonce_is_corrupt() {
    corrupt_past_the_base(&[Part::Record(move_main()), Part::Nonce(1), Part::Nonce(2)]);
    corrupt_past_the_base(&[Part::Nonce(1), Part::Record(move_main())]);
}

/// A frame of a nonce alone is damage.
#[test]
fn a_nonce_only_frame_is_corrupt() {
    corrupt_past_the_base(&[Part::Nonce(1)]);
}

/// A `Compacted` past the base is damage: no FORMAT 3 writer writes one.
#[test]
fn a_compacted_past_the_base_is_corrupt() {
    let compacted = Record::Compacted { sorted_count: 1 };
    corrupt_past_the_base(&[Part::Record(compacted), Part::Nonce(1)]);
}

/// The three frames of one `commit_frames` carry three nonces, each the one
/// [`swarmops_store::MetaWrite::nonce`] gave while its frame was open; each frame's CRC, as the
/// commit reports it, is the one its closing gave the write queue
/// ([`swarmops_store::MetaWrite::frames_from`]), and the one a frame of its records and nonce has.
#[test]
fn batched_frames_get_one_nonce_each() {
    let (_dir, layout, store) = store_of(StoreFormat::V3);
    commit(&store, 1);
    let first = frames(&layout).len() as u64;

    // 1. Three frames, each with the nonce open while it was queued.
    let mut write = store.write().unwrap();
    let mut nonces = Vec::new();
    for n in 2..=4 {
        nonces.push(write.nonce().expect("a FORMAT 3 frame has a nonce"));
        let meta = changeset(n);
        write.insert_changeset(meta).unwrap();
        write.begin_frame().unwrap();
    }
    let closed: Vec<(u64, u32)> = (write.frames_from(first).unwrap().iter())
        .map(|frame| (frame.seq, frame.crc))
        .collect();
    let infos = write.commit_frames().unwrap();

    // 2. Three nonces, different; each frame's CRC its closing's and its commit's.
    let got: Vec<Frame> = frames(&layout).split_off(usize::try_from(first).unwrap());
    assert_eq!(got.len(), 3);
    let written: Vec<Option<u64>> = got.iter().map(|frame| frame.nonce).collect();
    assert_eq!(
        written,
        nonces.iter().copied().map(Some).collect::<Vec<_>>()
    );
    assert!(nonces[0] != nonces[1] && nonces[1] != nonces[2] && nonces[0] != nonces[2]);
    for ((frame, info), (seq, crc)) in got.iter().zip(&infos).zip(&closed) {
        assert_eq!((frame.seq, frame.crc), (*seq, *crc));
        assert_eq!(info.frame_crc, Some(frame.crc));
        let again = Frame::new_in(
            StoreFormat::V3,
            frame.seq,
            frame.offset,
            frame.records.clone(),
            frame.nonce,
        );
        assert_eq!(again.crc, frame.crc);
    }

    // 3. Below FORMAT 3 no frame has one.
    let (_dir, _layout, v2) = store_of(StoreFormat::V2);
    assert_eq!(v2.write().unwrap().nonce(), None);
}
