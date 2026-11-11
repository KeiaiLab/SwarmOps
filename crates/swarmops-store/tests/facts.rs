//! FORMAT 3's fact index (the FORMAT 3 design §2.8): one row of `facts.col` per fact, appended as
//! its frame is materialized; lookups by subject and by other over the fact runs and the rows
//! past them; the version CAS of `put_fact`; blobs a fact names durable before its frame (W9);
//! rows past the checkpoint counted only as the journal vouches for them; entity ids that no
//! later frame repeats; and a fact on a FORMAT 1 or 2 store refused before anything is written.
//!
//! The stores are made with `MetaStore::create_in` of a FORMAT 3 layout (no `init` makes one
//! yet), the fact kinds enabled with the test hooks' `supported_for_tests`.

use std::fs;

use swarmops_core::{ChangesetId, ObjectId, ObjectKind, hash_object};
use swarmops_store::{
    Base, CasResult, ChangesetMeta, Expect, Fact, FactKind, Guard, Layout, MetaStore, Record,
    StoreError, StoreFormat, supported_for_tests,
};
use swarmops_testkit::store::{new_root, real_fs};
use tempfile::TempDir;

const ACTOR: &str = "test/1";

/// The bytes of one row of `facts.col`.
const ROW_LEN: usize = 24;

/// The features the tests enable: the kinds their facts are of.
const FEATURES: [&str; 2] = ["lineage", "review"];

/// A fresh store of `format`, the test features supported while the guard lives.
fn store(format: StoreFormat) -> (TempDir, Layout, MetaStore, Guard) {
    let guard = supported_for_tests(&FEATURES);
    let (dir, layout) = new_root();
    let layout = Layout::with_format(layout.root(), format);
    let store = MetaStore::create_in(layout.clone(), real_fs()).unwrap();
    (dir, layout, store, guard)
}

/// Changeset `n`'s id.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// A key of 32 bytes of `n`'s hash, for subjects and others.
fn key(n: u64) -> [u8; 32] {
    hash_object(ObjectKind::Blob, &n.to_le_bytes()).0
}

/// A Lineage fact (no blob) about `subject`, its lineage `other`.
fn lineage(subject: [u8; 32], other: [u8; 32]) -> Fact {
    Fact::new(
        FactKind::Lineage,
        1,
        subject,
        other,
        7,
        ACTOR.to_owned(),
        vec![3; 32],
    )
    .unwrap()
}

/// A Review Resolve of version `version` about change request `cr` (no blob).
fn resolve(cr: [u8; 32], version: u32) -> Fact {
    let body = version.to_le_bytes().to_vec();
    Fact::new(FactKind::Review, 4, cr, [5; 32], 7, ACTOR.to_owned(), body).unwrap()
}

/// A Review Open about `cr` naming the blobs `intent` and `title`.
fn open(cr: [u8; 32], intent: [u8; 32], title: [u8; 32]) -> Fact {
    let body = [&[0; 12][..], &[4], b"main", &[1; 32], &intent, &title].concat();
    Fact::new(FactKind::Review, 0, cr, [6; 32], 7, ACTOR.to_owned(), body).unwrap()
}

/// Commits `facts` in one frame of their own; returns its seq.
fn put_all(store: &MetaStore, facts: &[Fact]) -> u64 {
    let mut write = store.write().unwrap();
    for fact in facts {
        let applied = write.put_fact(fact.clone(), Expect::Any).unwrap();
        assert_eq!(applied, CasResult::Applied { old: None });
    }
    write.commit().unwrap().frame_seq
}

/// The rows of `facts.col`: (offset, len, kind, type, frame seq).
fn rows(layout: &Layout) -> Vec<(u64, u32, u8, u8, u64)> {
    let bytes = fs::read(layout.facts_col()).unwrap();
    assert_eq!(bytes.len() % ROW_LEN, 0, "whole rows");
    let u64_at = |row: &[u8], at: usize| u64::from_le_bytes(row[at..at + 8].try_into().unwrap());
    (bytes.chunks(ROW_LEN))
        .map(|row| {
            assert_eq!(row[14..16], [0, 0], "the reserved bytes");
            let len = u32::from_le_bytes(row[8..12].try_into().unwrap());
            (u64_at(row, 0), len, row[12], row[13], u64_at(row, 16))
        })
        .collect()
}

