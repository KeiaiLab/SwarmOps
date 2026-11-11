//! Journal records and frames: fixed little-endian layouts, no varints.
//!
//! ```text
//! frame     = u32le len | u32le crc32(payload) | payload      len = payload bytes, never 0
//! frame2    = u32le len | u32le crc32(payload) | u64le seq | u32le prev_crc
//!             | u32le crc32(the 20 bytes before) | payload    (FORMAT 2, the M6 design §2.3)
//! payload   = u16le record count (1 to 65,535) | records
//! Changeset = u8 1 | id[32] | u64le generation | delta_hash[32] | u8 n | parents[n][32]
//! RefMove   = u8 2 | u8 name_len | name | u8 has_old | old[32]? | u8 has_new | new[32]?
//!             | u8 actor_len | actor | i64le ts
//! Compacted = u8 4 | u64le sorted_count
//! Fact      = u8 kind | u8 type | u16le body_len | subject[32] | other[32] | i64le ts
//!             | u8 actor_len | actor | body                     (FORMAT 3: kinds 3, 5 to 10)
//! Nonce     = u8 11 | u64le nonce                     (FORMAT 3: every frame's last record)
//! ```
//!
//! A frame is one transaction: its records commit together. FORMAT 3 (the FORMAT 3 design §2.7,
//! §2.8) keeps FORMAT 2's frame layout and adds the fact kinds, whose bodies the design's table
//! fixes per kind and type, and the nonce, which [`Frame::nonce`] carries rather than
//! [`Frame::records`]. Decoding checks every length against the bytes at hand before it slices,
//! so garbage gives an error, never a panic.

use swarmops_core::bytes::{self, Reader};
use swarmops_core::{ChangesetId, ObjectId};
use thiserror::Error;

use crate::error::{LimitKind, StoreError};
use crate::format::{JournalFamily, StoreFormat};

/// A frame header: `u32le len | u32le crc32(payload)`.
pub(crate) const HEADER_LEN: usize = 8;

/// A FORMAT 2 frame header: `u32le len | u32le crc32(payload) | u64le seq | u32le prev_crc |
/// u32le hcrc`, `hcrc` the CRC of the 20 bytes before it.
pub(crate) const HEADER_LEN_V2: usize = 24;

/// The bytes of a FORMAT 2 header that its own CRC covers.
const HCRC_AT: usize = 20;

/// The most records a frame holds: its record count is a `u16le`.
pub(crate) const MAX_RECORDS: usize = 65_535;

/// The longest record: a changeset with 255 parents (74 bytes and 32 per parent).
const MAX_RECORD_LEN: usize = 74 + 32 * 255;

/// The largest payload this build decodes, 539,615,192 bytes: its count, then 65,535 records of
/// the longest kind. A FORMAT 2 header that checks and announces more (or 0) is no frame a
/// writer wrote.
pub(crate) const MAX_PAYLOAD_LEN: usize = 2 + MAX_RECORDS * MAX_RECORD_LEN;

const KIND_CHANGESET: u8 = 1;
const KIND_REF_MOVE: u8 = 2;
const KIND_COMPACTED: u8 = 4;
const KIND_NONCE: u8 = 11;

/// The most caller records of a FORMAT 3 frame: its nonce takes one of [`MAX_RECORDS`].
pub(crate) const MAX_RECORDS_V3: usize = MAX_RECORDS - 1;

/// The longest fact body, in bytes (the FORMAT 3 design §2.8): with the 77 fixed bytes and the
/// longest actor, a fact (4,428 bytes) is shorter than the longest changeset record.
pub const MAX_FACT_BODY: usize = 4_096;

/// What an entity id's hash starts with (the FORMAT 3 design §2.8).
const ENTITY_DOMAIN: &[u8] = b"swarmops:entity:v1\0";

/// One change to the metadata, as the journal records it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Record {
    /// A changeset joined the index.
    Changeset {
        /// The changeset's id.
        id: ChangesetId,
        /// 1 + the largest parent generation; 1 for a root.
        generation: u64,
        /// The hash of the changeset's delta section (storage spec §1).
        delta_hash: [u8; 32],
        /// The parents in order, the mainline parent first; at most 255.
        parents: Vec<ChangesetId>,
    },
    /// A ref moved.
    RefMove {
        /// The ref's name; at most 255 bytes.
        name: String,
        /// The target before the move; `None` if the ref did not exist.
        old: Option<ChangesetId>,
        /// The target after the move; `None` if the ref was deleted.
        new: Option<ChangesetId>,
        /// Who moved it; at most 255 bytes.
        actor: String,
        /// When it moved, as the writer recorded it; stored verbatim.
        ts: i64,
    },
    /// A compaction of the changeset index finished.
    Compacted {
        /// How many index records the sorted views hold now.
        sorted_count: u64,
    },
    /// A fact (FORMAT 3 only, the FORMAT 3 design §2.8).
    Fact(Fact),
}

/// The kinds of fact a FORMAT 3 journal holds (the FORMAT 3 design §2.7, §2.8): each one
/// feature's, whose milestone gives it its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FactKind {
    /// Kind 3: a change request's events.
    Review,
    /// Kind 5: a note on a changeset, under a key.
    Annotation,
    /// Kind 6: a check's state on a tree.
    Check,
    /// Kind 7: a land queue entry's events.
    Queue,
    /// Kind 8: a blob withdrawn, restored or erased.
    Withdrawal,
    /// Kind 9: a changeset's place in its lineage.
    Lineage,
    /// Kind 10: where a changeset came from.
    Provenance,
}

/// Where a FORMAT 3 frame lies against the journal's base seq: before it, a FORMAT 2 writer
/// wrote it (an upgraded store's); from it on, a FORMAT 3 writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Base {
    /// Before the base seq: FORMAT 2's records, `Compacted` among them; no fact, no nonce.
    Before,
    /// From the base seq on: no `Compacted`; facts of the kinds the caller knows.
    After,
}

/// One fact: the envelope of the FORMAT 3 design §2.8 around a body whose layout its kind and
/// type fix. Built only through [`Fact::new`], which checks the body, so every fact encodes.
/// The envelope is fixed; a kind's bodies are provisional until its feature is released.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Fact {
    kind: FactKind,
    ty: u8,
    subject: [u8; 32],
    other: [u8; 32],
    ts: i64,
    actor: String,
    body: Vec<u8>,
}

/// Bytes that are not a journal record: where [`Record::decode`] stopped in them. A position in
/// the bytes it was given, not in `journal.log`: a reader of the journal reports a frame whose
/// records do not decode as [`StoreError::JournalCorrupt`] at the frame's own offset there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("not a journal record: it does not decode at byte {at}")]
#[non_exhaustive]
pub struct BadRecord {
    /// Where in the bytes decoding stopped: the start of the field it could not read, or of
    /// the record for a kind it does not know.
    pub at: usize,
}

