//! What the landing measurements share: M4 Task 1's measurement of the four landing models
//! (`swarmops-repo`'s `tests/landing_models.rs`) and, later, M4 Task 11's landing through the
//! binary.
//!
//! - [`Workload`], [`base_files`] and [`change_for`]: what the store starts with and what each
//!   lander lands, drawn from a seed with [`mix`], so that every run of a cell lands the same
//!   changes.
//! - [`LandRecord`]: one land as its lander saw it; [`SyncTimer`] times the journal's and the
//!   ref files' syncs a land made, its shares of the lock hold (the M4 plan's Decision 14).
//! - [`check_invariants`]: the gate every run passes (the M4 plan's Decision 1): every landed
//!   change on the target's first-parent chain exactly once and nothing else new there, each
//!   lander's in its own order, every move of the target a one-step fast-forward, no merge on
//!   the chain, a clean deep `verify`, no op-head ref left and no land past [`MAX_LATENCY`].
//! - [`Stats`]: a run's numbers (lands/s, latency, retry rate, conflicts, lock hold and its
//!   sync shares, relocations), with the summary line a run prints and reads back.
//! - [`backoff`]: ops spec §3 step 7's jittered backoff, its jitter from [`mix`]; [`slot`] and
//!   [`open_loop_lands`]: when an open loop's lands are due.
//! - [`Cell`] and [`Run`]: a cell of the measurement, and one run of one model in it, with the
//!   line the harness prints for it ([`RUN_TAG`]) and reads back.
//! - [`decide`]: the M4 plan's Decision 1, written before any number exists, applied to a
//!   matrix's runs ([`Verdict`], [`Decision`]); [`tables`]: each cell's medians and ranges, as
//!   `docs/perf.md` gives them.
//! - [`load1`] and [`fdatasync_probe`]: the host, as `docs/perf.md`'s tables record it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use swarmops_core::bytes::to_u64;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_repo::LocalRepo;
use swarmops_store::{FsMethod, MetaStore};

use crate::fs::{Call, Watch};
use crate::measure::{mix, percentile};

/// The file the sharing workloads land lines of: [`REGISTRY_LINES`] lines, `line <l>\n`.
pub const REGISTRY: &str = "REGISTRY";

/// How many lines [`REGISTRY`] holds.
pub const REGISTRY_LINES: usize = 200;

/// How many files [`Workload::Large`]'s base holds under `big/`, each rewritten by lander 0's
/// every land.
pub const BIG_FILES: usize = 500;

/// How many bytes each file under `big/` holds.
pub const BIG_FILE_BYTES: usize = 1_024;

/// The most attempts a land makes: ops spec §3's `max_attempts`, 8 by default.
pub const MAX_ATTEMPTS: u32 = 8;

/// The longest a land may take: a run with a land past it fails the gate.
pub const MAX_LATENCY: Duration = Duration::from_secs(30);

/// What the op-heads model's refs start with: a recorded land, and a conflicted one. The gate
/// wants none of them left once a run has settled.
pub const OP_HEAD_REFS: [&str; 2] = ["release/oph-", "release/ophc-"];

/// The domain of a lander's draws that picks its files ([`draw`]); a file's line count draws in
/// `LINES + file`.
const FILES: u64 = 0;
const LINES: u64 = 1;
/// The domain of the draw that picks whether a land changes a [`REGISTRY`] line.
const REGISTRY_DRAW: u64 = 1 << 40;
/// The domain of the draw that picks whether a conflicting land changes line 0 instead.
const CONFLICT_DRAW: u64 = (1 << 40) + 1;
/// The domain of a backoff's jitter, plus the attempt.
const BACKOFF_DRAW: u64 = 1 << 41;
/// The domain of the draw that places a lander's grid in an open loop ([`slot`]).
const PHASE_DRAW: u64 = 1 << 42;
/// The domain of the draw that moves an open loop's slot off its grid point.
const SLOT_DRAW: u64 = (1 << 42) + 1;

/// How far an open loop's slot may fall from its grid point, either way, as a share of the
/// interval: the M4 plan's "± 20 %".
pub const SLOT_JITTER: f64 = 0.2;

/// The tag of the line a lander prints for each of its lands ([`LandRecord::to_line`]).
pub const LAND_TAG: &str = "landing-land: ";

/// The tag of the line the harness prints for each run ([`Run::to_line`]).
pub const RUN_TAG: &str = "landing-model: ";

/// How many appends the raw `fdatasync` probe takes the median of: 40, as `docs/perf.md`'s.
pub const PROBE_APPENDS: usize = 40;

/// What the landers land, and what the store starts with ([`base_files`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Workload {
    /// Each land puts 1 to 4 files of 1 to 64 lines under the lander's own `agents/<n>/`:
    /// no two landers' paths meet.
    Disjoint,
    /// Disjoint, and one land in five also replaces line `3n + 1` of [`REGISTRY`] (`n` the
    /// lander): a file every lander shares, never a line two share.
    Shared,
    /// Shared, and one land in twenty replaces line 0 instead: a line landers share.
    Conflict,
    /// Lander 0 rewrites all of `big/` ([`BIG_FILES`] files) in each land; the others land as
    /// in Disjoint.
    Large,
}

impl Workload {
    /// The four.
    pub const ALL: [Workload; 4] = [
        Workload::Disjoint,
        Workload::Shared,
        Workload::Conflict,
        Workload::Large,
    ];

    /// Its name: `disjoint`, `shared`, `conflict` or `large`.
    pub fn name(self) -> &'static str {
        match self {
            Workload::Disjoint => "disjoint",
            Workload::Shared => "shared",
            Workload::Conflict => "conflict",
            Workload::Large => "large",
        }
    }

    /// The workload named `name` ([`Workload::name`]).
    pub fn from_name(name: &str) -> Option<Workload> {
        Workload::ALL
            .into_iter()
            .find(|workload| workload.name() == name)
    }
}

/// The files the store starts with for `landers` landers under `workload`, as (path, content):
/// [`REGISTRY`], `agents/<n>/f0.txt` for each lander, and for [`Workload::Large`] the
/// [`BIG_FILES`] files `big/<i>.txt` of [`BIG_FILE_BYTES`] each.
pub fn base_files(landers: usize, workload: Workload) -> Vec<(String, Vec<u8>)> {
    let registry: Vec<String> = (0..REGISTRY_LINES).map(|l| format!("line {l}\n")).collect();
    let mut files = vec![(REGISTRY.to_owned(), registry.concat().into_bytes())];
    for lander in 0..landers {
        let content = format!("lander {lander} base\n").into_bytes();
        files.push((format!("agents/{lander}/f0.txt"), content));
    }
    if workload == Workload::Large {
        for file in 0..BIG_FILES {
            files.push((format!("big/{file}.txt"), big_file(file, None)));
        }
    }
    files
}

