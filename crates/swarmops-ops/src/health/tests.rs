//! `health`'s computation over fixture lines (the M21 design §2.3, §4: G24, A60, R-8, S8,
//! S9), and the operation records' file.

use std::fs;
use std::io;

use camino::Utf8Path;
use swarmops_api::health::{
    AlertRule, Direction, Drift, Figure, HealthReq, HealthResp, Measure, OpRecord, Unknown,
};
use swarmops_api::{ErrorCode, ServeRecord, ServeService};

use super::{FILES, HEALTH_DIR, append_within, compute, read};

/// Now, in the fixtures: a Unix millisecond far from zero.
const AT: u64 = 10_000_000_000;

/// The recent window's start, by default.
const RECENT_START: u64 = AT - 300_000;

/// The baseline window's start, by default.
const BASELINE_START: u64 = RECENT_START - 3_600_000;

/// Microseconds in a millisecond.
const MS: u64 = 1_000;

/// A seeded generator (xorshift64): the same fixture on every run.
struct Seeded(u64);

impl Seeded {
    /// The next value below `bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

/// A land record started at `at` that took `elapsed_us`, in one attempt.
fn land(at: u64, elapsed_us: u64) -> OpRecord {
    let mut record = OpRecord::new(at, "land".to_owned(), format!("r{at}"));
    record.elapsed_us = elapsed_us;
    record.attempts = Some(1);
    record
}

/// `records` as the lines of the operation records' file.
fn lines(records: &[OpRecord]) -> Vec<io::Result<String>> {
    let line = |r: &OpRecord| Ok(serde_json::to_string(r).unwrap());
    records.iter().map(line).collect()
}

/// No serve records.
fn no_serve() -> Vec<io::Result<String>> {
    Vec::new()
}

/// The request of every default, at [`AT`].
fn at_now() -> HealthReq {
    let mut req = HealthReq::new();
    req.at = Some(AT);
    req
}

/// `records` read with the defaults at [`AT`], no serve record.
fn health(records: &[OpRecord]) -> HealthResp {
    compute(lines(records), no_serve(), &at_now()).unwrap()
}

/// The figure `name` of `resp`, unsplit.
fn figure<'a>(resp: &'a HealthResp, name: &str) -> &'a Figure {
    let found = resp.figures.iter().find(|f| f.name == name);
    found.unwrap_or_else(|| panic!("no figure {name} in {:?}", resp.figures))
}

/// `n` records spread evenly over `[start, start + span_ms)`, each built by `make` from its
/// start and its index.
fn spread(
    n: u64,
    start: u64,
    span_ms: u64,
    mut make: impl FnMut(u64, u64) -> OpRecord,
) -> Vec<OpRecord> {
    (0..n).map(|i| make(start + i * span_ms / n, i)).collect()
}

/// A land whose p99 moves from 50 ms to 120 ms while its median stays is marked on its p99,
/// and stays under the fixed 500 ms alert (G24).
#[test]
fn a_land_p99_from_50_to_120_ms_is_marked_and_raises_no_alert() {
    // 1. 1,000 samples a window: around 30 ms, the top 11 at the window's tail value.
    let mut seeded = Seeded(7);
    let mut window = |start: u64, span: u64, tail_ms: u64| {
        spread(1_000, start, span, |at, i| {
            let ms = if i >= 989 {
                tail_ms
            } else {
                28 + seeded.below(5)
            };
            land(at, ms * MS)
        })
    };
    let mut records = window(BASELINE_START, 3_600_000, 50);
    records.extend(window(RECENT_START, 300_000, 120));

    // 2. Marked on the p99, upward, at about 2.4×; no alert.
    let resp = health(&records);
    let latency = figure(&resp, "op.land.latency");
    let Drift::Departed {
        of,
        ratio_milli,
        direction,
    } = latency.drift
    else {
        panic!("op.land.latency is not marked: {latency:?}");
    };
    assert_eq!(
        (of, direction),
        (Measure::P99, Direction::Up),
        "{latency:?}"
    );
    assert!((2_300..=2_500).contains(&ratio_milli), "{latency:?}");
    assert_eq!(resp.alerts, [], "120 ms raises no alert");
}

