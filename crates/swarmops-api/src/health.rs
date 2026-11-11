//! The store's health (the M21 design §2.2, §2.3): the record each operation leaves and the
//! read that compares two windows of them.
//!
//! ```text
//!   health/op-records.jsonl ─┐   one OpRecord line per call that took the lock or failed
//!   git/serve-records.jsonl ─┴─▶ health: two windows side by side
//!
//!   ──── baseline (3,600 s) ────┼── recent (300 s) ──┤ at
//!   each figure: both windows' Stat, and its Drift (steady, departed, unknown)
//! ```
//!
//! Diagnostics, never acknowledged state: no path, no environment value, no content byte.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// The recent window a [`HealthReq`] that names none takes: 300 seconds.
pub const HEALTH_RECENT_S: u32 = 300;

/// The baseline window a [`HealthReq`] that names none takes: 3,600 seconds.
pub const HEALTH_BASELINE_S: u32 = 3_600;

/// The factor a [`HealthReq`] that names none takes, in thousandths: 2.0×.
pub const HEALTH_FACTOR_MILLI: u32 = 2_000;

/// The least factor a [`HealthReq`] takes, in thousandths: 1.1×.
pub const MIN_HEALTH_FACTOR_MILLI: u32 = 1_100;

/// The most factor a [`HealthReq`] takes, in thousandths: 100×.
pub const MAX_HEALTH_FACTOR_MILLI: u32 = 100_000;

/// The samples each window needs before a p50, a share or a rate is judged: 20.
pub const DRIFT_MIN_SAMPLES: u64 = 20;

/// The samples each window needs before a p99 is judged: 100.
pub const DRIFT_MIN_SAMPLES_P99: u64 = 100;

/// The land p99 past which [`AlertRule::LandP99`] is raised, in milliseconds: G4's 500.
pub const ALERT_LAND_P99_MS: u64 = 500;

/// The share of lands that retried past which [`AlertRule::RetryShare`] is raised, in
/// thousandths: 5 %.
pub const ALERT_RETRY_SHARE_MILLI: u64 = 50;

/// The most operation ids an [`OpRecord`] holds: 8.
pub const MAX_RECORD_OPS: usize = 8;

/// One call's record: one JSON line of the store's `health/op-records.jsonl`. Written for
/// every call that took the store's lock and every call that failed; a successful pure read
/// leaves none.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpRecord {
    /// When the call started, in Unix milliseconds.
    pub at: u64,
    /// The envelope's `op`.
    pub op: String,
    /// The call's request id.
    pub request_id: String,
    /// The caller, when the call resolved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// The run the call belongs to, when one was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The host's name, when it reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The microseconds from the start to just before the envelope was written.
    #[serde(default)]
    pub elapsed_us: u64,
    /// The microseconds spent waiting for the store's lock.
    #[serde(default)]
    pub lock_wait_us: u64,
    /// The microseconds the store's lock was held.
    #[serde(default)]
    pub lock_hold_us: u64,
    /// The journal syncs.
    #[serde(default)]
    pub journal_syncs: u64,
    /// The microseconds the journal syncs took, together.
    #[serde(default)]
    pub journal_sync_us: u64,
    /// The objects read.
    #[serde(default)]
    pub object_reads: u64,
    /// The objects written.
    #[serde(default)]
    pub object_writes: u64,
    /// A land's attempts, from its response; absent for other operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// The call's exit code.
    #[serde(default)]
    pub exit: u8,
    /// The error's code, when the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
    /// The operation ids the call wrote, at most [`MAX_RECORD_OPS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
    /// The command's canonical form, at most 1,024 bytes: argument names and identifiers,
    /// paths as `<path>`, contents as `<length>:<16 hex digits of BLAKE3>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
}

impl OpRecord {
    /// The record of `op` started at `at` under `request_id`, with no count, exit or code yet.
    pub fn new(at: u64, op: String, request_id: String) -> OpRecord {
        OpRecord {
            at,
            op,
            request_id,
            identity: None,
            run_id: None,
            host: None,
            elapsed_us: 0,
            lock_wait_us: 0,
            lock_hold_us: 0,
            journal_syncs: 0,
            journal_sync_us: 0,
            object_reads: 0,
            object_writes: 0,
            attempts: None,
            exit: 0,
            code: None,
            ops: Vec::new(),
            args: None,
        }
    }
}

