//! swarmops-bench-compare: SwarmOps' native metadata store against SQLite, redb, fjall and
//! sled, on SwarmOps' own workloads (M1.5 Task 8). It exists only to measure the native store;
//! SwarmOps itself depends on none of these databases. See README.md; docs/perf.md records
//! the numbers.

#![forbid(unsafe_code)]

mod children;
mod data;
mod engines;
mod measure;
mod report;
mod trace;
mod workloads;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use engines::Engine;
use measure::{Env, recorded};
use report::{Col, detail, num, summary};
use workloads::sync::PerCommit;
use workloads::w2::Mode;
use workloads::{Plan, Record, Seed, Values, same_files_as, same_writes_as, size_name};

const USAGE: &str = "\
usage: swarmops-bench-compare [--quick] [--runs N] [--engines a,b,...] [--only w1,w2,...]
                              [--w1-txns N] [--w2-txns N] [--sizes N,N] [--out FILE]

  --quick     a smoke run of every workload at small sizes (under 2 minutes once built)
  --runs N    runs of each measurement (default 3)
  --engines   native, sqlite, sqlite-normal, redb, fjall, sled (default: all)
  --only      sync, breakdown, w1, w2, w3, w4, w5, w6 (default: all)
  --w1-txns N, --w2-txns N, --sizes N,N
              other sizes than the plan's (full: 10,000; 1,000 per writer; 100,000,1,000,000)
  --out FILE  the JSON record (default: target/results/bench-compare-<unix time>.json)

Stores are made under $SWARMOPS_BENCH_DIR, or ~/.local/state/swarmops-bench.";

/// The seeded stores of W3–W6, by engine and size.
type Seeds = BTreeMap<(String, u64), Seed>;

/// Each seeded store's size and its files, largest first, by engine and size.
type SeedFiles = BTreeMap<(String, u64), (measure::Size, Vec<(String, u64)>)>;