/// Every fact is one row: its place and length in `journal2.log`, which hold it, its kind, type
/// and frame; a frame's changesets and ref moves get none, and its commit counts no fact as a
/// ref move.
#[test]
fn facts_materialize_one_row_each() {
    let (_dir, layout, store, _guard) = store(StoreFormat::V3);

    // 1. A frame of a changeset, a ref move and three facts between them.
    let facts = [
        lineage(key(1), key(2)),
        resolve(key(3), 0),
        lineage(key(4), [0; 32]),
    ];
    let mut write = store.write().unwrap();
    write.put_fact(facts[0].clone(), Expect::Any).unwrap();
    let meta = ChangesetMeta::new(id(1), 0, 0, key(9), Vec::new());
    write.insert_changeset(meta).unwrap();
    write.put_fact(facts[1].clone(), Expect::Any).unwrap();
    write.cas_ref("main", None, Some(id(1)), ACTOR).unwrap();
    write.put_fact(facts[2].clone(), Expect::Any).unwrap();
    let info = write.commit().unwrap();
    assert_eq!((info.inserted, info.moved_refs), (1, 1));

    // 2. Then two more in a frame of their own, and the store flushed.
    let more = [resolve(key(3), 1), lineage(key(1), key(5))];
    let second = put_all(&store, &more);
    store.flush().unwrap();

    // 3. Five rows, each naming its fact's bytes in the journal.
    let journal = fs::read(layout.journal_log()).unwrap();
    let all: Vec<&Fact> = facts.iter().chain(&more).collect();
    let seqs = [
        info.frame_seq,
        info.frame_seq,
        info.frame_seq,
        second,
        second,
    ];
    let rows = rows(&layout);
    assert_eq!(rows.len(), 5);
    for ((row, fact), seq) in rows.iter().zip(&all).zip(seqs) {
        let (offset, len, kind, ty, frame_seq) = *row;
        let at = usize::try_from(offset).unwrap();
        let bytes = &journal[at..at + usize::try_from(len).unwrap()];
        let decoded = Record::decode_in(StoreFormat::V3, Base::After, &FactKind::ALL, bytes);
        assert_eq!(decoded, Ok((Record::Fact((*fact).clone()), bytes.len())));
        assert_eq!((kind, ty, frame_seq), (fact.kind().code(), fact.ty(), seq));
    }

    // 4. Each found by its subject and its other, in journal order.
    let read = store.read().unwrap();
    assert_eq!(
        read.facts_by_subject(key(1)).unwrap(),
        [facts[0].clone(), more[1].clone()]
    );
    assert_eq!(
        read.facts_by_subject(key(3)).unwrap(),
        [facts[1].clone(), more[0].clone()]
    );
    assert_eq!(read.facts_by_other(key(2)).unwrap(), [facts[0].clone()]);
    assert_eq!(read.facts_by_other([0; 32]).unwrap(), Vec::<Fact>::new());
}

/// A fact whose version is not the count of its kind's facts about its subject, the committed
/// and the queued, queues nothing; one whose version is, queues.
#[test]
fn a_version_cas_mismatch_queues_nothing() {
    let (_dir, layout, store, _guard) = store(StoreFormat::V3);
    let cr = key(1);
    let mismatch = CasResult::Mismatch { actual: None };

    // 1. No fact yet: version 1 is refused, 0 queued; then 1 counts the queued one.
    let mut write = store.write().unwrap();
    assert_eq!(
        write.put_fact(resolve(cr, 1), Expect::Version(1)).unwrap(),
        mismatch
    );
    assert_eq!(write.queued(), []);
    let applied = CasResult::Applied { old: None };
    assert_eq!(
        write.put_fact(resolve(cr, 0), Expect::Version(0)).unwrap(),
        applied
    );
    assert_eq!(
        write.put_fact(resolve(cr, 1), Expect::Version(1)).unwrap(),
        applied
    );
    assert_eq!(
        write.put_fact(resolve(cr, 1), Expect::Version(1)).unwrap(),
        mismatch
    );
    assert_eq!(write.queued().len(), 2);
    write.commit().unwrap();

    // 2. Committed: 2 now; another kind or subject counts apart.
    let journal_len = fs::metadata(layout.journal_log()).unwrap().len();
    let mut write = store.write().unwrap();
    assert_eq!(
        write.put_fact(resolve(cr, 0), Expect::Version(0)).unwrap(),
        mismatch
    );
    assert_eq!(
        write.put_fact(resolve(cr, 1), Expect::Version(1)).unwrap(),
        mismatch
    );
    assert_eq!(write.queued(), []);
    write.commit().unwrap();
    assert_eq!(
        fs::metadata(layout.journal_log()).unwrap().len(),
        journal_len
    );
    let mut write = store.write().unwrap();
    let other_kind = lineage(cr, key(2));
    assert_eq!(
        write.put_fact(other_kind, Expect::Version(0)).unwrap(),
        applied
    );
    assert_eq!(
        write
            .put_fact(resolve(key(2), 0), Expect::Version(0))
            .unwrap(),
        applied
    );
    assert_eq!(
        write.put_fact(resolve(cr, 2), Expect::Version(2)).unwrap(),
        applied
    );
    write.commit().unwrap();
    assert_eq!(store.read().unwrap().facts_by_subject(cr).unwrap().len(), 4);
}

