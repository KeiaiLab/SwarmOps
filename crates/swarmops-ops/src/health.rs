//! The store's health (the M21 design §2.2, §2.3): the operation records' file, and the read
//! that compares two windows of it and of the serve records.
//!
//! ```text
//!   op-records, serve-records ──lines──▶ Tally ─┬─ per figure and identity: recent,
//!   (a line that does not read: skipped)        │  baseline, total
//!                                               ├─ per file: its oldest record, its
//!                                               │  records in each window
//!                                               ├─ the recent window's alert inputs
//!                                               └─ the last maintain that succeeded
//!                                     ──finish──▶ HealthResp
//! ```
//!
//! Streaming: memory grows with the figures and identities, never with the records. The
//! records are diagnostics, never acknowledged state: `health` writes nothing and takes no
//! lock.

mod histogram;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use swarmops_api::health::{
    ALERT_LAND_P99_MS, ALERT_RETRY_SHARE_MILLI, Alert, AlertRule, Coverage, DRIFT_MIN_SAMPLES,
    DRIFT_MIN_SAMPLES_P99, Direction, Drift, Figure, Gauge, HEALTH_BASELINE_S, HEALTH_FACTOR_MILLI,
    HEALTH_RECENT_S, HealthReq, HealthResp, MAX_HEALTH_FACTOR_MILLI, MIN_HEALTH_FACTOR_MILLI,
    Measure, OpRecord, Stat, Unit, Unknown,
};
use swarmops_api::{ErrorCode, OpError, ServeRecord, ServeService};

use crate::records::{self, Files};
use histogram::Histogram;

/// The most bytes the operation records' file holds before it is renamed to the one older
/// file: 64 MiB.
pub const MAX_OP_RECORDS_BYTES: u64 = 64 << 20;

/// The operation records' directory, below the store's root.
const HEALTH_DIR: &str = "health";

/// The operation records' files, in [`HEALTH_DIR`].
const FILES: Files = Files {
    current: "op-records.jsonl",
    older: "op-records.1.jsonl",
    lock: "op-records.lock",
};

/// Milliseconds in a second, and microseconds in a millisecond.
const MS_PER_S: u64 = 1_000;
const US_PER_MS: u64 = 1_000;

/// Thousandths in one: the unit of a ratio.
const MILLI: u64 = 1_000;

/// The percentiles a [`Stat`] gives.
const P50: u64 = 50;
const P99: u64 = 99;

/// The operation whose last success `maintain.age_s` counts from.
const MAINTAIN_OP: &str = "maintain";

/// The `E_BUSY` refusals the recent window may hold before [`AlertRule::Busy`]: none.
const ALERT_BUSY_LIMIT: u64 = 0;

/// Appends `record` to the operation records of the store at `root`, in `health/`, which it
/// makes one level below `root` and never a root of its own. Unsynced: diagnostics.
pub fn append(root: &Utf8Path, record: &OpRecord) -> io::Result<()> {
    append_within(root, record, MAX_OP_RECORDS_BYTES)
}

/// Reads the operation and serve records of the store at `root` and compares their windows:
/// [`compute`] over the files, older first.
pub fn read(root: &Utf8Path, req: &HealthReq) -> Result<HealthResp, OpError> {
    let root = root.as_std_path();
    let ops = records::lines(&root.join(HEALTH_DIR), &FILES);
    let served = records::lines(&root.join(records::SERVE_DIR), &records::SERVE);
    compute(ops, served, req)
}

/// Compares the recent window of `records` (operation record lines) and `serve_records`
/// (serve record lines) with the baseline before it, streaming both. A line that does not
/// read is skipped and counted; a read that fails ends its source, counted the same way.
/// `req.at` absent is the clock's now. A bad combination of windows, factor and `since` is
/// `E_INVALID_ARG`.
pub fn compute<R, S>(records: R, serve_records: S, req: &HealthReq) -> Result<HealthResp, OpError>
where
    R: IntoIterator<Item = io::Result<String>>,
    S: IntoIterator<Item = io::Result<String>>,
{
    // 1. The windows the request names.
    let windows = Windows::of(req)?;
    let split = if req.by_identity {
        Split::ByIdentity
    } else {
        Split::Whole
    };
    let mut tally = Tally::new(&windows, split);

    // 2. Both files, line by line.
    let mut skipped = 0;
    for record in parsed::<OpRecord>(records, &mut skipped) {
        tally.op(&record);
    }
    for record in parsed::<ServeRecord>(serve_records, &mut skipped) {
        tally.serve(&record);
    }
    tally.skipped += skipped;

    // 3. The figures, their drift, the alerts and the gauges.
    Ok(tally.finish())
}

