//! Markdown tables of the records: per workload, one summary row per engine (the median over
//! its runs, `[min–max]` beside it, and the load and probe of those runs), and one detail row
//! per run.

use std::fmt::Write;

use crate::measure::median_range;
use crate::workloads::Record;
use crate::workloads::sync::PerCommit;

/// One column: a value's key and its header.
pub struct Col(pub &'static str, pub &'static str);

/// A number to about three significant digits, thousands grouped.
pub fn num(v: f64) -> String {
    if v.is_nan() {
        return "–".into();
    }
    let a = v.abs();
    if a >= 1_000.0 {
        let s = format!("{:.0}", v.abs());
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        if v < 0.0 { format!("-{out}") } else { out }
    } else if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else if a >= 1.0 || a == 0.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

/// `median [min–max]` of `values`, or the one value.
pub fn cell(values: &[f64]) -> String {
    let mut v = values.to_vec();
    let (median, lo, hi) = median_range(&mut v);
    if v.len() <= 1 || num(lo) == num(hi) {
        num(median)
    } else {
        format!("{} [{}–{}]", num(median), num(lo), num(hi))
    }
}

fn engines(records: &[&Record]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in records {
        if !out.contains(&r.engine) {
            out.push(r.engine.clone());
        }
    }
    out
}

fn select<'a>(records: &'a [Record], workload: &str, variant: &str) -> Vec<&'a Record> {
    records
        .iter()
        .filter(|r| r.workload == workload && r.variant == variant)
        .collect()
}

/// The load and probe of `runs`: `12.1/11.8/13.0 (11.2–15.3)`, `3.9–4.4 ms`.
fn env(runs: &[&Record]) -> (String, String) {
    let starts: Vec<String> = runs
        .iter()
        .map(|r| format!("{:.1}", r.env.load_start))
        .collect();
    let lo = runs
        .iter()
        .map(|r| r.env.load_min)
        .fold(f64::INFINITY, f64::min);
    let hi = runs
        .iter()
        .map(|r| r.env.load_max)
        .fold(f64::NEG_INFINITY, f64::max);
    let probes: Vec<f64> = runs
        .iter()
        .flat_map(|r| [r.env.probe_start_ms, r.env.probe_end_ms])
        .collect();
    let plo = probes.iter().copied().fold(f64::INFINITY, f64::min);
    let phi = probes.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (
        format!("{} ({lo:.1}–{hi:.1})", starts.join("/")),
        format!("{plo:.1}–{phi:.1} ms"),
    )
}

pub fn summary(records: &[Record], workload: &str, variant: &str, cols: &[Col]) -> String {
    let selected = select(records, workload, variant);
    if selected.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let _ = write!(out, "| engine |");
    for c in cols {
        let _ = write!(out, " {} |", c.1);
    }
    let _ = writeln!(
        out,
        " runs | load at start (range during) | raw fdatasync |"
    );
    let _ = writeln!(out, "|---|{}---|---|---|", "---|".repeat(cols.len()));
    for engine in engines(&selected) {
        let runs: Vec<&Record> = selected
            .iter()
            .copied()
            .filter(|r| r.engine == engine)
            .collect();
        let _ = write!(out, "| {engine} |");
        for c in cols {
            let values: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.values.get(c.0).copied())
                .collect();
            let _ = write!(out, " {} |", cell(&values));
        }
        let (load, probe) = env(&runs);
        let _ = writeln!(out, " {} | {load} | {probe} |", runs.len());
    }
    out
}