/// What one land changes ([`change_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The files it puts, as (path, content): its own, never [`REGISTRY`].
    pub puts: Vec<(String, Vec<u8>)>,
    /// The line of [`REGISTRY`] it replaces, if any, by [`registry_text`]: the lander puts
    /// [`REGISTRY`] as the target's head holds it with that line replaced ([`with_line`]).
    pub registry_line: Option<usize>,
}

/// The change lander `lander` (from 0) lands in its land `land` (from 0) of a run seeded
/// `seed` under `workload`: see [`Workload`]. Every draw is [`draw`] of the four, so the same
/// arguments give the same change.
pub fn change_for(seed: u64, lander: usize, land: usize, workload: Workload) -> Change {
    if workload == Workload::Large && lander == 0 {
        let puts = (0..BIG_FILES)
            .map(|file| (format!("big/{file}.txt"), big_file(file, Some(land))))
            .collect();
        return Change {
            puts,
            registry_line: None,
        };
    }
    let files = 1 + draw(seed, lander, land, FILES) % 4;
    let puts = (0..files)
        .map(|file| {
            let lines = 1 + draw(seed, lander, land, LINES + file) % 64;
            let content: Vec<String> = (0..lines)
                .map(|line| format!("lander {lander} land {land} file {file} line {line}\n"))
                .collect();
            (
                format!("agents/{lander}/f{file}.txt"),
                content.concat().into_bytes(),
            )
        })
        .collect();
    let own_line = (3 * lander + 1) % REGISTRY_LINES;
    let shares = draw(seed, lander, land, REGISTRY_DRAW).is_multiple_of(5);
    let registry_line = match workload {
        Workload::Disjoint | Workload::Large => None,
        Workload::Conflict if draw(seed, lander, land, CONFLICT_DRAW).is_multiple_of(20) => Some(0),
        Workload::Shared | Workload::Conflict => shares.then_some(own_line),
    };
    Change {
        puts,
        registry_line,
    }
}

/// The [`REGISTRY`] line lander `lander`'s land `land` writes: `lander <n> land <i>`, without
/// its line break.
pub fn registry_text(lander: usize, land: usize) -> String {
    format!("lander {lander} land {land}")
}

/// `file` with its line `line` (from 0) replaced by `text` and a line break; unchanged if it
/// has no such line.
pub fn with_line(file: &[u8], line: usize, text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(file.len() + text.len());
    for (at, old) in file.split_inclusive(|&b| b == b'\n').enumerate() {
        if at == line {
            out.extend_from_slice(text.as_bytes());
            out.push(b'\n');
        } else {
            out.extend_from_slice(old);
        }
    }
    out
}

/// A lander's draw: `(seed, lander, land, domain)` folded into one [`mix`], as
/// `mix(mix(mix(seed ^ lander) ^ land) ^ domain)`.
pub fn draw(seed: u64, lander: usize, land: usize, domain: u64) -> u64 {
    mix(mix(mix(seed ^ to_u64(lander)) ^ to_u64(land)) ^ domain)
}

/// File `file` of `big/`: [`BIG_FILE_BYTES`] bytes naming it, and the land that wrote it
/// (`None` for the base).
fn big_file(file: usize, land: Option<usize>) -> Vec<u8> {
    let line = match land {
        Some(land) => format!("big {file} land {land}\n"),
        None => format!("big {file} base\n"),
    };
    line.into_bytes()
        .into_iter()
        .cycle()
        .take(BIG_FILE_BYTES)
        .collect()
}

/// How long a lander waits before attempt `attempt` (from 1) of land `land`: nothing before
/// the fourth; from it, ops spec §3 step 7's `min(2 s, 25 ms·2^attempt)·U(0.5, 1.5)`, `U` the
/// high 32 bits of [`draw`] of `(seed, lander, land, attempt)` scaled to [0.5, 1.5).
pub fn backoff(seed: u64, lander: usize, land: usize, attempt: u32) -> Duration {
    if attempt < 4 {
        return Duration::ZERO;
    }
    let step = Duration::from_millis(25)
        .saturating_mul(1 << attempt.min(16))
        .min(Duration::from_secs(2));
    let jitter = unit(draw(seed, lander, land, BACKOFF_DRAW + u64::from(attempt)));
    step.mul_f64(0.5 + jitter)
}

/// A draw as a fraction in [0, 1): its high 32 bits over 2^32.
fn unit(bits: u64) -> f64 {
    let high = u32::try_from(bits >> 32).expect("32 bits");
    f64::from(high) / 4_294_967_296.0
}

/// Where lander `lander`'s grid starts in an open loop of one land per `interval`: a draw in
/// [0, interval), so that the landers' lands spread over each interval instead of arriving
/// together.
fn phase(seed: u64, lander: usize, interval: Duration) -> Duration {
    interval.mul_f64(unit(draw(seed, lander, 0, PHASE_DRAW)))
}

/// How many lands lander `lander` makes in an open loop of `duration` at one land per
/// `interval`: one for each point of its grid, `phase + i·interval` ([`slot`]), before
/// `duration`.
pub fn open_loop_lands(seed: u64, lander: usize, interval: Duration, duration: Duration) -> usize {
    let phase = phase(seed, lander, interval);
    if interval.is_zero() || phase >= duration {
        return 0;
    }
    let points = duration
        .saturating_sub(phase)
        .as_nanos()
        .div_ceil(interval.as_nanos());
    usize::try_from(points).expect("a count of lands fits in usize")
}

/// When lander `lander`'s land `land` of an open loop at one land per `interval` is due, after
/// the go: its grid point, `phase + land·interval`, the phase drawn once per lander in
/// [0, interval), moved by a draw within ±[`SLOT_JITTER`] of the interval, and never before
/// the go. A land due while its lander's previous one still runs starts when that one ends.
pub fn slot(seed: u64, lander: usize, land: usize, interval: Duration) -> Duration {
    let steps = u32::try_from(land).expect("an open loop's land number fits in u32");
    let point = phase(seed, lander, interval) + interval.saturating_mul(steps);
    let moved = interval.mul_f64(2.0 * SLOT_JITTER * unit(draw(seed, lander, land, SLOT_DRAW)));
    (point + moved).saturating_sub(interval.mul_f64(SLOT_JITTER))
}