/// [`append`] with `bound` for the rotation.
fn append_within(root: &Utf8Path, record: &OpRecord, bound: u64) -> io::Result<()> {
    let dir = root.as_std_path().join(HEALTH_DIR);
    if let Err(e) = fs::create_dir(&dir)
        && e.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(e);
    }
    records::append(&dir, &FILES, record, bound)
}

/// The records of `lines` that read as `T`, each other line counted in `skipped`; a read
/// error other than a line that is not UTF-8 ends them.
fn parsed<T: serde::de::DeserializeOwned>(
    lines: impl IntoIterator<Item = io::Result<String>>,
    skipped: &mut u64,
) -> impl Iterator<Item = T> {
    let mut ended = false;
    let read = lines.into_iter().map_while(move |line| {
        // 1. A failed read ends the source; a line that is not UTF-8 is one skipped.
        if ended {
            return None;
        }
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                *skipped += 1;
                ended = e.kind() != io::ErrorKind::InvalidData;
                return Some(None);
            }
        };

        // 2. A torn or foreign line is one skipped.
        let record = serde_json::from_str::<T>(&line).ok();
        *skipped += u64::from(record.is_none());
        Some(record)
    });
    read.flatten()
}

/// Whether figures split by the caller's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Split {
    /// One figure per name.
    Whole,
    /// One figure per name and identity.
    ByIdentity,
}

/// The window a record falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// `[at − recent, at)`.
    Recent,
    /// `[at − recent − baseline, at − recent)`.
    Baseline,
}

/// The file a record comes from: each has its own oldest record, so its own coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// `health/op-records.jsonl`.
    Ops,
    /// `git/serve-records.jsonl`.
    Serve,
}

/// What a figure's share is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Base {
    /// Its own count: samples, records, lands.
    Own,
    /// The operation records of its identity in the window (a code's share).
    Operations,
}

/// One request's windows, in Unix milliseconds.
#[derive(Debug)]
struct Windows {
    at: u64,
    recent_s: u32,
    baseline_s: u32,
    factor_milli: u32,
    recent_start: u64,
    baseline_start: u64,
    since: Option<u64>,
}

impl Windows {
    /// The windows of `req`, refused when they cannot be compared.
    fn of(req: &HealthReq) -> Result<Windows, OpError> {
        // 1. The defaults for what is not given.
        let recent_s = req.recent_s.unwrap_or(HEALTH_RECENT_S);
        let baseline_s = req.baseline_s.unwrap_or(HEALTH_BASELINE_S);
        let factor_milli = req.factor_milli.unwrap_or(HEALTH_FACTOR_MILLI);
        let at = req.at.unwrap_or_else(now_ms);

        // 2. Each refused with what it should be.
        let invalid = |message: &str| Err(OpError::new(ErrorCode::InvalidArg, message));
        if recent_s == 0 {
            return invalid("health: recent_s is at least 1");
        }
        if baseline_s <= recent_s {
            return invalid("health: baseline_s is longer than recent_s");
        }
        if !(MIN_HEALTH_FACTOR_MILLI..=MAX_HEALTH_FACTOR_MILLI).contains(&factor_milli) {
            return invalid("health: factor_milli is from 1,100 to 100,000");
        }
        if req.since.is_some_and(|since| since > at) {
            return invalid("health: since is not after at");
        }

        // 3. The windows' starts, back from `at`.
        let recent_start = at.saturating_sub(u64::from(recent_s) * MS_PER_S);
        let baseline_start = recent_start.saturating_sub(u64::from(baseline_s) * MS_PER_S);
        Ok(Windows {
            at,
            recent_s,
            baseline_s,
            factor_milli,
            recent_start,
            baseline_start,
            since: req.since,
        })
    }

