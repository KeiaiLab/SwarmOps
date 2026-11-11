//! FORMAT 3's golden vectors (the FORMAT 3 design §6): the marker, the fact envelope, the body
//! of every kind and type, the nonce frame and the entity id, written by the encoder once and
//! pinned here; and the byte layouts of the checkpoint, feature list, manifest, runs and fact
//! rows, built here from the design's tables so that their codecs (later slices) meet fixed
//! bytes. FORMAT 1 and 2 keep their own vectors untouched.
//!
//! `tests/golden3/` holds the confirmed layouts; `tests/golden3/provisional/` the fact bodies,
//! which stay provisional until their kind's feature is supported and released (the FORMAT 3
//! design's amendment of 2026-11-11, G2 and G9): a consuming milestone may change one in place
//! until then. The envelope around them is confirmed (`envelope.hex`, the body opaque).
//!
//! Each file is lines of lowercase hex (the nonce frame's second line is the op id as text).
//! A fact file holds two facts: every optional field filled, then every one empty.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{
    Base, Fact, FactKind, Frame, Layout, MAX_FACT_BODY, MetaStore, RealFs, Record,
    SUPPORTED_FEATURES, StoreError, StoreFormat, StoreFs, UPGRADE_POISON_V3, read_format,
};

/// The nonce of the nonce frame and the entity id.
const NONCE: u64 = 0x1122_3344_5566_7788;

/// The seq of the nonce frame and the entity id's frame.
const SEQ: u64 = 3;

/// The goldens whose codecs later slices write: built from the design's tables here, and checked
/// byte for byte, but not yet decoded (so not in the flipped-byte check).
const DECODED_LATER: [&str; 4] = ["manifest", "run_c", "run_f", "facts_col"];

/// The directory, under `tests/golden3/`, of the goldens of fact bodies not yet released.
const PROVISIONAL: &str = "provisional";

/// The confirmed goldens: the design §6 table less the fact cells, plus the envelope.
const CONFIRMED_COUNT: usize = 10;

/// Each fact kind's feature name (the FORMAT 3 design §2.6).
const KIND_FEATURES: [(FactKind, &str); 7] = [
    (FactKind::Review, "review"),
    (FactKind::Annotation, "annotation"),
    (FactKind::Check, "check"),
    (FactKind::Queue, "queue"),
    (FactKind::Withdrawal, "withdrawal"),
    (FactKind::Lineage, "lineage"),
    (FactKind::Provenance, "provenance"),
];

/// Every (kind, type) cell of the design §2.8 table: 35.
const CELLS: [(FactKind, u8); 35] = {
    use FactKind::{Annotation, Check, Lineage, Provenance, Queue, Review, Withdrawal};
    [
        (Review, 0),
        (Review, 1),
        (Review, 2),
        (Review, 3),
        (Review, 4),
        (Review, 5),
        (Review, 6),
        (Review, 7),
        (Annotation, 0),
        (Annotation, 1),
        (Check, 0),
        (Check, 1),
        (Check, 2),
        (Check, 3),
        (Check, 4),
        (Check, 5),
        (Check, 6),
        (Queue, 0),
        (Queue, 1),
        (Queue, 2),
        (Queue, 3),
        (Queue, 4),
        (Withdrawal, 0),
        (Withdrawal, 1),
        (Withdrawal, 2),
        (Withdrawal, 3),
        (Lineage, 0),
        (Lineage, 1),
        (Lineage, 2),
        (Lineage, 3),
        (Lineage, 4),
        (Lineage, 5),
        (Lineage, 6),
        (Provenance, 0),
        (Provenance, 1),
    ]
};

/// Which of a cell's two facts: every optional field filled, or every one empty.
#[derive(Clone, Copy)]
enum Fill {
    Full,
    Empty,
}

#[test]
fn format3_marker_round_trips_and_v1_v2_are_unchanged() {
    let v3 = StoreFormat::V3;
    assert_eq!(v3.version(), 3);
    assert_eq!(v3.marker(), b"swarmops-store 3\n");
    assert_eq!(StoreFormat::from_marker(v3.marker()), Some(v3));
    assert_eq!(StoreFormat::from_version(3), Some(v3));
    assert_eq!(UPGRADE_POISON_V3, v3.marker());
    assert_eq!(StoreFormat::V1.marker(), b"swarmops-store 1\n");
    assert_eq!(StoreFormat::V2.marker(), b"swarmops-store 2\n");
    for format in [StoreFormat::V1, StoreFormat::V2] {
        assert_ne!(format.marker(), v3.marker());
    }

    // The golden: the marker, then the poison; 17 bytes each.
    let lines = read_golden("marker");
    assert_eq!(lines, [v3.marker().to_vec(), UPGRADE_POISON_V3.to_vec()]);
    assert!(lines.iter().all(|line| line.len() == 17));
}