/// One transaction as the journal holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Frame {
    /// The frame's index in `journal.off`, from 0: its place in the journal's total order.
    pub seq: u64,
    /// Where the frame starts in `journal.log`.
    pub offset: u64,
    /// Its records, in the order they were appended.
    pub records: Vec<Record>,
    /// The `crc32` of its payload, as its header holds it (in either format): with `seq`, the
    /// operation's id. A writer that takes the frame back and gives its seq to another frame
    /// gives that one another CRC unless the payloads collide (2^-32).
    pub crc: u32,
    /// The nonce a FORMAT 3 frame ends in (the FORMAT 3 design §2.7), which its payload, and so
    /// its CRC, holds; `None` in FORMAT 1 and 2. Never among `records`.
    pub nonce: Option<u64>,
}

impl Frame {
    /// The frame `seq` at `offset`, holding `records`; its `crc` is that of their payload
    /// encoding, or 0 for records no frame can hold (none, or past the layout's limits).
    ///
    /// Invariant: `seq` and `offset` are the frame's own in `journal.off` and `journal.log`.
    /// Nothing checks it here, and nothing needs to: no store call takes a frame, and the
    /// journal's reads take both from `journal.off` (see [`crate::Journal`]). A frame built
    /// here is a value to compare with what the journal returns.
    pub fn new(seq: u64, offset: u64, records: Vec<Record>) -> Frame {
        let crc = encode_frame(&records).map_or(0, |bytes| payload_crc(&bytes));
        Frame {
            seq,
            offset,
            records,
            crc,
            nonce: None,
        }
    }

    /// [`Frame::new`] in `format`: the frame `seq` at `offset`, holding `records` and, in FORMAT
    /// 3, ending in `nonce`. Its `crc` is 0 for what no frame of `format` holds (a FORMAT 3
    /// frame without a nonce, a FORMAT 1 or 2 frame with one, or [`Frame::new`]'s cases).
    pub fn new_in(
        format: StoreFormat,
        seq: u64,
        offset: u64,
        records: Vec<Record>,
        nonce: Option<u64>,
    ) -> Frame {
        let crc = encode_frame_with_nonce(format, &records, nonce, seq, 0)
            .map_or(0, |bytes| payload_crc(&bytes));
        Frame {
            seq,
            offset,
            records,
            crc,
            nonce,
        }
    }
}

impl Record {
    /// Decodes the record at the start of `buf`, returning it and how many bytes it took;
    /// the bytes after it are left alone.
    ///
    /// Malformed or truncated bytes give [`BadRecord`], at the position in `buf` where decoding
    /// stopped.
    pub fn decode(buf: &[u8]) -> Result<(Record, usize), BadRecord> {
        let mut r = Reader::new(buf, bad_record);
        let record = match r.u8()? {
            KIND_CHANGESET => Self::Changeset {
                id: r.id()?,
                generation: r.u64_le()?,
                delta_hash: r.array()?,
                parents: {
                    let n = r.u8()?;
                    (0..n).map(|_| r.id()).collect::<Result<_, _>>()?
                },
            },
            KIND_REF_MOVE => Self::RefMove {
                name: r.string()?,
                old: r.optional_id()?,
                new: r.optional_id()?,
                actor: r.string()?,
                ts: r.i64_le()?,
            },
            KIND_COMPACTED => Self::Compacted {
                sorted_count: r.u64_le()?,
            },
            _ => return Err(r.error_at(0)),
        };
        Ok((record, r.pos()))
    }

    /// [`Record::decode`] in `format`, for a frame on the `base` side of the journal's base seq:
    /// FORMAT 1 and 2, and FORMAT 3 before the base, decode kinds 1, 2 and 4, as
    /// [`Record::decode`] does; FORMAT 3 from the base on decodes kinds 1 and 2 and the facts of
    /// the kinds in `known`. A nonce is no record: the frame's reader takes it.
    pub fn decode_in(
        format: StoreFormat,
        base: Base,
        known: &[FactKind],
        buf: &[u8],
    ) -> Result<(Record, usize), BadRecord> {
        // 1. Every format's records but FORMAT 3's past the base: kinds 1, 2 and 4.
        if format != StoreFormat::V3 || base == Base::Before {
            return Self::decode(buf);
        }

        // 2. Past the base: no compaction or nonce record, and facts of the kinds known.
        let code = buf.first().copied().ok_or(bad_record(0))?;
        match FactKind::from_code(code) {
            Some(kind) if known.contains(&kind) => {
                Fact::decode(buf).map(|(fact, used)| (Self::Fact(fact), used))
            }
            Some(_) => Err(bad_record(0)),
            None if matches!(code, KIND_COMPACTED | KIND_NONCE) => Err(bad_record(0)),
            None => Self::decode(buf),
        }
    }

    /// Appends the record's encoding to `out`. A length that does not fit its `u8` field (more
    /// than 255 parents, or a ref name or an actor longer than 255 bytes) is
    /// [`StoreError::Limit`], and `out` is left untouched then.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), StoreError> {
        match self {
            Self::Changeset {
                id,
                generation,
                delta_hash,
                parents,
            } => {
                let n = fits_u8(LimitKind::Parents, parents.len())?;
                out.push(KIND_CHANGESET);
                out.extend_from_slice(&id.0.0);
                out.extend_from_slice(&generation.to_le_bytes());
                out.extend_from_slice(delta_hash);
                out.push(n);
                for parent in parents {
                    out.extend_from_slice(&parent.0.0);
                }
            }
            Self::RefMove {
                name,
                old,
                new,
                actor,
                ts,
            } => {
                let name_len = fits_u8(LimitKind::RefNameBytes, name.len())?;
                let actor_len = fits_u8(LimitKind::ActorBytes, actor.len())?;
                out.push(KIND_REF_MOVE);
                out.push(name_len);
                out.extend_from_slice(name.as_bytes());
                put_optional_id(out, *old);
                put_optional_id(out, *new);
                out.push(actor_len);
                out.extend_from_slice(actor.as_bytes());
                out.extend_from_slice(&ts.to_le_bytes());
            }
            Self::Compacted { sorted_count } => {
                out.push(KIND_COMPACTED);
                out.extend_from_slice(&sorted_count.to_le_bytes());
            }
            Self::Fact(fact) => fact.encode(out),
        }
        Ok(())
    }
}

/// A field of a fact body (the FORMAT 3 design §2.8): integers little-endian, `id` 32 bytes.
#[derive(Debug, Clone, Copy)]
enum Field {
    U16,
    U32,
    U64,
    I64,
    Id,
    /// A `u32` that is 0: a first event's version.
    Zero32,
    /// An `i64` that is 0: a time only another type sets.
    Zero64,
    /// An id that is all zero: a blob a type names none of.
    ZeroId,
    /// A `u8` that is 1: the provenance schema.
    Schema,
    /// A `u8` below the number: one of an enumeration's values.
    Enum(u8),
    /// `u8 len | UTF-8` of at most this many bytes.
    Str(usize),
    /// `u16le len | bytes`, of the first to the second number of bytes.
    Bytes16(usize, usize),
    /// A comment's anchor: `u8 has_anchor`, then, if 1, `u8 side{0, 1} | u32 start | u32 end |
    /// str path`.
    Anchor,
}

/// A string field of up to 255 bytes, all a `u8` length holds.
const STR: Field = Field::Str(255);

/// A Check's body, of every type (state).
const CHECK_BODY: &[Field] = &[Field::Str(128), Field::Id, Field::Id, STR];

