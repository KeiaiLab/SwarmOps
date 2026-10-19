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
}