/// A marker that names a later format, in the 17 bytes a marker has, is a store too new for this
/// build; any other bytes name no store format.
#[test]
fn a_marker_past_3_is_too_new() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let layout = Layout::with_format(root.clone(), StoreFormat::V3);
    let fs = StoreFs::new(Arc::new(RealFs::new()), &root);
    let read = |marker: &[u8]| {
        fs::write(layout.format_file(), marker).unwrap();
        read_format(&layout, &fs)
    };

    // 1. Too new: the format it names, and no features.
    for (marker, format) in [(&b"swarmops-store 4\n"[..], 4), (b"swarmops-store 9\n", 9)] {
        match read(marker) {
            Err(StoreError::TooNew {
                format: got,
                features,
            }) => assert_eq!((got, features), (format, Vec::<String>::new())),
            other => panic!("{marker:?}: expected TooNew, got {other:?}"),
        }
    }

    // 2. Not a marker: no digit, a trailing byte, a leading zero, or 18 bytes.
    for marker in [
        &b"swarmops-store 3x\n"[..],
        b"swarmops-store 17\n",
        b"swarmops-store 04\n",
        b"swarmops-store x\n",
        b"swarmops-store 4",
    ] {
        assert!(
            matches!(read(marker), Err(StoreError::Format(_))),
            "{marker:?}"
        );
    }
    assert_eq!(read(b"swarmops-store 3\n").unwrap(), StoreFormat::V3);
}

/// A store whose `FORMAT` names 3 but whose files are FORMAT 2's has no `journal3.ckpt`: its
/// base seq reads 0, so its FORMAT 2 frame, which ends in no nonce, is damage (the FORMAT 3 design
/// §2.3: a lost base shows as damage). It is opened at most: its reads and writes fail, and its
/// journal files keep every byte.
#[test]
fn a_store_marked_3_too_early_is_refused_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let v2 = Layout::with_format(root.clone(), StoreFormat::V2);
    let store = MetaStore::create_in(v2.clone(), Arc::new(RealFs::new())).unwrap();
    let mut write = store.write().unwrap();
    write.cas_ref("main", None, None, "test/1").unwrap();
    write.commit().unwrap();
    drop(store);
    fs::write(v2.format_file(), StoreFormat::V3.marker()).unwrap();
    let files = [v2.journal_log(), v2.journal_off(), v2.journal_ckpt()];
    let before: Vec<Vec<u8>> = files.iter().map(|f| fs::read(f).unwrap()).collect();

    // Opened (the journal files are where FORMAT 2 has them), but nothing reads or writes.
    let store = MetaStore::open(&root).unwrap();
    assert_eq!(store.format(), StoreFormat::V3);
    let corrupt = |result: Result<(), StoreError>| {
        matches!(result, Err(StoreError::JournalCorrupt { offset: 0 }))
    };
    assert!(corrupt(store.read().map(drop)));
    assert!(corrupt(store.write().map(drop)));
    drop(store);
    let after: Vec<Vec<u8>> = files.iter().map(|f| fs::read(f).unwrap()).collect();
    assert_eq!(after, before);
    assert!(!fs::exists(Layout::with_format(root, StoreFormat::V3).journal_ckpt()).unwrap());
}

#[test]
fn every_fact_kind_and_type_matches_its_golden() {
    for (kind, ty) in CELLS {
        let name = fact_file(kind, ty);
        let lines = read_golden(&name);
        let facts = [fact(kind, ty, Fill::Full), fact(kind, ty, Fill::Empty)];
        assert_eq!(lines.len(), facts.len(), "{name}");
        for (line, fact) in lines.iter().zip(facts) {
            // a. The encoder writes the file's bytes.
            let record = Record::Fact(fact);
            let mut bytes = Vec::new();
            record.encode(&mut bytes).unwrap();
            assert_eq!(&bytes, line, "{name}");

            // b. The decoder reads them as the same value, all of them.
            let decoded = Record::decode_in(StoreFormat::V3, Base::After, &FactKind::ALL, line);
            assert_eq!(decoded, Ok((record, line.len())), "{name}");
        }
    }
}

/// The detector's self-check: a golden with any one byte changed is refused, or reads as
/// another value.
#[test]
fn a_flipped_byte_in_any_golden_is_refused() {
    let names = golden_names();
    assert_eq!(
        names.len(),
        CONFIRMED_COUNT + CELLS.len(),
        "the design §6 table, with 35 fact cells, and the envelope"
    );
    let provisional = names.iter().filter(|name| is_provisional(name)).count();
    assert_eq!(provisional, CELLS.len(), "every fact cell provisional");
    for name in &names {
        if DECODED_LATER.contains(&name.as_str()) {
            continue;
        }
        let lines = read_golden(name);
        for (index, line) in lines.iter().enumerate() {
            let value = golden_value(name, index, line);
            assert!(value.is_some(), "{name} line {index} reads");
            for at in 0..line.len() {
                let mut flipped = line.clone();
                flipped[at] ^= 0x01;
                assert_ne!(
                    golden_value(name, index, &flipped),
                    value,
                    "{name} line {index} byte {at}"
                );
            }
        }
    }
}

