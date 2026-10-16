//! Markdown tables of the records: per workload, one summary row per engine (the median over
//! its runs, `[min–max]` beside it, and the load and probe of those runs), and one detail row
//! per run.

use std::fmt::Write;

use crate::measure::median_range;
use crate::workloads::Record;

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