/// A Lineage's body, of every type (cause).
const LINEAGE_BODY: &[Field] = &[Field::Id];

/// A withdrawal's reason; Withdraw, Restore and Erase give one.
const REASON: Field = Field::Bytes16(1, 1_024);

/// The bodies of each kind, by type (the FORMAT 3 design §2.8 table, with its amendment of
/// 2026-11-11): free text a writer could not take back is a blob's id, never inline (a review's
/// title, an annotation's value, a provenance's host, session and task, which its `doc` holds).
/// Provisional until the kind's feature is released: a consuming milestone may change them in
/// place until then.
const REVIEW: &[&[Field]] = {
    use Field::{Anchor, Enum, Id, U32, U64, Zero32};
    &[
        &[Zero32, U64, STR, Id, Id, Id],
        &[U32, U32, Id, Id, Id, Enum(5)],
        &[U32, U32, Enum(4), Id, Id],
        &[U32, U32, Enum(3), Id, Anchor, Id],
        &[U32],
        &[U32, Id],
        &[U32, Enum(4)],
        &[U32, U32, Enum(3), STR, Id],
    ]
};
const ANNOTATION: &[&[Field]] = &[
    &[Field::Str(64), Field::Id],
    &[Field::Str(64), Field::ZeroId],
];
const CHECK: &[&[Field]] = &[CHECK_BODY; 7];
const QUEUE: &[&[Field]] = {
    use Field::{Enum, Id, U16, U32, Zero32};
    &[
        &[Zero32, STR, Id, Id],
        &[U32, Id, Id, Id, U16],
        &[U32, STR],
        &[U32, Id],
        &[U32, Enum(5), STR, Id, Id],
    ]
};
const WITHDRAWAL: &[&[Field]] = {
    use Field::{Bytes16, I64, Zero64};
    &[
        &[I64, REASON],
        &[Zero64, REASON],
        &[Zero64, REASON],
        &[Zero64, Bytes16(0, 1_024)],
    ]
};
const LINEAGE: &[&[Field]] = &[LINEAGE_BODY; 7];
const PROVENANCE: &[&[Field]] = {
    use Field::{Id, Schema, U64};
    &[&[Schema, U64, U64, U64, STR, Id], &[Schema]]
};

impl FactKind {
    /// Every kind, in kind number order.
    pub const ALL: [FactKind; 7] = [
        Self::Review,
        Self::Annotation,
        Self::Check,
        Self::Queue,
        Self::Withdrawal,
        Self::Lineage,
        Self::Provenance,
    ];

    /// Its kind number in the journal: 3, or 5 to 10.
    pub fn code(self) -> u8 {
        match self {
            Self::Review => 3,
            Self::Annotation => 5,
            Self::Check => 6,
            Self::Queue => 7,
            Self::Withdrawal => 8,
            Self::Lineage => 9,
            Self::Provenance => 10,
        }
    }

    /// The kind whose number is `code`; `None` for a number no fact has.
    pub fn from_code(code: u8) -> Option<FactKind> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }

    /// The id of the entity a fact of this kind creates as record `index` of the frame of seq
    /// `seq` and nonce `nonce`: `blake3("swarmops:entity:v1\0" ‖ u8 kind ‖ u64le seq ‖ u64le
    /// nonce ‖ u16le index)` (the FORMAT 3 design §2.8, its amendment of 2026-11-11, G4). The
    /// open frame's seq is fixed when a fact is queued; its nonce is the frame's own.
    pub fn entity_id(self, seq: u64, nonce: u64, index: u16) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ENTITY_DOMAIN);
        hasher.update(&[self.code()]);
        hasher.update(&seq.to_le_bytes());
        hasher.update(&nonce.to_le_bytes());
        hasher.update(&index.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// The fields of the body of type `ty`; `None` for a type the kind does not have.
    fn body(self, ty: u8) -> Option<&'static [Field]> {
        let bodies = match self {
            Self::Review => REVIEW,
            Self::Annotation => ANNOTATION,
            Self::Check => CHECK,
            Self::Queue => QUEUE,
            Self::Withdrawal => WITHDRAWAL,
            Self::Lineage => LINEAGE,
            Self::Provenance => PROVENANCE,
        };
        bodies.get(usize::from(ty)).copied()
    }
}

/// Where in a fact `other` starts: after kind, type, body length and subject.
const OTHER_AT: usize = 36;

impl Fact {
    /// The fact of `kind` and type `ty` about `subject` (and `other`, all zero for none), made
    /// at `ts` by `actor`, with `body`. Refuses an actor longer than 255 bytes with
    /// [`StoreError::Limit`]; a type the kind does not have, a body its type's layout does not
    /// read as exactly, or a withdrawal naming an `other`, with [`StoreError::BadCall`].
    pub fn new(
        kind: FactKind,
        ty: u8,
        subject: [u8; 32],
        other: [u8; 32],
        ts: i64,
        actor: String,
        body: Vec<u8>,
    ) -> Result<Fact, StoreError> {
        fits_u8(LimitKind::ActorBytes, actor.len())?;
        let refused =
            |why: &str| StoreError::BadCall(format!("a {kind:?} fact of type {ty} {why}"));
        let fields = kind.body(ty).ok_or_else(|| refused("is no fact"))?;
        if !other_fits(kind, &other) {
            return Err(refused("names no other"));
        }
        if body.len() > MAX_FACT_BODY || check_body(fields, &body).is_err() {
            return Err(refused("has another body"));
        }
        Ok(Fact {
            kind,
            ty,
            subject,
            other,
            ts,
            actor,
            body,
        })
    }

    /// Its kind.
    pub fn kind(&self) -> FactKind {
        self.kind
    }

    /// Its type, within its kind.
    pub fn ty(&self) -> u8 {
        self.ty
    }

    /// The id the fact is about, which the fact index finds it by.
    pub fn subject(&self) -> [u8; 32] {
        self.subject
    }

    /// The second id the fact index finds it by; all zero for none.
    pub fn other(&self) -> [u8; 32] {
        self.other
    }

    /// When it was queued, in Unix seconds, as the writer recorded it.
    pub fn ts(&self) -> i64 {
        self.ts
    }

    /// Who made it.
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// Its body, in its type's layout.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The fact at the start of `buf`, whose kind byte names a fact kind, and how many bytes it
    /// took.
    fn decode(buf: &[u8]) -> Result<(Fact, usize), BadRecord> {
        // 1. The envelope.
        let (envelope, used) = Envelope::decode(buf)?;
        let Envelope {
            kind,
            ty,
            subject,
            other,
            ts,
            actor,
            body,
        } = envelope;

        // 2. The body, which its kind and type lay out, exactly.
        let fields = kind.body(ty).ok_or(bad_record(TY_AT))?;
        if !other_fits(kind, &other) {
            return Err(bad_record(OTHER_AT));
        }
        let body_at = used - body.len();
        check_body(fields, body).map_err(|e| bad_record(body_at + e.at))?;
        let fact = Fact {
            kind,
            ty,
            subject,
            other,
            ts,
            actor,
            body: body.to_vec(),
        };
        Ok((fact, used))
    }

