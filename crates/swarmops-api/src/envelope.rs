//! The JSON envelope every command prints (operations spec §0).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::OpError;

/// The API version every envelope carries in `api`.
pub const API_VERSION: u32 = 1;

/// One command's output: `result` on success, `error` on failure, `meta` either way.
///
/// `Envelope::ok` and `Envelope::err` set exactly one of `result` and `error`; the other is
/// omitted from the JSON, never written as null.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Envelope<T> {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// The operation's name, such as `commit`.
    pub op: String,
    /// The API version: `API_VERSION`.
    pub api: u32,
    /// The response; present exactly when `ok`.
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
}