/// A steady run, its counts at the same rate per second over 300 s and 3,600 s, marks
/// nothing (S8): every figure is steady.
#[test]
fn a_steady_run_marks_nothing() {
    // 1. One record each 300 ms in both windows, from one seeded distribution.
    let mut seeded = Seeded(11);
    let mut make = |at: u64, _: u64| {
        let mut record = land(at, (20 + seeded.below(21)) * MS);
        record.lock_wait_us = 100 + seeded.below(900);
        record.lock_hold_us = 1_000 + seeded.below(1_000);
        record.journal_syncs = 1;
        record.journal_sync_us = 2_000 + seeded.below(1_000);
        record.object_reads = 3;
        record.object_writes = 2;
        record
    };
    let mut records = spread(12_000, BASELINE_START, 3_600_000, &mut make);
    records.extend(spread(1_000, RECENT_START, 300_000, &mut make));

    // 2. Nothing departs, the counts among them; both windows covered whole.
    let resp = health(&records);
    for figure in &resp.figures {
        assert_eq!(figure.drift, Drift::Steady, "{figure:?}");
    }
    let count = figure(&resp, "op.land.count");
    assert_eq!((count.recent.sum, count.baseline.sum), (1_000, 12_000));
    let reads = figure(&resp, "objects.read");
    assert_eq!((reads.recent.sum, reads.baseline.sum), (3_000, 36_000));
    let covered = (resp.coverage.recent_s, resp.coverage.baseline_s);
    assert_eq!(covered, (300, 3_600), "{:?}", resp.coverage);
    assert_eq!(resp.alerts, [], "a steady run raises no alert");
}

/// Records in the recent window alone: every figure is unknown, never steady.
#[test]
fn an_empty_baseline_is_unknown_not_steady() {
    let records = spread(200, RECENT_START, 300_000, |at, _| land(at, 30 * MS));
    let resp = health(&records);
    assert!(!resp.figures.is_empty(), "the recent window has figures");
    for figure in &resp.figures {
        let empty = Drift::Unknown {
            reason: Unknown::BaselineEmpty,
        };
        assert_eq!(figure.drift, empty, "{figure:?}");
    }
}

/// Records that cover less than half the baseline window leave every figure unknown.
#[test]
fn a_short_baseline_is_unknown() {
    // 1. The baseline's last 1,000 s only, as after a rotation.
    let mut records = spread(400, RECENT_START - 1_000_000, 1_000_000, |at, _| {
        land(at, 30 * MS)
    });
    records.extend(spread(200, RECENT_START, 300_000, |at, _| {
        land(at, 30 * MS)
    }));

    // 2. Unknown, the coverage saying why.
    let resp = health(&records);
    assert_eq!(resp.coverage.baseline_s, 1_000, "{:?}", resp.coverage);
    for figure in &resp.figures {
        let short = Drift::Unknown {
            reason: Unknown::BaselineShort,
        };
        assert_eq!(figure.drift, short, "{figure:?}");
    }
}

/// A code the baseline never had, 25 times in the recent window, is marked: departed, at the
/// most ratio there is.
#[test]
fn a_code_new_to_the_window_is_marked() {
    // 1. A steady run, 25 of its recent records refused.
    let mut records = spread(12_000, BASELINE_START, 3_600_000, |at, _| land(at, 30 * MS));
    records.extend(spread(1_000, RECENT_START, 300_000, |at, i| {
        let mut record = land(at, 30 * MS);
        if i % 40 == 0 {
            record.exit = 1;
            record.code = Some(ErrorCode::Conflict);
            record.attempts = None;
        }
        record
    }));

    // 2. Its share departs upward at `u32::MAX`; the share counts the operations.
    let resp = health(&records);
    let code = figure(&resp, "code.E_CONFLICT");
    let marked = Drift::Departed {
        of: Measure::Rate,
        ratio_milli: u32::MAX,
        direction: Direction::Up,
    };
    assert_eq!(code.drift, marked, "{code:?}");
    assert_eq!((code.recent.sum, code.recent.count), (25, 1_000));
}

/// A land p99 past 500 ms raises the fixed alert, whatever the baseline (A60); an `E_BUSY`
/// raises its own.
#[test]
fn land_p99_over_500_ms_raises_the_alert() {
    // 1. 200 recent lands, 10 of them at 600 ms; one busy refusal.
    let mut records = spread(200, RECENT_START, 300_000, |at, i| {
        land(at, if i < 10 { 600 * MS } else { 40 * MS })
    });
    let mut busy = OpRecord::new(AT - 1_000, "commit".to_owned(), "b".to_owned());
    busy.exit = 4;
    busy.code = Some(ErrorCode::Busy);
    records.push(busy);

    // 2. Both alerts, in the rules' units.
    let resp = health(&records);
    let rules: Vec<(AlertRule, &str)> = (resp.alerts.iter())
        .map(|a| (a.rule, a.figure.as_str()))
        .collect();
    assert_eq!(
        rules,
        [
            (AlertRule::LandP99, "op.land.latency"),
            (AlertRule::Busy, "code.E_BUSY")
        ]
    );
    let p99 = &resp.alerts[0];
    assert_eq!(p99.limit, 500 * MS, "{p99:?}");
    assert!(p99.value.abs_diff(600 * MS) <= 600 * MS / 90, "{p99:?}");
}