/// One land, as its lander saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandRecord {
    /// The changeset it landed, in full hex; `None` if it ended conflicted or exhausted.
    pub id: Option<String>,
    /// How many times it published (op heads: recorded).
    pub attempts: u32,
    /// Whether the changeset that landed was made on another head than the land's last
    /// attempt prepared it on: by the lock holder (relocated, or merged in the lock).
    pub relocated: bool,
    /// Whether it ended in a conflict its lander could not merge.
    pub conflicted: bool,
    /// Whether it ended after [`MAX_ATTEMPTS`] attempts without landing.
    pub exhausted: bool,
    /// From its first read of the target to the publish that put it there (op heads: to the
    /// poll that saw it there), or to its end, in microseconds.
    pub latency_us: u64,
    /// How long each of its write transactions held the lock, in microseconds: one per
    /// publish, and one per settle its lander ran.
    pub lock_hold_us: Vec<u64>,
    /// How long its write transactions spent in the journal's syncs ([`SyncTimer`]).
    pub journal_sync_us: u64,
    /// How long they spent in the ref files' syncs ([`SyncTimer`]).
    pub ref_sync_us: u64,
    /// How many times a write transaction of it gave up waiting for the lock (`Busy`) and was
    /// tried again.
    pub busy: u32,
}

impl LandRecord {
    /// The record as one line of `key=value` fields, which [`LandRecord::parse`] reads back.
    pub fn to_line(&self) -> String {
        let holds: Vec<String> = self.lock_hold_us.iter().map(u64::to_string).collect();
        format!(
            "id={} attempts={} relocated={} conflicted={} exhausted={} latency_us={} \
             journal_sync_us={} ref_sync_us={} busy={} lock_hold_us={}",
            self.id.as_deref().unwrap_or("-"),
            self.attempts,
            u8::from(self.relocated),
            u8::from(self.conflicted),
            u8::from(self.exhausted),
            self.latency_us,
            self.journal_sync_us,
            self.ref_sync_us,
            self.busy,
            if holds.is_empty() {
                "-".to_owned()
            } else {
                holds.join(",")
            },
        )
    }

    /// The record [`LandRecord::to_line`] wrote as `line`; `None` if it is not one.
    pub fn parse(line: &str) -> Option<LandRecord> {
        let fields = fields(line);
        let flag = |name: &str| match fields.get(name)?.as_str() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let number = |name: &str| fields.get(name)?.parse::<u64>().ok();
        let id = match fields.get("id")?.as_str() {
            "-" => None,
            hex => Some(hex.to_owned()),
        };
        let lock_hold_us = match fields.get("lock_hold_us")?.as_str() {
            "-" => Vec::new(),
            holds => holds
                .split(',')
                .map(|hold| hold.parse().ok())
                .collect::<Option<_>>()?,
        };
        Some(LandRecord {
            id,
            attempts: u32::try_from(number("attempts")?).ok()?,
            relocated: flag("relocated")?,
            conflicted: flag("conflicted")?,
            exhausted: flag("exhausted")?,
            latency_us: number("latency_us")?,
            lock_hold_us,
            journal_sync_us: number("journal_sync_us")?,
            ref_sync_us: number("ref_sync_us")?,
            busy: u32::try_from(number("busy")?).ok()?,
        })
    }
}

