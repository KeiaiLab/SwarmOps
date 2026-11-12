//! The native wire between two stores (M11-B, native sync design §2.2): the requests a host
//! sends `swarmops serve --stdio` and the results it answers with.
//!
//! A request is one JSON line of at most [`MAX_WIRE_LINE`] bytes, a [`WireRequest`]; an answer
//! is one line holding the existing [`Envelope`](crate::Envelope), a refusal its
//! [`OpError`](crate::OpError) with an existing code and a `details.reason`. Object bytes
//! travel after the line that announces them, in chunks ([`WireChunks`]). Ids are lowercase
//! hex. A land answers with the existing [`LandResp`](crate::LandResp), or `E_CONFLICT` with a
//! [`ConflictReport`](crate::ConflictReport).
//!
//! These types are not operations: `swarmops schema` does not list them. Within one protocol
//! number fields are only added; a field whose absence changes meaning needs a new number.
//!
//! ```text
//! host                                     upstream (serve --stdio)
//!   wire_hello ──────────────────────────▶ protocol, identity, format
//!   wire_refs / wire_list / wire_get ────▶ refs, ids, chunks of objects
//!   wire_has / wire_put (+ chunks) ──────▶ held, adopted (no ref moved)
//!   wire_land / wire_move ───────────────▶ LandResp / the moved ref
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::envelope::API_VERSION;
use crate::ops::{DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_CONFLICT_BYTES};

/// The wire protocol this build speaks.
pub const WIRE_PROTOCOL: u32 = 1;

/// The longest request or answer line, in bytes (1 MiB).
pub const MAX_WIRE_LINE: usize = 1024 * 1024;

/// The most ids one list of a request or a result carries; longer lists page.
pub const MAX_WIRE_IDS: usize = 10_000;

/// The most record bytes one chunk holds (64 MiB); one object alone may be longer, up to the
/// store's own bound.
pub const MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;

/// The seconds without a byte from the peer that end a session (`E_REMOTE_UNREACHABLE`,
/// reason `timeout`, retryable).
pub const WIRE_STALL_SECS: u64 = 300;

/// `wire_hello`: the protocol both sides take, and who the caller is.
pub const WIRE_HELLO: &str = "wire_hello";
/// `wire_refs`: a page of the store's refs.
pub const WIRE_REFS: &str = "wire_refs";
/// `wire_list`: a page of the changesets a ref reaches that the caller lacks.
pub const WIRE_LIST: &str = "wire_list";
/// `wire_get`: changesets and blobs by id, in chunks.
pub const WIRE_GET: &str = "wire_get";
/// `wire_has`: which of some ids the store holds.
pub const WIRE_HAS: &str = "wire_has";
/// `wire_put`: objects in chunks, and the changesets to adopt.
pub const WIRE_PUT: &str = "wire_put";
/// `wire_land`: land one change on a target.
pub const WIRE_LAND: &str = "wire_land";
/// `wire_move`: move, create or delete one ref by compare-and-swap.
pub const WIRE_MOVE: &str = "wire_move";
/// `wire_facts`: reserved for records that cross with their changesets; nothing answers it yet.
pub const WIRE_FACTS: &str = "wire_facts";

fn default_ids_limit() -> usize {
    MAX_WIRE_IDS
}

fn default_max_attempts() -> u32 {
    DEFAULT_MAX_ATTEMPTS
}

fn default_max_conflict_bytes() -> u64 {
    DEFAULT_MAX_CONFLICT_BYTES
}

// ---- the request line ----

/// One request line: the op, the API version, the protocol it is written in, and its body.
///
/// A server refuses a `protocol` it does not take (`E_INVALID_ARG`, `protocol`) before it reads
/// `req`. A `wire_hello` names the client's highest protocol here and its range in its body.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireRequest<T> {
    /// The op's name, such as `wire_refs`.
    pub op: String,
    /// The API version.
    pub api: u32,
    /// The wire protocol the request is written in.
    pub protocol: u32,
    /// The op's request.
    pub req: T,
}

impl<T> WireRequest<T> {
    /// The line of `op` under `protocol`, with this build's API version.
    pub fn new(op: impl Into<String>, protocol: u32, req: T) -> Self {
        Self {
            op: op.into(),
            api: API_VERSION,
            protocol,
            req,
        }
    }
}

// ---- wire_hello ----

/// `wire_hello`: the client's protocol range and what it takes.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireHelloReq {
    /// The lowest protocol the client takes.
    pub protocol_min: u32,
    /// The highest protocol the client takes.
    pub protocol_max: u32,
    /// The object kinds the client takes, by name: `blob`, `changeset`.
    pub object_kinds: Vec<String>,
    /// The features the client takes, by name; an op or record of a feature only one side
    /// lists is never sent.
    pub features: Vec<String>,
}