/// The windows and the factor are in the request's schema and in the answer, defaults and
/// given ones alike; a bad combination is `E_INVALID_ARG`.
#[test]
fn the_windows_and_factor_are_in_the_schema_and_the_output() {
    // 1. The schema names them, with their defaults.
    let schema = schemars::schema_for!(HealthReq).to_value();
    for (name, default) in [
        ("recent_s", "300"),
        ("baseline_s", "3,600"),
        ("factor_milli", "2,000"),
    ] {
        let said = schema["properties"][name]["description"].as_str();
        assert!(
            said.is_some_and(|d| d.contains(default)),
            "{name}: {said:?}"
        );
    }

    // 2. The answer says the defaults, then the given ones.
    let defaults = health(&[]);
    let said = (
        defaults.recent_s,
        defaults.baseline_s,
        defaults.factor_milli,
    );
    assert_eq!(said, (300, 3_600, 2_000));
    let mut req = at_now();
    req.recent_s = Some(60);
    req.baseline_s = Some(600);
    req.factor_milli = Some(1_500);
    let given = compute(lines(&[]), no_serve(), &req).unwrap();
    assert_eq!(
        (given.recent_s, given.baseline_s, given.factor_milli),
        (60, 600, 1_500)
    );

    // 3. A baseline no longer than the recent window, a factor out of range, an empty window
    //    and a `since` after `at` are refused.
    let refused = |edit: &dyn Fn(&mut HealthReq)| {
        let mut req = at_now();
        edit(&mut req);
        compute(lines(&[]), no_serve(), &req).unwrap_err().code
    };
    let invalid = ErrorCode::InvalidArg.as_str();
    assert_eq!(refused(&|r| r.baseline_s = Some(300)), invalid);
    assert_eq!(refused(&|r| r.factor_milli = Some(1_000)), invalid);
    assert_eq!(refused(&|r| r.factor_milli = Some(100_001)), invalid);
    assert_eq!(refused(&|r| r.recent_s = Some(0)), invalid);
    assert_eq!(refused(&|r| r.since = Some(AT + 1)), invalid);
}

/// A torn last line, in the file as a crash leaves it, is skipped and counted; the lines
/// before it are read.
#[test]
fn a_torn_last_line_is_skipped_and_counted() {
    // 1. 30 records appended, the last line cut in half.
    let store = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(store.path()).unwrap();
    for i in 0..30 {
        append_within(root, &land(RECENT_START + i * 1_000, 30 * MS), u64::MAX).unwrap();
    }
    let path = store.path().join(HEALTH_DIR).join(FILES.current);
    let text = fs::read(&path).unwrap();
    let last = text[..text.len() - 1]
        .iter()
        .rposition(|&b| b == b'\n')
        .unwrap();
    fs::write(&path, &text[..last + 1 + (text.len() - last) / 2]).unwrap();

    // 2. 29 read, one skipped.
    let resp = read(root, &at_now()).unwrap();
    assert_eq!((resp.coverage.records, resp.coverage.skipped), (29, 1));
    assert_eq!(figure(&resp, "op.land.count").recent.sum, 29);
}

/// `maintain.age_s` counts from the last `maintain` that succeeded, however old, never from
/// a later one that failed (S9).
#[test]
fn maintain_age_reads_the_last_successful_run() {
    let maintain = |at: u64, exit: u8| {
        let mut record = OpRecord::new(at, "maintain".to_owned(), format!("m{at}"));
        record.exit = exit;
        record
    };
    let records = [
        maintain(AT - 9_000_000, 0),
        maintain(AT - 7_200_000, 0),
        maintain(AT - 60_000, 1),
    ];
    let resp = health(&records);
    let age = resp.gauges.iter().find(|g| g.name == "maintain.age_s");
    assert_eq!(age.and_then(|g| g.value), Some(7_200), "{:?}", resp.gauges);
}