/// The `key=value` fields of `line`, split at whitespace; words without `=` are passed over.
fn fields(line: &str) -> HashMap<String, String> {
    line.split_whitespace()
        .filter_map(|field| field.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

thread_local! {
    /// When the call a [`SyncTimer`] times on this thread began.
    static STARTED: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// A watcher ([`crate::fs::Watched`]) that sums the time a store spends in the syncs a write
/// transaction makes under the lock: the journal's (`append_durable` to `meta/journal.log` or
/// `meta/journal.off`, and a `sync_data` of `journal.log`), and the ref files' (the
/// `atomic_replace`, `remove` and `fsync_dir` of `meta/refs/` and below), for the M4 plan's
/// Decision 14. Calls are timed on the thread that makes them, so several threads may share it.
#[derive(Debug)]
pub struct SyncTimer {
    /// The store's `meta/`.
    meta: PathBuf,
    /// The store's `meta/refs`.
    refs: PathBuf,
    /// Nanoseconds in the journal's syncs.
    journal_ns: AtomicU64,
    /// Nanoseconds in the ref files' syncs.
    ref_ns: AtomicU64,
}

/// Which of a [`SyncTimer`]'s sums a call adds to.
#[derive(Debug, Clone, Copy)]
enum Synced {
    Journal,
    Ref,
}

impl SyncTimer {
    /// A timer of the syncs of the store rooted at `store`.
    pub fn new(store: &Utf8Path) -> SyncTimer {
        let meta = store.as_std_path().join("meta");
        SyncTimer {
            refs: meta.join("refs"),
            meta,
            journal_ns: AtomicU64::new(0),
            ref_ns: AtomicU64::new(0),
        }
    }

    /// The microseconds spent in the journal's syncs so far.
    pub fn journal_us(&self) -> u64 {
        self.journal_ns.load(Ordering::Relaxed) / 1_000
    }

    /// The microseconds spent in the ref files' syncs so far.
    pub fn ref_us(&self) -> u64 {
        self.ref_ns.load(Ordering::Relaxed) / 1_000
    }

    /// Which sum `call` adds to, if any.
    fn synced(&self, call: &Call<'_>) -> Option<Synced> {
        let path = call.path();
        let journal = path.parent() == Some(self.meta.as_path())
            && path
                .file_name()
                .is_some_and(|name| name == "journal.log" || name == "journal.off");
        match call.method {
            FsMethod::AppendDurable | FsMethod::SyncData if journal => Some(Synced::Journal),
            FsMethod::AtomicReplace | FsMethod::Remove | FsMethod::FsyncDir
                if path.starts_with(&self.refs) =>
            {
                Some(Synced::Ref)
            }
            _ => None,
        }
    }
}

impl Watch for SyncTimer {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if self.synced(call).is_some() {
            STARTED.set(Some(Instant::now()));
        }
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if let Some(synced) = self.synced(call)
            && let Some(started) = STARTED.take()
        {
            let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let sum = match synced {
                Synced::Journal => &self.journal_ns,
                Synced::Ref => &self.ref_ns,
            };
            sum.fetch_add(ns, Ordering::Relaxed);
        }
    }
}

/// Checks the gate of a run (the M4 plan's Decision 1) on the store at `store`, whose ref
/// `target` named `base` when the run began, against what each lander `reported` (one list per
/// lander, its lands in order):
///
/// - every land took at most [`MAX_LATENCY`] (`slow land`);
/// - no changeset is reported landed twice (`doubled landing`);
/// - `target`'s first-parent chain from its head reaches `base`, and every changeset on it
///   above `base` has one parent (`a merge on the chain`);
/// - every changeset on it above `base` is a reported landed one (`unreported changeset`), and
///   every reported one is on it (`lost landing`): each exactly once;
/// - each lander's landed changesets are on it in the lander's own order (`out of order`);
/// - every move of `target` since it named `base` went from the changeset's first parent to
///   the changeset (`not a fast-forward`);
/// - no op-head ref is left ([`OP_HEAD_REFS`]; `op-head ref left`);
/// - a deep `verify` finds nothing (`verify`).
///
/// The error starts with the name of the first rule a run breaks, in that order, and a colon,
/// and says what broke it; `bad record` (a reported id that is not one) and `the store` (a read
/// that failed) come before the rules they keep from being checked.
pub fn check_invariants(
    store: &Utf8Path,
    target: &str,
    base: ChangesetId,
    reported: &[Vec<LandRecord>],
) -> Result<(), String> {
    let max_us = u64::try_from(MAX_LATENCY.as_micros()).expect("30 s in microseconds");
    let mut landed: HashMap<ChangesetId, (usize, usize)> = HashMap::new();
    for (lander, records) in reported.iter().enumerate() {
        for (land, record) in records.iter().enumerate() {
            if record.latency_us > max_us {
                return Err(format!(
                    "slow land: lander {lander}'s land {land} took {} ms, past {} s",
                    record.latency_us / 1_000,
                    MAX_LATENCY.as_secs()
                ));
            }
        }
    }
    for (lander, records) in reported.iter().enumerate() {
        for (land, record) in records.iter().enumerate() {
            let Some(hex) = &record.id else { continue };
            let id = ObjectId::from_hex(hex).map(ChangesetId).map_err(|e| {
                format!("bad record: lander {lander}'s land {land} reports {hex:?}: {e}")
            })?;
            if let Some((other, other_land)) = landed.insert(id, (lander, land)) {
                return Err(format!(
                    "doubled landing: {id} is reported by lander {other}'s land {other_land} and \
                     lander {lander}'s land {land}"
                ));
            }
        }
    }
    let repo = LocalRepo::open(store).map_err(|e| format!("the store: open it: {e}"))?;
    let head = repo
        .get_ref(target)
        .map_err(|e| format!("the store: read {target}: {e}"))?
        .ok_or_else(|| format!("the chain: {target} does not exist"))?
        .target;
    let chain = first_parent_chain(&repo, target, head, base, landed.len())?;
    let on_chain: HashSet<ChangesetId> = chain.iter().copied().collect();
    for id in &chain {
        if !landed.contains_key(id) {
            return Err(format!(
                "unreported changeset: {id} is on {target}'s first-parent chain, and no lander \
                 reported landing it"
            ));
        }
    }
    for (id, (lander, land)) in &landed {
        if !on_chain.contains(id) {
            return Err(format!(
                "lost landing: lander {lander}'s land {land} landed {id}, which is not on \
                 {target}'s first-parent chain"
            ));
        }
    }
    // `chain` is newest first: a lander's later land sits at a smaller index.
    let at: HashMap<ChangesetId, usize> =
        chain.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    for (lander, records) in reported.iter().enumerate() {
        let mut last: Option<(usize, usize)> = None;
        for (land, record) in records.iter().enumerate() {
            let Some(id) = record
                .id
                .as_deref()
                .and_then(|hex| ObjectId::from_hex(hex).ok())
            else {
                continue;
            };
            let place = at[&ChangesetId(id)];
            if let Some((before, before_place)) = last
                && place >= before_place
            {
                return Err(format!(
                    "out of order: lander {lander}'s land {land} is on {target} before its land \
                     {before}"
                ));
            }
            last = Some((land, place));
        }
    }
    check_moves(store, target, base, &repo)?;
    let refs = repo
        .list_refs()
        .map_err(|e| format!("the store: list the refs: {e}"))?;
    if let Some((name, _)) = refs
        .iter()
        .find(|(name, _)| OP_HEAD_REFS.iter().any(|prefix| name.starts_with(prefix)))
    {
        return Err(format!("op-head ref left: {name}"));
    }
    let report = repo.verify(true).map_err(|e| format!("verify: {e}"))?;
    if !report.problems.is_empty() {
        return Err(format!("verify: {:?}", report.problems));
    }
    Ok(())
}

/// The changesets on `target`'s first-parent chain from `head` down to `base`, newest first,
/// `base` left out: at most `landed` of them plus one, since a longer chain holds an unreported
/// changeset.
fn first_parent_chain(
    repo: &LocalRepo,
    target: &str,
    head: ChangesetId,
    base: ChangesetId,
    landed: usize,
) -> Result<Vec<ChangesetId>, String> {
    let read = |id: ChangesetId| {
        repo.changeset(id)
            .map_err(|e| format!("the store: read {id}: {e}"))?
            .ok_or_else(|| format!("the chain: {target}'s chain names {id}, which is not stored"))
    };
    let base_generation = read(base)?.generation;
    let mut chain = Vec::new();
    let mut at = head;
    while at != base {
        let stored = read(at)?;
        if stored.generation <= base_generation || chain.len() > landed {
            return Err(format!(
                "the chain: {target}'s first-parent chain from {head} does not come down to the \
                 base {base} (stopped at {at})"
            ));
        }
        let parents = &stored.changeset.parents;
        if parents.len() != 1 {
            return Err(format!(
                "a merge on the chain: {at} on {target} has {} parents",
                parents.len()
            ));
        }
        chain.push(at);
        at = parents[0];
    }
    Ok(chain)
}

/// Checks that every move of `target` since it named `base` was a one-step fast-forward: its
/// new changeset's first parent is the changeset the move left.
fn check_moves(
    store: &Utf8Path,
    target: &str,
    base: ChangesetId,
    repo: &LocalRepo,
) -> Result<(), String> {
    let meta = MetaStore::open(store).map_err(|e| format!("the store: open the metadata: {e}"))?;
    let read = meta
        .read()
        .map_err(|e| format!("the store: read the metadata: {e}"))?;
    let moves = read
        .reflog(target, usize::MAX)
        .map_err(|e| format!("the store: read {target}'s moves: {e}"))?;
    for entry in moves {
        if entry.new == Some(base) {
            return Ok(());
        }
        let new = entry.new.ok_or_else(|| {
            format!(
                "not a fast-forward: {target} was deleted (frame {})",
                entry.seq
            )
        })?;
        let first = repo
            .changeset(new)
            .map_err(|e| format!("the store: read {new}: {e}"))?
            .and_then(|stored| stored.changeset.parents.first().copied());
        if first != entry.old {
            return Err(format!(
                "not a fast-forward: frame {} moved {target} from {} to {new}, whose first parent \
                 is {}",
                entry.seq,
                entry
                    .old
                    .map_or_else(|| "nothing".to_owned(), |old| old.to_string()),
                first.map_or_else(|| "none".to_owned(), |first| first.to_string()),
            ));
        }
    }
    Err(format!("the chain: {target} never named the base {base}"))
}

/// What a run measured, over every lander's [`LandRecord`]s.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    /// How many lands landed.
    pub landed: u64,
    /// Landed lands per second of the wall time [`Stats::from`] was given.
    pub lands_per_s: f64,
    /// The latency of the landed lands, nearest-rank p50, in microseconds.
    pub latency_p50_us: u64,
    /// The same, p99.
    pub latency_p99_us: u64,
    /// The retries per landed land: Σ(attempts − 1) over the lands that did not conflict,
    /// divided by the landed ones.
    pub retry_rate: f64,
    /// How many lands ended conflicted: left out of the other numbers but this one.
    pub conflicts: u64,
    /// How many lands ended after [`MAX_ATTEMPTS`] attempts.
    pub exhausted: u64,
    /// How many write transactions gave up waiting for the lock and were tried again.
    pub busy: u64,
    /// The lock hold of every write transaction, nearest-rank p50, in microseconds.
    pub lock_hold_p50_us: u64,
    /// The same, p99.
    pub lock_hold_p99_us: u64,
    /// The share of the summed lock hold spent in the journal's syncs, from 0 to 1.
    pub journal_share: f64,
    /// The share spent in the ref files' syncs, from 0 to 1.
    pub ref_share: f64,
    /// How many lands landed relocated.
    pub relocations: u64,
}

impl Stats {
    /// The numbers of `records` (each lander's lands), the lands/s over `wall`: from the go to
    /// the last lander's end in a closed loop, the run's duration in an open one.
    pub fn from(records: &[Vec<LandRecord>], wall: Duration) -> Stats {
        let all = || records.iter().flatten();
        let landed: Vec<&LandRecord> = all().filter(|record| record.id.is_some()).collect();
        let count = |pick: fn(&LandRecord) -> bool| to_u64(all().filter(|r| pick(r)).count());
        let mut latencies: Vec<Duration> = landed
            .iter()
            .map(|record| Duration::from_micros(record.latency_us))
            .collect();
        latencies.sort_unstable();
        let mut holds: Vec<Duration> = all()
            .flat_map(|record| record.lock_hold_us.iter().copied())
            .map(Duration::from_micros)
            .collect();
        holds.sort_unstable();
        let retries: u64 = all()
            .filter(|record| !record.conflicted)
            .map(|record| u64::from(record.attempts.saturating_sub(1)))
            .sum();
        let held: u64 = all().flat_map(|record| &record.lock_hold_us).sum();
        let journal: u64 = all().map(|record| record.journal_sync_us).sum();
        let refs: u64 = all().map(|record| record.ref_sync_us).sum();
        let landed_count = to_u64(landed.len());
        Stats {
            landed: landed_count,
            lands_per_s: as_f64(landed_count) / wall.as_secs_f64(),
            latency_p50_us: micros(&latencies, 50),
            latency_p99_us: micros(&latencies, 99),
            retry_rate: ratio(retries, landed_count),
            conflicts: count(|record| record.conflicted),
            exhausted: count(|record| record.exhausted),
            busy: all().map(|record| u64::from(record.busy)).sum(),
            lock_hold_p50_us: micros(&holds, 50),
            lock_hold_p99_us: micros(&holds, 99),
            journal_share: ratio(journal, held),
            ref_share: ratio(refs, held),
            relocations: count(|record| record.relocated),
        }
    }

    /// The numbers as one line of `key=value` fields, which [`Stats::parse`] reads back:
    /// latencies and holds in whole microseconds, rates to three decimals.
    pub fn summary(&self) -> String {
        format!(
            "landed={} lands_per_s={:.3} latency_p50_us={} latency_p99_us={} retry_pct={:.3} \
             conflicts={} exhausted={} busy={} lock_p50_us={} lock_p99_us={} journal_pct={:.3} \
             ref_pct={:.3} relocations={}",
            self.landed,
            self.lands_per_s,
            self.latency_p50_us,
            self.latency_p99_us,
            self.retry_rate * 100.0,
            self.conflicts,
            self.exhausted,
            self.busy,
            self.lock_hold_p50_us,
            self.lock_hold_p99_us,
            self.journal_share * 100.0,
            self.ref_share * 100.0,
            self.relocations,
        )
    }

    /// The numbers a [`Stats::summary`] line holds, read from `line` (other fields in it are
    /// passed over); `None` if one is missing or malformed.
    pub fn parse(line: &str) -> Option<Stats> {
        let fields = fields(line);
        let number = |name: &str| fields.get(name)?.parse::<u64>().ok();
        let real = |name: &str| fields.get(name)?.parse::<f64>().ok();
        Some(Stats {
            landed: number("landed")?,
            lands_per_s: real("lands_per_s")?,
            latency_p50_us: number("latency_p50_us")?,
            latency_p99_us: number("latency_p99_us")?,
            retry_rate: real("retry_pct")? / 100.0,
            conflicts: number("conflicts")?,
            exhausted: number("exhausted")?,
            busy: number("busy")?,
            lock_hold_p50_us: number("lock_p50_us")?,
            lock_hold_p99_us: number("lock_p99_us")?,
            journal_share: real("journal_pct")? / 100.0,
            ref_share: real("ref_pct")? / 100.0,
            relocations: number("relocations")?,
        })
    }
}

/// The nearest-rank `percent` percentile of `sorted`, in microseconds; 0 for no samples.
fn micros(sorted: &[Duration], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    u64::try_from(percentile(sorted, percent).as_micros()).unwrap_or(u64::MAX)
}

/// `n` as a float (exact up to 2^53, which no count here comes near).
fn as_f64(n: u64) -> f64 {
    let high = f64::from(u32::try_from(n >> 32).unwrap_or(u32::MAX));
    let low = f64::from(u32::try_from(n & 0xffff_ffff).expect("32 bits"));
    high * 4_294_967_296.0 + low
}

/// `part / whole`, 0 for a whole of 0.
fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        as_f64(part) / as_f64(whole)
    }
}