/// The envelope is confirmed whatever its body: the golden's bytes are the design §2.8
/// envelope around an opaque body, and the encoder lays every cell's fact out the same way.
#[test]
fn the_envelope_matches_its_golden() {
    let lines = read_golden("envelope");
    assert_eq!(lines, envelopes(), "the golden's bytes");

    // 1. 77 fixed bytes, then the actor, then the body.
    for (line, (actor, body)) in lines.iter().zip(ENVELOPE_PARTS) {
        assert_eq!(line.len(), 77 + actor.len() + body.len());
        let value = envelope_value(line).expect("an envelope");
        assert!(value.ends_with(&format!("{actor:?} {body:?}")), "{value}");
    }
    assert_eq!(MAX_FACT_BODY, 4_096);

    // 2. Every cell's encoding is that envelope around its own body.
    for (kind, ty) in CELLS {
        for fill in [Fill::Full, Fill::Empty] {
            let fact = fact(kind, ty, fill);
            let mut bytes = Vec::new();
            Record::Fact(fact.clone()).encode(&mut bytes).unwrap();
            assert_eq!(bytes, envelope_of(&fact), "{kind:?} {ty}");
        }
    }
}

/// No fact body of a supported feature's kind is still provisional: once a feature is
/// supported, its bodies are confirmed and their goldens leave `provisional/`.
#[test]
fn an_enabled_feature_has_no_provisional_golden() {
    let offenders = provisional_offenders(SUPPORTED_FEATURES, &golden_names());
    assert_eq!(offenders, Vec::<String>::new());
}

/// The detector's self-check: a supported feature whose goldens are still provisional is
/// caught, and so is a supported feature no kind has (fail closed).
#[test]
fn the_provisional_detector_catches_a_planted_one() {
    let names = golden_names();
    let caught = provisional_offenders(&["review"], &names);
    assert_eq!(caught.len(), 8, "{caught:?}");
    assert!(caught.iter().all(|name| name.contains("fact_3_")));
    assert_eq!(
        provisional_offenders(&["nothing"], &names),
        ["nothing: no fact kind"]
    );

    // The planted file alone, in a list without the real ones.
    let planted = [format!("{PROVISIONAL}/fact_10_1")];
    assert_eq!(provisional_offenders(&["provenance"], &planted), planted);
    assert_eq!(
        provisional_offenders(&["lineage"], &planted),
        Vec::<String>::new()
    );
}

/// A FORMAT 3 writer writes no `Compacted`: one past the base is not a record.
#[test]
fn v3_decodes_compacted_only_before_base() {
    let compacted = Record::Compacted { sorted_count: 7 };
    let mut bytes = Vec::new();
    compacted.encode(&mut bytes).unwrap();
    let decode = |format, base| Record::decode_in(format, base, &FactKind::ALL, &bytes);
    assert_eq!(
        decode(StoreFormat::V3, Base::Before),
        Ok((compacted.clone(), 9))
    );
    assert!(decode(StoreFormat::V3, Base::After).is_err());
    for format in [StoreFormat::V1, StoreFormat::V2] {
        assert_eq!(decode(format, Base::After), Ok((compacted.clone(), 9)));
    }

    // A fact is no record before the base, nor in FORMAT 1 and 2, nor of a kind not known.
    let mut fact_bytes = Vec::new();
    Record::Fact(fact(FactKind::Lineage, 0, Fill::Full))
        .encode(&mut fact_bytes)
        .unwrap();
    let fact_in = |format, base, known: &[FactKind]| {
        Record::decode_in(format, base, known, &fact_bytes).is_ok()
    };
    assert!(fact_in(StoreFormat::V3, Base::After, &[FactKind::Lineage]));
    assert!(!fact_in(StoreFormat::V3, Base::After, &[FactKind::Review]));
    assert!(!fact_in(StoreFormat::V3, Base::Before, &FactKind::ALL));
    assert!(!fact_in(StoreFormat::V2, Base::After, &FactKind::ALL));

    // A nonce is the frame's, never a record.
    let mut nonce = vec![11];
    nonce.extend_from_slice(&NONCE.to_le_bytes());
    assert!(Record::decode_in(StoreFormat::V3, Base::After, &FactKind::ALL, &nonce).is_err());
}

