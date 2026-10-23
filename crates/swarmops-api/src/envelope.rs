//! The JSON envelope every command prints (operations spec §0).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::OpError;

/// The API version every envelope carries in `api`.
pub const API_VERSION: u32 = 1;

/// One command's output: `result` on success, `error` on failure, `meta` either way.
///
/// Exactly one of `result` and `error` is present; the other is omitted, never written as null.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[schemars(rename = "Envelope", transform = exactly_one_outcome)]
pub struct Envelope<T> {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// The operation's name, such as `commit`.
    pub op: String,
    /// The API version.
    pub api: u32,
    /// The operation's response; present exactly when `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    /// The failure; present exactly when not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OpError>,
    /// Context for either outcome.
    pub meta: Meta,
}

impl<T> Envelope<T> {
    /// The envelope of a successful `op`.
    pub fn ok(op: impl Into<String>, result: T, meta: Meta) -> Self {
        Self {
            ok: true,
            op: op.into(),
            api: API_VERSION,
            result: Some(result),
            error: None,
            meta,
        }
    }

    /// The envelope of a failed `op`.
    pub fn err(op: impl Into<String>, error: OpError, meta: Meta) -> Self {
        Self {
            ok: false,
            op: op.into(),
            api: API_VERSION,
            result: None,
            error: Some(error),
            meta,
        }
    }
}

/// States in `Envelope`'s schema what its two constructors keep, which the struct cannot say:
/// exactly one of `result` (when `ok`) and `error` (when not) is present, the other omitted,
/// and `error` is never null. The wire does not change; a schema-validating client sees the rule.
fn exactly_one_outcome(schema: &mut schemars::Schema) {
    if let Some(Value::Object(error)) = schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("error"))
    {
        // `Option<OpError>` gives `anyOf [OpError, null]`; the error is never written as null.
        if let Some(Value::Array(branches)) = error.remove("anyOf") {
            let mut written = branches
                .into_iter()
                .filter(|branch| branch.get("type") != Some(&Value::from("null")));
            if let Some(Value::Object(only)) = written.next() {
                error.extend(only);
            }
        }
    }
    schema.insert(
        "oneOf".to_owned(),
        json!([
            {"properties": {"ok": {"const": true}}, "required": ["result"],
             "not": {"required": ["error"]}},
            {"properties": {"ok": {"const": false}}, "required": ["error"],
             "not": {"required": ["result"]}},
        ]),
    );
}

/// What the envelope reports besides the outcome.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// The store as the user named it, for display; omitted when the command used no store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// How long the command took, in milliseconds.
    pub elapsed_ms: u64,
    /// Every revision the command resolved: the string given and the full changeset id it named.
    pub resolved: BTreeMap<String, String>,
    /// The store's authentication mode (`trust` in M1), not how this caller was identified.
    pub auth: String,
    /// What the command's write repaired before its own work, of what earlier writers left when
    /// they crashed or failed after their commit point; omitted when there was nothing to repair,
    /// and for a command that writes nothing. `commit`, `ref_set` and `ref_delete` report it on
    /// success and on `E_CAS_MISMATCH` alike, and lose it when they fail otherwise. Whoever
    /// writes next gets it, not the agent whose process crashed, and it is no failure: `warnings`
    /// still names only the steps after this command's own commit point that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered: Option<RecoveredOut>,
}

impl Meta {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        elapsed_ms: u64,
        resolved: BTreeMap<String, String>,
        auth: impl Into<String>,
    ) -> Self {
        Self {
            repo: None,
            elapsed_ms,
            resolved,
            auth: auth.into(),
            recovered: None,
        }
    }
}

/// What a write's recovery repaired (`meta.recovered`), as offsets and counts in the store's
/// metadata files, with no paths: the storage spec (§2 rule 3, §3) says what each repair is.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveredOut {
    /// The torn tail cut off the end of the journal, if there was one: the bytes of a transaction
    /// that never reached its commit point; omitted when nothing was cut.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_cut: Option<LogCutOut>,
    /// The journal frames listed that their writer never listed: transactions that committed,
    /// their writer having crashed between the frame's sync and its listing, or failed to list
    /// it (a warning then); and frames listed again whose entries a power loss zeroed.
    pub frames_listed: u64,
    /// Of those, the frames written to the journal afresh first, because the sync that would
    /// have made them durable failed.
    pub frames_rewritten: u64,
    /// The bytes dropped from the end of the journal's list of frames: part of an entry, or
    /// entries a power loss zeroed.
    pub off_bytes_dropped: u64,
    /// The changeset index records cut, counted in its longest column: records a crash left torn,
    /// or a power loss emptied.
    pub index_records_dropped: u64,
    /// Whether a compaction of the changeset index that a crash interrupted was finished.
    pub views_rebuilt: bool,
    /// The changesets replayed from the journal into the index.
    pub records_replayed: u64,
    /// The ref files replayed from the journal: replaced, or removed.
    pub refs_replayed: u64,
}

/// Where the journal was cut (`meta.recovered.log_cut`).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCutOut {
    /// The journal's byte offset where the torn tail started, and where the journal ends now.
    pub at: u64,
    /// How many bytes were cut, to where the journal ended.
    pub bytes: u64,
}

impl LogCutOut {
    /// Takes every field.
    pub fn new(at: u64, bytes: u64) -> Self {
        Self { at, bytes }
    }
}