/// How the landers of a run pace their lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Pace {
    /// A closed loop: each lander lands `lands` times, each land as soon as its last one ended.
    Closed {
        /// The lands of each lander.
        lands: usize,
    },
    /// An open loop: each lander lands once per `interval_ms` ± [`SLOT_JITTER`] ([`slot`]),
    /// until `duration_s` has passed since the go.
    Open {
        /// The interval between two lands of a lander, in milliseconds.
        interval_ms: u64,
        /// How long the run offers lands, in seconds.
        duration_s: u64,
    },
}

/// A cell of the measurement: a workload, how many landers, and their pace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cell {
    /// What the landers land.
    pub workload: Workload,
    /// How many landers land at once, each a process of its own.
    pub landers: usize,
    /// How they pace their lands.
    pub pace: Pace,
}

/// The cell the M4 plan's Decision 1 scores the models in: 50 landers, open loop, one land per
/// lander every 2 s ± 20 %, workload [`Workload::Shared`], for 120 s.
pub const SCORED_CELL: Cell = Cell {
    workload: Workload::Shared,
    landers: 50,
    pace: Pace::Open {
        interval_ms: 2_000,
        duration_s: 120,
    },
};

impl Cell {
    /// The cell in words, as a table of `docs/perf.md` heads it.
    pub fn describe(&self) -> String {
        match self.pace {
            Pace::Closed { lands } => format!(
                "W-{}, {} landers x {lands} lands, closed loop",
                self.workload.name(),
                self.landers
            ),
            Pace::Open {
                interval_ms,
                duration_s,
            } => format!(
                "W-{}, {} landers, open loop: one land each every {interval_ms} ms +/- 20 % for \
                 {duration_s} s",
                self.workload.name(),
                self.landers
            ),
        }
    }
}