pub fn detail(records: &[Record], workload: &str, variant: &str, cols: &[Col]) -> String {
    let selected = select(records, workload, variant);
    if selected.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let _ = write!(out, "| engine | run |");
    for c in cols {
        let _ = write!(out, " {} |", c.1);
    }
    let _ = writeln!(
        out,
        " load at start (range) / end | fdatasync start / end |"
    );
    let _ = writeln!(out, "|---|---|{}---|---|", "---|".repeat(cols.len()));
    let mut sorted = selected.clone();
    sorted.sort_by(|a, b| a.engine.cmp(&b.engine).then(a.run.cmp(&b.run)));
    for r in sorted {
        let _ = write!(out, "| {} | {} |", r.engine, r.run);
        for c in cols {
            let _ = write!(
                out,
                " {} |",
                r.values.get(c.0).copied().map_or("–".into(), num)
            );
        }
        let _ = writeln!(
            out,
            " {} / {:.1} | {} |",
            r.env.load(),
            r.env.load_end,
            r.env.probe()
        );
    }
    out
}

/// The sync calls per commit: one row per engine, each cell the median `[min–max]` over its
/// runs (one count per engine and run in `syncs`), and the calls by file, most first, of the
/// files whose median is at least 0.01 per commit.
pub fn sync_table(syncs: &[PerCommit]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "| engine | fsync | fdatasync | sync_file_range | all syncs | syscalls | bytes written | runs | sync calls by file (per commit, ≥ 0.01) |\n|---|---|---|---|---|---|---|---|---|"
    );
    let mut engines: Vec<&str> = Vec::new();
    for s in syncs {
        if !engines.contains(&s.engine.as_str()) {
            engines.push(&s.engine);
        }
    }
    for engine in engines {
        let runs: Vec<&PerCommit> = syncs.iter().filter(|s| s.engine == engine).collect();
        let col =
            |f: &dyn Fn(&PerCommit) -> f64| cell(&runs.iter().map(|s| f(s)).collect::<Vec<_>>());
        let by_name = |k: &str| col(&|s| s.by_name.get(k).copied().unwrap_or(0.0));
        let mut files: Vec<&String> = runs.iter().flat_map(|s| s.syncs_by_target.keys()).collect();
        files.sort_unstable();
        files.dedup();
        let mut files: Vec<(f64, &String, Vec<f64>)> = files
            .into_iter()
            .map(|file| {
                let values: Vec<f64> = runs
                    .iter()
                    .map(|s| s.syncs_by_target.get(file).copied().unwrap_or(0.0))
                    .collect();
                (median_range(&mut values.clone()).0, file, values)
            })
            .filter(|(median, _, _)| *median >= 0.01)
            .collect();
        files.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
        let files: Vec<String> = files
            .iter()
            .map(|(_, file, values)| format!("{file} {}", cell(values)))
            .collect();
        let _ = writeln!(
            out,
            "| {engine} | {} | {} | {} | {} | {} | {} | {} | {} |",
            by_name("fsync"),
            by_name("fdatasync"),
            by_name("sync_file_range"),
            col(&PerCommit::syncs),
            col(&|s| s.syscalls),
            col(&|s| s.written_bytes),
            runs.len(),
            files.join("; ")
        );
    }
    out
}