#[test]
fn nonce_frame_matches_its_golden() {
    let lines = read_golden("nonce_frame");
    let [bytes, op_id] = lines.as_slice() else {
        panic!("two lines");
    };
    let (records, frame) = nonce_frame();
    assert_eq!(bytes, &frame, "the frame's bytes");

    // The header is FORMAT 2's 24 bytes; the frame's CRC is its payload's; the op id names both.
    let built = Frame::new_in(StoreFormat::V3, 3, 0, records.clone(), Some(NONCE));
    assert_eq!(built.nonce, Some(NONCE));
    assert_eq!(built.crc, crc32fast::hash(&bytes[24..]));
    assert_eq!(u32_at(bytes, 4), built.crc);
    assert_eq!(op_id, format!("3:{:08x}", built.crc).as_bytes());
    let read = format!("3:{} {} {NONCE}", 0xa1b2_c3d4u32, records_value(&records));
    assert_eq!(golden_value("nonce_frame", 0, bytes), Some(read));

    // FORMAT 1 and 2 frames carry no nonce, and a FORMAT 3 frame needs one.
    let v2 = Frame::new(3, 0, records.clone());
    assert_eq!(v2.nonce, None);
    assert_ne!(v2.crc, built.crc);
    assert_eq!(Frame::new_in(StoreFormat::V3, 3, 0, records, None).crc, 0);
}

#[test]
fn entity_id_matches_its_golden() {
    let lines = read_golden("entity_id");
    assert_eq!(lines, [entity_id_bytes()]);

    // The recipe: blake3("swarmops:entity:v1\0" ‖ u8 kind ‖ u64le seq ‖ u64le nonce ‖
    // u16le index).
    let mut input = b"swarmops:entity:v1\0".to_vec();
    input.push(3);
    input.extend_from_slice(&SEQ.to_le_bytes());
    input.extend_from_slice(&NONCE.to_le_bytes());
    input.extend_from_slice(&2u16.to_le_bytes());
    assert_eq!(lines[0], blake3::hash(&input).as_bytes());
    assert_ne!(
        FactKind::Review.entity_id(SEQ, NONCE, 2),
        FactKind::Queue.entity_id(SEQ, NONCE, 2)
    );
    assert_ne!(
        FactKind::Review.entity_id(SEQ, NONCE, 2),
        FactKind::Review.entity_id(SEQ, NONCE + 1, 2)
    );
}

/// Two frames that end in the same nonce at different seqs give their facts different ids,
/// and so does one record number past another: the frame's seq is in the recipe.
#[test]
fn entity_ids_take_the_frame_seq() {
    let review = |seq, index| FactKind::Review.entity_id(seq, NONCE, index);
    assert_ne!(review(SEQ, 2), review(SEQ + 1, 2));
    assert_ne!(review(SEQ, 2), review(SEQ, 3));
    assert_eq!(review(SEQ, 2), review(SEQ, 2));

    // The seq is its own field: seq and nonce swapped name another entity.
    assert_ne!(
        FactKind::Review.entity_id(SEQ, NONCE, 2),
        FactKind::Review.entity_id(NONCE, SEQ, 2)
    );
}

/// The layouts whose codecs come later match the design's tables: lengths and CRCs where the
/// tables put them.
#[test]
fn later_layouts_match_their_goldens() {
    for (name, want) in later_layouts() {
        let lines = read_golden(name);
        assert_eq!(lines, std::slice::from_ref(&want), "{name}");
        let crc_tail = |len: usize| u32_at(&want, len - 4) == crc32fast::hash(&want[..len - 4]);
        match name {
            "ckpt3" => assert!(want.len() == 44 && crc_tail(44)),
            "manifest" => assert!(want.len() == 20 + 24 * 3 + 4 && crc_tail(want.len())),
            "run_c" | "run_f" => {
                let n = usize::try_from(u64_at(&want, 24) + u64_at(&want, 32)).unwrap();
                assert_eq!(want.len(), 48 + 40 * n, "{name}");
                assert_eq!(u32_at(&want, 40), crc32fast::hash(&want[..40]), "{name}");
                let end = want.len() - 4;
                assert_eq!(u32_at(&want, end), crc32fast::hash(&want[44..end]));
            }
            "facts_col" => assert_eq!(want.len(), 48),
            "features" => assert_eq!(want, b"swarmops-features 1\nreview\nwithdrawal\n"),
            other => panic!("{other}"),
        }
    }
}