/// No record of a `maintain` that succeeded gives no age: the gauge says it is unknown,
/// never that a run is young (S9).
#[test]
fn no_maintain_record_is_unknown_not_young() {
    let mut failed = OpRecord::new(AT - 60_000, "maintain".to_owned(), "m".to_owned());
    failed.exit = 1;
    for records in [vec![], vec![failed]] {
        let resp = health(&records);
        let names: Vec<(&str, Option<u64>)> = (resp.gauges.iter())
            .filter(|g| g.name.starts_with("maintain."))
            .map(|g| (g.name.as_str(), g.value))
            .collect();
        assert_eq!(names, [("maintain.last", None)], "{:?}", resp.gauges);
    }
}

/// Past the bound the operation records move to the one older file; `health/` is made one
/// level below a root that exists, never a root of its own.
#[test]
fn op_records_rotate_past_the_bound() {
    // 1. Three lines past a bound of two: rotated, then one line in a new file.
    let store = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(store.path()).unwrap();
    let record = land(AT, 1);
    let line_len = serde_json::to_vec(&record).unwrap().len() as u64 + 1;
    for _ in 0..3 {
        append_within(root, &record, line_len * 2).unwrap();
    }
    let dir = store.path().join(HEALTH_DIR);
    assert!(!dir.join(FILES.current).exists(), "rotated");
    let older = fs::read_to_string(dir.join(FILES.older)).unwrap();
    assert_eq!(older.lines().count(), 3);
    append_within(root, &record, line_len * 2).unwrap();
    let current = fs::read_to_string(dir.join(FILES.current)).unwrap();
    assert_eq!(current.lines().count(), 1);

    // 2. Both files read, older first.
    let resp = read(root, &at_now()).unwrap();
    assert_eq!(resp.coverage.skipped, 0);

    // 3. A root that is not there gets nothing.
    let gone = root.join("gone");
    assert!(append_within(&gone, &record, u64::MAX).is_err());
    assert!(!gone.exists(), "no root made");
}

/// The serve records add up in `health` beside the operations' (R-8): times per service and
/// side, bytes per service.
#[test]
fn serve_records_add_up_in_health() {
    let served: Vec<io::Result<String>> = (0..40)
        .map(|i| {
            let mut record =
                ServeRecord::new(RECENT_START + i * 1_000, "agent/a", ServeService::Fetch);
            record.prepare_ms = 10;
            record.send_ms = 5;
            record.bytes = 100;
            Ok(serde_json::to_string(&record).unwrap())
        })
        .collect();
    let resp = compute(lines(&[]), served, &at_now()).unwrap();
    let prepare = figure(&resp, "serve.fetch.prepare");
    assert_eq!(
        (prepare.recent.count, prepare.recent.p50),
        (40, Some(10 * MS))
    );
    assert_eq!(figure(&resp, "serve.fetch.bytes").recent.sum, 4_000);
    assert_eq!(resp.coverage.serve_records, 40);
}

/// With `by_identity` each figure splits by caller, and the parts add up to the whole.
#[test]
fn by_identity_splits_each_figure() {
    let records = spread(30, RECENT_START, 300_000, |at, i| {
        let mut record = land(at, 30 * MS);
        record.identity = Some(if i % 3 == 0 { "agent/a" } else { "agent/b" }.to_owned());
        record
    });
    let mut req = at_now();
    req.by_identity = true;
    let resp = compute(lines(&records), no_serve(), &req).unwrap();
    let counts: Vec<(Option<&str>, u64)> = (resp.figures.iter())
        .filter(|f| f.name == "op.land.count")
        .map(|f| (f.identity.as_deref(), f.recent.sum))
        .collect();
    assert_eq!(counts, [(Some("agent/a"), 10), (Some("agent/b"), 20)]);
}

/// With `since`, `total` adds up the records from then to `at`, uncompared.
#[test]
fn since_adds_a_total_window() {
    let records = spread(50, AT - 10_000_000, 10_000_000, |at, _| land(at, 30 * MS));
    let mut req = at_now();
    req.since = Some(AT - 5_000_000);
    let resp = compute(lines(&records), no_serve(), &req).unwrap();
    let total = resp.total.expect("a total with since");
    let count = total.iter().find(|f| f.name == "op.land.count").unwrap();
    assert_eq!(count.recent.sum, 25);
    let not = Drift::Unknown {
        reason: Unknown::NotCompared,
    };
    assert_eq!(count.drift, not);
}
