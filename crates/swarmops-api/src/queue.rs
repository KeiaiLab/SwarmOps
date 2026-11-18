//! The landing queue (M22 core design §2–§6): the requests and responses of `queue enqueue`,
//! `queue status`, `queue withdraw` and `queue wait`, and those its runner exchanges with the
//! store for a round.
//!
//! An entry asks for a change, `head` with its first-parent chain after `base`, to land on a
//! target ref. A runner, one per target, builds the open entries into a chain on the target's
//! head, checks candidate prefixes of it outside the store's lock, and moves the target only to
//! a checked tree. Every state change is a queue fact in the store's journal, so an entry
//! outlives the process that enqueued it.
//!
//! ```text
//!             enqueue            candidate               publish
//!  (none) ──────────▶ waiting ─────────────▶ checking ─────────────▶ landed
//!                        │ ▲                     │
//!                        │ └── rebuilt next round┘
//!                        └──────────────┬────────┘
//!                                       ▼
//!                               removed { reason }
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::rev::Rev;

// ---- shared ----

/// Where an entry stands, as the store's queue facts say.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueueEntryState {
    /// Open, and no check of a candidate on the target's head runs for it.
    Waiting,
    /// Open, and a candidate built on the target's current head is being checked.
    Checking,
    /// The target moved to a changeset holding its change.
    Landed,
    /// Taken out of the queue without landing; `removal` says why.
    Removed,
}

/// What a check of a candidate tree came to: a verdict.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VerdictState {
    /// The check exited 0.
    Passed,
    /// The check exited with another code.
    Failed,
    /// The runner stopped the check at its timeout.
    TimedOut,
    /// A signal from outside the runner stopped the check: no one is blamed, and the entry
    /// waits for another round (the third such verdict removes it).
    Cancelled,
}

/// Why an entry was taken out of the queue without landing.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RemovalReason {
    /// Its prefix's check failed while the prefix before it passed.
    CheckFailed,
    /// Its change conflicts with the chain before it; `report` holds the conflict report.
    Conflict,
    /// Its prefix's check ran past its timeout while the prefix before it passed.
    TimedOut,
    /// It was withdrawn.
    Withdrawn,
    /// Its target ref was deleted.
    TargetGone,
    /// Its prefix's check was cancelled from outside three times.
    Cancelled,
}

/// One entry of the queue, as `queue status` lists it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueEntryOut {
    /// The entry's id: 64 hex digits.
    pub id: String,
    /// The ref it lands on.
    pub target: String,
    /// The change's head, a full id.
    pub head: String,
    /// Where the change starts, a full id.
    pub base: String,
    /// The key it was enqueued with; omitted when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Where it stands.
    pub state: QueueEntryState,
    /// Its place among the target's open entries, from 1 in enqueue order; omitted once it
    /// landed or was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
    /// The candidate changeset of its latest round: the chain up to it on the target's head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// The root tree of that candidate, or of the prefix a removal names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The verdict recorded for `tree`; omitted while there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictState>,
    /// The target's changeset that holds its change, once landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed: Option<String>,
    /// Why it was removed, once removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removal: Option<RemovalReason>,
    /// The check a verdict or a removal names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// The blob that holds a removal's report: the conflict report, or the check's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
}

impl QueueEntryOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        id: impl Into<String>,
        target: impl Into<String>,
        head: impl Into<String>,
        base: impl Into<String>,
        state: QueueEntryState,
    ) -> Self {
        Self {
            id: id.into(),
            target: target.into(),
            head: head.into(),
            base: base.into(),
            key: None,
            state,
            position: None,
            candidate: None,
            tree: None,
            verdict: None,
            landed: None,
            removal: None,
            check: None,
            report: None,
        }
    }
}

// ---- enqueue ----

