//! `health`'s reply as text (the M21 design §10.2): one sample a line, plain or in the
//! OpenMetrics text format.
//!
//! ```text
//!   figure op.land.latency ──FAMILIES──▶ swarmops_op_latency{op="land",window="recent",stat="p99"} 41000
//!   drift                  ──────────▶ swarmops_drift{figure="op.land.latency"} 0|1|2
//!   each alert rule        ──────────▶ swarmops_alert{rule="land_p99"} 0|1
//!   gauge records.recent_s ──FAMILIES──▶ swarmops_health_covered_seconds{window="recent"} 300
//! ```
//!
//! Pure: no I/O, no clock. Values are integers, samples carry no timestamp, and an absent value
//! is an absent line, never 0. Lines sort by family, then labels, so one reply is one text.
//! Family names and label sets are a contract once released: rows are added, never renamed.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use swarmops_api::health::{AlertRule, Drift, Figure, HealthResp, Stat};

/// The prefix of every family name.
pub const METRIC_PREFIX: &str = "swarmops_";

/// The figure and gauge names, by pattern, and the family and label names each is rendered as.
/// A `<label>` in a pattern matches one dot-free run of a name and becomes that label's value;
/// the label names are the pattern's, in order. A name no row matches is rendered as
/// `swarmops_unlisted{name="<name>"}`, never dropped.
pub const FAMILIES: &[(&str, &str, &[&str])] = &[
    ("changesets", "swarmops_changesets", &[]),
    ("code.<code>", "swarmops_code", &["code"]),
    ("journal.sync", "swarmops_journal_sync", &[]),
    ("land.retries", "swarmops_land_retries", &[]),
    ("lock.hold", "swarmops_lock_hold", &[]),
    ("lock.wait", "swarmops_lock_wait", &[]),
    ("maintain.age_s", "swarmops_maintain_age_s", &[]),
    ("maintain.last", "swarmops_maintain_last", &[]),
    ("objects.read", "swarmops_objects_read", &[]),
    ("objects.written", "swarmops_objects_written", &[]),
    ("op.<op>.count", "swarmops_op_count", &["op"]),
    ("op.<op>.latency", "swarmops_op_latency", &["op"]),
    (
        "records.<window>_s",
        "swarmops_health_covered_seconds",
        &["window"],
    ),
    ("refs", "swarmops_refs", &[]),
    ("serve.<verb>.bytes", "swarmops_serve_bytes", &["verb"]),
    ("serve.<verb>.prepare", "swarmops_serve_prepare", &["verb"]),
    ("serve.<verb>.send", "swarmops_serve_send", &["verb"]),
];

/// The family of a name no [`FAMILIES`] row matches.
const UNLISTED: &str = "swarmops_unlisted";

/// The families of the reply's fixed parts.
const DRIFT: &str = "swarmops_drift";
const ALERT: &str = "swarmops_alert";
const ALERT_VALUE: &str = "swarmops_alert_value";
const ALERT_LIMIT: &str = "swarmops_alert_limit";
const WINDOW_SECONDS: &str = "swarmops_health_window_seconds";
const FACTOR: &str = "swarmops_health_factor_milli";
const SKIPPED: &str = "swarmops_health_skipped_lines";

/// The rules every reply has a line for, raised or not.
const RULES: [AlertRule; 3] = [AlertRule::LandP99, AlertRule::RetryShare, AlertRule::Busy];

/// A drift as a number: steady, departed, unknown.
const STEADY: u64 = 0;
const DEPARTED: u64 = 1;
const UNKNOWN: u64 = 2;

/// The last line of the OpenMetrics form.
const EOF: &str = "# EOF\n";

/// The text forms of `health`'s reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TextForm {
    /// The sample lines alone.
    Plain,
    /// The OpenMetrics text format: each family's `# TYPE … gauge` line before its samples,
    /// `# EOF` last.
    OpenMetrics,
}