    /// Appends its encoding to `out`; [`Fact::new`] checked every length.
    fn encode(&self, out: &mut Vec<u8>) {
        let envelope = Envelope {
            kind: self.kind,
            ty: self.ty,
            subject: self.subject,
            other: self.other,
            ts: self.ts,
            actor: self.actor.clone(),
            body: &self.body,
        };
        envelope.encode(out);
    }
}

/// Where in a fact its type byte is, and its body length.
const TY_AT: usize = 1;
const BODY_LEN_AT: usize = 2;

/// A fact's envelope (the FORMAT 3 design §2.8, confirmed at v0.12.0), its body taken as bytes
/// no kind's layout is checked against: `u8 kind | u8 type | u16le body_len | subject[32] |
/// other[32] | i64le ts | u8 actor_len | actor | body`. [`Fact`]'s codec is this one, then its
/// body's check; a kind's body layout is provisional, the envelope is not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Envelope<'a> {
    kind: FactKind,
    ty: u8,
    subject: [u8; 32],
    other: [u8; 32],
    ts: i64,
    actor: String,
    body: &'a [u8],
}

impl<'a> Envelope<'a> {
    /// The envelope at the start of `buf`, and how many bytes it took: a kind byte that names
    /// a fact kind, a body of at most [`MAX_FACT_BODY`] bytes, an actor in UTF-8.
    fn decode(buf: &'a [u8]) -> Result<(Envelope<'a>, usize), BadRecord> {
        let mut r = Reader::new(buf, bad_record);
        let kind = FactKind::from_code(r.u8()?).ok_or(bad_record(0))?;
        let ty = r.u8()?;
        let body_len = usize::from(u16::from_le_bytes(r.array()?));
        if body_len > MAX_FACT_BODY {
            return Err(bad_record(BODY_LEN_AT));
        }
        let subject = r.array()?;
        let other = r.array()?;
        let ts = r.i64_le()?;
        let actor = r.string()?;
        let body = r.bytes(body_len)?;
        let envelope = Envelope {
            kind,
            ty,
            subject,
            other,
            ts,
            actor,
            body,
        };
        Ok((envelope, r.pos()))
    }

    /// Appends its encoding to `out`; the caller checked the body's and the actor's lengths.
    fn encode(&self, out: &mut Vec<u8>) {
        let body_len = u16::try_from(self.body.len()).expect("a body of at most 4,096 bytes");
        let actor_len = u8::try_from(self.actor.len()).expect("an actor of at most 255 bytes");
        out.push(self.kind.code());
        out.push(self.ty);
        out.extend_from_slice(&body_len.to_le_bytes());
        out.extend_from_slice(&self.subject);
        out.extend_from_slice(&self.other);
        out.extend_from_slice(&self.ts.to_le_bytes());
        out.push(actor_len);
        out.extend_from_slice(self.actor.as_bytes());
        out.extend_from_slice(self.body);
    }
}

/// Whether `other` is one a fact of `kind` may name: a withdrawal names none.
fn other_fits(kind: FactKind, other: &[u8; 32]) -> bool {
    kind != FactKind::Withdrawal || other.iter().all(|&b| b == 0)
}

/// Whether `body` is exactly `fields`; [`BadRecord`] at the field in it that is not.
fn check_body(fields: &[Field], body: &[u8]) -> Result<(), BadRecord> {
    let mut r = Reader::new(body, bad_record);
    for &field in fields {
        read_field(&mut r, field)?;
    }
    if r.rest().is_empty() {
        Ok(())
    } else {
        Err(bad_record(r.pos()))
    }
}

/// Reads `field` off `r`; [`BadRecord`] at its start when it is not one.
fn read_field(r: &mut Reader<'_, BadRecord>, field: Field) -> Result<(), BadRecord> {
    let at = r.pos();
    let zero = |bytes: &[u8]| bytes.iter().all(|&b| b == 0);
    let fits = match field {
        Field::U16 => r.bytes(2).is_ok(),
        Field::U32 => r.bytes(4).is_ok(),
        Field::U64 | Field::I64 => r.bytes(8).is_ok(),
        Field::Id => r.bytes(32).is_ok(),
        Field::Zero32 => r.bytes(4).is_ok_and(zero),
        Field::Zero64 => r.bytes(8).is_ok_and(zero),
        Field::ZeroId => r.bytes(32).is_ok_and(zero),
        Field::Schema => r.u8()? == 1,
        Field::Enum(values) => r.u8()? < values,
        Field::Str(max) => {
            let len = usize::from(r.u8()?);
            r.bytes(len)
                .is_ok_and(|text| std::str::from_utf8(text).is_ok())
                && len <= max
        }
        Field::Bytes16(min, max) => {
            let len = usize::from(u16::from_le_bytes(r.array()?));
            r.bytes(len).is_ok() && (min..=max).contains(&len)
        }
        Field::Anchor => match r.u8()? {
            0 => true,
            1 => {
                for part in [Field::Enum(2), Field::U32, Field::U32, STR] {
                    read_field(r, part)?;
                }
                true
            }
            _ => false,
        },
    };
    if fits { Ok(()) } else { Err(bad_record(at)) }
}

/// `len` as the `u8` a record's length field holds; past 255, [`StoreError::Limit`] for `what`.
fn fits_u8(what: LimitKind, len: usize) -> Result<u8, StoreError> {
    u8::try_from(len).map_err(|_| StoreError::Limit {
        what,
        max: u64::from(u8::MAX),
        got: bytes::to_u64(len),
    })
}

fn put_optional_id(out: &mut Vec<u8>, id: Option<ChangesetId>) {
    match id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.0.0);
        }
        None => out.push(0),
    }
}

/// The bytes of a frame holding `records`. Refuses more than 65,535 records, or a record
/// whose lengths do not fit the layout, with [`StoreError::Limit`]; no records, which no
/// transaction appends, with [`StoreError::BadCall`].
pub(crate) fn encode_frame(records: &[Record]) -> Result<Vec<u8>, StoreError> {
    refuse_facts(StoreFormat::V1, records)?;
    encode_behind(HEADER_LEN, records, None)
}

/// Refuses a fact among `records` of a frame of `format`, FORMAT 1 or 2, whose decoder takes
/// none, with [`StoreError::BadCall`] before anything is encoded: a frame of theirs never holds a
/// record their readers refuse (the FORMAT 3 design §2.6).
fn refuse_facts(format: StoreFormat, records: &[Record]) -> Result<(), StoreError> {
    let fact = records.iter().find_map(|record| match record {
        Record::Fact(fact) => Some(fact.kind),
        _ => None,
    });
    match fact {
        Some(kind) => Err(StoreError::BadCall(format!(
            "a {kind:?} fact in a FORMAT {} frame: facts are FORMAT 3's",
            format.version()
        ))),
        None => Ok(()),
    }
}