/// The rows past the checkpoint count only while the journal vouches for each field: a row
/// whose offset, length, kind, type, reserved bytes or frame seq is not its fact's is no row of
/// a snapshot (which takes the fact from the journal), and the next writer cuts it and appends
/// it again.
#[test]
fn facts_past_the_checkpoint_are_vouched_field_by_field() {
    let (_dir, layout, store, _guard) = store(StoreFormat::V3);
    put_all(&store, &[lineage(key(1), key(2)), lineage(key(3), key(4))]);
    put_all(&store, &[resolve(key(5), 0)]);
    let good = fs::read(layout.facts_col()).unwrap();
    assert_eq!(good.len(), 3 * ROW_LEN);

    // Each field of the last row: (its first byte, what to flip it by).
    for (at, flip) in [(0, 1), (8, 1), (12, 1), (13, 1), (14, 1), (16, 1)] {
        // 1. The last row's field changed, as a power loss's lost page could leave it.
        let mut bad = good.clone();
        bad[2 * ROW_LEN + at] ^= flip;
        fs::write(layout.facts_col(), &bad).unwrap();

        // 2. A snapshot finds each fact once, the unvouched one from the journal.
        let read = store.read().unwrap();
        assert_eq!(
            read.facts_by_subject(key(5)).unwrap(),
            [resolve(key(5), 0)],
            "{at}"
        );
        assert_eq!(read.facts_by_subject(key(1)).unwrap().len(), 1, "{at}");
        drop(read);

        // 3. The next writer cuts the row and replays it: the file as it was.
        drop(store.write().unwrap());
        assert_eq!(fs::read(layout.facts_col()).unwrap(), good, "{at}");
    }
}

/// A fact that names a blob the store does not hold is refused (W9: the blob is durable before
/// the frame), and nothing is queued; once every blob it names is there, it queues.
#[test]
fn a_fact_naming_an_unflushed_blob_is_refused() {
    let (_dir, layout, store, _guard) = store(StoreFormat::V3);
    let (intent, title) = (key(10), key(11));
    let fact = open(key(1), intent, title);

    // 1. Neither blob, then the intent alone: refused.
    let place = |blob: [u8; 32]| {
        let path = layout.object_path(ObjectId::new(blob));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"blob").unwrap();
    };
    for planted in [None, Some(intent)] {
        if let Some(blob) = planted {
            place(blob);
        }
        let mut write = store.write().unwrap();
        let got = write.put_fact(fact.clone(), Expect::Any);
        assert!(matches!(got, Err(StoreError::BadCall(_))), "{got:?}");
        assert_eq!(write.queued(), []);
    }

    // 2. Both: queued and committed.
    place(title);
    let mut write = store.write().unwrap();
    assert_eq!(
        write.put_fact(fact.clone(), Expect::Any).unwrap(),
        CasResult::Applied { old: None }
    );
    write.commit().unwrap();
    assert_eq!(
        store.read().unwrap().facts_by_subject(key(1)).unwrap(),
        [fact]
    );
}

/// A transaction that never commits leaves its seq to the next, which draws another nonce: the
/// entity id the first could have given names nothing, and the second's differs from it.
#[test]
fn entity_ids_differ_across_retaken_seqs() {
    let (_dir, _layout, store, _guard) = store(StoreFormat::V3);
    put_all(&store, &[lineage(key(1), key(2))]);
    let seq = store.read().unwrap().next_seq();
    let mut ids = Vec::new();
    for _ in 0..8 {
        // 1. A Review opened at the next seq, its id from that frame's nonce, then dropped.
        let mut write = store.write().unwrap();
        let nonce = write.nonce().expect("a FORMAT 3 frame's nonce");
        let cr = FactKind::Review.entity_id(seq, nonce, 0);
        write.put_fact(resolve(cr, 0), Expect::Any).unwrap();
        drop(write);
        ids.push(cr);
    }

    // 2. Every one differs, and none is in the store; the seq is still free.
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len());
    let read = store.read().unwrap();
    assert_eq!(read.next_seq(), seq);
    for cr in &ids {
        assert_eq!(read.facts_by_subject(*cr).unwrap(), Vec::<Fact>::new());
    }

    // 3. The seq taken at last: its id is none of those.
    let mut write = store.write().unwrap();
    let cr = FactKind::Review.entity_id(seq, write.nonce().unwrap(), 0);
    write.put_fact(resolve(cr, 0), Expect::Any).unwrap();
    assert_eq!(write.commit().unwrap().frame_seq, seq);
    assert!(!ids.contains(&cr));
    assert_eq!(store.read().unwrap().facts_by_subject(cr).unwrap().len(), 1);
}