    /// The window a record started at `at` falls in.
    fn window(&self, at: u64) -> Option<Window> {
        if (self.recent_start..self.at).contains(&at) {
            return Some(Window::Recent);
        }
        if (self.baseline_start..self.recent_start).contains(&at) {
            return Some(Window::Baseline);
        }
        None
    }

    /// Whether a record started at `at` falls in the total from `since`.
    fn in_total(&self, at: u64) -> bool {
        self.since
            .is_some_and(|since| (since..self.at).contains(&at))
    }

    /// The milliseconds of `window` after the oldest record `oldest` of a file.
    fn covered_ms(&self, oldest: Option<u64>, window: Window) -> u64 {
        let (start, end) = match window {
            Window::Recent => (self.recent_start, self.at),
            Window::Baseline => (self.baseline_start, self.recent_start),
        };
        oldest.map_or(0, |oldest| end.saturating_sub(start.max(oldest)))
    }
}

/// One figure's values in one window.
#[derive(Debug, Default)]
struct Agg {
    /// The adds: whether the window has the figure at all.
    hits: u64,
    count: u64,
    sum: u64,
    histogram: Histogram,
}

impl Agg {
    /// One sample of `value`, with its percentiles.
    fn sample(&mut self, value: u64) {
        self.hits += 1;
        self.count += 1;
        self.sum = self.sum.saturating_add(value);
        self.histogram.add(value);
    }

    /// One event, counted and summed, without percentiles.
    fn event(&mut self) {
        self.hits += 1;
        self.count += 1;
        self.sum += 1;
    }

    /// One more of a share's numerator; its denominator comes at the end.
    fn share(&mut self) {
        self.hits += 1;
        self.sum += 1;
    }

    /// One land that retried `retries` times: counted, summed when it retried at all, its
    /// retries in the percentiles.
    fn land(&mut self, retries: u64) {
        self.hits += 1;
        self.count += 1;
        self.sum += u64::from(retries > 0);
        self.histogram.add(retries);
    }

    /// The window's stat, its count the share's denominator `of` when it has one.
    fn stat(&self, of: Option<u64>) -> Stat {
        let mut stat = Stat::new(of.unwrap_or(self.count), self.sum);
        stat.p50 = self.histogram.percentile(P50);
        stat.p99 = self.histogram.percentile(P99);
        stat
    }
}

/// One figure of one identity: its unit, its file and its three windows.
#[derive(Debug)]
struct Acc {
    unit: Unit,
    source: Source,
    base: Base,
    recent: Agg,
    baseline: Agg,
    total: Agg,
}

/// Where one record counts: its window, the total, its file, its identity.
#[derive(Debug, Clone)]
struct Slots {
    window: Option<Window>,
    total: bool,
    source: Source,
    group: Option<String>,
}

/// A file's records: its oldest, and how many fell in each window.
#[derive(Debug, Default)]
struct Seen {
    oldest: Option<u64>,
    recent: u64,
    baseline: u64,
}

/// The operation records of one identity in each window: a code's share is of them.
#[derive(Debug, Default)]
struct Operations {
    recent: u64,
    baseline: u64,
    total: u64,
}

/// The recent window's alert inputs, never split by identity.
#[derive(Debug, Default)]
struct AlertInputs {
    /// Each land operation's latencies, in µs.
    land: BTreeMap<String, Histogram>,
    lands: u64,
    retried: u64,
    busy: u64,
}

/// The records read so far.
struct Tally<'w> {
    windows: &'w Windows,
    split: Split,
    figures: BTreeMap<(String, Option<String>), Acc>,
    operations: BTreeMap<Option<String>, Operations>,
    ops_seen: Seen,
    serve_seen: Seen,
    alerts: AlertInputs,
    maintained: Option<u64>,
    skipped: u64,
}