struct Options {
    plan: Plan,
    engines: Vec<Box<dyn Engine>>,
    only: Option<Vec<String>>,
    out: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut quick = false;
    let mut runs = None;
    let mut engines = None;
    let mut only = None;
    let mut out = None;
    let (mut w1_txns, mut w2_txns, mut sizes) = (None, None, None);
    let number = |v: String| {
        v.replace(['_', ','], "")
            .parse::<u64>()
            .map_err(|e| e.to_string())
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--quick" => quick = true,
            "--runs" => runs = Some(value()?.parse::<usize>().map_err(|e| e.to_string())?),
            "--engines" => engines = Some(value()?),
            "--only" => only = Some(value()?.split(',').map(str::to_owned).collect()),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--w1-txns" => w1_txns = Some(number(value()?)?),
            "--w2-txns" => w2_txns = Some(number(value()?)?),
            "--sizes" => {
                sizes = Some(
                    value()?
                        .split(',')
                        .map(|n| number(n.to_owned()))
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mut plan = if quick { Plan::quick() } else { Plan::full() };
    if let Some(runs) = runs {
        plan.runs = runs.max(1);
    }
    if let Some(n) = w1_txns {
        plan.w1_txns = n.max(1);
    }
    if let Some(n) = w2_txns {
        plan.w2_txns = n.max(1);
    }
    if let Some(sizes) = sizes {
        if sizes.is_empty() || sizes.contains(&0) {
            return Err("--sizes needs sizes above 0".into());
        }
        plan.sizes = sizes;
    }
    let engines = match engines {
        None => engines::all(),
        Some(list) => list
            .split(',')
            .map(|name| engines::by_name(name).ok_or(format!("no engine {name}")))
            .collect::<Result<_, _>>()?,
    };
    Ok(Options {
        plan,
        engines,
        only,
        out,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a.starts_with("__")) {
        return children::main(&args);
    }
    match parse(&args) {
        Ok(options) => orchestrate(&options),
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// The engines in run `run`'s order: rotated, so that no engine always goes first.
fn rotated(engines: &[Box<dyn Engine>], run: usize) -> Vec<&dyn Engine> {
    let mut order: Vec<&dyn Engine> = engines.iter().map(AsRef::as_ref).collect();
    let len = order.len().max(1);
    order.rotate_left((run - 1) % len);
    order
}

struct Bench<'a> {
    plan: &'a Plan,
    work: PathBuf,
    exe: PathBuf,
    started: Instant,
    records: Vec<Record>,
    errors: Vec<String>,
}

impl Bench<'_> {
    fn progress(&self, what: &str) {
        eprintln!(
            "[{:>7.1} s, load {:.1}] {what}",
            self.started.elapsed().as_secs_f64(),
            measure::load1()
        );
    }

    /// Runs one measurement, recording the host around it.
    fn measure(
        &mut self,
        workload: &'static str,
        variant: &str,
        engine: &str,
        run: usize,
        f: impl FnOnce() -> engines::Result<Values>,
    ) {
        self.progress(&format!(
            "{workload} {variant} {engine} run {run}/{}",
            self.plan.runs
        ));
        let (result, env) = recorded(&self.work, self.plan.probes, f);
        match result {
            Ok(values) => self.records.push(Record {
                workload,
                variant: variant.to_owned(),
                engine: engine.to_owned(),
                run,
                env,
                values,
            }),
            Err(e) => {
                let e = format!("{workload} {variant} {engine} run {run}: {e}");
                eprintln!("error: {e}");
                self.errors.push(e);
            }
        }
    }
}

#[allow(clippy::too_many_lines)] // the run, in order, as one script
fn orchestrate(options: &Options) -> ExitCode {
    let plan = &options.plan;
    let base = measure::base_dir();
    let work = match tempfile::Builder::new()
        .prefix("bench-compare-")
        .tempdir_in(&base)
    {
        Ok(work) => work,
        Err(e) => {
            eprintln!("cannot make a work dir under {}: {e}", base.display());
            return ExitCode::FAILURE;
        }
    };
    let exe = std::env::current_exe().expect("this binary's path");
    let wants = |w: &str| {
        options
            .only
            .as_ref()
            .is_none_or(|o| o.iter().any(|x| x == w))
    };
    let mut bench = Bench {
        plan,
        work: work.path().to_path_buf(),
        exe,
        started: Instant::now(),
        records: Vec::new(),
        errors: Vec::new(),
    };
    let host = host(&base, plan.probes);
    let engines = &options.engines;
    let runs = plan.runs;

    let mut syncs: Vec<PerCommit> = Vec::new();
    if wants("sync") {
        let (work, exe) = (bench.work.clone(), bench.exe.clone());
        let (counted, errors) = sync_runs(engines, runs, |engine, run| {
            bench.progress(&format!(
                "sync calls per commit: {} run {run}/{runs}",
                engine.name()
            ));
            workloads::sync::per_commit(engine, run, &work, plan.sync_txns, &exe)
        });
        syncs = counted;
        bench.errors.extend(errors);
    }

    if wants("breakdown") && engines.iter().any(|e| e.name() == "native") {
        let txns = plan.breakdown_txns;
        for run in 1..=runs {
            let work = bench.work.clone();
            bench.measure("breakdown", "W1", "native", run, || {
                workloads::breakdown::run(&work, plan.w1_warmup, txns)
            });
        }
    }

    if wants("w1") {
        for run in 1..=runs {
            for engine in rotated(engines, run) {
                if same_writes_as(engine.name()).is_some() {
                    continue;
                }
                let work = bench.work.clone();
                bench.measure("w1", "W1", engine.name(), run, || {
                    workloads::w1::run(engine, &work, plan.w1_warmup, plan.w1_txns)
                });
            }
        }
    }

    if wants("w2") {
        for run in 1..=runs {
            for engine in rotated(engines, run) {
                if same_writes_as(engine.name()).is_some() {
                    continue;
                }
                let mut modes = vec![if engine.multi_process() {
                    Mode::Processes
                } else {
                    Mode::Threads
                }];
                if engine.multi_process() && plan.w2_threads_too {
                    modes.push(Mode::Threads);
                }
                for mode in modes {
                    let (work, exe) = (bench.work.clone(), bench.exe.clone());
                    bench.measure("w2", mode.name(), engine.name(), run, || {
                        workloads::w2::run(engine, &work, plan.w2_writers, plan.w2_txns, mode, &exe)
                    });
                }
            }
        }
    }

    let (seeds, seed_files) = if ["w3", "w4", "w5", "w6"].iter().any(|w| wants(w)) {
        seed_all(&mut bench, engines, wants("w5").then_some(runs))
    } else {
        (Seeds::new(), SeedFiles::new())
    };
    let seed_of = |engine: &str, n: u64| {
        let name = same_files_as(engine).unwrap_or(engine);
        seeds.get(&(name.to_owned(), n))
    };
    let (smallest, largest) = (
        *plan.sizes.iter().min().expect("sizes"),
        *plan.sizes.iter().max().expect("sizes"),
    );

    if wants("w3") {
        for &n in &plan.sizes {
            for run in 1..=runs {
                for engine in rotated(engines, run) {
                    if same_files_as(engine.name()).is_some() {
                        continue;
                    }
                    let Some(seed) = seed_of(engine.name(), n) else {
                        continue;
                    };
                    bench.measure("w3", &size_name(n), engine.name(), run, || {
                        workloads::w3::run(engine, seed, plan.lookups)
                    });
                }
            }
        }
    }

    if wants("w4") {
        for run in 1..=runs {
            for engine in rotated(engines, run) {
                if same_files_as(engine.name()).is_some() {
                    continue;
                }
                let Some(seed) = seed_of(engine.name(), largest) else {
                    continue;
                };
                let exe = bench.exe.clone();
                bench.measure("w4-open", &size_name(largest), engine.name(), run, || {
                    workloads::w4::open(engine, seed, plan.w4_opens, &exe)
                });
            }
        }
        for run in 1..=runs {
            for engine in rotated(engines, run) {
                if same_files_as(engine.name()).is_some() {
                    continue;
                }
                let Some(seed) = seed_of(engine.name(), largest) else {
                    continue;
                };
                let exe = bench.exe.clone();
                let first = ((run - 1) * plan.w4_kills) as u64;
                bench.measure(
                    "w4-recover",
                    &size_name(largest),
                    engine.name(),
                    run,
                    || {
                        workloads::w4::recover(
                            engine,
                            seed,
                            first,
                            plan.w4_kills,
                            plan.w4_kill_after_ms,
                            &exe,
                        )
                    },
                );
            }
        }
    }

    if wants("w6") {
        for run in 1..=runs {
            for engine in rotated(engines, run) {
                if same_writes_as(engine.name()).is_some() {
                    continue;
                }
                let Some(seed) = seed_of(engine.name(), smallest) else {
                    continue;
                };
                let work = bench.work.clone();
                bench.measure("w6", &size_name(smallest), engine.name(), run, || {
                    workloads::w6::run(
                        engine,
                        seed,
                        &work,
                        run as u64,
                        plan.w6_idle_lookups,
                        plan.w6_busy,
                    )
                });
            }
        }
    }

    let text = render(
        &host,
        plan,
        engines,
        &bench.records,
        &syncs,
        &seed_files,
        &bench.errors,
    );
    println!("{text}");
    let out = options.out.clone().unwrap_or_else(|| {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/results/bench-compare-{unix}.json"))
    });
    let json = serde_json::json!({
        "host": host,
        "quick": plan.quick,
        "runs": plan.runs,
        "plan": format!("{plan:?}"),
        "engines": engines.iter().map(|e| serde_json::json!({
            "name": e.name(), "version": version(e.name()), "durability": e.durability(),
            "multi_process": e.multi_process(),
        })).collect::<Vec<_>>(),
        "records": bench.records,
        "syncs": syncs,
        "seeds": seeds.iter().map(|((e, n), s)| serde_json::json!({
            "engine": e, "n": n, "load_s": s.load_secs,
            "largest_files": seed_files.get(&(e.clone(), *n))
                .map(|(_, files)| files.iter().take(12).cloned().collect::<Vec<_>>()),
        })).collect::<Vec<_>>(),
        "errors": bench.errors,
        "total_s": bench.started.elapsed().as_secs_f64(),
    });
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(
        &out,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    ) {
        Ok(()) => eprintln!("the JSON record: {}", out.display()),
        Err(e) => eprintln!("cannot write {}: {e}", out.display()),
    }
    eprintln!("done in {:.1} s", bench.started.elapsed().as_secs_f64());
    drop(seeds);
    drop(work);
    if bench.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprintln!("{} measurement(s) failed:", bench.errors.len());
        for e in &bench.errors {
            eprintln!("  {e}");
        }
        ExitCode::FAILURE
    }
}

/// The sync count of every engine whose writes are its own, by `count` (an engine, the run),
/// once per run as every measurement, the engines in each run's rotated order.
fn sync_runs(
    engines: &[Box<dyn Engine>],
    runs: usize,
    mut count: impl FnMut(&dyn Engine, usize) -> engines::Result<PerCommit>,
) -> (Vec<PerCommit>, Vec<String>) {
    let (mut syncs, mut errors) = (Vec::new(), Vec::new());
    for run in 1..=runs {
        for engine in rotated(engines, run) {
            if same_writes_as(engine.name()).is_some() {
                continue;
            }
            match count(engine, run) {
                Ok(per) => syncs.push(per),
                Err(e) => errors.push(format!("sync {} run {run}: {e}", engine.name())),
            }
        }
    }
    (syncs, errors)
}

/// The seeded stores of W3–W6, one per engine whose files are its own and size, each with its
/// size and files taken once it is settled and closed, before any workload writes into it.
/// With `w5_runs`, W5 builds every store that many times, the engines in each run's rotated
/// order, and records each build's size: the first build's stores are kept for W3, W4 and W6,
/// the others deleted once measured.
fn seed_all(
    bench: &mut Bench<'_>,
    engines: &[Box<dyn Engine>],
    w5_runs: Option<usize>,
) -> (Seeds, SeedFiles) {
    let plan = bench.plan;
    let builds = w5_runs.unwrap_or(1).max(1);
    let (mut seeds, mut seed_files) = (Seeds::new(), SeedFiles::new());
    for build in 1..=builds {
        for engine in rotated(engines, build) {
            if same_files_as(engine.name()).is_some() {
                continue;
            }
            for &n in &plan.sizes {
                bench.progress(&format!(
                    "seeding {} with {}, build {build}/{builds}",
                    engine.name(),
                    size_name(n)
                ));
                let (seed, env) = recorded(&bench.work, plan.probes, || {
                    workloads::seed(engine, n, &bench.work)
                });
                let seed = match seed {
                    Ok(seed) => seed,
                    Err(e) => {
                        let e = format!("seed {} {n} build {build}: {e}", engine.name());
                        bench.errors.push(e);
                        continue;
                    }
                };
                let (size, files) = workloads::w3::size(&seed.dir);
                if w5_runs.is_some() {
                    let record =
                        w5_record(engine.name(), n, build, env, &size, &files, seed.load_secs);
                    bench.records.push(record);
                }
                if build == 1 {
                    let key = (engine.name().to_owned(), n);
                    seed_files.insert(key.clone(), (size, files));
                    seeds.insert(key, seed);
                }
            }
        }
    }
    (seeds, seed_files)
}

/// W5's record of one build of a store of `n` changesets: its size and files, settled and
/// closed, and how long the load took.
fn w5_record(
    engine: &str,
    n: u64,
    run: usize,
    env: Env,
    size: &measure::Size,
    files: &[(String, u64)],
    load_secs: f64,
) -> Record {
    let log: u64 = files
        .iter()
        .filter(|(f, _)| is_log(f))
        .map(|(_, b)| b)
        .sum();
    Record {
        workload: "w5",
        variant: size_name(n),
        engine: engine.to_owned(),
        run,
        env,
        values: Values::from([
            ("allocated_mib".into(), size.allocated as f64 / 1_048_576.0),
            ("apparent_mib".into(), size.apparent as f64 / 1_048_576.0),
            ("bytes_per_record".into(), size.allocated as f64 / n as f64),
            ("files".into(), size.files as f64),
            ("log_mib".into(), log as f64 / 1_048_576.0),
            ("load_s".into(), load_secs),
        ]),
    }
}

/// Whether a store file (relative to the store) is the engine's log rather than its main
/// structure: the native journal, SQLite's WAL, fjall's journals. (redb has no log; sled's
/// log is its one data file.)
fn is_log(file: &str) -> bool {
    file.starts_with("meta/journal.")
        || file.ends_with("-wal")
        || std::path::Path::new(file)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("jnl"))
}

fn version(engine: &str) -> String {
    match engine {
        "native" => "swarmops-store 0.1.0 (this tree)".into(),
        "sqlite" | "sqlite-normal" => {
            format!("rusqlite 0.40.2, SQLite {} (bundled)", rusqlite::version())
        }
        "redb" => "redb 4.3.0".into(),
        "fjall" | "fjall-unflushed" => "fjall 3.1.12 (lsm-tree 3.1.10)".into(),
        "sled" => "sled 0.34.7 (2021-09-12, the last stable release)".into(),
        _ => "?".into(),
    }
}

/// The host, as the report's header gives it.
fn host(base: &Path, probes: usize) -> serde_json::Value {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let cpu = read("/proc/cpuinfo")
        .lines()
        .find_map(|l| {
            l.strip_prefix("model name")?
                .split(':')
                .nth(1)
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_default();
    let mem = read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:").map(|s| s.trim().to_owned()))
        .unwrap_or_default();
    let base_s = base.display().to_string();
    let mount = read("/proc/self/mounts")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4 && base_s.starts_with(f[1])).then(|| {
                (
                    f[1].len(),
                    format!("{} {} on {} ({})", f[2], f[0], f[1], f[3]),
                )
            })
        })
        .max()
        .map(|(_, m)| m)
        .unwrap_or_default();
    let git = Command::new("git")
        .args([
            "-C",
            env!("CARGO_MANIFEST_DIR"),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    serde_json::json!({
        "date": date,
        "git": git,
        "kernel": read("/proc/sys/kernel/osrelease").trim(),
        "cpu": cpu,
        "threads": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        "memory": mem,
        "base_dir": base_s,
        "filesystem": mount,
        "load_at_start": measure::load1(),
        "fdatasync_probe_ms_at_start": measure::ms(measure::probe(base, probes)),
    })
}

#[allow(clippy::too_many_lines)]
fn render(
    host: &serde_json::Value,
    plan: &Plan,
    engines: &[Box<dyn Engine>],
    records: &[Record],
    syncs: &[PerCommit],
    seed_files: &SeedFiles,
    errors: &[String],
) -> String {
    let mut out = String::new();
    let h = |k: &str| host[k].to_string().trim_matches('"').to_owned();
    let _ = writeln!(
        out,
        "# Native metadata store vs embedded databases{}\n",
        if plan.quick {
            " (--quick: a smoke run, not for publication)"
        } else {
            ""
        }
    );
    let _ = writeln!(
        out,
        "- date {}, tree {}; kernel {}; {} ({} threads), {}",
        h("date"),
        h("git"),
        h("kernel"),
        h("cpu"),
        h("threads"),
        h("memory")
    );
    let _ = writeln!(out, "- stores under {}: {}", h("base_dir"), h("filesystem"));
    let _ = writeln!(
        out,
        "- at the start: load {}, raw fdatasync {} ms (median of {} appends of 120 B)",
        h("load_at_start"),
        num(host["fdatasync_probe_ms_at_start"]
            .as_f64()
            .unwrap_or(f64::NAN)),
        plan.probes
    );
    let _ = writeln!(
        out,
        "- {} runs of each measurement; cells are the median [min–max] over the runs; every row gives the 1-minute load at the start of each run (and its range during them) and the raw fdatasync probe taken at the start and end of each run\n",
        plan.runs
    );
    let _ = writeln!(
        out,
        "| engine | version | durability of a commit | processes |\n|---|---|---|---|"
    );
    for e in engines {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            e.name(),
            version(e.name()),
            e.durability(),
            if e.multi_process() {
                "many"
            } else {
                "one (threads)"
            }
        );
    }

    if !syncs.is_empty() {
        let _ = writeln!(
            out,
            "\n## Sync calls per commit (W1's transaction, traced)\n"
        );
        let _ = writeln!(
            out,
            "Counted with this harness's ptrace tracer over two runs of {} and {} transactions; the difference per transaction (batched work averaged in). The pair runs once per run; cells are the median [min–max] over the runs.\n",
            plan.sync_txns.0, plan.sync_txns.1
        );
        out.push_str(&report::sync_table(syncs));
    }

    let breakdown: Vec<&Record> = records
        .iter()
        .filter(|r| r.workload == "breakdown")
        .collect();
    if !breakdown.is_empty() {
        let _ = writeln!(out, "\n## Where a native W1 commit's time goes\n");
        let _ = writeln!(
            out,
            "Every filesystem call of {} commits per run timed (a timing `FsOps` around `RealFs`; the journal appends and the ref file's replacement done step by step, with `RealFs`'s calls in its order, so that each sync is timed on its own); means per commit, so the parts add up; share of the mean commit. The `Compacted` frame of a compaction (every 1,024 records) counts with the journal rows.\n",
            plan.breakdown_txns
        );
        let mut kinds: Vec<String> = breakdown
            .iter()
            .flat_map(|r| {
                r.values
                    .keys()
                    .filter(|k| k.ends_with(" ms") && *k != "commit_ms")
                    .cloned()
            })
            .collect();
        kinds.sort();
        kinds.dedup();
        let commit: Vec<f64> = breakdown
            .iter()
            .filter_map(|r| r.values.get("commit_ms").copied())
            .collect();
        let _ = writeln!(
            out,
            "| part of the commit | calls per commit | ms per commit | share |\n|---|---|---|---|"
        );
        let mut rows: Vec<(f64, String)> = kinds
            .iter()
            .map(|k| {
                let ms: Vec<f64> = breakdown
                    .iter()
                    .filter_map(|r| r.values.get(k).copied())
                    .collect();
                let calls_key = k.replace(" ms", " calls");
                let calls: Vec<f64> = breakdown
                    .iter()
                    .filter_map(|r| r.values.get(&calls_key).copied())
                    .collect();
                let shares: Vec<f64> = breakdown
                    .iter()
                    .filter_map(|r| Some(r.values.get(k)? / r.values.get("commit_ms")? * 100.0))
                    .collect();
                let mut m = ms.clone();
                let (median, _, _) = measure::median_range(&mut m);
                (
                    median,
                    format!(
                        "| {} | {} | {} | {} % |",
                        k.trim_end_matches(" ms"),
                        if calls.is_empty() {
                            "–".into()
                        } else {
                            report::cell(&calls)
                        },
                        report::cell(&ms),
                        report::cell(&shares)
                    ),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, row) in rows {
            let _ = writeln!(out, "{row}");
        }
        let _ = writeln!(
            out,
            "| **the whole commit** | | {} | 100 % |",
            report::cell(&commit)
        );
        let _ = writeln!(
            out,
            "\n{}",
            detail(records, "breakdown", "W1", &[Col("commit_ms", "commit ms")])
        );
    }

    let section = |out: &mut String,
                   title: &str,
                   note: &str,
                   workload: &str,
                   variant: &str,
                   cols: &[Col]| {
        let s = summary(records, workload, variant, cols);
        if s.is_empty() {
            return;
        }
        let _ = writeln!(
            out,
            "\n## {title}\n\n{note}\n\n{s}\n<details><summary>per run</summary>\n\n{}\n</details>",
            detail(records, workload, variant, cols)
        );
    };
    section(
        &mut out,
        &format!("W1: one writer, {} durable transactions", plan.w1_txns),
        &format!(
            "Insert one changeset + compare-and-swap of `main`, back to back after {} warm-up transactions. Size: the closed store's allocated bytes per transaction.",
            plan.w1_warmup
        ),
        "w1",
        "W1",
        &[
            Col("commits_per_s", "commits/s"),
            Col("p50_ms", "p50 ms"),
            Col("p99_ms", "p99 ms"),
            Col("max_ms", "max ms"),
            Col("bytes_per_txn", "bytes/txn on disk"),
        ],
    );
    let w2_cols = [
        Col("txns_per_s", "txns/s"),
        Col("p50_ms", "p50 ms"),
        Col("p99_ms", "p99 ms"),
        Col("max_ms", "max ms"),
        Col("busy", "Busy"),
        Col("failed_writers", "failed"),
        Col("conflict_reruns", "conflict reruns"),
        Col("spread", "spread"),
    ];
    section(
        &mut out,
        &format!(
            "W2: {} writers × {} transactions, processes",
            plan.w2_writers, plan.w2_txns
        ),
        "W1's transaction, `main` compared against its value inside the transaction. One process per writer (native, SQLite). Latency includes `Busy` retries; spread = slowest writer's total time / fastest's.",
        "w2",
        "processes",
        &w2_cols,
    );
    section(
        &mut out,
        &format!(
            "W2: {} writers × {} transactions, threads of one process",
            plan.w2_writers, plan.w2_txns
        ),
        "redb, fjall and sled allow one process only; native and SQLite here too, for the asymmetry.",
        "w2",
        "threads",
        &w2_cols,
    );
    for &n in &plan.sizes {
        section(
            &mut out,
            &format!("W3: lookups, {} changesets, warm", size_name(n)),
            "µs per lookup on one open snapshot (get, prefix of 6 bytes, delta hash, a miss), and by id in a fresh read transaction each (fresh).",
            "w3",
            &size_name(n),
            &[
                Col("get_p50_us", "get p50"),
                Col("get_p99_us", "get p99"),
                Col("prefix_p50_us", "prefix p50"),
                Col("prefix_p99_us", "prefix p99"),
                Col("delta_p50_us", "delta p50"),
                Col("delta_p99_us", "delta p99"),
                Col("miss_p50_us", "miss p50"),
                Col("fresh_p50_us", "fresh p50"),
                Col("fresh_p99_us", "fresh p99"),
            ],
        );
        // Every kind's ratio, so that a verdict quotes them all, not the kind it likes best.
        let ratios = report::ratios(
            records,
            "w3",
            &size_name(n),
            "native",
            &[
                Col("get_p50_us", "get"),
                Col("prefix_p50_us", "prefix"),
                Col("delta_p50_us", "delta"),
                Col("miss_p50_us", "miss"),
                Col("fresh_p50_us", "fresh"),
            ],
        );
        if !ratios.is_empty() {
            let _ = writeln!(
                out,
                "\nThe native store's p50 over each engine's, by kind ({} changesets; above 1, the native store is slower):\n\n{ratios}",
                size_name(n)
            );
        }
    }
    let largest = size_name(*plan.sizes.iter().max().expect("sizes"));
    section(
        &mut out,
        &format!("W4: opening a {largest}-changeset store (warm), in a fresh process"),
        &format!("Median of {} opens per run.", plan.w4_opens),
        "w4-open",
        &largest,
        &[
            Col("open_ms", "open ms"),
            Col("first_lookup_ms", "first lookup ms"),
            Col("open_first_ms", "open + lookup ms"),
            Col("process_ms", "process ms"),
        ],
    );
    section(
        &mut out,
        &format!("W4: recovery after a writer is killed (SIGKILL), {largest} changesets"),
        &format!(
            "Median of {} kills per run; the next process's open, its first write, both; every acknowledged commit checked present.",
            plan.w4_kills
        ),
        "w4-recover",
        &largest,
        &[
            Col("open_ms", "open ms"),
            Col("write_ms", "first write ms"),
            Col("total_ms", "open + write ms"),
            Col("total_max_ms", "worst ms"),
            Col("acked", "commits acked"),
        ],
    );
    for &n in &plan.sizes {
        section(
            &mut out,
            &format!("W5: on-disk size, {} changesets bulk-loaded", size_name(n)),
            &format!(
                "Closed after a settle (native: flush; SQLite: wal_checkpoint(TRUNCATE); fjall: memtables flushed, fjall-unflushed: journal synced only; sled: flush); allocated bytes (`du`). Log: the native journal, SQLite's WAL, fjall's journals. Each store built {}, a run each (the first build is the one W3, W4 and W6 read); the seeding load time is for the record.",
                if plan.runs == 1 {
                    "once".to_owned()
                } else {
                    format!("{} times", plan.runs)
                }
            ),
            "w5",
            &size_name(n),
            &[
                Col("allocated_mib", "MiB"),
                Col("apparent_mib", "apparent MiB"),
                Col("bytes_per_record", "bytes/record"),
                Col("log_mib", "of which log MiB"),
                Col("files", "files"),
                Col("load_s", "load s"),
            ],
        );
    }
    let smallest = size_name(*plan.sizes.iter().min().expect("sizes"));
    section(
        &mut out,
        &format!("W6: reader latency while a writer commits, {smallest} changesets"),
        &format!(
            "By id, a fresh read transaction each, µs: idle first, then for {:?} beside a writer committing W1's transaction back to back.",
            plan.w6_busy
        ),
        "w6",
        &smallest,
        &[
            Col("idle_p50_us", "idle p50"),
            Col("idle_p99_us", "idle p99"),
            Col("busy_p50_us", "busy p50"),
            Col("busy_p99_us", "busy p99"),
            Col("busy_p999_us", "busy p99.9"),
            Col("busy_max_us", "busy max"),
            Col("writer_commits_per_s", "writer commits/s"),
        ],
    );

    if !seed_files.is_empty() {
        let _ = writeln!(
            out,
            "\n## The largest files of each seeded store, settled and closed\n"
        );
        for ((engine, n), (_, files)) in seed_files {
            let files: Vec<String> = files
                .iter()
                .take(8)
                .map(|(f, b)| format!("{f} {}", num(*b as f64 / 1_048_576.0)))
                .collect();
            let _ = writeln!(
                out,
                "- {engine}, {}: {} (MiB allocated)",
                size_name(*n),
                files.join(", ")
            );
        }
    }
    if !errors.is_empty() {
        let _ = writeln!(out, "\n## Errors\n");
        for e in errors {
            let _ = writeln!(out, "- {e}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::{Bench, seed_all, sync_runs};
    use crate::engines::{self, Engine};
    use crate::workloads::Plan;
    use crate::workloads::sync::PerCommit;

    fn named(names: &[&str]) -> Vec<Box<dyn Engine>> {
        names
            .iter()
            .map(|n| engines::by_name(n).expect("an engine"))
            .collect()
    }

    #[test]
    fn the_sync_count_runs_once_per_run_in_the_rotated_order() {
        let engines = named(&["native", "sqlite", "fjall-unflushed"]);
        let mut calls = Vec::new();
        let (syncs, errors) = sync_runs(&engines, 3, |engine, run| {
            calls.push((engine.name(), run));
            Ok(PerCommit {
                engine: engine.name().to_owned(),
                run,
                txns: (64, 1_088),
                syscalls: 0.0,
                written_bytes: 0.0,
                by_name: BTreeMap::new(),
                syncs_by_target: BTreeMap::new(),
            })
        });
        assert!(errors.is_empty(), "{errors:?}");
        // fjall-unflushed writes as fjall does, so it is not counted.
        let expected = [
            ("native", 1),
            ("sqlite", 1),
            ("sqlite", 2),
            ("native", 2),
            ("native", 3),
            ("sqlite", 3),
        ];
        assert_eq!(calls, expected);
        let counted: Vec<(&str, usize)> =
            syncs.iter().map(|s| (s.engine.as_str(), s.run)).collect();
        assert_eq!(counted, expected);
    }

    #[test]
    fn w5_builds_each_store_once_per_run_and_keeps_the_first_build() {
        let work = tempfile::tempdir().expect("a temp dir");
        let mut plan = Plan::quick();
        plan.sizes = vec![1_000, 2_000];
        plan.probes = 1;
        let mut bench = Bench {
            plan: &plan,
            work: work.path().to_path_buf(),
            exe: PathBuf::new(),
            started: Instant::now(),
            records: Vec::new(),
            errors: Vec::new(),
        };
        // sqlite-normal reads SQLite's files, so it has no store of its own.
        let engines = named(&["sqlite", "sqlite-normal", "redb"]);
        let (seeds, files) = seed_all(&mut bench, &engines, Some(3));
        assert!(bench.errors.is_empty(), "{:?}", bench.errors);
        let mut w5: Vec<(String, String, usize)> = bench
            .records
            .iter()
            .filter(|r| r.workload == "w5")
            .map(|r| (r.engine.clone(), r.variant.clone(), r.run))
            .collect();
        w5.sort();
        let mut expected = Vec::new();
        for engine in ["redb", "sqlite"] {
            for size in ["1k", "2k"] {
                for run in 1..=3 {
                    expected.push((engine.to_owned(), size.to_owned(), run));
                }
            }
        }
        assert_eq!(w5, expected);
        assert!(
            bench
                .records
                .iter()
                .all(|r| r.values["allocated_mib"] > 0.0),
            "every build was measured"
        );
        // The first build of each store is kept for W3, W4 and W6; the others are gone.
        let kept: Vec<(String, u64)> = seeds.keys().cloned().collect();
        let keys = [
            ("redb", 1_000),
            ("redb", 2_000),
            ("sqlite", 1_000),
            ("sqlite", 2_000),
        ]
        .map(|(e, n)| (e.to_owned(), n));
        assert_eq!(kept, keys);
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), keys);
        let left = std::fs::read_dir(work.path())
            .expect("the work dir")
            .count();
        assert_eq!(left, 4, "only the kept stores are left in the work dir");
    }
}