/// The bytes of a frame holding `records`, then the nonce record of `nonce` if there is one,
/// behind a header of `header_len` bytes, of which the first 8 (`len`, `crc`) are written and the
/// rest left 0: [`encode_frame`]'s checks.
fn encode_behind(
    header_len: usize,
    records: &[Record],
    nonce: Option<u64>,
) -> Result<Vec<u8>, StoreError> {
    if records.is_empty() {
        return Err(StoreError::BadCall(
            "a frame holds at least one record".to_owned(),
        ));
    }
    let records_len = records.len() + usize::from(nonce.is_some());
    let count = u16::try_from(records_len).map_err(|_| StoreError::Limit {
        what: LimitKind::Records,
        max: bytes::to_u64(MAX_RECORDS),
        got: bytes::to_u64(records_len),
    })?;
    let mut frame = vec![0; header_len];
    frame.extend_from_slice(&count.to_le_bytes());
    for record in records {
        record.encode(&mut frame)?;
    }
    if let Some(nonce) = nonce {
        frame.push(KIND_NONCE);
        frame.extend_from_slice(&nonce.to_le_bytes());
    }
    let payload = &frame[header_len..];
    // At most 65,535 records of at most 8,234 bytes (a changeset with 255 parents).
    let len = u32::try_from(payload.len()).expect("a frame payload fits in u32");
    let crc = crc32fast::hash(payload);
    frame[..4].copy_from_slice(&len.to_le_bytes());
    frame[4..8].copy_from_slice(&crc.to_le_bytes());
    Ok(frame)
}

/// The payload CRC a frame's header holds, in either format: its bytes 4 to 8.
pub(crate) fn payload_crc(frame: &[u8]) -> u32 {
    u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]])
}

/// Writes the place of `frame`, the bytes of a FORMAT 2 frame, into its header: its `seq`, the
/// payload CRC `prev_crc` of the frame before it, and the header's own CRC over them.
pub(crate) fn set_place(frame: &mut [u8], seq: u64, prev_crc: u32) {
    frame[8..16].copy_from_slice(&seq.to_le_bytes());
    frame[16..HCRC_AT].copy_from_slice(&prev_crc.to_le_bytes());
    let hcrc = crc32fast::hash(&frame[..HCRC_AT]);
    frame[HCRC_AT..HEADER_LEN_V2].copy_from_slice(&hcrc.to_le_bytes());
}

/// A frame header, as [`decode_header_in`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    /// The payload's length in bytes.
    pub(crate) len: u32,
    /// The payload's CRC.
    pub(crate) crc: u32,
    /// The frame's seq, which FORMAT 2 headers hold; `None` in FORMAT 1.
    pub(crate) seq: Option<u64>,
    /// The CRC of the payload of the frame before, which FORMAT 2 headers hold (0 for seq 0);
    /// `None` in FORMAT 1.
    pub(crate) prev_crc: Option<u32>,
}

/// The size of a frame header in `format`.
pub(crate) fn header_len(format: StoreFormat) -> usize {
    match format.journal_family() {
        JournalFamily::One => HEADER_LEN,
        JournalFamily::Two => HEADER_LEN_V2,
    }
}

/// The bytes of a frame of `format` holding `records`: [`encode_frame`] in FORMAT 1, where
/// `seq` and `prev_crc` are not written; in FORMAT 2 the frame's seq and the payload CRC of the
/// frame before it go in its header ([`set_place`]). A FORMAT 3 frame ends in a nonce, which
/// this takes none of: [`StoreError::BadCall`] (see [`encode_frame_with_nonce`]).
pub(crate) fn encode_frame_in(
    format: StoreFormat,
    records: &[Record],
    seq: u64,
    prev_crc: u32,
) -> Result<Vec<u8>, StoreError> {
    match format {
        StoreFormat::V1 => encode_frame(records),
        StoreFormat::V2 => {
            refuse_facts(format, records)?;
            let mut frame = encode_behind(HEADER_LEN_V2, records, None)?;
            set_place(&mut frame, seq, prev_crc);
            Ok(frame)
        }
        StoreFormat::V3 => Err(StoreError::BadCall(
            "a FORMAT 3 frame ends in a nonce: encode it with one".to_owned(),
        )),
    }
}

/// [`encode_frame_in`] for every format: in FORMAT 3 the frame's records, then the nonce record
/// of `nonce` (the FORMAT 3 design §2.7), behind FORMAT 2's header. Refuses a FORMAT 3 frame
/// without a nonce, or a FORMAT 1 or 2 frame with one, with [`StoreError::BadCall`]; a FORMAT 3
/// frame of more than [`MAX_RECORDS_V3`] records, whose nonce would not fit the count, with
/// [`StoreError::Limit`].
pub(crate) fn encode_frame_with_nonce(
    format: StoreFormat,
    records: &[Record],
    nonce: Option<u64>,
    seq: u64,
    prev_crc: u32,
) -> Result<Vec<u8>, StoreError> {
    match (format, nonce) {
        (StoreFormat::V1 | StoreFormat::V2, None) => {
            encode_frame_in(format, records, seq, prev_crc)
        }
        (StoreFormat::V1 | StoreFormat::V2, Some(_)) => Err(StoreError::BadCall(
            "a FORMAT 1 or 2 frame holds no nonce".to_owned(),
        )),
        (StoreFormat::V3, None) => Err(StoreError::BadCall(
            "a FORMAT 3 frame ends in a nonce".to_owned(),
        )),
        (StoreFormat::V3, Some(nonce)) => {
            if records.len() > MAX_RECORDS_V3 {
                return Err(StoreError::Limit {
                    what: LimitKind::Records,
                    max: bytes::to_u64(MAX_RECORDS_V3),
                    got: bytes::to_u64(records.len()),
                });
            }
            let mut frame = encode_behind(HEADER_LEN_V2, records, Some(nonce))?;
            set_place(&mut frame, seq, prev_crc);
            Ok(frame)
        }
    }
}

/// Why the bytes at a FORMAT 2 frame boundary are no header to trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadHeader {
    /// Its CRC does not match the 20 bytes before it: a torn write, unlisted at the tail; damage
    /// elsewhere.
    Crc,
    /// Its CRC matches, but its length is 0 or more than [`MAX_PAYLOAD_LEN`]: no writer writes
    /// that, so it is damage wherever it lies.
    Len,
}

/// The header at the start of `buf`, a frame boundary, in `format`; `None` if `buf` ends
/// before the header does. FORMAT 2 checks the header's own CRC before it trusts `len`
/// ([`BadHeader::Crc`]), then `len` ([`BadHeader::Len`]); FORMAT 1 has no check to make.
pub(crate) fn decode_header_in(
    format: StoreFormat,
    buf: &[u8],
) -> Result<Option<FrameHeader>, BadHeader> {
    match format.journal_family() {
        JournalFamily::One => {
            let Some((header, _)) = buf.split_first_chunk::<HEADER_LEN>() else {
                return Ok(None);
            };
            let (len, crc) = header.split_at(4);
            Ok(Some(FrameHeader {
                len: u32::from_le_bytes(len.try_into().expect("4 bytes")),
                crc: u32::from_le_bytes(crc.try_into().expect("4 bytes")),
                seq: None,
                prev_crc: None,
            }))
        }
        JournalFamily::Two => {
            let Some((header, _)) = buf.split_first_chunk::<HEADER_LEN_V2>() else {
                return Ok(None);
            };

            // 1. The header's own CRC first: nothing in it is trusted before.
            let u32_at = |at: usize| {
                u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
            };
            if crc32fast::hash(&header[..HCRC_AT]) != u32_at(HCRC_AT) {
                return Err(BadHeader::Crc);
            }

            // 2. Then its length, against what this build decodes.
            let len = u32_at(0);
            if len == 0 || usize::try_from(len).is_ok_and(|len| len > MAX_PAYLOAD_LEN) {
                return Err(BadHeader::Len);
            }
            let (seq, _) = header[8..].split_first_chunk::<8>().expect("24 bytes");
            Ok(Some(FrameHeader {
                len,
                crc: u32_at(4),
                seq: Some(u64::from_le_bytes(*seq)),
                prev_crc: Some(u32_at(16)),
            }))
        }
    }
}