/// Writes `tests/golden3/` from the values above. Writes nothing unless
/// `SWARMOPS_REGEN_GOLDEN=1` is set, so `--run-ignored all` never rewrites the pin. The files are
/// new layouts' (FORMAT 3); FORMAT 1 and 2 have their own, which this never touches.
#[test]
#[ignore = "writes tests/golden3 when SWARMOPS_REGEN_GOLDEN=1; run on purpose"]
fn regenerate_golden3() {
    if std::env::var_os("SWARMOPS_REGEN_GOLDEN").is_none() {
        eprintln!("regenerate_golden3: SWARMOPS_REGEN_GOLDEN not set; nothing written");
        return;
    }
    fs::create_dir_all(golden_dir().join(PROVISIONAL)).unwrap();
    let write = |name: &str, lines: &[Vec<u8>]| {
        let text: String = lines.iter().map(|line| hex(line) + "\n").collect();
        fs::write(golden_dir().join(format!("{name}.hex")), text).unwrap();
    };

    // 1. The marker, the entity id and the later layouts.
    write(
        "marker",
        &[
            StoreFormat::V3.marker().to_vec(),
            UPGRADE_POISON_V3.to_vec(),
        ],
    );
    write("entity_id", &[entity_id_bytes()]);
    write("envelope", &envelopes());
    for (name, bytes) in later_layouts() {
        write(name, &[bytes]);
    }

    // 2. The nonce frame: its bytes, then its op id as text.
    let (records, frame) = nonce_frame();
    let crc = Frame::new_in(StoreFormat::V3, 3, 0, records, Some(NONCE)).crc;
    let text = format!("{}\n3:{crc:08x}\n", hex(&frame));
    fs::write(golden_dir().join("nonce_frame.hex"), text).unwrap();

    // 3. Every fact cell: filled, then empty.
    for (kind, ty) in CELLS {
        let lines: Vec<Vec<u8>> = [Fill::Full, Fill::Empty]
            .into_iter()
            .map(|fill| {
                let mut bytes = Vec::new();
                Record::Fact(fact(kind, ty, fill))
                    .encode(&mut bytes)
                    .unwrap();
                bytes
            })
            .collect();
        write(&fact_file(kind, ty), &lines);
    }
}

// ---- the values ----

fn id(byte: u8) -> [u8; 32] {
    [byte; 32]
}

/// A cell's fact: its body by the design §2.8 table, built here field by field.
fn fact(kind: FactKind, ty: u8, fill: Fill) -> Fact {
    use FactKind::{Annotation, Check, Lineage, Provenance, Queue, Review, Withdrawal};
    let filled = matches!(fill, Fill::Full);
    let some = |byte: u8| if filled { id(byte) } else { [0; 32] };
    let text = |s: &'static str| if filled { s } else { "" };
    let mut b = Body::default();
    match (kind, ty) {
        (Review, 0) => b
            .u32(0)
            .u64(12)
            .str(text("main"))
            .id(some(1))
            .id(some(2))
            .id(some(3)),
        (Review, 1) => b.u32(1).u32(2).id(some(1)).id(some(2)).id(some(3)).u8(3),
        (Review, 2) => b.u32(2).u32(2).u8(1).id(some(1)).id(some(2)),
        (Review, 3) => {
            b.u32(3).u32(2).u8(2).id(some(1));
            if filled {
                b.u8(1).u8(1).u32(10).u32(20).str("src/lib.rs");
            } else {
                b.u8(0);
            }
            b.id(some(2))
        }
        (Review, 4) => b.u32(4),
        (Review, 5) => b.u32(5).id(some(1)),
        (Review, 6) => b.u32(6).u8(2),
        (Review, 7) => b.u32(7).u32(2).u8(1).str(text("ci/lint")).id(some(1)),
        (Annotation, 0) => b.str(text("owner")).id(some(1)),
        (Annotation, 1) => b.str(text("owner")).id([0; 32]),
        (Check, _) => b
            .str(text("ci/test"))
            .id(some(1))
            .id(some(2))
            .str(text("ok")),
        (Queue, 0) => b.u32(0).str(text("main")).id(some(1)).id(some(2)),
        (Queue, 1) => b.u32(1).id(some(1)).id(some(2)).id(some(3)).u16(4),
        (Queue, 2) => b.u32(2).str(text("ci/test")),
        (Queue, 3) => b.u32(3).id(some(1)),
        (Queue, 4) => b.u32(4).u8(1).str(text("ci/test")).id(some(1)).id(some(2)),
        (Withdrawal, 0) => b
            .i64(if filled { 1_700_000_000 } else { 0 })
            .bytes16(b"leak"),
        (Withdrawal, 1 | 2) => b.i64(0).bytes16(b"ok"),
        (Withdrawal, 3) => b.i64(0).bytes16(text("done").as_bytes()),
        (Lineage, _) => b.id(some(1)),
        (Provenance, 0) => b
            .u8(1)
            .u64(100)
            .u64(200)
            .u64(300)
            .str(text("model"))
            .id(some(1)),
        (Provenance, 1) => b.u8(1),
        other => panic!("no cell {other:?}"),
    };

    // The envelope: a withdrawal names no other; an empty fact names none either.
    let other = if kind == Withdrawal { [0; 32] } else { some(9) };
    let actor = text("agent/one").to_owned();
    Fact::new(kind, ty, id(8), other, TS, actor, b.0).unwrap()
}

/// A fact body, field by field: integers little-endian, strings `u8 len | UTF-8`.
#[derive(Default)]
struct Body(Vec<u8>);

impl Body {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn id(&mut self, v: [u8; 32]) -> &mut Self {
        self.0.extend_from_slice(&v);
        self
    }

    fn str(&mut self, v: &str) -> &mut Self {
        self.u8(u8::try_from(v.len()).unwrap());
        self.0.extend_from_slice(v.as_bytes());
        self
    }