/// The host around a run, as `docs/perf.md`'s tables record it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Host {
    /// The 1-minute load average when the run began ([`load1`]).
    pub load_start: f64,
    /// The same when it ended.
    pub load_end: f64,
    /// The raw `fdatasync` probe when the run began ([`fdatasync_probe`] of [`PROBE_APPENDS`]),
    /// in microseconds.
    pub fdatasync_start_us: u64,
    /// The same when it ended.
    pub fdatasync_end_us: u64,
}

/// One run of one model in one cell, as the harness reports it on its [`RUN_TAG`] line.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// The model, by the name the prototypes give it: `cas_loop`, `pushrebase`, `metalog` or
    /// `op_heads`.
    pub model: String,
    /// The cell.
    pub cell: Cell,
    /// The seed every draw of the run starts from ([`draw`]).
    pub seed: u64,
    /// Whether the run passed the gate ([`check_invariants`]); the harness prints the rule a
    /// failed one broke on a line of its own.
    pub gate: bool,
    /// What it measured.
    pub stats: Stats,
    /// The host around it.
    pub host: Host,
}

impl Run {
    /// The run as one line of `key=value` fields, which [`Run::parse`] reads back: the model,
    /// the cell, the seed, the gate (`ok` or `fail`), the host, then [`Stats::summary`]'s
    /// fields.
    pub fn to_line(&self) -> String {
        let pace = match self.cell.pace {
            Pace::Closed { lands } => format!("pace=closed lands={lands}"),
            Pace::Open {
                interval_ms,
                duration_s,
            } => format!("pace=open interval_ms={interval_ms} duration_s={duration_s}"),
        };
        format!(
            "model={} workload={} landers={} {pace} seed={} gate={} load_start={:.2} \
             load_end={:.2} fdatasync_start_us={} fdatasync_end_us={} {}",
            self.model,
            self.cell.workload.name(),
            self.cell.landers,
            self.seed,
            if self.gate { "ok" } else { "fail" },
            self.host.load_start,
            self.host.load_end,
            self.host.fdatasync_start_us,
            self.host.fdatasync_end_us,
            self.stats.summary(),
        )
    }

    /// The run a [`Run::to_line`] line holds, read from `line` (other fields in it are passed
    /// over); `None` if a field is missing or malformed.
    pub fn parse(line: &str) -> Option<Run> {
        let fields = fields(line);
        let text = |name: &str| fields.get(name).map(String::as_str);
        let number = |name: &str| text(name)?.parse::<u64>().ok();
        let real = |name: &str| text(name)?.parse::<f64>().ok();
        let pace = match text("pace")? {
            "closed" => Pace::Closed {
                lands: usize::try_from(number("lands")?).ok()?,
            },
            "open" => Pace::Open {
                interval_ms: number("interval_ms")?,
                duration_s: number("duration_s")?,
            },
            _ => return None,
        };
        let gate = match text("gate")? {
            "ok" => true,
            "fail" => false,
            _ => return None,
        };
        Some(Run {
            model: text("model")?.to_owned(),
            cell: Cell {
                workload: Workload::from_name(text("workload")?)?,
                landers: usize::try_from(number("landers")?).ok()?,
                pace,
            },
            seed: number("seed")?,
            gate,
            stats: Stats::parse(line)?,
            host: Host {
                load_start: real("load_start")?,
                load_end: real("load_end")?,
                fdatasync_start_us: number("fdatasync_start_us")?,
                fdatasync_end_us: number("fdatasync_end_us")?,
            },
        })
    }
}

/// The candidates the M4 plan's Decision 1 chooses among, by the prototypes' names, in the order
/// of the work they do under the lock, least first: the order its "within 10 %" clause prefers.
pub const CANDIDATES: [&str; 3] = ["pushrebase", "metalog", "op_heads"];

/// The yardstick, the CAS loop: reported beside the candidates and never chosen (decision 10
/// chooses among the three; "§9 를 그대로 구현" is its rejected alternative).
pub const YARDSTICK: &str = "cas_loop";

/// The candidate whose production form needs a record FORMAT 1 does not have.
const OP_HEADS: &str = "op_heads";

/// Decision 1's first scored condition, umbrella §15's: at least 20 lands per second.
pub const MIN_LANDS_PER_S: f64 = 20.0;

/// Its second: a p99 latency under 500 ms, in microseconds.
pub const MAX_LATENCY_P99_US: f64 = 500_000.0;

/// Its third: a retry rate under 5 %. The fourth: no land that exhausted its attempts.
pub const MAX_RETRY_RATE: f64 = 0.05;

/// The names of the four scored conditions, in [`Verdict::met`]'s order.
pub const CONDITIONS: [&str; 4] = ["lands_per_s", "latency_p99", "retry_rate", "none_exhausted"];

/// How close to the lowest scored p99 a candidate that does less work under the lock may come
/// and still be chosen over it: within 10 %.
pub const P99_MARGIN: f64 = 0.10;

/// Op heads is chosen only with a scored p99 at most this share of the best other eligible
/// candidate's: half.
pub const OP_HEADS_SHARE: f64 = 0.5;

/// The tag of the lines [`Decision::lines`] gives.
pub const DECISION_TAG: &str = "landing-decision: ";

/// How one model fared under Decision 1's rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// The model.
    pub model: String,
    /// Its runs, in every cell.
    pub runs: usize,
    /// How many of them failed the gate.
    pub failed_gates: usize,
    /// How many of them ran in the scored cell.
    pub scored_runs: usize,
    /// The median over its scored runs of their lands/s.
    pub lands_per_s: f64,
    /// The median over its scored runs of their p99 latency, in microseconds.
    pub latency_p99_us: f64,
    /// The median over its scored runs of their retry rate.
    pub retry_rate: f64,
    /// The lands of its scored runs that exhausted their attempts, all runs together.
    pub exhausted: u64,
    /// Which of the four scored conditions ([`CONDITIONS`]) those meet.
    pub met: [bool; 4],
}

impl Verdict {
    /// Whether the model is eligible: the gate passed in every run of every cell, and the four
    /// scored conditions met.
    pub fn eligible(&self) -> bool {
        self.failed_gates == 0 && self.met.iter().all(|&met| met)
    }

