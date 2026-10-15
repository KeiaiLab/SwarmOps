//! [`Rev`]: a revision as the caller wrote it.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A revision as the caller wrote it, for the store to resolve: a ref name (`main`,
/// `release/1.2`), a full 64-hex changeset id or a unique prefix of at least 12 hex digits,
/// optionally followed by `~N` for the N-th first-parent ancestor.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct Rev(pub String);

impl Rev {
    /// The revision as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Rev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Rev {
    fn from(rev: String) -> Self {
        Self(rev)
    }
}

impl From<&str> for Rev {
    fn from(rev: &str) -> Self {
        Self(rev.to_owned())
    }
}