/// `health`: two windows of the store's records side by side. The recent window is
/// `[at − recent_s, at)`, the baseline `[at − recent_s − baseline_s, at − recent_s)`; they do
/// not overlap. A figure departs when its ratio, recent over baseline, passes `factor_milli`
/// either way.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HealthReq {
    /// The recent window, in seconds: 300 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_s: Option<u32>,
    /// The baseline window, in seconds, longer than the recent one: 3,600 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_s: Option<u32>,
    /// The factor, in thousandths, from 1,100 to 100,000: 2,000 (2.0×) by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factor_milli: Option<u32>,
    /// Now, in Unix milliseconds; the clock's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    /// When given, `total` adds up the records from this Unix millisecond to `at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Splits each figure by the caller's identity.
    #[serde(default)]
    pub by_identity: bool,
}

impl HealthReq {
    /// The request of every default: 300 s against 3,600 s, 2.0×, now, no total, no split.
    pub fn new() -> HealthReq {
        HealthReq::default()
    }
}

/// `health`'s answer.
///
/// Figure names (added to, never renamed): `op.<op>.latency`, `op.<op>.count`, `code.<E_*>`
/// (a share of the operations), `land.retries` (the share of lands that retried),
/// `lock.wait`, `lock.hold`, `journal.sync` (each record's mean sync), `objects.read`,
/// `objects.written`, `serve.<clone|fetch|push>.<prepare|send>`,
/// `serve.<clone|fetch|push>.bytes`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HealthResp {
    /// Now, in Unix milliseconds.
    pub at: u64,
    /// The recent window, in seconds.
    pub recent_s: u32,
    /// The baseline window, in seconds.
    pub baseline_s: u32,
    /// The factor, in thousandths.
    pub factor_milli: u32,
    /// What the records cover of each window.
    pub coverage: Coverage,
    /// Each figure in both windows, by name (then identity).
    pub figures: Vec<Figure>,
    /// The fixed limits the recent window passed, apart from any drift.
    pub alerts: Vec<Alert>,
    /// Single values.
    pub gauges: Vec<Gauge>,
    /// From `since` to `at`, when `since` was given: each figure's `recent` is that window,
    /// its `baseline` empty and its `drift` `not_compared`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<Vec<Figure>>,
}

impl HealthResp {
    /// The answer at `at` for these windows and factor, with nothing covered and no figure.
    pub fn new(at: u64, recent_s: u32, baseline_s: u32, factor_milli: u32) -> HealthResp {
        HealthResp {
            at,
            recent_s,
            baseline_s,
            factor_milli,
            coverage: Coverage::default(),
            figures: Vec::new(),
            alerts: Vec::new(),
            gauges: Vec::new(),
            total: None,
        }
    }
}

/// What the records cover: the seconds of each window after the oldest record still kept
/// (rotation drops older ones), the records in either window and the lines that did not read.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Coverage {
    /// The seconds of the recent window the operation records cover.
    pub recent_s: u32,
    /// The seconds of the baseline window the operation records cover.
    pub baseline_s: u32,
    /// The seconds of the recent window the serve records cover.
    #[serde(default)]
    pub serve_recent_s: u32,
    /// The seconds of the baseline window the serve records cover.
    #[serde(default)]
    pub serve_baseline_s: u32,
    /// The operation records in either window.
    pub records: u64,
    /// The serve records in either window.
    #[serde(default)]
    pub serve_records: u64,
    /// The lines of either file that did not read: a torn last line, or one from elsewhere.
    pub skipped: u64,
}

impl Coverage {
    /// Nothing covered.
    pub fn new() -> Coverage {
        Coverage::default()
    }
}

/// One figure in both windows.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Figure {
    /// The figure's name.
    pub name: String,
    /// The identity it is split by, with `by_identity`; absent for a record without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// What its values count.
    pub unit: Unit,
    /// The recent window.
    pub recent: Stat,
    /// The baseline window.
    pub baseline: Stat,
    /// How the recent window compares with the baseline.
    pub drift: Drift,
}

impl Figure {
    /// Takes every field but the identity.
    pub fn new(name: String, unit: Unit, recent: Stat, baseline: Stat, drift: Drift) -> Figure {
        Figure {
            name,
            identity: None,
            unit,
            recent,
            baseline,
            drift,
        }
    }
}

/// What a figure's values count.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unit {
    /// Microseconds: `count` samples, their `sum` and percentiles.
    Micros,
    /// Events: `count` records, `sum` events, compared per second of covered time.
    Count,
    /// A share: `sum` of `count` (the operations, or the lands) had it.
    Share,
    /// Bytes: as `count`, in bytes.
    Bytes,
}

/// One window of a figure.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stat {
    /// The samples: records, or a share's denominator.
    pub count: u64,
    /// The median, when there is a sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<u64>,
    /// The 99th percentile, when there is a sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99: Option<u64>,
    /// The values added up: a share's numerator.
    pub sum: u64,
}