impl WireHelloReq {
    /// Takes every field.
    pub fn new(
        protocol_min: u32,
        protocol_max: u32,
        object_kinds: Vec<String>,
        features: Vec<String>,
    ) -> Self {
        Self {
            protocol_min,
            protocol_max,
            object_kinds,
            features,
        }
    }
}

/// `wire_hello`'s result: the protocol of the session and the server's side of it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireHello {
    /// The highest protocol both sides take.
    pub protocol: u32,
    /// The caller, as the server identified the connection.
    pub identity: String,
    /// The store's `FORMAT`.
    pub format: u32,
    /// The object kinds the server takes, by name.
    pub object_kinds: Vec<String>,
    /// The features the server takes, by name.
    pub features: Vec<String>,
    /// The server's version.
    pub server: String,
}

impl WireHello {
    /// Takes every field.
    pub fn new(
        protocol: u32,
        identity: impl Into<String>,
        format: u32,
        object_kinds: Vec<String>,
        features: Vec<String>,
        server: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            identity: identity.into(),
            format,
            object_kinds,
            features,
            server: server.into(),
        }
    }
}

// ---- wire_refs ----

/// `wire_refs`: a page of the store's refs, in name order.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireRefsReq {
    /// The page starts after this ref name; absent, at the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The most refs the page lists, at most [`MAX_WIRE_IDS`] (the default).
    #[serde(default = "default_ids_limit")]
    pub limit: usize,
}

impl WireRefsReq {
    /// The first page, of the most refs a page takes.
    pub fn new() -> Self {
        Self {
            after: None,
            limit: MAX_WIRE_IDS,
        }
    }
}

impl Default for WireRefsReq {
    fn default() -> Self {
        Self::new()
    }
}

/// `wire_refs`'s result: one page of refs.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireRefs {
    /// The page's refs.
    pub refs: Vec<WireRef>,
    /// Whether the store has `main`: a store without it says so here, never by an empty page.
    pub has_main: bool,
    /// Where the next page starts; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl WireRefs {
    /// Takes the required fields; `next` is absent.
    pub fn new(refs: Vec<WireRef>, has_main: bool) -> Self {
        Self {
            refs,
            has_main,
            next: None,
        }
    }
}

/// One ref and the changeset it names.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireRef {
    /// The ref's name, such as `main` or `tags/v1`.
    pub name: String,
    /// The changeset it names.
    pub changeset: String,
}

impl WireRef {
    /// Takes every field.
    pub fn new(name: impl Into<String>, changeset: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            changeset: changeset.into(),
        }
    }
}

// ---- wire_list ----

/// `wire_list`: a page of the changesets `want` reaches and no `have` reaches.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireListReq {
    /// The changeset a ref names as `wire_refs` lists it; any other is `E_NOT_FOUND`,
    /// `not_a_ref_head`.
    pub want: String,
    /// Changesets the caller holds; one the server lacks is passed over.
    #[serde(default)]
    pub have: Vec<String>,
    /// The page starts after this position; absent, at the top.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<WireCursor>,
    /// The most changesets the page lists, at most [`MAX_WIRE_IDS`] (the default).
    #[serde(default = "default_ids_limit")]
    pub limit: usize,
}

impl WireListReq {
    /// The first page of what `want` reaches, with no `have`.
    pub fn new(want: impl Into<String>) -> Self {
        Self {
            want: want.into(),
            have: Vec::new(),
            after: None,
            limit: MAX_WIRE_IDS,
        }
    }
}

/// `wire_list`'s result: changesets, highest generation first, ties by id.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireList {
    /// The page's changesets.
    pub changesets: Vec<String>,
    /// Where the next page starts; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<WireCursor>,
}

impl WireList {
    /// Takes the required fields; `next` is absent.
    pub fn new(changesets: Vec<String>) -> Self {
        Self {
            changesets,
            next: None,
        }
    }
}

/// A position in a listing: a changeset's generation and id.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireCursor {
    /// The changeset's generation.
    pub generation: u64,
    /// The changeset's id.
    pub id: String,
}

impl WireCursor {
    /// Takes every field.
    pub fn new(generation: u64, id: impl Into<String>) -> Self {
        Self {
            generation,
            id: id.into(),
        }
    }
}

// ---- wire_get ----

/// `wire_get`: changesets that `under`'s head reaches, and blobs their deltas name.
///
/// Anything else, as an id the store lacks, is `E_NOT_FOUND`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireGetReq {
    /// The ref whose head reaches every changeset asked for.
    pub under: String,
    /// The changesets to send.
    #[serde(default)]
    pub changesets: Vec<String>,
    /// The blobs to send, each with the changeset whose delta names it.
    #[serde(default)]
    pub blobs: Vec<WireBlobWant>,
}