/// After `drop_frame` the id given for the dropped frame's fact names no record: the next frame
/// takes the seq and keeps the nonce, and a fact of another kind there has another id.
#[test]
fn an_entity_id_of_a_dropped_frame_is_not_written() {
    let (_dir, _layout, store, _guard) = store(StoreFormat::V3);
    let seq = store.read().unwrap().next_seq();

    // 1. A Review at record 0 of the open frame, its id taken, the frame dropped.
    let mut write = store.write().unwrap();
    let nonce = write.nonce().unwrap();
    let cr = FactKind::Review.entity_id(seq, nonce, 0);
    write.put_fact(resolve(cr, 0), Expect::Any).unwrap();
    write.drop_frame();

    // 2. A Lineage in its place, committed at that seq with that nonce.
    write
        .put_fact(lineage(key(1), key(2)), Expect::Any)
        .unwrap();
    assert_eq!(write.nonce(), Some(nonce));
    assert_eq!(write.commit().unwrap().frame_seq, seq);

    // 3. No record names the dropped id.
    let read = store.read().unwrap();
    assert_eq!(read.facts_by_subject(cr).unwrap(), Vec::<Fact>::new());
    assert_eq!(read.facts_by_other(cr).unwrap(), Vec::<Fact>::new());
    let entries = read.journal_after(None, usize::MAX).unwrap();
    assert!(entries.iter().all(|entry| entry.key != cr), "{entries:?}");
    assert_ne!(FactKind::Lineage.entity_id(seq, nonce, 0), cr);
}

/// A fact run (domain 1) holds every row in its subject section and only the rows whose other is
/// not all zero in its other section; lookups read the run.
#[test]
fn fact_runs_skip_a_zero_other() {
    let (_dir, layout, store, _guard) = store(StoreFormat::V3);

    // 1. 1,100 facts, past a run's minimum, every other one naming no other.
    let other = |n: u64| {
        if n.is_multiple_of(2) {
            key(10_000 + n)
        } else {
            [0; 32]
        }
    };
    let facts: Vec<Fact> = (0..1_100).map(|n| lineage(key(n), other(n))).collect();
    put_all(&store, &facts);
    store.flush().unwrap();
    assert_eq!(store.step_views().unwrap(), swarmops_store::Step::Done);

    // 2. One fact run: n0 = 1,100 subjects, n1 = 550 others.
    let runs: Vec<String> = fs::read_dir(layout.views_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("f-"))
        .collect();
    assert_eq!(runs, [format!("f-{:016x}-{:016x}.run", 0, 1_100)]);
    let run = fs::read(layout.views_dir().join(&runs[0])).unwrap();
    let count = |at: usize| u64::from_le_bytes(run[at..at + 8].try_into().unwrap());
    assert_eq!((run[5], count(24), count(32)), (1, 1_100, 550));
    assert_eq!(run.len(), 48 + 40 * (1_100 + 550));

    // 3. Found through it, by subject and by other; all zero finds nothing.
    let read = store.read().unwrap();
    for n in [0, 1, 2, 999, 1_099] {
        assert_eq!(
            read.facts_by_subject(key(n)).unwrap(),
            [facts[usize::try_from(n).unwrap()].clone()]
        );
    }
    assert_eq!(read.facts_by_other(other(4)).unwrap(), [facts[4].clone()]);
    assert_eq!(read.facts_by_other([0; 32]).unwrap(), Vec::<Fact>::new());
}

/// A fact on a FORMAT 1 or 2 store needs FORMAT 3: refused, naming its feature, before anything
/// is queued or written.
#[test]
fn a_feature_record_on_v2_needs_format_3() {
    for (format, version) in [(StoreFormat::V2, 2), (StoreFormat::V1, 1)] {
        let (_dir, layout, store, _guard) = store(format);
        let journal_len = fs::metadata(layout.journal_log()).unwrap().len();
        let mut write = store.write().unwrap();
        match write.put_fact(lineage(key(1), key(2)), Expect::Any) {
            Err(StoreError::FormatRequired { name, needs, store }) => {
                assert_eq!((name.as_str(), needs, store), ("lineage", 3, version));
            }
            other => panic!("{format:?}: {other:?}"),
        }
        assert_eq!(write.queued(), []);
        write.commit().unwrap();
        assert_eq!(
            fs::metadata(layout.journal_log()).unwrap().len(),
            journal_len
        );
        assert!(!layout.features_file().exists());
    }
}