/// How many times `base`'s median each other engine's median is, column by column, on
/// `workload`'s `variant`: one row per engine, with the row's range. For a latency, above 1
/// means that `base` is slower. Empty if `base` has no record there.
pub fn ratios(
    records: &[Record],
    workload: &str,
    variant: &str,
    base: &str,
    cols: &[Col],
) -> String {
    let selected = select(records, workload, variant);
    let median = |engine: &str, key: &str| {
        let mut values: Vec<f64> = selected
            .iter()
            .filter(|r| r.engine == engine)
            .filter_map(|r| r.values.get(key).copied())
            .collect();
        median_range(&mut values).0
    };
    if !selected.iter().any(|r| r.engine == base) {
        return String::new();
    }
    let mut out = String::new();
    let _ = write!(out, "| {base} ÷ |");
    for c in cols {
        let _ = write!(out, " {} |", c.1);
    }
    let _ = writeln!(out, " range |");
    let _ = writeln!(out, "|---|{}---|", "---|".repeat(cols.len()));
    for engine in engines(&selected).into_iter().filter(|e| e != base) {
        let row: Vec<f64> = cols
            .iter()
            .map(|c| median(base, c.0) / median(&engine, c.0))
            .collect();
        let _ = write!(out, "| {engine} |");
        for ratio in &row {
            let _ = write!(out, " {}× |", num(*ratio));
        }
        let finite: Vec<f64> = row.into_iter().filter(|r| r.is_finite()).collect();
        let lo = finite.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if finite.is_empty() {
            let _ = writeln!(out, " – |");
        } else {
            let _ = writeln!(out, " {}–{}× |", num(lo), num(hi));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Col, ratios, sync_table};
    use crate::measure::Env;
    use crate::workloads::Record;
    use crate::workloads::sync::PerCommit;

    fn count(engine: &str, run: usize, fsync: f64, wal: f64) -> PerCommit {
        PerCommit {
            engine: engine.to_owned(),
            run,
            txns: (64, 1_088),
            syscalls: 10.0 + fsync,
            written_bytes: 100.0,
            by_name: BTreeMap::from([("fsync".to_owned(), fsync)]),
            syncs_by_target: BTreeMap::from([
                ("fsync db-wal".to_owned(), wal),
                ("fsync rare".to_owned(), 0.001),
            ]),
        }
    }

    #[test]
    fn the_sync_table_gives_each_engine_the_median_and_range_of_its_runs() {
        let syncs = [
            count("a", 1, 1.0, 1.0),
            count("b", 1, 2.0, 2.0),
            count("a", 2, 1.2, 1.2),
            count("b", 2, 2.0, 2.0),
            count("a", 3, 1.1, 1.1),
            count("b", 3, 2.0, 2.0),
        ];
        let table = sync_table(&syncs);
        let rows: Vec<&str> = table.lines().skip(2).collect();
        assert_eq!(
            rows,
            [
                "| a | 1.10 [1.00–1.20] | 0.00 | 0.00 | 1.10 [1.00–1.20] | 11.1 [11.0–11.2] | 100 | 3 | fsync db-wal 1.10 [1.00–1.20] |",
                "| b | 2.00 | 0.00 | 0.00 | 2.00 | 12.0 | 100 | 3 | fsync db-wal 2.00 |",
            ]
        );
    }

    fn lookups(engine: &str, variant: &str, run: usize, get: f64, prefix: f64) -> Record {
        Record {
            workload: "w3",
            variant: variant.to_owned(),
            engine: engine.to_owned(),
            run,
            env: Env {
                load_start: 1.0,
                load_min: 1.0,
                load_max: 1.0,
                load_end: 1.0,
                probe_start_ms: 4.0,
                probe_end_ms: 4.0,
            },
            values: BTreeMap::from([
                ("get_p50_us".to_owned(), get),
                ("prefix_p50_us".to_owned(), prefix),
            ]),
        }
    }

    #[test]
    fn ratios_divide_the_base_engines_median_by_each_others_per_column() {
        let records = [
            lookups("native", "1M", 1, 10.0, 18.0),
            lookups("sqlite", "1M", 1, 2.0, 2.0),
            lookups("redb", "1M", 1, 1.0, 0.5),
            lookups("native", "1M", 2, 12.0, 20.0),
            lookups("sqlite", "1M", 2, 2.2, 2.5),
            lookups("native", "1M", 3, 11.0, 19.0),
            lookups("sqlite", "1M", 3, 2.4, 2.4),
            // Another size: not part of the 1M table.
            lookups("sqlite", "100k", 1, 0.001, 0.001),
        ];
        let cols = [Col("get_p50_us", "get"), Col("prefix_p50_us", "prefix")];
        let table = ratios(&records, "w3", "1M", "native", &cols);
        assert_eq!(
            table.lines().collect::<Vec<_>>(),
            [
                "| native ÷ | get | prefix | range |",
                "|---|---|---|---|",
                "| sqlite | 5.00× | 7.92× | 5.00–7.92× |",
                "| redb | 11.0× | 38.0× | 11.0–38.0× |",
            ]
        );
        assert_eq!(ratios(&records, "w3", "1M", "lmdb", &cols), "");
    }
}