/// One sample: its family, its labels in order, its value.
type Line = (&'static str, Vec<(&'static str, String)>, u64);

/// The labels a [`FAMILIES`] row gives a name: the family and each capture under its label.
type Placed = (&'static str, Vec<(&'static str, String)>);

/// `resp` as text in `form`.
pub fn render(resp: &HealthResp, form: TextForm) -> String {
    // 1. Every sample, sorted by family, then labels.
    let mut lines = Vec::new();
    for figure in &resp.figures {
        figure_lines(figure, Windowed::Compared, &mut lines);
    }
    for figure in resp.total.iter().flatten() {
        figure_lines(figure, Windowed::Total, &mut lines);
    }
    gauge_lines(resp, &mut lines);
    alert_lines(resp, &mut lines);
    lines.sort();

    // 2. Each family's type line before its first sample, in the OpenMetrics form.
    let mut text = String::new();
    let mut previous = None;
    for (family, labels, value) in &lines {
        if form == TextForm::OpenMetrics && previous != Some(*family) {
            let _ = writeln!(text, "# TYPE {family} gauge");
        }
        previous = Some(*family);
        text.push_str(family);
        write_labels(&mut text, labels);
        let _ = writeln!(text, " {value}");
    }

    // 3. The OpenMetrics form's last line.
    if form == TextForm::OpenMetrics {
        text.push_str(EOF);
    }
    text
}

/// The family and labels the row `row` gives `name`, when its pattern matches.
fn family(
    row: &(&'static str, &'static str, &'static [&'static str]),
    name: &str,
) -> Option<Placed> {
    // 1. Segment by segment: literal, or a capture with what surrounds it.
    let (pattern, family, labels) = *row;
    let mut captures = Vec::new();
    let mut parts = name.split('.');
    for segment in pattern.split('.') {
        let part = parts.next()?;
        let Some((head, rest)) = segment.split_once('<') else {
            if segment != part {
                return None;
            }
            continue;
        };
        let (_, tail) = rest.split_once('>')?;
        let middle = part.strip_prefix(head)?.strip_suffix(tail)?;
        if middle.is_empty() {
            return None;
        }
        captures.push(middle.to_owned());
    }

    // 2. The whole name, each capture under its label.
    if parts.next().is_some() || captures.len() != labels.len() {
        return None;
    }
    Some((family, labels.iter().copied().zip(captures).collect()))
}

/// The family and labels of `name`: its row's, or the unlisted family's.
fn placed(name: &str) -> Placed {
    let found = FAMILIES.iter().find_map(|row| family(row, name));
    found.unwrap_or_else(|| (UNLISTED, vec![("name", name.to_owned())]))
}

/// Which windows a figure renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Windowed {
    /// Recent and baseline, and its drift.
    Compared,
    /// `total`'s one window, no drift.
    Total,
}

/// `figure`'s statistics in its windows, and its drift when compared.
fn figure_lines(figure: &Figure, windowed: Windowed, lines: &mut Vec<Line>) {
    // 1. Its family and labels, the identity after them when the reply is split.
    let (family, mut labels) = placed(&figure.name);
    if let Some(identity) = &figure.identity {
        labels.push(("identity", identity.clone()));
    }

    // 2. Each window's count and sum, and each percentile it has.
    let windows: &[(&str, &Stat)] = match windowed {
        Windowed::Compared => &[("recent", &figure.recent), ("baseline", &figure.baseline)],
        Windowed::Total => &[("total", &figure.recent)],
    };
    for (window, stat) in windows {
        let stats = [
            ("count", Some(stat.count)),
            ("sum", Some(stat.sum)),
            ("p50", stat.p50),
            ("p99", stat.p99),
        ];
        for (name, value) in stats {
            let Some(value) = value else {
                continue;
            };
            let mut at = labels.clone();
            at.extend([("window", (*window).to_owned()), ("stat", name.to_owned())]);
            lines.push((family, at, value));
        }
    }

    // 3. The drift, as a number.
    if windowed == Windowed::Total {
        return;
    }
    let mut at = vec![("figure", figure.name.clone())];
    if let Some(identity) = &figure.identity {
        at.push(("identity", identity.clone()));
    }
    lines.push((DRIFT, at, drift_number(figure.drift)));
}