/// Where a frame must sit, which a FORMAT 2 header names: its seq, and the payload CRC of the
/// frame before it (0 before frame 0). FORMAT 1 headers name neither, and nothing checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Place {
    /// The frame's seq: its index in the journal.
    pub(crate) seq: u64,
    /// The payload CRC of the frame before it; 0 for frame 0.
    pub(crate) prev_crc: u32,
}

impl FrameHeader {
    /// Whether the header names `place` as its own: always, in FORMAT 1.
    pub(crate) fn sits_at(&self, place: Place) -> bool {
        self.seq.is_none_or(|seq| seq == place.seq)
            && self.prev_crc.is_none_or(|prev| prev == place.prev_crc)
    }
}

/// What the bytes at a frame boundary hold.
pub(crate) enum Parsed {
    /// A whole frame whose payload matches its CRC and decodes.
    Whole {
        records: Vec<Record>,
        /// The frame's size in bytes, header included.
        size: usize,
        /// Its payload's CRC.
        crc: u32,
    },
    /// The bytes end before the frame does: its header, or the payload the header
    /// announces, is cut short.
    Short,
    /// The frame is all there, but its payload does not match its CRC; or a FORMAT 2 header
    /// does not match its own ([`BadHeader::Crc`]).
    BadCrc,
    /// A header announcing an empty payload (a length of 0, `zero_len`), or a payload that
    /// matches its CRC but does not decode. Listed, either is damage. Unlisted, the second is a
    /// frame a writer wrote, and the first a header that never reached the disk, a torn write
    /// (see [`crate::Journal::truncate_torn_tail`]). FORMAT 2 has no such header: see
    /// [`Parsed::Corrupt`].
    Invalid {
        /// Whether the header announced a length of 0.
        zero_len: bool,
    },
    /// A FORMAT 2 header that matches its CRC but announces a length no writer writes
    /// ([`BadHeader::Len`]), or a place that is not its own (a seq other than its position, a
    /// previous CRC other than the payload CRC of the frame before it): a misplaced, stale or
    /// lost frame, damage wherever it lies.
    Corrupt,
}

/// Parses the frame at the start of `buf`, which runs from a frame boundary to (at most)
/// the end of the journal.
pub(crate) fn parse_frame(buf: &[u8]) -> Parsed {
    let Some((header, rest)) = buf.split_first_chunk::<HEADER_LEN>() else {
        return Parsed::Short;
    };
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if len == 0 {
        return Parsed::Invalid { zero_len: true };
    }
    let Some(payload) = usize::try_from(len).ok().and_then(|len| rest.get(..len)) else {
        return Parsed::Short;
    };
    parse_payload(HEADER_LEN, crc, payload)
}

/// Parses the frame of `format` at the start of `buf`, which runs from a frame boundary to (at
/// most) the end of the journal, and must sit at `place` (FORMAT 2 checks its header names it).
/// The journal reads a FORMAT 2 frame header first, from the disk; this parses one at hand, for
/// the order checker and the codec's tests.
#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn parse_frame_in(format: StoreFormat, buf: &[u8], place: Place) -> Parsed {
    if format.journal_family() == JournalFamily::One {
        return parse_frame(buf);
    }
    let header = match decode_header_in(format, buf) {
        Ok(Some(header)) => header,
        Ok(None) => return Parsed::Short,
        Err(BadHeader::Crc) => return Parsed::BadCrc,
        Err(BadHeader::Len) => return Parsed::Corrupt,
    };
    if !header.sits_at(place) {
        return Parsed::Corrupt;
    }
    let payload = usize::try_from(header.len)
        .ok()
        .and_then(|len| buf[HEADER_LEN_V2..].get(..len));
    match payload {
        Some(payload) => parse_payload(HEADER_LEN_V2, header.crc, payload),
        None => Parsed::Short,
    }
}

/// Parses `payload`, all of it there, behind a header of `header_len` bytes that gives its CRC
/// as `crc`.
pub(crate) fn parse_payload(header_len: usize, crc: u32, payload: &[u8]) -> Parsed {
    if crc32fast::hash(payload) != crc {
        return Parsed::BadCrc;
    }
    match decode_payload(payload) {
        Some(records) => Parsed::Whole {
            records,
            size: header_len + payload.len(),
            crc,
        },
        None => Parsed::Invalid { zero_len: false },
    }
}

/// The records of a frame payload: a count of at least 1, then exactly that many records.
/// A record that does not decode makes the frame [`Parsed::Invalid`], which the journal's
/// readers report as [`StoreError::JournalCorrupt`] at the frame's offset in `journal.log`: the
/// position in the payload where its record stopped decoding goes no further.
fn decode_payload(payload: &[u8]) -> Option<Vec<Record>> {
    let (count, mut rest) = payload.split_first_chunk::<2>()?;
    let count = usize::from(u16::from_le_bytes(*count));
    if count == 0 {
        return None;
    }
    // Every record takes at least one byte, so the count cannot reserve beyond the payload.
    let mut records = Vec::with_capacity(count.min(rest.len()));
    for _ in 0..count {
        let (record, used) = Record::decode(rest).ok()?;
        records.push(record);
        rest = rest.get(used..)?;
    }
    rest.is_empty().then_some(records)
}

/// What a record's bytes that do not decode are: [`BadRecord`] at `at`, the position in them
/// where decoding stopped.
fn bad_record(at: usize) -> BadRecord {
    BadRecord { at }
}

/// The reads of a record's fields beyond fixed-width integers.
trait RecordReader {
    fn id(&mut self) -> Result<ChangesetId, BadRecord>;

    /// A `u8` flag (0 or 1), then the id if the flag is 1.
    fn optional_id(&mut self) -> Result<Option<ChangesetId>, BadRecord>;

    /// A `u8` length, then that many bytes of UTF-8.
    fn string(&mut self) -> Result<String, BadRecord>;
}

