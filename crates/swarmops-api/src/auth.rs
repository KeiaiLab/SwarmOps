//! The user's logins: the results of `auth login`, `auth status` and `auth logout`.
//!
//! These are client-side operations on one file of the user's, outside every store, so no
//! `StoreOps` has them and no MCP tool runs them. Unlike the store's types they carry a path of
//! the caller's host: `file`, the login file, which is the caller's to know. No token is ever a
//! field: a login is named by its provider and its host.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One login the user's login file holds, without its token.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthEntry {
    /// The provider the login is with: `gh` for GitHub. A file may hold the logins of providers
    /// this build does not know; they are listed and never used.
    pub provider: String,
    /// The host the token is for, as `host` or `host:port`: `github.com`.
    pub host: String,
    /// How the token was obtained: `with_token` (read from stdin) or `device` (the provider's
    /// device flow). Omitted for an entry that does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The scopes the provider granted the token. Omitted when they are not known, as for a
    /// token read from stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

impl AuthEntry {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(provider: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            host: host.into(),
            source: None,
            scopes: None,
        }
    }
}

/// `auth login`: the login now kept.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthLogin {
    /// The login, without its token.
    pub login: AuthEntry,
    /// The login file that holds it, a path on the caller's host.
    pub file: String,
    /// Whether the file held a login for this host already, which this one replaced.
    pub replaced: bool,
}

impl AuthLogin {
    /// Takes every field.
    pub fn new(login: AuthEntry, file: impl Into<String>, replaced: bool) -> Self {
        Self {
            login,
            file: file.into(),
            replaced,
        }
    }
}

/// `auth status`: the logins the user's login file holds.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthStatus {
    /// The login file, a path on the caller's host, whether or not it exists. Omitted on a host
    /// that names no directory for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The logins, in the file's order; empty when there is no file or it holds none.
    pub logins: Vec<AuthEntry>,
}

impl AuthStatus {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(logins: Vec<AuthEntry>) -> Self {
        Self { file: None, logins }
    }
}

/// `auth logout`: the login removed, or that there was none.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthLogout {
    /// The provider logged out of.
    pub provider: String,
    /// The host the login was for.
    pub host: String,
    /// Whether the file held the login; false when there was none to remove.
    pub removed: bool,
    /// The login file, a path on the caller's host. Omitted on a host that names no directory
    /// for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl AuthLogout {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(provider: impl Into<String>, host: impl Into<String>, removed: bool) -> Self {
        Self {
            provider: provider.into(),
            host: host.into(),
            removed,
            file: None,
        }
    }
}