/// A drift's number; one this renderer does not know is unknown.
fn drift_number(drift: Drift) -> u64 {
    match drift {
        Drift::Steady => STEADY,
        Drift::Departed { .. } => DEPARTED,
        _ => UNKNOWN,
    }
}

/// The windows, the factor, the skipped lines and each gauge that has a value.
fn gauge_lines(resp: &HealthResp, lines: &mut Vec<Line>) {
    // 1. The request's windows and factor, and the lines that did not read.
    let window = |name: &str| vec![("window", name.to_owned())];
    lines.push((WINDOW_SECONDS, window("recent"), resp.recent_s.into()));
    lines.push((WINDOW_SECONDS, window("baseline"), resp.baseline_s.into()));
    lines.push((FACTOR, Vec::new(), resp.factor_milli.into()));
    lines.push((SKIPPED, Vec::new(), resp.coverage.skipped));

    // 2. Each gauge with a value: an unknown one has no line.
    for gauge in &resp.gauges {
        let Some(value) = gauge.value else {
            continue;
        };
        let (family, labels) = placed(&gauge.name);
        lines.push((family, labels, value));
    }
}

/// Each rule's line, 1 when raised; each alert's value and limit.
fn alert_lines(resp: &HealthResp, lines: &mut Vec<Line>) {
    // 1. Every known rule at 0, then each raised one at 1.
    let mut rules: BTreeMap<String, u64> = RULES.iter().map(|r| (rule_name(*r), 0)).collect();
    for alert in &resp.alerts {
        rules.insert(rule_name(alert.rule), 1);
    }
    for (rule, raised) in rules {
        lines.push((ALERT, vec![("rule", rule)], raised));
    }

    // 2. What each alert read and the limit it passed.
    for alert in &resp.alerts {
        let at = vec![
            ("rule", rule_name(alert.rule)),
            ("figure", alert.figure.clone()),
        ];
        lines.push((ALERT_VALUE, at.clone(), alert.value));
        lines.push((ALERT_LIMIT, at, alert.limit));
    }
}

/// A rule's name as the reply's JSON spells it.
fn rule_name(rule: AlertRule) -> String {
    let value = serde_json::to_value(rule).ok();
    let name = value.as_ref().and_then(serde_json::Value::as_str);
    name.unwrap_or("unnamed").to_owned()
}

/// `{k="v",…}` with each value escaped; nothing for no label.
fn write_labels(text: &mut String, labels: &[(&str, String)]) {
    if labels.is_empty() {
        return;
    }
    text.push('{');
    for (index, (key, value)) in labels.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        let _ = write!(text, "{key}=\"{}\"", escape(value));
    }
    text.push('}');
}