impl RecordReader for Reader<'_, BadRecord> {
    fn id(&mut self) -> Result<ChangesetId, BadRecord> {
        self.array()
            .map(|bytes| ChangesetId::new(ObjectId::new(bytes)))
    }

    fn optional_id(&mut self) -> Result<Option<ChangesetId>, BadRecord> {
        let flag_at = self.pos();
        match self.u8()? {
            0 => Ok(None),
            1 => self.id().map(Some),
            _ => Err(self.error_at(flag_at)),
        }
    }

    fn string(&mut self) -> Result<String, BadRecord> {
        let len = usize::from(self.u8()?);
        let start = self.pos();
        let bytes = self.bytes(len)?;
        let text = std::str::from_utf8(bytes).map_err(|_| self.error_at(start))?;
        Ok(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compacted(sorted_count: u64) -> Record {
        Record::Compacted { sorted_count }
    }

    /// The frame bound is the umbrella design's 539,615,192 bytes, and the longest record is
    /// a changeset with 255 parents.
    #[test]
    fn the_largest_payload_is_65535_of_the_longest_record() {
        assert_eq!(MAX_PAYLOAD_LEN, 539_615_192);
        let longest = Record::Changeset {
            id: ChangesetId::new(ObjectId::new([1; 32])),
            generation: 2,
            delta_hash: [3; 32],
            parents: vec![ChangesetId::new(ObjectId::new([4; 32])); 255],
        };
        let mut bytes = Vec::new();
        longest.encode(&mut bytes).unwrap();
        assert_eq!(bytes.len(), MAX_RECORD_LEN);
    }

    /// A FORMAT 2 frame round-trips with its place; FORMAT 1's bytes do not change.
    #[test]
    fn frame2_round_trips_with_its_place() {
        let records = [compacted(1), compacted(2)];
        let v1 = encode_frame(&records).unwrap();
        assert_eq!(
            encode_frame_in(StoreFormat::V1, &records, 9, 9).unwrap(),
            v1
        );
        let v2 = encode_frame_in(StoreFormat::V2, &records, 7, 0xdead_beef).unwrap();
        assert_eq!(v2[..8], v1[..8], "len and crc as in FORMAT 1");
        assert_eq!(v2[HEADER_LEN_V2..], v1[HEADER_LEN..], "the same payload");
        assert_eq!(v2.len(), HEADER_LEN_V2 + v1.len() - HEADER_LEN);

        let header = decode_header_in(StoreFormat::V2, &v2).unwrap().unwrap();
        assert_eq!(header.seq, Some(7));
        assert_eq!(header.prev_crc, Some(0xdead_beef));
        assert_eq!(header.crc, payload_crc(&v1));
        let place = Place {
            seq: 7,
            prev_crc: 0xdead_beef,
        };
        let Parsed::Whole {
            records: got,
            size,
            crc,
        } = parse_frame_in(StoreFormat::V2, &v2, place)
        else {
            panic!("a whole frame");
        };
        assert_eq!(
            (got.as_slice(), size, crc),
            (&records[..], v2.len(), header.crc)
        );

        // Anywhere else, it is damage, not a torn write.
        for place in [
            Place { seq: 8, ..place },
            Place {
                prev_crc: 0,
                ..place
            },
        ] {
            assert!(matches!(
                parse_frame_in(StoreFormat::V2, &v2, place),
                Parsed::Corrupt
            ));
        }

        // Short of its header or payload it is short; a flipped header byte fails its CRC.
        assert!(matches!(
            parse_frame_in(StoreFormat::V2, &v2[..23], place),
            Parsed::Short
        ));
        let cut = &v2[..v2.len() - 1];
        assert!(matches!(
            parse_frame_in(StoreFormat::V2, cut, place),
            Parsed::Short
        ));
        assert_eq!(decode_header_in(StoreFormat::V2, &v2[..23]), Ok(None));
        for at in 0..HEADER_LEN_V2 {
            let mut bad = v2.clone();
            bad[at] ^= 0x10;
            assert_eq!(
                decode_header_in(StoreFormat::V2, &bad),
                Err(BadHeader::Crc),
                "{at}"
            );
            assert!(matches!(
                parse_frame_in(StoreFormat::V2, &bad, place),
                Parsed::BadCrc
            ));
        }
    }

    /// FORMAT 3's encoder writes the nonce frame's golden bytes (built there from the header
    /// layout), takes no frame without a nonce, and keeps a place in the count for it.
    #[test]
    fn frame3_encodes_with_its_nonce() {
        // 1. The golden's frame: a RefMove at seq 3, its nonce last.
        let golden = include_str!("../../tests/golden3/nonce_frame.hex");
        let line = golden.lines().next().unwrap();
        let want: Vec<u8> = (0..line.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&line[at..at + 2], 16).unwrap())
            .collect();
        let records = [Record::RefMove {
            name: "main".to_owned(),
            old: None,
            new: Some(ChangesetId::new(ObjectId::new([5; 32]))),
            actor: "agent/one".to_owned(),
            ts: 1_700_000_123,
        }];
        let nonce = Some(0x1122_3344_5566_7788);
        let got = encode_frame_with_nonce(StoreFormat::V3, &records, nonce, 3, 0xa1b2_c3d4);
        assert_eq!(got.unwrap(), want);

        // 2. A nonce in FORMAT 3 only, and the old encoder takes none.
        let bad_call =
            |got: Result<Vec<u8>, StoreError>| matches!(got, Err(StoreError::BadCall(_)));
        assert!(bad_call(encode_frame_with_nonce(
            StoreFormat::V3,
            &records,
            None,
            3,
            0
        )));
        assert!(bad_call(encode_frame_with_nonce(
            StoreFormat::V2,
            &records,
            nonce,
            3,
            0
        )));
        assert!(bad_call(encode_frame_in(StoreFormat::V3, &records, 3, 0)));
        assert_eq!(
            encode_frame_with_nonce(StoreFormat::V2, &records, None, 3, 7).unwrap(),
            encode_frame_in(StoreFormat::V2, &records, 3, 7).unwrap()
        );

        // 3. 65,534 records and the nonce fit; one more does not.
        let mut many = vec![compacted(0); MAX_RECORDS_V3];
        assert!(encode_frame_with_nonce(StoreFormat::V3, &many, nonce, 0, 0).is_ok());
        many.push(compacted(0));
        assert!(matches!(
            encode_frame_with_nonce(StoreFormat::V3, &many, nonce, 0, 0),
            Err(StoreError::Limit {
                what: LimitKind::Records,
                max: 65_534,
                got: 65_535
            })
        ));
        assert!(encode_frame_with_nonce(StoreFormat::V2, &many, None, 0, 0).is_ok());
    }

    /// A fact is FORMAT 3's: a FORMAT 1 or 2 frame refuses it before anything is encoded, so no
    /// journal of theirs holds a record their decoder refuses.
    #[test]
    fn format_1_and_2_frames_refuse_a_fact() {
        let fact = Fact::new(
            FactKind::Lineage,
            0,
            [1; 32],
            [0; 32],
            0,
            String::new(),
            vec![7; 32],
        )
        .unwrap();
        let records = [compacted(1), Record::Fact(fact)];
        let refused = |got: Result<Vec<u8>, StoreError>| matches!(got, Err(StoreError::BadCall(_)));
        assert!(refused(encode_frame(&records)));
        for format in [StoreFormat::V1, StoreFormat::V2] {
            assert!(
                refused(encode_frame_in(format, &records, 0, 0)),
                "{format:?}"
            );
        }
        assert_eq!(Frame::new(0, 0, records.to_vec()).crc, 0);
        assert!(encode_frame_with_nonce(StoreFormat::V3, &records[1..], Some(1), 0, 0).is_ok());
    }

    /// A fact is built only with a body its kind and type lay out, exactly.
    #[test]
    fn a_fact_takes_only_its_types_body() {
        let new = |kind, ty, other, actor: &str, body: &[u8]| {
            Fact::new(kind, ty, [1; 32], other, 0, actor.to_owned(), body.to_vec())
        };
        let lineage = [7; 32];
        assert!(new(FactKind::Lineage, 6, [2; 32], "a", &lineage).is_ok());

        // 1. A type the kind has not, a body too short or too long, an actor past 255 bytes.
        let bad_call = |got: Result<Fact, StoreError>| matches!(got, Err(StoreError::BadCall(_)));
        assert!(bad_call(new(FactKind::Lineage, 7, [2; 32], "a", &lineage)));
        assert!(bad_call(new(
            FactKind::Lineage,
            0,
            [2; 32],
            "a",
            &lineage[1..]
        )));
        assert!(bad_call(new(FactKind::Lineage, 0, [2; 32], "a", &[0; 33])));
        assert!(matches!(
            new(FactKind::Lineage, 0, [2; 32], &"a".repeat(256), &lineage),
            Err(StoreError::Limit {
                what: LimitKind::ActorBytes,
                ..
            })
        ));

        // 2. Enumerations, fixed values, string bounds and UTF-8; a withdrawal names no other.
        let review_state = |state: u8| [&[6, 0, 0, 0][..], &[state]].concat();
        assert!(new(FactKind::Review, 6, [0; 32], "", &review_state(3)).is_ok());
        assert!(bad_call(new(
            FactKind::Review,
            6,
            [0; 32],
            "",
            &review_state(4)
        )));
        assert!(bad_call(new(FactKind::Provenance, 1, [0; 32], "", &[2])));
        let key = |len: usize, byte: u8| {
            [
                &[u8::try_from(len).unwrap()][..],
                &vec![byte; len],
                &[0; 32],
            ]
            .concat()
        };
        assert!(new(FactKind::Annotation, 1, [0; 32], "", &key(64, b'k')).is_ok());
        assert!(bad_call(new(
            FactKind::Annotation,
            1,
            [0; 32],
            "",
            &key(65, b'k')
        )));
        assert!(bad_call(new(
            FactKind::Annotation,
            1,
            [0; 32],
            "",
            &key(1, 0xff)
        )));
        let reason = [&0i64.to_le_bytes()[..], &[1, 0, b'x']].concat();
        assert!(new(FactKind::Withdrawal, 1, [0; 32], "", &reason).is_ok());
        assert!(bad_call(new(FactKind::Withdrawal, 1, [3; 32], "", &reason)));
        assert!(bad_call(new(
            FactKind::Withdrawal,
            1,
            [0; 32],
            "",
            &reason[..10]
        )));

        // 3. Blob ids where text would be: an annotation's value (all zero on Remove), a
        // review's title, a provenance's free text (the FORMAT 3 design's amendment of
        // 2026-11-11, G3, G10).
        let set = [&[5][..], b"owner", &[4; 32]].concat();
        assert!(new(FactKind::Annotation, 0, [0; 32], "", &set).is_ok());
        assert!(bad_call(new(FactKind::Annotation, 1, [0; 32], "", &set)));
        let open = [&[0; 12][..], &[0], &[1; 32], &[2; 32], &[3; 32]].concat();
        assert!(new(FactKind::Review, 0, [0; 32], "", &open).is_ok());
        assert!(bad_call(new(
            FactKind::Review,
            0,
            [0; 32],
            "",
            &open[..open.len() - 1]
        )));
        let model = [&[1][..], &[0; 24], &[1, b'm'], &[5; 32]].concat();
        assert!(new(FactKind::Provenance, 0, [0; 32], "", &model).is_ok());
        let free_text = [&model[..27], &[1, b'h'], &model[27..]].concat();
        assert!(bad_call(new(
            FactKind::Provenance,
            0,
            [0; 32],
            "",
            &free_text
        )));
    }

    /// The envelope golden (`envelope.hex`, the FORMAT 3 design §2.8, confirmed): each line
    /// decodes by the envelope alone, its body opaque bytes of no kind's layout, and encodes back
    /// to itself; a flipped byte reads as another envelope or none; a fact's encoding is its
    /// envelope around its body.
    #[test]
    fn the_envelope_matches_its_golden() {
        // 1. The golden: two lines of hex, an envelope filled and an empty one.
        let golden = include_str!("../../tests/golden3/envelope.hex");
        let lines: Vec<Vec<u8>> = (golden.lines())
            .map(|line| {
                (0..line.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&line[at..at + 2], 16).unwrap())
                    .collect()
            })
            .collect();
        let wants = [
            ([9; 32], "agent/one", &b"opaque\x00\xff"[..]),
            ([0; 32], "", &b""[..]),
        ];
        assert_eq!(lines.len(), wants.len());

        // 2. Each decodes to its fields and encodes back.
        for (line, (other, actor, body)) in lines.iter().zip(wants) {
            let (envelope, used) = Envelope::decode(line).expect("an envelope");
            assert_eq!(used, line.len());
            assert_eq!(line.len(), 77 + actor.len() + body.len());
            assert_eq!((envelope.kind, envelope.ty), (FactKind::Review, 0));
            assert_eq!((envelope.subject, envelope.other), ([8; 32], other));
            assert_eq!(envelope.ts, 1_700_000_123);
            assert_eq!((envelope.actor.as_str(), envelope.body), (actor, body));
            let mut again = Vec::new();
            envelope.encode(&mut again);
            assert_eq!(&again, line);

            // A flipped byte: refused, or another envelope.
            for at in 0..line.len() {
                let mut flipped = line.clone();
                flipped[at] ^= 0x01;
                let read = Envelope::decode(&flipped).ok();
                assert_ne!(read, Some((envelope.clone(), used)));
            }
        }

        // 3. A fact's bytes are its envelope around its body.
        let fact = Fact::new(
            FactKind::Lineage,
            2,
            [1; 32],
            [2; 32],
            7,
            "a".to_owned(),
            vec![3; 32],
        )
        .unwrap();
        let mut bytes = Vec::new();
        fact.encode(&mut bytes);
        let (envelope, used) = Envelope::decode(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(
            (envelope.body, envelope.actor.as_str()),
            (&[3; 32][..], "a")
        );
    }

    /// A header that checks but announces 0 or more than the bound is refused from the header.
    #[test]
    fn a_checked_header_past_the_bound_is_refused() {
        for (len, bad) in [
            (0, true),
            (1, false),
            (MAX_PAYLOAD_LEN, false),
            (MAX_PAYLOAD_LEN + 1, true),
        ] {
            let mut header = encode_frame_in(StoreFormat::V2, &[compacted(0)], 0, 0).unwrap();
            header.truncate(HEADER_LEN_V2);
            header[..4].copy_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
            set_place(&mut header, 0, 0);
            let decoded = decode_header_in(StoreFormat::V2, &header);
            assert_eq!(
                decoded.is_err_and(|e| e == BadHeader::Len),
                bad,
                "len {len}"
            );
            let parsed = parse_frame_in(
                StoreFormat::V2,
                &header,
                Place {
                    seq: 0,
                    prev_crc: 0,
                },
            );
            assert_eq!(matches!(parsed, Parsed::Corrupt), bad, "len {len}");
        }
    }
}