impl Stat {
    /// `count` samples adding up to `sum`, without percentiles.
    pub fn new(count: u64, sum: u64) -> Stat {
        Stat {
            count,
            p50: None,
            p99: None,
            sum,
        }
    }
}

/// How a figure's recent window compares with its baseline.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Drift {
    /// Within the factor either way.
    Steady,
    /// Past the factor: `ratio_milli` is recent over baseline, in thousandths (`u32::MAX`
    /// for something the baseline never had).
    Departed {
        /// What was compared.
        of: Measure,
        /// Recent over baseline, in thousandths.
        ratio_milli: u32,
        /// Which way.
        direction: Direction,
    },
    /// Not judged: an empty window is never read as no change.
    Unknown {
        /// Why.
        reason: Unknown,
    },
}

/// What a departure compared.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Measure {
    /// The medians.
    P50,
    /// The 99th percentiles.
    P99,
    /// The rates: per second of covered time for counts and bytes, of the denominator for
    /// shares.
    Rate,
}

/// Which way a figure departed.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Direction {
    /// The recent window is higher.
    Up,
    /// The recent window is lower.
    Down,
}

/// Why a figure, or a gauge, is not judged.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unknown {
    /// A window holds fewer samples than the measure needs.
    FewSamples,
    /// The records cover less than half the baseline window.
    BaselineShort,
    /// No record falls in the baseline window.
    BaselineEmpty,
    /// A `total` figure: one window, nothing to compare.
    NotCompared,
}

/// A fixed limit the recent window passed (A60): raised whatever the drift says.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Alert {
    /// The limit's rule.
    pub rule: AlertRule,
    /// The figure it reads.
    pub figure: String,
    /// The figure's value, in the rule's unit.
    pub value: u64,
    /// The limit `value` passed: an alert is `value > limit`.
    pub limit: u64,
}

impl Alert {
    /// Takes every field.
    pub fn new(rule: AlertRule, figure: String, value: u64, limit: u64) -> Alert {
        Alert {
            rule,
            figure,
            value,
            limit,
        }
    }
}

/// A fixed limit.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlertRule {
    /// A land operation's p99 past 500 ms (G4); in microseconds.
    LandP99,
    /// The share of lands that retried past 5 %; in thousandths.
    RetryShare,
    /// Any `E_BUSY`; a count.
    Busy,
}

/// A single value: `records.recent_s`, `records.baseline_s`, `maintain.age_s` (seconds since
/// the last `maintain` that succeeded), or `maintain.last` with no value when no record names
/// one (never read as a young run).
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Gauge {
    /// The gauge's name.
    pub name: String,
    /// Its value; absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<u64>,
}

impl Gauge {
    /// Takes every field.
    pub fn new(name: String, value: Option<u64>) -> Gauge {
        Gauge { name, value }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{HealthReq, OpRecord};
    use crate::error::ErrorCode;

    /// The property names of `T`'s published schema.
    fn properties<T: schemars::JsonSchema>() -> BTreeSet<String> {
        let schema = schemars::schema_for!(T).to_value();
        let names = schema["properties"].as_object().map(|p| p.keys().cloned());
        names.into_iter().flatten().collect()
    }

    /// An `OpRecord` with every field set writes each of them, and the schema names exactly
    /// those.
    #[test]
    fn every_record_field_is_in_the_schema() {
        // 1. Every field set.
        let mut record = OpRecord::new(1, "land".to_owned(), "r-1".to_owned());
        record.identity = Some("agent/a".to_owned());
        record.run_id = Some("run".to_owned());
        record.host = Some("h".to_owned());
        record.elapsed_us = 1;
        record.lock_wait_us = 1;
        record.lock_hold_us = 1;
        record.journal_syncs = 1;
        record.journal_sync_us = 1;
        record.object_reads = 1;
        record.object_writes = 1;
        record.attempts = Some(2);
        record.exit = 4;
        record.code = Some(ErrorCode::Busy);
        record.ops = vec!["7:abcd".to_owned()];
        record.args = Some("land --onto main".to_owned());

        // 2. Written, its keys are the schema's properties, and it reads back.
        let Value::Object(written) = serde_json::to_value(&record).unwrap() else {
            panic!("an OpRecord is written as an object");
        };
        let keys: BTreeSet<String> = written.keys().cloned().collect();
        assert_eq!(keys, properties::<OpRecord>(), "fields against the schema");
        let read: OpRecord = serde_json::from_value(Value::Object(written)).unwrap();
        assert_eq!(read, record, "read back");
    }

    /// The empty object reads as the request of every default.
    #[test]
    fn the_empty_request_is_every_default() {
        let read: HealthReq = serde_json::from_str("{}").unwrap();
        assert_eq!(read, HealthReq::new(), "the empty object");
    }
}