    /// How many of the four scored conditions it meets.
    pub fn conditions_met(&self) -> usize {
        self.met.iter().filter(|&&met| met).count()
    }

    /// The verdict as one line of `key=value` fields.
    fn line(&self) -> String {
        let met: Vec<&str> = CONDITIONS
            .iter()
            .zip(self.met)
            .filter_map(|(name, met)| met.then_some(*name))
            .collect();
        let role = if self.model == YARDSTICK {
            "yardstick"
        } else {
            "candidate"
        };
        format!(
            "{DECISION_TAG}model={} role={role} runs={} failed_gates={} scored_runs={} \
             lands_per_s={:.3} latency_p99_us={:.0} retry_pct={:.3} exhausted={} met={} \
             eligible={}",
            self.model,
            self.runs,
            self.failed_gates,
            self.scored_runs,
            self.lands_per_s,
            self.latency_p99_us,
            self.retry_rate * 100.0,
            self.exhausted,
            if met.is_empty() {
                "-".to_owned()
            } else {
                met.join(",")
            },
            if self.eligible() { "yes" } else { "no" },
        )
    }
}

/// Decision 1's rule applied to a matrix's runs ([`decide`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Every model's verdict: the yardstick's first, then the candidates' in [`CANDIDATES`]'s
    /// order.
    pub verdicts: Vec<Verdict>,
    /// The candidate chosen.
    pub chosen: String,
    /// Whether it is eligible. If it is not, no candidate is: the M4 plan sends the gap to
    /// `docs/known-limitations.md` (target M9) and tells the user, decision 10's alternative
    /// being theirs to take.
    pub eligible: bool,
    /// The clauses of the rule that chose it, in words.
    pub reason: String,
}

impl Decision {
    /// The decision as lines, each starting [`DECISION_TAG`]: one per verdict, then the choice.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.verdicts.iter().map(Verdict::line).collect();
        lines.push(format!(
            "{DECISION_TAG}chosen={} eligible={} reason: {}",
            self.chosen,
            if self.eligible { "yes" } else { "no" },
            self.reason
        ));
        lines
    }
}

/// The M4 plan's Decision 1, applied to `runs`, every run of a matrix (several cells, each
/// model, several runs of each), scored in cell `scored` ([`SCORED_CELL`] for the plan's
/// matrix; a smoke run of the matrix scores its own small open loop):
///
/// - a model is *eligible* when it passed the gate in every one of its runs, in every cell, and
///   its runs in `scored` meet the four scored conditions: a median of at least
///   [`MIN_LANDS_PER_S`] lands/s, a median p99 latency under [`MAX_LATENCY_P99_US`], a median
///   retry rate under [`MAX_RETRY_RATE`], and no land that exhausted its attempts in any of
///   them ([`Verdict`]);
/// - the *choice* is among the eligible [`CANDIDATES`], never the [`YARDSTICK`]: op heads only
///   with a scored p99 at most [`OP_HEADS_SHARE`] of the best other eligible candidate's (with
///   no other eligible candidate, there is none to beat); of those left, the one with the
///   lowest scored p99, unless one that does less work under the lock (an earlier one in
///   [`CANDIDATES`]) has a p99 within [`P99_MARGIN`] of it, the earliest such;
/// - with *no eligible candidate*, the candidate that meets the most of the four scored
///   conditions, ties broken by the lower scored retry rate, and a tie the rule leaves after
///   that by the order of work ([`CANDIDATES`]); [`Decision::eligible`] is then `false`.
///
/// An error when the yardstick or a candidate has no run in the scored cell.
pub fn decide(runs: &[Run], scored: Cell) -> Result<Decision, String> {
    let verdicts = std::iter::once(YARDSTICK)
        .chain(CANDIDATES)
        .map(|model| verdict(runs, model, scored))
        .collect::<Result<Vec<Verdict>, String>>()?;
    let candidates = &verdicts[1..];
    let eligible: Vec<&Verdict> = candidates.iter().filter(|v| v.eligible()).collect();
    let (chosen, reason) = if eligible.is_empty() {
        fallback(candidates)
    } else {
        choose(&eligible)
    };
    Ok(Decision {
        chosen: chosen.model.clone(),
        eligible: chosen.eligible(),
        reason,
        verdicts,
    })
}

/// How `model` fared in `runs`, scored in cell `cell`; an error if it has no run there.
fn verdict(runs: &[Run], model: &str, cell: Cell) -> Result<Verdict, String> {
    let all: Vec<&Run> = runs.iter().filter(|run| run.model == model).collect();
    let scored: Vec<&Run> = all.iter().copied().filter(|run| run.cell == cell).collect();
    let median_of = |pick: fn(&Run) -> f64| {
        let values: Vec<f64> = scored.iter().map(|run| pick(run)).collect();
        median(&values).ok_or_else(|| format!("{model} has no run in the scored cell"))
    };
    let lands_per_s = median_of(|run| run.stats.lands_per_s)?;
    let latency_p99_us = median_of(|run| as_f64(run.stats.latency_p99_us))?;
    let retry_rate = median_of(|run| run.stats.retry_rate)?;
    let exhausted = scored.iter().map(|run| run.stats.exhausted).sum();
    Ok(Verdict {
        model: model.to_owned(),
        runs: all.len(),
        failed_gates: all.iter().filter(|run| !run.gate).count(),
        scored_runs: scored.len(),
        lands_per_s,
        latency_p99_us,
        retry_rate,
        exhausted,
        met: [
            lands_per_s >= MIN_LANDS_PER_S,
            latency_p99_us < MAX_LATENCY_P99_US,
            retry_rate < MAX_RETRY_RATE,
            exhausted == 0,
        ],
    })
}

/// Decision 1's choice among `eligible` (in [`CANDIDATES`]'s order, at least one): see
/// [`decide`].
fn choose<'v>(eligible: &[&'v Verdict]) -> (&'v Verdict, String) {
    let ms = |v: &Verdict| v.latency_p99_us / 1_000.0;
    let names: Vec<&str> = eligible.iter().map(|v| v.model.as_str()).collect();
    let mut reason = format!("eligible: {}", names.join(", "));
    let best_other = eligible
        .iter()
        .filter(|v| v.model != OP_HEADS)
        .map(|v| v.latency_p99_us)
        .reduce(f64::min);
    let mut choosable: Vec<&Verdict> = Vec::new();
    for &v in eligible {
        match best_other {
            Some(best) if v.model == OP_HEADS && v.latency_p99_us > OP_HEADS_SHARE * best => {
                let _ = write!(
                    reason,
                    "; op_heads's scored p99 {:.1} ms is more than half the best other's, \
                     {:.1} ms",
                    ms(v),
                    best / 1_000.0
                );
            }
            _ => choosable.push(v),
        }
    }
    let lowest = choosable
        .iter()
        .copied()
        .reduce(|a, b| {
            if b.latency_p99_us < a.latency_p99_us {
                b
            } else {
                a
            }
        })
        .expect("an eligible candidate");
    let chosen = choosable
        .iter()
        .copied()
        .find(|v| v.latency_p99_us <= lowest.latency_p99_us * (1.0 + P99_MARGIN))
        .expect("the lowest is within its own margin");
    let _ = write!(
        reason,
        "; the lowest scored p99: {} at {:.1} ms",
        lowest.model,
        ms(lowest)
    );
    if chosen.model != lowest.model {
        let _ = write!(
            reason,
            "; within 10 % of it, less work under the lock: {} at {:.1} ms",
            chosen.model,
            ms(chosen)
        );
    }
    (chosen, reason)
}