impl<'w> Tally<'w> {
    /// Nothing read yet.
    fn new(windows: &'w Windows, split: Split) -> Tally<'w> {
        Tally {
            windows,
            split,
            figures: BTreeMap::new(),
            operations: BTreeMap::new(),
            ops_seen: Seen::default(),
            serve_seen: Seen::default(),
            alerts: AlertInputs::default(),
            maintained: None,
            skipped: 0,
        }
    }

    /// Counts one operation record.
    fn op(&mut self, r: &OpRecord) {
        // 1. The last maintain that succeeded, whatever the windows; then where it counts.
        if r.op == MAINTAIN_OP && r.exit == 0 && r.at <= self.windows.at {
            self.maintained = self.maintained.max(Some(r.at));
        }
        let Some(slots) = self.slots(Source::Ops, r.at, r.identity.as_deref()) else {
            return;
        };

        // 2. The identity's operations: a code's share is of them.
        let operations = self.operations.entry(slots.group.clone()).or_default();
        match slots.window {
            Some(Window::Recent) => operations.recent += 1,
            Some(Window::Baseline) => operations.baseline += 1,
            None => {}
        }
        operations.total += u64::from(slots.total);

        // 3. The operation's latency and count, its code.
        let op = &r.op;
        self.add(&format!("op.{op}.latency"), Unit::Micros, &slots, |a| {
            a.sample(r.elapsed_us);
        });
        self.add(&format!("op.{op}.count"), Unit::Count, &slots, Agg::event);
        if let Some(code) = r.code {
            let code = format!("code.{}", code.as_str());
            self.add_share(&code, &slots);
        }

        // 4. A land's retries: the share of lands that retried, the retries' percentiles.
        if let Some(attempts) = r.attempts {
            let retries = u64::from(attempts.saturating_sub(1));
            self.add("land.retries", Unit::Share, &slots, |a| a.land(retries));
        }

        // 5. The lock, the journal's syncs and the objects.
        if r.lock_wait_us > 0 || r.lock_hold_us > 0 {
            self.add("lock.wait", Unit::Micros, &slots, |a| {
                a.sample(r.lock_wait_us);
            });
        }
        if r.lock_hold_us > 0 {
            self.add("lock.hold", Unit::Micros, &slots, |a| {
                a.sample(r.lock_hold_us);
            });
        }
        if let Some(mean) = r.journal_sync_us.checked_div(r.journal_syncs) {
            self.add("journal.sync", Unit::Micros, &slots, |a| a.sample(mean));
        }
        self.add("objects.read", Unit::Count, &slots, |a| {
            a.sample(r.object_reads);
        });
        self.add("objects.written", Unit::Count, &slots, |a| {
            a.sample(r.object_writes);
        });

        // 6. The recent window's alert inputs, whole.
        if slots.window == Some(Window::Recent) {
            self.alert_inputs(r);
        }
    }

    /// Counts one serve record: its two times and its bytes, by service.
    fn serve(&mut self, r: &ServeRecord) {
        // 1. Where it counts, and its service's name.
        let Some(slots) = self.slots(Source::Serve, r.at, Some(&r.identity)) else {
            return;
        };
        let Some(service) = service_name(r.service) else {
            self.skipped += 1;
            return;
        };

        // 2. Its prepare and send times, in µs, and its bytes.
        let prepare = r.prepare_ms.saturating_mul(US_PER_MS);
        let send = r.send_ms.saturating_mul(US_PER_MS);
        let name = |part: &str| format!("serve.{service}.{part}");
        self.add(&name("prepare"), Unit::Micros, &slots, |a| {
            a.sample(prepare);
        });
        self.add(&name("send"), Unit::Micros, &slots, |a| a.sample(send));
        self.add(&name("bytes"), Unit::Bytes, &slots, |a| a.sample(r.bytes));
    }

    /// Notes a record of `source` started at `at` by `identity` (its file's oldest, its
    /// window's count); where it counts, none when nowhere.
    fn slots(&mut self, source: Source, at: u64, identity: Option<&str>) -> Option<Slots> {
        // 1. The file's oldest record and its windows' counts.
        let windows = self.windows;
        let seen = match source {
            Source::Ops => &mut self.ops_seen,
            Source::Serve => &mut self.serve_seen,
        };
        seen.oldest = Some(seen.oldest.map_or(at, |oldest| oldest.min(at)));
        let window = windows.window(at);
        match window {
            Some(Window::Recent) => seen.recent += 1,
            Some(Window::Baseline) => seen.baseline += 1,
            None => {}
        }

        // 2. Where it counts, under which identity.
        let total = windows.in_total(at);
        if window.is_none() && !total {
            return None;
        }
        let group = match self.split {
            Split::Whole => None,
            Split::ByIdentity => identity.map(str::to_owned),
        };
        Some(Slots {
            window,
            total,
            source,
            group,
        })
    }

    /// Applies `f` to the figure `name`'s aggregates where a record counts.
    fn add(&mut self, name: &str, unit: Unit, slots: &Slots, f: impl Fn(&mut Agg)) {
        self.add_to(name, unit, Base::Own, slots, f);
    }

    /// One more of the share `name` of the operations, where a record counts.
    fn add_share(&mut self, name: &str, slots: &Slots) {
        self.add_to(name, Unit::Share, Base::Operations, slots, Agg::share);
    }

    /// Applies `f` to the aggregates of the figure `name`, of `base`, where a record counts.
    fn add_to(&mut self, name: &str, unit: Unit, base: Base, slots: &Slots, f: impl Fn(&mut Agg)) {
        let key = (name.to_owned(), slots.group.clone());
        let acc = self.figures.entry(key).or_insert_with(|| Acc {
            unit,
            source: slots.source,
            base,
            recent: Agg::default(),
            baseline: Agg::default(),
            total: Agg::default(),
        });
        match slots.window {
            Some(Window::Recent) => f(&mut acc.recent),
            Some(Window::Baseline) => f(&mut acc.baseline),
            None => {}
        }
        if slots.total {
            f(&mut acc.total);
        }
    }

    /// Notes a recent record for the fixed alerts.
    fn alert_inputs(&mut self, r: &OpRecord) {
        if let Some(attempts) = r.attempts {
            let land = self.alerts.land.entry(r.op.clone()).or_default();
            land.add(r.elapsed_us);
            self.alerts.lands += 1;
            self.alerts.retried += u64::from(attempts > 1);
        }
        if r.code == Some(ErrorCode::Busy) {
            self.alerts.busy += 1;
        }
    }

    /// The answer: coverage, figures with their drift, alerts and gauges.
    fn finish(self) -> HealthResp {
        // 1. The coverage of each file.
        let w = self.windows;
        let mut resp = HealthResp::new(w.at, w.recent_s, w.baseline_s, w.factor_milli);
        let seconds = |oldest: Option<u64>, window: Window| {
            u32::try_from(w.covered_ms(oldest, window) / MS_PER_S).unwrap_or(u32::MAX)
        };
        let mut coverage = Coverage::new();
        coverage.recent_s = seconds(self.ops_seen.oldest, Window::Recent);
        coverage.baseline_s = seconds(self.ops_seen.oldest, Window::Baseline);
        coverage.serve_recent_s = seconds(self.serve_seen.oldest, Window::Recent);
        coverage.serve_baseline_s = seconds(self.serve_seen.oldest, Window::Baseline);
        coverage.records = self.ops_seen.recent + self.ops_seen.baseline;
        coverage.serve_records = self.serve_seen.recent + self.serve_seen.baseline;
        coverage.skipped = self.skipped;

        // 2. Each figure the windows hold, judged; and the total's, uncompared.
        let mut total = Vec::new();
        for ((name, identity), acc) in &self.figures {
            let operations = self.operations.get(identity);
            let of = |pick: fn(&Operations) -> u64| match acc.base {
                Base::Own => None,
                Base::Operations => Some(operations.map_or(0, pick)),
            };
            let figure = |recent: Stat, baseline: Stat, drift: Drift| {
                let mut figure = Figure::new(name.clone(), acc.unit, recent, baseline, drift);
                figure.identity.clone_from(identity);
                figure
            };
            if acc.total.hits > 0 {
                let recent = acc.total.stat(of(|o| o.total));
                let not = unknown(Unknown::NotCompared);
                total.push(figure(recent, Stat::default(), not));
            }
            if acc.recent.hits + acc.baseline.hits == 0 {
                continue;
            }
            let recent = acc.recent.stat(of(|o| o.recent));
            let baseline = acc.baseline.stat(of(|o| o.baseline));
            let drift = self.drift(acc, &recent, &baseline);
            resp.figures.push(figure(recent, baseline, drift));
        }
        resp.total = w.since.map(|_| total);

        // 3. The fixed alerts and the gauges; a maintain no record names is unknown.
        resp.alerts = self.alerts();
        let gauge = |name: &str, value: Option<u64>| Gauge::new(name.to_owned(), value);
        resp.gauges = vec![
            gauge("records.recent_s", Some(u64::from(coverage.recent_s))),
            gauge("records.baseline_s", Some(u64::from(coverage.baseline_s))),
        ];
        resp.gauges.push(match self.maintained {
            Some(last) => gauge("maintain.age_s", Some((w.at - last) / MS_PER_S)),
            None => gauge("maintain.last", None),
        });
        resp.coverage = coverage;
        resp
    }

    /// How a figure's recent window compares with its baseline (the M21 design §2.3).
    fn drift(&self, acc: &Acc, recent: &Stat, baseline: &Stat) -> Drift {
        // 1. A baseline its file does not cover is no ground to judge on.
        let w = self.windows;
        let seen = match acc.source {
            Source::Ops => &self.ops_seen,
            Source::Serve => &self.serve_seen,
        };
        if seen.baseline == 0 {
            return unknown(Unknown::BaselineEmpty);
        }
        let baseline_ms = w.covered_ms(seen.oldest, Window::Baseline);
        if baseline_ms * 2 < u64::from(w.baseline_s) * MS_PER_S {
            return unknown(Unknown::BaselineShort);
        }

        // 2. By unit: percentiles, a share, or a rate per covered second.
        let factor = u64::from(w.factor_milli);
        match acc.unit {
            Unit::Micros => latency(recent, baseline, factor),
            Unit::Share => share(recent, baseline, factor),
            _ => {
                let recent_ms = w.covered_ms(seen.oldest, Window::Recent);
                rate((recent, recent_ms), (baseline, baseline_ms), factor)
            }
        }
    }

    /// The fixed limits the recent window passed: a land's p99, the retries' share, any
    /// `E_BUSY`.
    fn alerts(&self) -> Vec<Alert> {
        // 1. Each land operation's p99, in µs.
        let mut alerts = Vec::new();
        let p99_limit = ALERT_LAND_P99_MS * US_PER_MS;
        for (op, latency) in &self.alerts.land {
            let Some(p99) = latency.percentile(P99) else {
                continue;
            };
            if p99 > p99_limit {
                let figure = format!("op.{op}.latency");
                alerts.push(Alert::new(AlertRule::LandP99, figure, p99, p99_limit));
            }
        }

        // 2. The share of lands that retried, in thousandths.
        let retried = self.alerts.retried * MILLI;
        if let Some(share) = retried.checked_div(self.alerts.lands)
            && share > ALERT_RETRY_SHARE_MILLI
        {
            let figure = "land.retries".to_owned();
            let limit = ALERT_RETRY_SHARE_MILLI;
            alerts.push(Alert::new(AlertRule::RetryShare, figure, share, limit));
        }

        // 3. Any `E_BUSY`.
        let busy = self.alerts.busy;
        if busy > ALERT_BUSY_LIMIT {
            let figure = format!("code.{}", ErrorCode::Busy.as_str());
            alerts.push(Alert::new(AlertRule::Busy, figure, busy, ALERT_BUSY_LIMIT));
        }
        alerts
    }
}

/// A figure not judged, for `reason`.
fn unknown(reason: Unknown) -> Drift {
    Drift::Unknown { reason }
}

/// A latency's drift: its p99 with 100 samples a window, else its p50 with 20.
fn latency(recent: &Stat, baseline: &Stat, factor: u64) -> Drift {
    // 1. Enough samples in both windows.
    let fewest = recent.count.min(baseline.count);
    if fewest < DRIFT_MIN_SAMPLES {
        return unknown(Unknown::FewSamples);
    }

    // 2. The p99 first, when it can be judged; then the p50.
    let ratio =
        |r: Option<u64>, b: Option<u64>| ratio_milli(r.unwrap_or(0).into(), b.unwrap_or(0).into());
    if fewest >= DRIFT_MIN_SAMPLES_P99
        && let Some(departed) = judge(Measure::P99, ratio(recent.p99, baseline.p99), factor)
    {
        return departed;
    }
    judge(Measure::P50, ratio(recent.p50, baseline.p50), factor).unwrap_or(Drift::Steady)
}

/// A share's drift: `sum` of `count`, each window's denominator at least 20; a share the
/// baseline never had departs at the most ratio once the recent window has it 20 times.
fn share(recent: &Stat, baseline: &Stat, factor: u64) -> Drift {
    // 1. Enough of a denominator in both windows.
    if recent.count.min(baseline.count) < DRIFT_MIN_SAMPLES {
        return unknown(Unknown::FewSamples);
    }

    // 2. Something new: departed at the most ratio, or too few to say.
    if baseline.sum == 0 {
        return new_to_the_window(recent.sum);
    }

    // 3. The shares' ratio.
    let r = u128::from(recent.sum) * u128::from(baseline.count);
    let b = u128::from(baseline.sum) * u128::from(recent.count);
    judge(Measure::Rate, ratio_milli(r, b), factor).unwrap_or(Drift::Steady)
}

/// A count's drift, compared per covered second (S8): 300 s and 3,600 s of a steady run
/// have the same rate, never a 12× drop.
fn rate(recent: (&Stat, u64), baseline: (&Stat, u64), factor: u64) -> Drift {
    // 1. Something new: departed at the most ratio, or too few to say.
    let ((recent, recent_ms), (baseline, baseline_ms)) = (recent, baseline);
    if baseline.sum == 0 {
        return new_to_the_window(recent.sum);
    }
    if recent.count.max(baseline.count) < DRIFT_MIN_SAMPLES || recent_ms == 0 {
        return unknown(Unknown::FewSamples);
    }

    // 2. The rates' ratio: (recent / recent_ms) / (baseline / baseline_ms).
    let r = u128::from(recent.sum) * u128::from(baseline_ms);
    let b = u128::from(baseline.sum) * u128::from(recent_ms);
    judge(Measure::Rate, ratio_milli(r, b), factor).unwrap_or(Drift::Steady)
}

/// The drift of what the baseline never had and the recent window had `recent` times.
fn new_to_the_window(recent: u64) -> Drift {
    if recent == 0 {
        return Drift::Steady;
    }
    if recent < DRIFT_MIN_SAMPLES {
        return unknown(Unknown::FewSamples);
    }
    Drift::Departed {
        of: Measure::Rate,
        ratio_milli: u32::MAX,
        direction: Direction::Up,
    }
}

/// `recent / baseline` in thousandths, `u32::MAX` at most.
fn ratio_milli(recent: u128, baseline: u128) -> u32 {
    if baseline == 0 {
        return if recent == 0 { 1_000 } else { u32::MAX };
    }
    u32::try_from(recent * u128::from(MILLI) / baseline).unwrap_or(u32::MAX)
}

/// Departed when `ratio_milli` passes `factor` either way, both in thousandths.
fn judge(of: Measure, ratio_milli: u32, factor: u64) -> Option<Drift> {
    let ratio = u64::from(ratio_milli);
    let direction = if ratio > factor {
        Direction::Up
    } else if ratio * factor < MILLI * MILLI {
        Direction::Down
    } else {
        return None;
    };
    Some(Drift::Departed {
        of,
        ratio_milli,
        direction,
    })
}

/// The name a figure gives `service`, as the wire spells it.
fn service_name(service: ServeService) -> Option<String> {
    let value = serde_json::to_value(service).ok()?;
    value.as_str().map(str::to_owned)
}

/// The Unix milliseconds now.
fn now_ms() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