    fn bytes16(&mut self, v: &[u8]) -> &mut Self {
        self.u16(u16::try_from(v.len()).unwrap());
        self.0.extend_from_slice(v);
        self
    }
}

/// The envelope golden's actor and opaque body, filled and empty.
const ENVELOPE_PARTS: [(&str, &[u8]); 2] = [("agent/one", b"opaque\x00\xff"), ("", b"")];

/// The time every fact here was made at.
const TS: i64 = 1_700_000_123;

/// The envelope golden's lines: a review fact of type 0 around each opaque body.
fn envelopes() -> Vec<Vec<u8>> {
    let others = [id(9), [0; 32]];
    (ENVELOPE_PARTS.iter().zip(others))
        .map(|((actor, body), other)| envelope(3, 0, id(8), other, actor, body))
        .collect()
}

/// `fact`'s envelope around its body, built here field by field.
fn envelope_of(fact: &Fact) -> Vec<u8> {
    let (kind, ty) = (fact.kind().code(), fact.ty());
    assert_eq!(fact.ts(), TS);
    envelope(
        kind,
        ty,
        fact.subject(),
        fact.other(),
        fact.actor(),
        fact.body(),
    )
}

/// The design §2.8 envelope: `u8 kind | u8 type | u16le body_len | subject | other | i64le ts |
/// u8 actor_len | actor | body`, at [`TS`].
fn envelope(
    kind: u8,
    ty: u8,
    subject: [u8; 32],
    other: [u8; 32],
    actor: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut bytes = vec![kind, ty];
    bytes.extend_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&subject);
    bytes.extend_from_slice(&other);
    bytes.extend_from_slice(&TS.to_le_bytes());
    bytes.push(u8::try_from(actor.len()).unwrap());
    bytes.extend_from_slice(actor.as_bytes());
    bytes.extend_from_slice(body);
    bytes
}

/// What an envelope golden line reads as by the envelope alone: its fields, the body opaque;
/// `None` for a kind no fact has, a length that does not match, or an actor not UTF-8.
fn envelope_value(line: &[u8]) -> Option<String> {
    // 1. The fixed fields.
    let (kind, rest) = line.split_first()?;
    FactKind::from_code(*kind)?;
    let (ty, rest) = rest.split_first()?;
    let (body_len, rest) = rest.split_first_chunk::<2>()?;
    let (ids, rest) = rest.split_first_chunk::<64>()?;
    let (ts, rest) = rest.split_first_chunk::<8>()?;

    // 2. The actor, then a body of exactly `body_len` bytes.
    let (actor_len, rest) = rest.split_first()?;
    let (actor, body) = rest.split_at_checked(usize::from(*actor_len))?;
    let actor = std::str::from_utf8(actor).ok()?;
    if body.len() != usize::from(u16::from_le_bytes(*body_len)) {
        return None;
    }
    let ts = i64::from_le_bytes(*ts);
    Some(format!("{kind} {ty} {} {ts} {actor:?} {body:?}", hex(ids)))
}

/// The nonce frame's records and its bytes, built here from the FORMAT 2 header layout: a
/// RefMove, then the nonce record, at seq 3 behind a frame whose payload CRC is `0xa1b2c3d4`.
fn nonce_frame() -> (Vec<Record>, Vec<u8>) {
    let target = ChangesetId::new(ObjectId::new(id(5)));
    let records = vec![Record::RefMove {
        name: "main".to_owned(),
        old: None,
        new: Some(target),
        actor: "agent/one".to_owned(),
        ts: 1_700_000_123,
    }];

    // 1. The payload: the count (the nonce counted), the records, then `u8 11 | u64le nonce`.
    let mut payload = 2u16.to_le_bytes().to_vec();
    records[0].encode(&mut payload).unwrap();
    payload.push(11);
    payload.extend_from_slice(&NONCE.to_le_bytes());

    // 2. The header: len, crc, seq, prev_crc, then the CRC of those 20 bytes.
    let mut frame = Vec::new();
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    frame.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    frame.extend_from_slice(&3u64.to_le_bytes());
    frame.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    let hcrc = crc32fast::hash(&frame);
    frame.extend_from_slice(&hcrc.to_le_bytes());
    frame.extend_from_slice(&payload);
    (records, frame)
}

/// The entity id of the third record (index 2) of a review fact in the frame of seq [`SEQ`]
/// and nonce [`NONCE`].
fn entity_id_bytes() -> Vec<u8> {
    FactKind::Review.entity_id(SEQ, NONCE, 2).to_vec()
}