/// `queue_enqueue`: put a change in a target's queue. The answer comes once the entry is
/// durable. A store before `FORMAT 3` is `E_FORMAT_REQUIRED`; a change the target holds already
/// is `E_NOTHING_TO_LAND`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueEnqueueReq {
    /// The ref to land on, which must exist.
    pub target: String,
    /// The change's head.
    pub head: Rev,
    /// Where the change starts: a first-parent ancestor of `head`. When omitted, the merge base
    /// of `head` and the target's head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Rev>,
    /// Makes a retry safe: at most 128 bytes. The same key with the same target, head and base
    /// answers the entry it made; with another, `E_INVALID_ARG` (`details.reason`
    /// `key_reused`). When omitted, each enqueue makes a new entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl QueueEnqueueReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(target: impl Into<String>, head: impl Into<Rev>) -> Self {
        Self {
            target: target.into(),
            head: head.into(),
            base: None,
            key: None,
        }
    }
}

/// `queue_enqueue`: the entry, and whether an earlier enqueue with the same key made it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueEnqueueResp {
    /// The entry.
    pub entry: QueueEntryOut,
    /// True when the key named an entry already, and nothing was written.
    pub replayed: bool,
}

impl QueueEnqueueResp {
    /// Takes the required fields.
    pub fn new(entry: QueueEntryOut, replayed: bool) -> Self {
        Self { entry, replayed }
    }
}

// ---- status, withdraw, wait ----

/// `queue_status`: the entries of one target, or of every target, or one entry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QueueStatusReq {
    /// Only this target's entries; every target's when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Only this entry, by its id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

impl QueueStatusReq {
    /// Every target's entries.
    pub fn new() -> Self {
        Self::default()
    }
}

/// `queue_status`: the entries, open ones in enqueue order first.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueStatusResp {
    /// The entries.
    pub entries: Vec<QueueEntryOut>,
}

impl QueueStatusResp {
    /// Takes the required fields.
    pub fn new(entries: Vec<QueueEntryOut>) -> Self {
        Self { entries }
    }
}

/// `queue_withdraw`: take an open entry out of its queue (reason `withdrawn`). One that landed
/// is `E_INVALID_ARG` (`details.reason` `already_landed`), and nothing is written. The answer
/// is the entry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueWithdrawReq {
    /// The entry's id.
    pub entry: String,
}

impl QueueWithdrawReq {
    /// Takes the required fields.
    pub fn new(entry: impl Into<String>) -> Self {
        Self {
            entry: entry.into(),
        }
    }
}

/// `queue_wait`: wait for an entry to land. The answer is the landed entry; a removed one is
/// an error naming its removal, and one still open at the timeout is `E_QUEUE_WAIT_TIMEOUT`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueWaitReq {
    /// The entry's id.
    pub entry: String,
    /// The most seconds to wait; without limit when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

impl QueueWaitReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(entry: impl Into<String>) -> Self {
        Self {
            entry: entry.into(),
            timeout_s: None,
        }
    }
}

// ---- the runner's round ----

/// A runner's round: build the target's open entries into a chain on its head, and record the
/// candidates it will check. With `probes` empty it records only the entries the chain removes
/// (a conflict) or lands (a change on the target already), so that the runner can choose its
/// probes from the answer.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueRoundReq {
    /// The target ref.
    pub target: String,
    /// Build only while the target names this changeset; the answer names the head it found,
    /// and nothing is recorded, when it names another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_head: Option<String>,
    /// The prefixes to record as candidates, each by the 1-based place of its last entry in the
    /// chain: the chain's changesets and one candidate fact for each, in one frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<u32>,
}

impl QueueRoundReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            expect_head: None,
            probes: Vec::new(),
        }
    }
}

/// One link of a round's chain: an open entry and the prefix that ends with it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueRoundItem {
    /// The entry's id.
    pub entry: String,
    /// The changeset of the prefix ending with it, built on the round's head.
    pub candidate: String,
    /// That changeset's root tree.
    pub tree: String,
    /// The verdict the store holds for `tree`, from any round; omitted when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictState>,
    /// How many cancelled verdicts the entry has.
    #[serde(default)]
    pub cancelled: u32,
}