/// A label value with its backslashes, quotes and newlines escaped.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;

    use serde_json::Value;
    use swarmops_api::health::{
        Alert, AlertRule, Direction, Drift, Figure, Gauge, HealthReq, HealthResp, Measure,
        OpRecord, Stat, Unit, Unknown,
    };
    use swarmops_api::{ErrorCode, ServeRecord, ServeService};
    use swarmops_testkit::openmetrics::{self, Sample};

    use super::{FAMILIES, METRIC_PREFIX, TextForm, family, render};
    use crate::health::compute;

    /// Now, in the fixtures: a Unix millisecond far from zero.
    const AT: u64 = 10_000_000_000;

    /// The recent window's start, by default.
    const RECENT_START: u64 = AT - 300_000;

    /// The baseline window's start, by default.
    const BASELINE_START: u64 = RECENT_START - 3_600_000;

    /// The family a name outside [`FAMILIES`] is rendered in.
    const UNLISTED: &str = "swarmops_unlisted";

    /// The request of every default, at [`AT`].
    fn at_now() -> HealthReq {
        let mut req = HealthReq::new();
        req.at = Some(AT);
        req
    }

    /// A record of `op` at `at` by `identity`, with every count the record carries.
    fn record(at: u64, op: &str, identity: &str) -> OpRecord {
        let mut record = OpRecord::new(at, op.to_owned(), format!("r{at}"));
        record.identity = Some(identity.to_owned());
        record.elapsed_us = 30_000 + at % 7;
        record.lock_wait_us = 10;
        record.lock_hold_us = 20;
        record.journal_syncs = 2;
        record.journal_sync_us = 300;
        record.object_reads = 3;
        record.object_writes = 1;
        record
    }

    /// Records in both windows that give every name `compute` emits: lands that retried and
    /// were refused busy, a maintain, and a served request of each service.
    fn every_name(req: &HealthReq) -> HealthResp {
        // 1. Forty lands in each window, one in four refused busy, each retried once.
        let mut ops = Vec::new();
        for i in 0..40 {
            for start in [BASELINE_START, RECENT_START] {
                let identity = if i % 2 == 0 { "agent/a" } else { "agent/b" };
                let mut land = record(start + i * 1_000, "land", identity);
                land.attempts = Some(2);
                if i % 4 == 0 {
                    land.code = Some(ErrorCode::Busy);
                }
                ops.push(land);
            }
        }
        let mut maintain = record(RECENT_START, "maintain", "agent/a");
        maintain.exit = 0;
        ops.push(maintain);

        // 2. A served request of each service in each window.
        let mut served = Vec::new();
        for service in [ServeService::Clone, ServeService::Fetch, ServeService::Push] {
            for start in [BASELINE_START, RECENT_START] {
                let mut r = ServeRecord::new(start + 5_000, "agent/a", service);
                r.prepare_ms = 4;
                r.send_ms = 6;
                r.bytes = 1_024;
                served.push(r);
            }
        }

        // 3. Both read as files' lines.
        let line = |r: String| -> io::Result<String> { Ok(r) };
        let ops = ops.iter().map(|r| line(serde_json::to_string(r).unwrap()));
        let served = served
            .iter()
            .map(|r| line(serde_json::to_string(r).unwrap()));
        compute(ops, served, req).unwrap()
    }

    /// A hand-built reply: one figure, no gauge, no alert.
    fn one_figure(name: &str, recent: Stat, drift: Drift) -> HealthResp {
        let mut resp = HealthResp::new(AT, 300, 3_600, 2_000);
        let baseline = Stat::new(0, 0);
        let figure = Figure::new(name.to_owned(), Unit::Micros, recent, baseline, drift);
        resp.figures.push(figure);
        resp
    }

    /// `resp` in the OpenMetrics form, read back by the test kit's parser.
    fn samples(resp: &HealthResp) -> Vec<Sample> {
        let text = render(resp, TextForm::OpenMetrics);
        openmetrics::parse(&text).unwrap_or_else(|e| panic!("{e:?} in\n{text}"))
    }

    /// The value of the sample `name` whose labels include every one of `labels`.
    fn value(samples: &[Sample], name: &str, labels: &[(&str, &str)]) -> Option<i64> {
        let has =
            |s: &Sample, (k, v): &(&str, &str)| s.labels.iter().any(|(sk, sv)| sk == k && sv == v);
        let found = samples
            .iter()
            .filter(|s| s.name == name)
            .find(|s| labels.iter().all(|l| has(s, l)));
        found.map(|s| s.value)
    }

    /// Whether some sample of `name` has the label `key`.
    fn has_label(samples: &[Sample], name: &str, key: &str) -> bool {
        let named = samples.iter().filter(|s| s.name == name);
        named
            .into_iter()
            .any(|s| s.labels.iter().any(|(k, _)| k == key))
    }

    /// The figure and gauge names of `resp` that match no [`FAMILIES`] row or more than one,
    /// and the names of the samples rendered in [`UNLISTED`].
    fn unplaced(resp: &HealthResp) -> Vec<String> {
        // 1. Every name the reply carries, its rows counted.
        let figures = resp.figures.iter().chain(resp.total.iter().flatten());
        let names = figures.map(|f| f.name.clone());
        let names: BTreeSet<String> = names
            .chain(resp.gauges.iter().map(|g| g.name.clone()))
            .collect();
        let mut wrong: Vec<String> = names
            .into_iter()
            .filter(|name| {
                FAMILIES
                    .iter()
                    .filter(|row| family(row, name).is_some())
                    .count()
                    != 1
            })
            .collect();

        // 2. Every sample that fell in the unlisted family.
        for sample in samples(resp).iter().filter(|s| s.name == UNLISTED) {
            wrong.push(format!("{:?}", sample.labels));
        }
        wrong
    }

    /// The rules of `rules` with no `swarmops_alert` line in `resp`'s text.
    fn rules_without_line(resp: &HealthResp, rules: &[String]) -> Vec<String> {
        let samples = samples(resp);
        let lined = |rule: &&String| value(&samples, "swarmops_alert", &[("rule", rule)]).is_none();
        rules.iter().filter(lined).cloned().collect()
    }

    /// Every string value an enum's schema lists: its `enum` arrays and its `const`s.
    fn schema_values(schema: &Value, out: &mut Vec<String>) {
        match schema {
            Value::Object(map) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("const", Value::String(s)) => out.push(s.clone()),
                        ("enum", Value::Array(values)) => {
                            out.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
                        }
                        _ => schema_values(value, out),
                    }
                }
            }
            Value::Array(values) => values.iter().for_each(|v| schema_values(v, out)),
            _ => {}
        }
    }

    /// Each figure's windows and statistics, each gauge with a value and each alert have their
    /// line, with the reply's value.
    #[test]
    fn every_figure_alert_and_gauge_has_its_line() {
        // 1. The reply of every name, rendered and read back.
        let resp = every_name(&at_now());
        let samples = samples(&resp);
        assert!(!resp.alerts.is_empty(), "the fixture raises an alert");

        // 2. Each figure: count and sum in both windows, each percentile it has.
        for figure in &resp.figures {
            let (fam, labels) = FAMILIES
                .iter()
                .find_map(|row| family(row, &figure.name))
                .unwrap_or_else(|| panic!("{} has a family", figure.name));
            for (window, stat) in [("recent", &figure.recent), ("baseline", &figure.baseline)] {
                let stats = [
                    ("count", Some(stat.count)),
                    ("sum", Some(stat.sum)),
                    ("p50", stat.p50),
                    ("p99", stat.p99),
                ];
                for (name, want) in stats {
                    let mut want_labels: Vec<(&str, &str)> =
                        labels.iter().map(|(k, v)| (*k, v.as_str())).collect();
                    want_labels.extend([("window", window), ("stat", name)]);
                    let got = value(&samples, fam, &want_labels);
                    let want = want.map(|v| i64::try_from(v).unwrap());
                    assert_eq!(got, want, "{} {window} {name}", figure.name);
                }
            }
        }

        // 3. Some names, spelled out.
        let land = [("op", "land"), ("window", "recent"), ("stat", "count")];
        assert_eq!(value(&samples, "swarmops_op_latency", &land), Some(40));
        let busy = [("code", "E_BUSY"), ("window", "recent"), ("stat", "sum")];
        assert_eq!(value(&samples, "swarmops_code", &busy), Some(10));
        let bytes = [("verb", "fetch"), ("window", "baseline"), ("stat", "sum")];
        assert_eq!(value(&samples, "swarmops_serve_bytes", &bytes), Some(1_024));

        // 4. Each gauge with a value; the recent and baseline coverage as one family.
        for gauge in &resp.gauges {
            let Some(want) = gauge.value else {
                continue;
            };
            let (fam, labels) = FAMILIES
                .iter()
                .find_map(|row| family(row, &gauge.name))
                .unwrap();
            let labels: Vec<(&str, &str)> = labels.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let got = value(&samples, fam, &labels);
            assert_eq!(got, Some(i64::try_from(want).unwrap()), "{}", gauge.name);
        }
        let covered = value(
            &samples,
            "swarmops_health_covered_seconds",
            &[("window", "recent")],
        );
        assert_eq!(covered, Some(300));

        // 5. Each alert: its rule's line at 1, its value and limit.
        for alert in &resp.alerts {
            let rule = serde_json::to_value(alert.rule).unwrap();
            let rule = rule.as_str().unwrap();
            assert_eq!(
                value(&samples, "swarmops_alert", &[("rule", rule)]),
                Some(1)
            );
            let at = [("rule", rule), ("figure", alert.figure.as_str())];
            let got = value(&samples, "swarmops_alert_value", &at);
            assert_eq!(
                got,
                Some(i64::try_from(alert.value).unwrap()),
                "{rule} value"
            );
            let got = value(&samples, "swarmops_alert_limit", &at);
            assert_eq!(
                got,
                Some(i64::try_from(alert.limit).unwrap()),
                "{rule} limit"
            );
        }

        // 6. The windows, the factor, the skipped lines.
        let recent = value(
            &samples,
            "swarmops_health_window_seconds",
            &[("window", "recent")],
        );
        assert_eq!(recent, Some(300));
        let baseline = value(
            &samples,
            "swarmops_health_window_seconds",
            &[("window", "baseline")],
        );
        assert_eq!(baseline, Some(3_600));
        assert_eq!(
            value(&samples, "swarmops_health_factor_milli", &[]),
            Some(2_000)
        );
        assert_eq!(
            value(&samples, "swarmops_health_skipped_lines", &[]),
            Some(0)
        );
    }

    /// A percentile the reply does not have, and a gauge without a value, have no line: an
    /// unknown is never written as 0.
    #[test]
    fn an_absent_percentile_has_no_line() {
        // 1. A figure without percentiles, and the unknown maintain gauge.
        let mut resp = one_figure("lock.wait", Stat::new(5, 50), Drift::Steady);
        resp.gauges
            .push(Gauge::new("maintain.last".to_owned(), None));
        let samples = samples(&resp);

        // 2. Its count and sum, no percentile, no gauge line.
        let at = |stat| [("window", "recent"), ("stat", stat)];
        assert_eq!(value(&samples, "swarmops_lock_wait", &at("count")), Some(5));
        assert_eq!(value(&samples, "swarmops_lock_wait", &at("sum")), Some(50));
        assert_eq!(value(&samples, "swarmops_lock_wait", &at("p50")), None);
        assert_eq!(value(&samples, "swarmops_lock_wait", &at("p99")), None);
        let text = render(&resp, TextForm::Plain);
        let percentile = text.contains("stat=\"p50\"") || text.contains("stat=\"p99\"");
        assert!(!percentile, "{text}");
        assert!(!text.contains("swarmops_maintain_last"), "{text}");
    }

    /// A drift is 0 steady, 1 departed, 2 unknown; each alert rule has a line, 1 raised and 0
    /// not.
    #[test]
    fn drift_and_alerts_are_numbers_with_a_fixed_meaning() {
        // 1. Three figures, one of each drift.
        let departed = Drift::Departed {
            of: Measure::P99,
            ratio_milli: 3_000,
            direction: Direction::Up,
        };
        let unknown = Drift::Unknown {
            reason: Unknown::FewSamples,
        };
        let mut resp = one_figure("lock.wait", Stat::new(1, 1), Drift::Steady);
        for (name, drift) in [("lock.hold", departed), ("journal.sync", unknown)] {
            let figure = Figure::new(
                name.to_owned(),
                Unit::Micros,
                Stat::new(1, 1),
                Stat::new(1, 1),
                drift,
            );
            resp.figures.push(figure);
        }
        let figure = "op.land.latency".to_owned();
        resp.alerts
            .push(Alert::new(AlertRule::LandP99, figure, 600_000, 500_000));
        let samples = samples(&resp);

        // 2. The drifts.
        let drift = |name| value(&samples, "swarmops_drift", &[("figure", name)]);
        assert_eq!(drift("lock.wait"), Some(0));
        assert_eq!(drift("lock.hold"), Some(1));
        assert_eq!(drift("journal.sync"), Some(2));

        // 3. The alerts: every rule, raised or not.
        let alert = |rule| value(&samples, "swarmops_alert", &[("rule", rule)]);
        assert_eq!(alert("land_p99"), Some(1));
        assert_eq!(alert("retry_share"), Some(0));
        assert_eq!(alert("busy"), Some(0));
    }

    /// The `identity` label is on a reply's lines only when the reply is split by identity.
    #[test]
    fn identities_are_labels_only_in_a_by_identity_reply() {
        // 1. Whole: no identity anywhere.
        let whole = render(&every_name(&at_now()), TextForm::OpenMetrics);
        assert!(!whole.contains("identity="), "{whole}");

        // 2. Split: each figure's lines and its drift carry the identity.
        let mut req = at_now();
        req.by_identity = true;
        let split = samples(&every_name(&req));
        assert!(has_label(&split, "swarmops_op_latency", "identity"));
        assert!(has_label(&split, "swarmops_drift", "identity"));
        let at = [
            ("op", "land"),
            ("identity", "agent/b"),
            ("window", "recent"),
            ("stat", "count"),
        ];
        assert_eq!(value(&split, "swarmops_op_count", &at), Some(20));
    }

    /// A label value's backslash, quote and newline are escaped, and read back as they were.
    #[test]
    fn label_values_are_escaped() {
        let odd = "a\\b\"c\nd";
        let resp = one_figure(&format!("op.{odd}.latency"), Stat::new(1, 1), Drift::Steady);
        let text = render(&resp, TextForm::Plain);
        assert!(text.contains("op=\"a\\\\b\\\"c\\nd\""), "{text}");
        let samples = samples(&resp);
        let op = [("op", odd), ("window", "recent"), ("stat", "count")];
        assert_eq!(value(&samples, "swarmops_op_latency", &op), Some(1));
    }

    /// The plain form is the OpenMetrics form without its `#` lines.
    #[test]
    fn the_two_forms_differ_only_in_comment_lines() {
        let resp = every_name(&at_now());
        let open = render(&resp, TextForm::OpenMetrics);
        let plain = render(&resp, TextForm::Plain);
        let stripped: String = open
            .split_inclusive('\n')
            .filter(|l| !l.starts_with('#'))
            .collect();
        assert_eq!(plain, stripped);
        assert!(!plain.contains('#'), "{plain}");
    }

    /// The OpenMetrics form ends in `# EOF`, each family typed once as a gauge, and parses.
    #[test]
    fn the_openmetrics_form_ends_in_eof_and_parses() {
        let resp = every_name(&at_now());
        let text = render(&resp, TextForm::OpenMetrics);
        assert!(text.ends_with("\n# EOF\n"), "{text}");
        let types = text.lines().filter(|l| l.starts_with("# TYPE "));
        assert!(types.clone().all(|l| l.ends_with(" gauge")), "{text}");
        let samples = samples(&resp);
        let families: BTreeSet<&str> = samples.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(types.count(), families.len(), "one # TYPE a family");
    }

    /// The same reply renders to the same bytes, in either form.
    #[test]
    fn render_is_a_function_of_the_reply() {
        let resp = every_name(&at_now());
        for form in [TextForm::Plain, TextForm::OpenMetrics] {
            assert_eq!(render(&resp, form), render(&resp.clone(), form), "{form:?}");
        }
    }

    /// A `since` reply's total figures carry `window="total"`, and no sample is twice.
    #[test]
    fn a_since_reply_has_a_total_window_and_no_twin_sample() {
        let mut req = at_now();
        req.since = Some(BASELINE_START);
        let resp = every_name(&req);
        let samples = samples(&resp);
        let total = [("op", "land"), ("window", "total"), ("stat", "count")];
        assert_eq!(value(&samples, "swarmops_op_count", &total), Some(80));
        let drifts = samples
            .iter()
            .filter(|s| s.name == "swarmops_drift")
            .count();
        assert_eq!(
            drifts,
            resp.figures.len(),
            "one drift a figure, none for the total"
        );
    }

    /// Every figure and gauge name `compute` can emit matches exactly one row, and none falls
    /// in the unlisted family.
    #[test]
    fn every_figure_name_has_one_family() {
        // 1. Whole, split and with a total.
        let mut split = at_now();
        split.by_identity = true;
        let mut since = at_now();
        since.since = Some(BASELINE_START);
        for req in [at_now(), split, since] {
            let resp = every_name(&req);
            assert_eq!(unplaced(&resp), Vec::<String>::new());
        }

        // 2. The unknown maintain gauge, which the fixture does not give.
        let mut resp = HealthResp::new(AT, 300, 3_600, 2_000);
        resp.gauges
            .push(Gauge::new("maintain.last".to_owned(), None));
        assert_eq!(unplaced(&resp), Vec::<String>::new());
    }

    /// A name no row matches is caught, and rendered under its name rather than dropped.
    #[test]
    fn a_figure_name_without_a_family_is_caught() {
        let resp = one_figure("brand.new", Stat::new(1, 1), Drift::Steady);
        let wrong = unplaced(&resp);
        assert!(wrong.iter().any(|w| w == "brand.new"), "{wrong:?}");
        let at = [
            ("name", "brand.new"),
            ("window", "recent"),
            ("stat", "count"),
        ];
        assert_eq!(value(&samples(&resp), UNLISTED, &at), Some(1));
    }

    /// Every rule `AlertRule`'s schema lists has its line in every reply.
    #[test]
    fn every_alert_rule_has_its_line() {
        let mut rules = Vec::new();
        schema_values(&schemars::schema_for!(AlertRule).to_value(), &mut rules);
        assert!(rules.len() >= 3, "{rules:?}");
        let resp = HealthResp::new(AT, 300, 3_600, 2_000);
        assert_eq!(rules_without_line(&resp, &rules), Vec::<String>::new());
    }

    /// A rule the renderer does not know is caught.
    #[test]
    fn an_alert_rule_without_a_line_is_caught() {
        let rules = vec!["busy".to_owned(), "planted_rule".to_owned()];
        let resp = HealthResp::new(AT, 300, 3_600, 2_000);
        assert_eq!(rules_without_line(&resp, &rules), ["planted_rule"]);
    }

    /// Each row's pattern captures exactly its label names, and each family is a valid name
    /// under the prefix.
    #[test]
    fn every_row_names_its_captures() {
        for (pattern, fam, labels) in FAMILIES {
            let captures: Vec<&str> = pattern
                .split('<')
                .skip(1)
                .filter_map(|rest| rest.split_once('>').map(|(name, _)| name))
                .collect();
            assert_eq!(&captures, labels, "{pattern}");
            assert!(fam.starts_with(METRIC_PREFIX), "{fam}");
        }
        let patterns: BTreeSet<&str> = FAMILIES.iter().map(|(p, _, _)| *p).collect();
        assert_eq!(patterns.len(), FAMILIES.len(), "one row a pattern");
    }
}