/// The checkpoint, feature list, manifest, two runs and two fact rows, by the design's tables
/// (§2.3, §2.6, §2.5, §2.4, §2.8).
fn later_layouts() -> Vec<(&'static str, Vec<u8>)> {
    let with_crc = |mut bytes: Vec<u8>| {
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    };
    let le = |values: &[u64]| -> Vec<u8> { values.iter().flat_map(|v| v.to_le_bytes()).collect() };

    // 1. journal3.ckpt: offset, seq, base_seq, records, fact_rows, CRC.
    let ckpt3 = with_crc(le(&[0x0102_0304_0506_0708, 7, 3, 9, 2]));

    // 2. meta/features: the header line, then names in byte order.
    let features = b"swarmops-features 1\nreview\nwithdrawal\n".to_vec();

    // 3. MANIFEST: magic, ver 1, 3 zero bytes, generation 5, 3 runs (domain, from) ascending.
    let mut manifest = b"SWMF\x01\0\0\0".to_vec();
    manifest.extend(le(&[5]));
    manifest.extend_from_slice(&3u32.to_le_bytes());
    for (domain, from, to) in [(0u8, 0, 1024), (0, 1024, 1280), (1, 0, 3)] {
        manifest.push(domain);
        manifest.extend_from_slice(&[0; 7]);
        manifest.extend(le(&[from, to]));
    }
    let manifest = with_crc(manifest);

    // 4. The runs of records [0, 3): domain 0 (ids, then delta hashes) and domain 1 (subjects,
    // then the non-zero others: record 1 names none).
    let run_c = run(
        0,
        &[(id(1), 2), (id(2), 0), (id(3), 1)],
        &[(id(4), 1), (id(6), 0), (id(6), 2)],
    );
    let run_f = run(
        1,
        &[(id(7), 0), (id(7), 2), (id(8), 1)],
        &[(id(9), 0), (id(9), 2)],
    );

    // 5. facts.col: offset, len, kind, type, 0, frame_seq.
    let row = |offset: u64, len: u32, kind: u8, ty: u8, seq: u64| {
        let mut row = offset.to_le_bytes().to_vec();
        row.extend_from_slice(&len.to_le_bytes());
        row.extend_from_slice(&[kind, ty, 0, 0]);
        row.extend_from_slice(&seq.to_le_bytes());
        row
    };
    let facts_col = [row(4096, 180, 3, 0, 5), row(4276, 110, 9, 1, 6)].concat();

    vec![
        ("ckpt3", ckpt3),
        ("features", features),
        ("manifest", manifest),
        ("run_c", run_c),
        ("run_f", run_f),
        ("facts_col", facts_col),
    ]
}

/// A run of records [0, 3) of `domain`: its two sections of (key, rec), keys first.
fn run(domain: u8, section0: &[([u8; 32], u64)], section1: &[([u8; 32], u64)]) -> Vec<u8> {
    let count = |section: &[([u8; 32], u64)]| u64::try_from(section.len()).unwrap();
    let mut header = b"SWRN\x01".to_vec();
    header.extend_from_slice(&[domain, 0, 0]);
    for v in [0, 3, count(section0), count(section1)] {
        header.extend_from_slice(&v.to_le_bytes());
    }
    let hcrc = crc32fast::hash(&header);
    header.extend_from_slice(&hcrc.to_le_bytes());
    let mut body = Vec::new();
    for section in [section0, section1] {
        body.extend(section.iter().flat_map(|(key, _)| *key));
        body.extend(section.iter().flat_map(|(_, rec)| rec.to_le_bytes()));
    }
    let crc = crc32fast::hash(&body);
    [header, body, crc.to_le_bytes().to_vec()].concat()
}

/// What line `index` of golden `name` reads as, or `None` if it does not: the decoder's view,
/// so that a changed byte shows as a refusal or another value.
fn golden_value(name: &str, index: usize, line: &[u8]) -> Option<String> {
    match name {
        "marker" => StoreFormat::from_marker(line)
            .filter(|f| *f == StoreFormat::V3)
            .map(|f| format!("{f:?}")),
        "entity_id" => (line == entity_id_bytes()).then(|| "review 2".to_owned()),
        "nonce_frame" if index == 1 => std::str::from_utf8(line).ok().map(str::to_owned),
        "nonce_frame" => frame_value(line),
        "ckpt3" => ckpt3_value(line),
        "features" => features_value(line),
        "envelope" => envelope_value(line),
        _ => {
            let (record, used) =
                Record::decode_in(StoreFormat::V3, Base::After, &FactKind::ALL, line).ok()?;
            (used == line.len()).then(|| format!("{record:?}"))
        }
    }
}

thread_local! {
    /// A FORMAT 3 store the value functions write a file of, one per thread, and its directory.
    static V3: (tempfile::TempDir, Layout) = {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let layout = Layout::with_format(root, StoreFormat::V3);
        MetaStore::create_in(layout.clone(), Arc::new(RealFs::new())).unwrap();
        (dir, layout)
    };
}

/// The thread's FORMAT 3 store ([`V3`]).
fn v3_layout() -> Layout {
    V3.with(|(_, layout)| layout.clone())
}