impl QueueRoundItem {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        entry: impl Into<String>,
        candidate: impl Into<String>,
        tree: impl Into<String>,
    ) -> Self {
        Self {
            entry: entry.into(),
            candidate: candidate.into(),
            tree: tree.into(),
            verdict: None,
            cancelled: 0,
        }
    }
}

/// A runner's round: the chain on the target's head, in enqueue order.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueRoundResp {
    /// The target ref.
    pub target: String,
    /// The head the chain is built on; omitted when the target is gone, which removed every
    /// open entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The chain, one link per open entry it holds.
    pub items: Vec<QueueRoundItem>,
    /// The entries this round recorded as landed: the target held their change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub landed: Vec<String>,
    /// The entries this round removed: a conflict, or the target gone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

impl QueueRoundResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(target: impl Into<String>, items: Vec<QueueRoundItem>) -> Self {
        Self {
            target: target.into(),
            head: None,
            items,
            landed: Vec::new(),
            removed: Vec::new(),
        }
    }
}

/// A runner's verdict: what checking a candidate tree came to, recorded on the entry that ends
/// the prefix as soon as the check ends.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueVerdictReq {
    /// The target ref.
    pub target: String,
    /// The entry that ends the prefix.
    pub entry: String,
    /// The candidate's root tree.
    pub tree: String,
    /// What the check came to.
    pub state: VerdictState,
    /// The check, as its command line.
    pub check: String,
    /// The tail of the check's output, which the store keeps as a blob.
    #[serde(default)]
    pub output: String,
}

impl QueueVerdictReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(
        target: impl Into<String>,
        entry: impl Into<String>,
        tree: impl Into<String>,
        state: VerdictState,
        check: impl Into<String>,
    ) -> Self {
        Self {
            target: target.into(),
            entry: entry.into(),
            tree: tree.into(),
            state,
            check: check.into(),
            output: String::new(),
        }
    }
}

/// One entry a publish removes: a prefix's red verdict blamed on it.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueRemovalIn {
    /// The entry's id.
    pub entry: String,
    /// Why.
    pub reason: RemovalReason,
    /// The check whose verdict blames it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// The candidate tree that verdict is of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
}

impl QueueRemovalIn {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(entry: impl Into<String>, reason: RemovalReason) -> Self {
        Self {
            entry: entry.into(),
            reason,
            check: None,
            tree: None,
        }
    }
}

/// A runner's publish: move the target from the round's head to a checked prefix's changeset
/// and record its entries landed, and record the removals the verdicts decided, all in one
/// frame or nothing.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueuePublishReq {
    /// The target ref.
    pub target: String,
    /// The head the chain was built on: the target moves only from it.
    pub head: String,
    /// The changeset to move the target to, whose root tree has a passed verdict; omitted when
    /// the publish only removes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
    /// The entries `tip` lands, in enqueue order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub landed: Vec<String>,
    /// The entries to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<QueueRemovalIn>,
}

impl QueuePublishReq {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(target: impl Into<String>, head: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            head: head.into(),
            tip: None,
            landed: Vec::new(),
            removed: Vec::new(),
        }
    }
}

/// What a publish came to.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueuePublishStatus {
    /// The frame is recorded: the target moved, and each entry landed or was removed.
    Published,
    /// The target no longer names the round's head, or an entry changed since the round
    /// (withdrawn): nothing was recorded, and the next round builds again.
    Moved,
}

/// A runner's publish: whether it was recorded, and where the target stands.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueuePublishResp {
    /// What the publish came to.
    pub status: QueuePublishStatus,
    /// What the target names now; omitted when it is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The operation that recorded the frame: its number in `op_log`; omitted when nothing was
    /// recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<u64>,
}

impl QueuePublishResp {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(status: QueuePublishStatus) -> Self {
        Self {
            status,
            head: None,
            op: None,
        }
    }
}