/// Decision 1's "no eligible candidate": see [`decide`].
fn fallback(candidates: &[Verdict]) -> (&Verdict, String) {
    let mut best = &candidates[0];
    for v in &candidates[1..] {
        let more = v.conditions_met() > best.conditions_met();
        let as_many_retrying_less =
            v.conditions_met() == best.conditions_met() && v.retry_rate < best.retry_rate;
        if more || as_many_retrying_less {
            best = v;
        }
    }
    let reason = format!(
        "no eligible candidate; the most scored conditions met: {} with {} of 4 ({}), \
         scored retry rate {:.3} %",
        best.model,
        best.conditions_met(),
        CONDITIONS
            .iter()
            .zip(best.met)
            .filter_map(|(name, met)| met.then_some(*name))
            .collect::<Vec<_>>()
            .join(", "),
        best.retry_rate * 100.0
    );
    (best, reason)
}

/// One Markdown table per cell that `runs` cover, in the cells' order, as `docs/perf.md`
/// gives them: a heading naming the cell, then a row per model (the yardstick first, then the
/// candidates, then any other) with how many of its runs passed the gate, and each number as
/// the median over its runs with the range `[min–max]` after it; the load and the raw
/// `fdatasync` probe are the runs' starts and ends, as a range.
pub fn tables(runs: &[Run]) -> String {
    let mut cells: Vec<Cell> = runs.iter().map(|run| run.cell).collect();
    cells.sort();
    cells.dedup();
    let mut models: Vec<&str> = std::iter::once(YARDSTICK).chain(CANDIDATES).collect();
    for run in runs {
        if !models.contains(&run.model.as_str()) {
            models.push(&run.model);
        }
    }
    let mut out = String::new();
    for cell in cells {
        let _ = write!(out, "\n{}\n\n", cell.describe());
        out.push_str(
            "| model | gate | lands/s | p50 ms | p99 ms | retries % | conflicts | exhausted | \
             busy | lock p50 ms | lock p99 ms | journal % | ref % | relocated | load | \
             fdatasync ms |\n",
        );
        let _ = writeln!(out, "|{}", "---|".repeat(16));
        for &model in &models {
            let of: Vec<&Run> = runs
                .iter()
                .filter(|run| run.cell == cell && run.model == model)
                .collect();
            if !of.is_empty() {
                out.push_str(&row(model, &of));
            }
        }
    }
    out
}

/// A table row of `model`'s runs `of` in one cell: see [`tables`].
fn row(model: &str, of: &[&Run]) -> String {
    let spread = |pick: &dyn Fn(&Run) -> f64, decimals: usize| {
        let values: Vec<f64> = of.iter().map(|run| pick(run)).collect();
        let median = median(&values).expect("a run");
        let (min, max) = values
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        format!("{median:.decimals$} [{min:.decimals$}–{max:.decimals$}]")
    };
    let ms = |us: u64| as_f64(us) / 1_000.0;
    let range = |values: Vec<f64>, decimals: usize| {
        let (min, max) = values
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        format!("{min:.decimals$}–{max:.decimals$}")
    };
    let loads = of
        .iter()
        .flat_map(|run| [run.host.load_start, run.host.load_end])
        .collect();
    let probes = of
        .iter()
        .flat_map(|run| {
            [
                ms(run.host.fdatasync_start_us),
                ms(run.host.fdatasync_end_us),
            ]
        })
        .collect();
    let cells = [
        model.to_owned(),
        format!("{}/{}", of.iter().filter(|run| run.gate).count(), of.len()),
        spread(&|run| run.stats.lands_per_s, 1),
        spread(&|run| ms(run.stats.latency_p50_us), 1),
        spread(&|run| ms(run.stats.latency_p99_us), 1),
        spread(&|run| run.stats.retry_rate * 100.0, 2),
        spread(&|run| as_f64(run.stats.conflicts), 0),
        spread(&|run| as_f64(run.stats.exhausted), 0),
        spread(&|run| as_f64(run.stats.busy), 0),
        spread(&|run| ms(run.stats.lock_hold_p50_us), 1),
        spread(&|run| ms(run.stats.lock_hold_p99_us), 1),
        spread(&|run| run.stats.journal_share * 100.0, 1),
        spread(&|run| run.stats.ref_share * 100.0, 1),
        spread(&|run| as_f64(run.stats.relocations), 0),
        range(loads, 1),
        range(probes, 1),
    ];
    format!("| {} |\n", cells.join(" | "))
}

/// The host's 1-minute load average now; NaN if `/proc/loadavg` does not say.
pub fn load1() -> f64 {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|loads| loads.split_whitespace().next()?.parse().ok())
        .unwrap_or(f64::NAN)
}

/// The raw `fdatasync` probe of `docs/perf.md`: the median of `count` appends of 120 bytes to a
/// fresh file in `dir`, each followed by `sync_data`. The file is removed after.
pub fn fdatasync_probe(dir: &Path, count: usize) -> io::Result<Duration> {
    let path = dir.join(format!("fdatasync-probe-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)?;
    let mut times = Vec::with_capacity(count);
    for _ in 0..count.max(1) {
        let started = Instant::now();
        file.write_all(&[0x70; 120])?;
        file.sync_data()?;
        times.push(started.elapsed());
    }
    drop(file);
    fs::remove_file(&path)?;
    times.sort_unstable();
    Ok(times[times.len() / 2])
}

/// The median of `values` (the lower middle one for an even count); `None` for none.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    let at = sorted.len().checked_sub(1)? / 2;
    Some(sorted[at])
}

/// The lines of `text` that start with `prefix`, the prefix taken off; for reading what a
/// run printed.
pub fn tagged<'t>(text: &'t str, prefix: &'t str) -> impl Iterator<Item = &'t str> {
    text.lines()
        .filter_map(move |line| line.strip_prefix(prefix))
}

/// [`BTreeMap`] of `key=value` fields, for a caller that reads a line's other fields: the same
/// split [`Stats::parse`] and [`LandRecord::parse`] use.
pub fn line_fields(line: &str) -> BTreeMap<String, String> {
    fields(line).into_iter().collect()
}