/// The checkpoint `bytes` hold as a FORMAT 3 store reads them: its offset, which the store's
/// empty journal then refuses as no frame boundary; `None` for no checkpoint at all.
fn ckpt3_value(bytes: &[u8]) -> Option<String> {
    let layout = v3_layout();
    fs::write(layout.journal_ckpt(), bytes).unwrap();
    let journal = swarmops_store::Journal::open(&layout, Arc::new(RealFs::new())).unwrap();
    match journal.ckpt() {
        Ok(offset) | Err(StoreError::JournalCorrupt { offset }) => Some(format!("at {offset}")),
        Err(_) => None,
    }
}

/// The feature list `bytes` hold as a FORMAT 3 store reads it: the features its open names as
/// ones this build lacks (all of them: it supports none); `None` for no list at all.
fn features_value(bytes: &[u8]) -> Option<String> {
    let layout = v3_layout();
    fs::write(layout.features_file(), bytes).unwrap();
    match MetaStore::open_in(layout, Arc::new(RealFs::new())) {
        Ok(_) => Some("[]".to_owned()),
        Err(StoreError::TooNew { features, .. }) => Some(format!("{features:?}")),
        Err(_) => None,
    }
}

/// A FORMAT 3 frame's records and nonce, if its header and payload check.
fn frame_value(frame: &[u8]) -> Option<String> {
    let (header, payload) = frame.split_at_checked(24)?;
    let fits = usize::try_from(u32_at(header, 0)).ok() == Some(payload.len());
    let checks = u32_at(header, 20) == crc32fast::hash(&header[..20])
        && u32_at(header, 4) == crc32fast::hash(payload);
    if !(fits && checks) {
        return None;
    }
    let (count, mut rest) = payload.split_first_chunk::<2>()?;
    let mut records = Vec::new();
    for _ in 1..u16::from_le_bytes(*count) {
        let (record, used) =
            Record::decode_in(StoreFormat::V3, Base::After, &FactKind::ALL, rest).ok()?;
        records.push(record);
        rest = &rest[used..];
    }
    let (kind, nonce) = rest.split_first()?;
    let nonce: [u8; 8] = nonce.try_into().ok()?;
    let at = format!("{}:{}", u64_at(header, 8), u32_at(header, 16));
    (*kind == 11).then(|| {
        format!(
            "{at} {} {}",
            records_value(&records),
            u64::from_le_bytes(nonce)
        )
    })
}

fn records_value(records: &[Record]) -> String {
    format!("{records:?}")
}

// ---- the files ----

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden3")
}

/// A cell's golden: under `provisional/` until its kind's feature is supported.
fn fact_file(kind: FactKind, ty: u8) -> String {
    let file = format!("fact_{}_{ty}", kind.code());
    let supported =
        (KIND_FEATURES.iter()).any(|&(of, name)| of == kind && SUPPORTED_FEATURES.contains(&name));
    if supported {
        file
    } else {
        format!("{PROVISIONAL}/{file}")
    }
}

/// Whether golden `name` is a provisional one.
fn is_provisional(name: &str) -> bool {
    name.starts_with(&format!("{PROVISIONAL}/"))
}

/// The provisional goldens among `names` of a kind whose feature is in `supported`, and each
/// supported feature no fact kind has: what [`an_enabled_feature_has_no_provisional_golden`]
/// refuses.
fn provisional_offenders(supported: &[&str], names: &[String]) -> Vec<String> {
    let mut offenders = Vec::new();
    for feature in supported {
        // 1. A feature that names no kind: fail closed.
        let Some((kind, _)) = KIND_FEATURES.iter().find(|(_, name)| name == feature) else {
            offenders.push(format!("{feature}: no fact kind"));
            continue;
        };

        // 2. Its kind's provisional goldens.
        let stem = format!("{PROVISIONAL}/fact_{}_", kind.code());
        offenders.extend(names.iter().filter(|name| name.starts_with(&stem)).cloned());
    }
    offenders
}

/// The names of the files of `tests/golden3/` and `tests/golden3/provisional/`, without
/// `.hex`, sorted; a provisional one prefixed with `provisional/`.
fn golden_names() -> Vec<String> {
    let hex_files = |dir: PathBuf| -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter_map(|file| file.strip_suffix(".hex").map(str::to_owned))
            .collect()
    };
    let provisional = hex_files(golden_dir().join(PROVISIONAL));
    let mut names = hex_files(golden_dir());
    names.extend(
        provisional
            .into_iter()
            .map(|file| format!("{PROVISIONAL}/{file}")),
    );
    names.sort();
    names
}

/// The lines of golden `name`: hex, except the nonce frame's op id, which is text.
fn read_golden(name: &str) -> Vec<Vec<u8>> {
    let text = fs::read_to_string(golden_dir().join(format!("{name}.hex"))).unwrap();
    text.lines()
        .enumerate()
        .map(|(index, line)| match (name, index) {
            ("nonce_frame", 1) => line.as_bytes().to_vec(),
            _ => unhex(line),
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, b| {
        write!(text, "{b:02x}").expect("a String takes every write");
        text
    })
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
