//! The record `swarmops serve` writes of each request it served (the M17 design §6): one JSON
//! line in the store's `git/serve-records.jsonl`, saying who asked for what and where the
//! connection's time went. Diagnostics, never acknowledged state: no path and no secret.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// What a served request asked for.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServeService {
    /// A fetch that excluded nothing: no have, no excluded commit, no shallow client.
    Clone,
    /// Any other fetch: with haves, an excluded commit or a shallow client, one that sent no
    /// pack, and a fetch's HTTP advertisement.
    Fetch,
    /// A push.
    Push,
}

/// One served request: when it started, its caller and service, and how its time split.
///
/// For a fetch, `prepare_ms` runs from the connection's start to the pack's first byte and
/// `send_ms` from there to its end. For a push, `send_ms` runs from the start to the push's
/// check (receiving and git's checks) and `prepare_ms` from there to the end (judging,
/// importing, landing, exporting). A request that sent no pack or reached no check is all
/// `prepare_ms`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServeRecord {
    /// When the connection started, in Unix milliseconds.
    pub at: u64,
    /// The caller `serve` resolved.
    pub identity: String,
    /// What it asked for.
    pub service: ServeService,
    /// The milliseconds spent preparing.
    #[serde(default)]
    pub prepare_ms: u64,
    /// The milliseconds spent sending a fetch's pack, or receiving a push's.
    #[serde(default)]
    pub send_ms: u64,
    /// The pack's bytes: sent by a fetch, received by a push.
    #[serde(default)]
    pub bytes: u64,
    /// The connection's exit code: git's, `128 + signal` for a git a signal ended, or a
    /// refusal's.
    #[serde(default)]
    pub exit: i32,
    /// The code of a SwarmOps refusal; absent when there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

impl ServeRecord {
    /// The record of `identity`'s `service` started at `at`, with no time, byte, exit or code
    /// yet.
    pub fn new(at: u64, identity: impl Into<String>, service: ServeService) -> Self {
        Self {
            at,
            identity: identity.into(),
            service,
            prepare_ms: 0,
            send_ms: 0,
            bytes: 0,
            exit: 0,
            code: None,
        }
    }
}