impl WireGetReq {
    /// A request under `under` that asks for nothing yet.
    pub fn new(under: impl Into<String>) -> Self {
        Self {
            under: under.into(),
            changesets: Vec::new(),
            blobs: Vec::new(),
        }
    }
}

/// One blob asked for, and the changeset whose delta names it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireBlobWant {
    /// The changeset whose delta names the blob.
    pub of: String,
    /// The blob.
    pub id: String,
}

impl WireBlobWant {
    /// Takes every field.
    pub fn new(of: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            of: of.into(),
            id: id.into(),
        }
    }
}

/// The chunks that follow a line: `wire_get`'s result, and what a `wire_put` announces.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireChunks {
    /// How many chunks follow.
    pub chunks: u32,
    /// Their bytes, headers included.
    pub bytes: u64,
}

impl WireChunks {
    /// Takes every field.
    pub fn new(chunks: u32, bytes: u64) -> Self {
        Self { chunks, bytes }
    }
}

// ---- wire_has ----

/// `wire_has`: which of `ids` the store holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireHasReq {
    /// Changeset and blob ids, at most [`MAX_WIRE_IDS`].
    pub ids: Vec<String>,
}

impl WireHasReq {
    /// Takes every field.
    pub fn new(ids: Vec<String>) -> Self {
        Self { ids }
    }
}

/// `wire_has`'s result: one answer per id asked for, in its order.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireHas {
    /// Whether the store holds each id.
    pub held: Vec<bool>,
}

impl WireHas {
    /// Takes every field.
    pub fn new(held: Vec<bool>) -> Self {
        Self { held }
    }
}

// ---- wire_put ----

/// `wire_put`: the chunks that follow the line, and the changesets to adopt from them.
///
/// Announced `bytes` above the server's push bound are refused before any chunk is read.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WirePutReq {
    /// How many chunks follow.
    pub chunks: u32,
    /// Their bytes, headers included.
    pub bytes: u64,
    /// The changesets to adopt, parents first; no ref moves.
    #[serde(default)]
    pub adopt: Vec<String>,
}

impl WirePutReq {
    /// A put of `chunks` chunks of `bytes` bytes that adopts nothing.
    pub fn new(chunks: u32, bytes: u64) -> Self {
        Self {
            chunks,
            bytes,
            adopt: Vec::new(),
        }
    }
}

/// `wire_put`'s result: what the put adopted, and what the store held already.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WirePut {
    /// The changesets this put adopted.
    pub adopted: Vec<String>,
    /// The changesets the store held already; they wrote nothing.
    pub present: Vec<String>,
}

impl WirePut {
    /// Takes every field.
    pub fn new(adopted: Vec<String>, present: Vec<String>) -> Self {
        Self { adopted, present }
    }
}

// ---- wire_land ----

/// `wire_land`: land the change `head` adds after `base` on `target`.
///
/// The server keys the land itself, from `target` and `head`, so one change lands once
/// whoever sends it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireLandReq {
    /// The ref to land on: `main` or `release/<name>`.
    pub target: String,
    /// The changeset the change was taken from.
    pub base: String,
    /// The change's head.
    pub head: String,
    /// How many times the land tries to publish, as `land`'s.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// The most bytes of a conflict report, as `land`'s.
    #[serde(default = "default_max_conflict_bytes")]
    pub max_conflict_bytes: u64,
}

impl WireLandReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        target: impl Into<String>,
        base: impl Into<String>,
        head: impl Into<String>,
    ) -> Self {
        Self {
            target: target.into(),
            base: base.into(),
            head: head.into(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_conflict_bytes: DEFAULT_MAX_CONFLICT_BYTES,
        }
    }
}

// ---- wire_move ----

/// `wire_move`: one ref moved by compare-and-swap.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireMoveReq {
    /// The ref's name.
    pub name: String,
    /// The changeset the ref must name now; absent, the ref must be absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// The changeset the ref names after; absent, the ref is deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    /// How the ref may move.
    #[serde(default)]
    pub mode: WireMoveMode,
}

impl WireMoveReq {
    /// A move of `name` from absent to absent, by advance: set `old` and `new`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            old: None,
            new: None,
            mode: WireMoveMode::default(),
        }
    }
}

/// How a `wire_move` may move its ref.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireMoveMode {
    /// Only to a changeset whose history holds `old`.
    #[default]
    Advance,
    /// Anywhere, for a caller who holds `force` on the ref.
    Force,
}

/// `wire_move`'s result: the ref as it moved.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireMove {
    /// The ref's name.
    pub name: String,
    /// What it named before; absent if it was absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// What it names now; absent if it was deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

impl WireMove {
    /// Takes every field.
    pub fn new(name: impl Into<String>, old: Option<String>, new: Option<String>) -> Self {
        Self {
            name: name.into(),
            old,
            new,
        }
    }
}
