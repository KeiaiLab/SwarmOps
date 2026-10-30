//! The supply-chain gate reaches every lockfile the automation builds. `cargo deny` checks the
//! lockfile of the one workspace it is pointed at, and nothing else notices a lockfile it is not
//! pointed at: a gate step or a scheduled job that builds another workspace (the fuzz crate's,
//! bench/compare's) runs that workspace's build scripts, its dependencies unchecked, while every
//! check stays green. These tests read the justfile, `scripts/*.sh` and the workflows as text (no
//! parser, no new dependency) and fail instead: the gate (`just check`, and `scripts/check.sh`
//! the same) checks the workspace's lockfile and every other one its own steps build, and a
//! recipe, script or job that builds a workspace the gate does not check checks that one itself.
//! bench/compare's policy is a file of its own, which must keep every line of the workspace's.
//! Read the same way, every case the weekly slow suite (`just check-ci-slow`) leaves out still
//! runs weekly: optimized, one at a time, in a job of `scheduled.yml`. And the gate, both ways,
//! runs `scripts/check-stamped.py`, the one step that builds with `SWARMOPS_BUILD_COMMIT` set.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The workspace root: this crate is `crates/swarmops-core`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The file at `path` under the workspace root.
fn read(path: &str) -> String {
    fs::read_to_string(workspace_root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The files of the directory at `path` under the workspace root whose names end in `suffix`,
/// as paths from the root, in name order.
fn files(path: &str, suffix: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(workspace_root().join(path))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(suffix))
        .map(|name| format!("{path}/{name}"))
        .collect();
    names.sort();
    names
}

/// The root workspace's manifest, as the scan names the workspace a cargo command builds when it
/// is given no other.
const ROOT: &str = "Cargo.toml";

/// The cargo subcommands that build a workspace's dependencies, and so run their build scripts.
const BUILDS: [&str; 10] = [
    "bench", "build", "check", "clippy", "doc", "fuzz", "nextest", "run", "rustc", "test",
];

/// What a piece of automation does with cargo: the workspaces it builds and those whose
/// lockfile `cargo deny` checks, each named by its manifest's path from the root.
#[derive(Debug, Default, PartialEq)]
struct Cargo {
    builds: BTreeSet<String>,
    denies: BTreeSet<String>,
}

/// The shell lines of `script`: comment lines dropped, a line that ends in `\` joined to the
/// next.
fn shell_lines(script: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending = String::new();
    for line in script.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(start) = line.trim_end().strip_suffix('\\') {
            pending.push_str(start);
        } else {
            pending.push_str(line);
            lines.push(std::mem::take(&mut pending));
        }
    }
    lines.push(pending);
    lines
}

/// The cargo commands of `script`, shell text (a recipe's body, a script, a job's steps). A
/// command builds or checks the workspace its `--manifest-path` names; without one, the
/// directory a `cd <dir> &&` earlier on its line moved to, `fuzz/` for cargo-fuzz (whose crate
/// is `fuzz/` from the root), or else the root's. `cargo install` builds a published crate with
/// its own lockfile, none of this repository's.
fn scan(script: &str) -> Cargo {
    let mut found = Cargo::default();
    for line in shell_lines(script) {
        let mut dir = None;
        let words: Vec<&str> = line.split_whitespace().collect();
        for command in words.split(|w| matches!(*w, "&&" | "||" | "|" | ";")) {
            let Some(at) = command
                .iter()
                .position(|w| w.rsplit(['(', '`']).next() == Some("cargo"))
            else {
                if let ["cd", to, ..] = command {
                    dir = Some(to.trim_end_matches('/'));
                }
                continue;
            };
            let args: Vec<&str> = (command[at + 1..].iter().copied())
                .skip_while(|w| w.starts_with('+'))
                .take_while(|w| *w != "--")
                .collect();
            let Some(&sub) = args.first() else { continue };
            let named = args.iter().enumerate().find_map(|(i, w)| match *w {
                "--manifest-path" => args.get(i + 1).map(|p| (*p).to_string()),
                _ => w.strip_prefix("--manifest-path=").map(str::to_string),
            });
            let manifest = named.unwrap_or_else(|| match (sub, dir) {
                ("fuzz", _) => "fuzz/Cargo.toml".to_string(),
                (_, Some(dir)) => format!("{dir}/Cargo.toml"),
                (_, None) => ROOT.to_string(),
            });
            if sub == "deny" {
                found.denies.insert(manifest);
            } else if BUILDS.contains(&sub) {
                found.builds.insert(manifest);
            }
        }
    }
    found
}

/// The recipes of a justfile, by name, each with its body: the indented lines under its
/// `name args: dependencies` line.
fn recipes(justfile: &str) -> Vec<(String, String)> {
    let mut recipes: Vec<(String, String)> = Vec::new();
    for line in justfile.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, body)) = recipes.last_mut() {
                body.push_str(line);
                body.push('\n');
            }
        } else if !(line.is_empty()
            || line.starts_with('#')
            || line.starts_with("set ")
            || line.contains(":="))
            && let Some(name) = line.split([' ', ':']).next()
        {
            recipes.push((name.to_string(), String::new()));
        }
    }
    recipes
}

/// The jobs of a workflow, by id, each with its lines: from its `  <id>:` line under `jobs:`
/// to the next job's.
fn jobs(workflow: &str) -> Vec<(String, String)> {
    let mut jobs: Vec<(String, String)> = Vec::new();
    let mut in_jobs = false;
    for line in workflow.lines() {
        if !(line.is_empty() || line.starts_with([' ', '#'])) {
            in_jobs = line.trim_end() == "jobs:";
            continue;
        }
        if !in_jobs {
            continue;
        }
        if let Some(id) = job_id(line) {
            jobs.push((id.to_string(), String::new()));
        } else if let Some((_, lines)) = jobs.last_mut() {
            lines.push_str(line);
            lines.push('\n');
        }
    }
    jobs
}

/// The id of the job that a line of a workflow's `jobs:` opens, `  <id>:`; `None` for any other
/// line.
fn job_id(line: &str) -> Option<&str> {
    let id = line.strip_prefix("  ")?.strip_suffix(':')?;
    let valid = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    (!id.is_empty() && id.chars().all(valid)).then_some(id)
}

/// The body of the justfile's recipe `name`.
fn recipe(justfile: &str, name: &str) -> String {
    match recipes(justfile).into_iter().find(|(n, _)| n == name) {
        Some((_, body)) => body,
        None => panic!("the justfile has no `{name}` recipe"),
    }
}

/// The lines of a TOML file, each with the table it is in (`[x]` or `[[x]]`, `""` above the
/// first): comments, trailing ones included, and blank lines dropped.
fn table_lines(toml: &str) -> BTreeSet<(String, String)> {
    let mut table = String::new();
    let mut lines = BTreeSet::new();
    for line in toml.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') && line.ends_with(']') && !line.contains('=') {
            table = line.to_string();
        } else if !line.is_empty() {
            lines.insert((table.clone(), line.to_string()));
        }
    }
    lines
}

/// The exclusions `just check-ci-slow` may add to `just check-slow`'s filter that no scheduled job
/// runs: the two 32-process contention measurements, which assume this host's disk and which
/// `just perf` runs.
const HOST_ONLY: [&str; 1] = ["not test(thirty_two_processes)"];

/// The flags of a `cargo nextest run` that runs left-out cases as `just perf` does: every one,
/// ignored or not, optimized and one at a time, red when the filter matches none.
const PERF_FLAGS: [&str; 7] = [
    "--locked",
    "--release",
    "--workspace",
    "--all-features",
    "--run-ignored all",
    "--no-tests=fail",
    "--no-capture",
];

/// The terms of the nextest filter (`-E '<filter>'`) in a recipe's body, split at ` and `; empty
/// without one.
fn filter_terms(body: &str) -> Vec<String> {
    let Some((_, rest)) = body.split_once("-E '") else {
        return Vec::new();
    };
    let filter = rest.split('\'').next().unwrap_or_default();
    filter
        .split(" and ")
        .map(|t| t.trim().to_string())
        .collect()
}

/// The justfile variable a filter term leaves out, `not ({{ name }})`; `None` for any other term.
fn left_out_variable(term: &str) -> Option<&str> {
    let inner = term.strip_prefix("not ({{")?.strip_suffix("}})")?;
    Some(inner.trim())
}

/// The recipes a workflow job runs: each word after `just` in its shell lines.
fn just_runs(lines: &str) -> BTreeSet<String> {
    let mut runs = BTreeSet::new();
    for line in shell_lines(lines) {
        let words: Vec<&str> = line.split_whitespace().collect();
        for pair in words.windows(2) {
            if pair[0] == "just" {
                runs.insert(pair[1].to_string());
            }
        }
    }
    runs
}

/// What keeps a case `check-ci-slow` leaves out from running weekly, optimized, in `workflow`'s
/// jobs, one line each; empty when every one does. A term it adds to `check-slow`'s filter is a
/// `HOST_ONLY` exclusion, or `not ({{ name }})` for a variable that some recipe runs with every
/// `PERF_FLAGS` flag and the filter `'{{ name }}'`, and that a job runs.
fn unscheduled(justfile: &str, workflow: &str) -> Vec<String> {
    // 1. The terms the weekly slow suite adds to the slow suite's filter.
    let slow = filter_terms(&recipe(justfile, "check-slow"));
    let weekly = filter_terms(&recipe(justfile, "check-ci-slow"));
    let added = weekly.iter().filter(|term| !slow.contains(term));

    // 2. The recipes each job of the workflow runs.
    let scheduled: BTreeSet<String> = (jobs(workflow).into_iter())
        .flat_map(|(_, lines)| just_runs(&lines))
        .collect();

    // 3. Each left-out variable needs a scheduled recipe that runs it as `just perf` does.
    let mut problems = Vec::new();
    for term in added {
        if HOST_ONLY.contains(&term.as_str()) {
            continue;
        }
        let Some(name) = left_out_variable(term) else {
            problems.push(format!(
                "`{term}` is no `not ({{{{ name }}}})` of a scheduled variable"
            ));
            continue;
        };
        let wanted = format!("-E '{{{{ {name} }}}}'");
        let runners: Vec<(String, String)> = (recipes(justfile).into_iter())
            .filter(|(_, body)| shell_lines(body).iter().any(|l| l.contains(&wanted)))
            .collect();
        let optimized = |body: &str| {
            shell_lines(body).iter().any(|line| {
                line.contains("cargo nextest run")
                    && line.contains(&wanted)
                    && PERF_FLAGS.iter().all(|flag| line.contains(flag))
            })
        };
        let runs_weekly = runners
            .iter()
            .any(|(recipe, body)| scheduled.contains(recipe) && optimized(body));
        if !runs_weekly {
            let names: Vec<&String> = runners.iter().map(|(n, _)| n).collect();
            problems.push(format!(
                "`{name}`: recipes {names:?} run it; none runs it with {PERF_FLAGS:?} and is \
                 run by a scheduled job"
            ));
        }
    }
    problems
}

/// Every case `just check-ci-slow` leaves out, but the host-only contention measurements, runs
/// weekly in a job of `scheduled.yml`, optimized and one at a time (`just check-ci-perf`).
#[test]
fn every_case_the_weekly_suite_leaves_out_runs_weekly() {
    let problems = unscheduled(&read("justfile"), &read(".github/workflows/scheduled.yml"));
    assert!(
        problems.is_empty(),
        "check-ci-slow leaves out cases no scheduled job runs optimized: {problems:#?}"
    );
}

/// The check fails a weekly job deleted, a recipe that drops `--release` or `--no-capture`, and a
/// case left out by name instead of through a scheduled variable.
#[test]
fn the_left_out_check_finds_each_gap() {
    let justfile = |slow_extra: &str, perf_flags: &str| {
        [
            "check-slow:".to_string(),
            "    cargo nextest run --run-ignored all -E 'not test(a)'".to_string(),
            "check-ci-slow:".to_string(),
            format!(
                "    cargo nextest run --run-ignored all -E 'not test(a) and not \
                 test(thirty_two_processes){slow_extra}'"
            ),
            "cases := 'test(=x::y)'".to_string(),
            "check-ci-perf:".to_string(),
            format!("    cargo nextest run {perf_flags} -E '{{{{ cases }}}}' --no-capture"),
        ]
        .join("\n")
    };
    let flags = "--locked --release --workspace --all-features --run-ignored all --no-tests=fail";
    let workflow = |run: &str| format!("jobs:\n  perf:\n    steps:\n      - run: {run}\n");
    let job = workflow("flock -w 60 \"$LOCK\" just check-ci-perf");

    // 1. The arrangement as it stands passes, and so does a filter that leaves nothing out.
    assert_eq!(
        unscheduled(&justfile(" and not ({{ cases }})", flags), &job),
        Vec::<String>::new()
    );
    assert_eq!(
        unscheduled(&justfile("", flags), &workflow("true")),
        Vec::<String>::new()
    );

    // 2. Each gap is found.
    let unoptimized = flags.replace("--release ", "");
    let gaps = [
        justfile(" and not ({{ cases }})", flags).replace(" --no-capture", ""),
        justfile(" and not ({{ cases }})", &unoptimized),
        justfile(" and not test(=x::y)", flags),
    ];
    for gap in &gaps {
        assert_eq!(unscheduled(gap, &job).len(), 1, "{gap}");
    }
    let deleted = workflow("just check-ci-slow");
    assert_eq!(
        unscheduled(&justfile(" and not ({{ cases }})", flags), &deleted).len(),
        1
    );
}

/// The gate runs `cargo deny` over the workspace's lockfile and over every other one its own
/// steps build (the fuzz crate's, which its clippy step builds), and `scripts/check.sh`, the
/// gate without `just`, builds and checks the same.
#[test]
fn the_gate_checks_every_lockfile_it_builds() {
    let gate = scan(&recipe(&read("justfile"), "check"));
    let mut builds = gate.builds.clone();
    builds.insert(ROOT.to_string());
    let unchecked: Vec<&String> = builds.difference(&gate.denies).collect();
    assert!(
        unchecked.is_empty(),
        "`just check` builds {unchecked:?} and runs cargo deny over none of them"
    );
    assert_eq!(
        scan(&read("scripts/check.sh")),
        gate,
        "scripts/check.sh builds and checks what `just check` does"
    );
}

/// The gate's step that builds swarmops-cli with `SWARMOPS_BUILD_COMMIT` set, which no cargo line
/// of the gate does.
const STAMPED: &str = "python3 -I scripts/check-stamped.py";

/// Whether shell text runs `step` as a line of its own, comments aside.
fn runs_step(script: &str, step: &str) -> bool {
    shell_lines(script).iter().any(|line| line.trim() == step)
}

/// `just check` and `scripts/check.sh` both run the stamped build's check: without it, a build
/// whose `--version` drops the commit, or whose compile-time guard is off, passes the gate.
#[test]
fn the_gate_runs_the_stamped_check() {
    let gate = recipe(&read("justfile"), "check");
    assert!(
        runs_step(&gate, STAMPED),
        "`just check` does not run `{STAMPED}`"
    );
    let script = read("scripts/check.sh");
    assert!(
        runs_step(&script, STAMPED),
        "scripts/check.sh does not run `{STAMPED}`"
    );
}

/// Every workspace a recipe, a script or a scheduled job builds has its lockfile checked: by the
/// gate, or by `cargo deny` in that same recipe, script or job (bench/compare's, which only
/// `just bench-compare` and the weekly run build).
#[test]
fn every_lockfile_the_automation_builds_is_checked_where_it_is_built() {
    let justfile = read("justfile");
    let gate = scan(&recipe(&justfile, "check")).denies;
    let mut scopes: Vec<(String, String)> = recipes(&justfile)
        .into_iter()
        .map(|(name, body)| (format!("justfile recipe `{name}`"), body))
        .collect();
    for script in files("scripts", ".sh") {
        let text = read(&script);
        scopes.push((script, text));
    }
    for workflow in files(".github/workflows", ".yml") {
        for (id, lines) in jobs(&read(&workflow)) {
            scopes.push((format!("{workflow} job `{id}`"), lines));
        }
    }
    let mut unchecked = Vec::new();
    for (scope, text) in &scopes {
        let found = scan(text);
        for manifest in found.builds.difference(&gate) {
            if !found.denies.contains(manifest) {
                unchecked.push(format!("{scope} builds {manifest}"));
            }
        }
    }
    assert!(
        scopes.iter().any(|(_, text)| !scan(text).builds.is_empty()),
        "the scan found no cargo build at all"
    );
    assert!(
        unchecked.is_empty(),
        "neither the gate nor the place that builds it runs cargo deny over: {unchecked:#?}"
    );
}

/// bench/compare's policy keeps every line of the workspace's, each in the same table: the
/// harness's own entries (the advisories it ignores, its license exceptions) come on top, and a
/// line the workspace's policy gains or changes reaches it too.
#[test]
fn bench_compare_keeps_every_line_of_the_workspace_policy() {
    let ours = table_lines(&read("deny.toml"));
    let bench = table_lines(&read("bench/compare/deny.toml"));
    let missing: Vec<_> = ours.difference(&bench).collect();
    assert!(
        missing.is_empty(),
        "bench/compare/deny.toml lacks these (table, line) of deny.toml: {missing:?}"
    );
}

/// `names` as a set of owned names.
fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// The scan reads what cargo commands build and check, however they are written: a manifest
/// path given apart or with `=`, a `cd` before the command, cargo-fuzz's own crate, a toolchain,
/// variables, a continued line, a command substitution or a pipe; and it passes over comments,
/// `cargo fmt`, `cargo install` and what comes after `--`.
#[test]
fn the_scan_reads_what_cargo_builds_and_checks() {
    let script = [
        "# cargo clippy --manifest-path commented/Cargo.toml",
        "cargo fmt --manifest-path fmt/Cargo.toml --check",
        "cargo clippy --manifest-path a/Cargo.toml -- -D warnings --manifest-path z/Cargo.toml",
        "RUSTDOCFLAGS=\"-D warnings\" cargo +nightly test --locked \\",
        "    --manifest-path b/Cargo.toml",
        "cd c/d && cargo run --release -- --quick",
        "targets=$(cargo +nightly fuzz list)",
        "out=$(cargo build --manifest-path=e/Cargo.toml 2>&1) || exit 1",
        "cargo install --locked cargo-fuzz@0.13.2",
        "cargo nextest run --workspace 2>&1 | tee log",
        "cargo deny --locked --manifest-path a/Cargo.toml --config deny.toml check",
        "cargo deny --locked check",
    ]
    .join("\n");
    let builds = [
        "Cargo.toml",
        "a/Cargo.toml",
        "b/Cargo.toml",
        "c/d/Cargo.toml",
        "e/Cargo.toml",
        "fuzz/Cargo.toml",
    ];
    let want = Cargo {
        builds: names(&builds),
        denies: names(&["Cargo.toml", "a/Cargo.toml"]),
    };
    assert_eq!(scan(&script), want);
}

/// A justfile's recipes are its lines that start a recipe, each with the indented lines under it,
/// blank ones among them; settings, variables and comments start none.
#[test]
fn the_recipes_of_a_justfile_are_read_by_name() {
    let justfile = [
        "set shell := [\"bash\", \"-c\"]",
        "level := \"3\"",
        "",
        "# The default.",
        "default: check",
        "",
        "# The gate.",
        "check:",
        "    cargo clippy --manifest-path a/Cargo.toml",
        "",
        "    cargo deny check",
        "",
        "# Another.",
        "other *args:",
        "    #!/usr/bin/env bash",
        "    cargo run --manifest-path b/Cargo.toml -- {{args}}",
    ]
    .join("\n");
    let found: Vec<(String, Cargo)> = (recipes(&justfile).into_iter())
        .map(|(name, body)| (name, scan(&body)))
        .collect();
    let want = [
        ("default", names(&[]), names(&[])),
        ("check", names(&["a/Cargo.toml"]), names(&["Cargo.toml"])),
        ("other", names(&["b/Cargo.toml"]), names(&[])),
    ];
    let want: Vec<(String, Cargo)> = (want.into_iter())
        .map(|(name, builds, denies)| (name.to_string(), Cargo { builds, denies }))
        .collect();
    assert_eq!(found, want);
}

/// A workflow's jobs are the `  <id>:` lines under `jobs:`, each with the lines up to the next;
/// the keys of other tables and comments start none.
#[test]
fn the_jobs_of_a_workflow_are_read_by_id() {
    let workflow = [
        "name: X",
        "on:",
        "  push:",
        "    branches: [main]",
        "jobs:",
        "  one:",
        "    steps:",
        "      - run: cargo test --manifest-path a/Cargo.toml",
        "  # A comment, no job:",
        "  two_b:",
        "    steps:",
        "      - run: |",
        "          cargo deny --manifest-path a/Cargo.toml \\",
        "            check",
    ]
    .join("\n");
    let found: Vec<(String, Cargo)> = (jobs(&workflow).into_iter())
        .map(|(id, lines)| (id, scan(&lines)))
        .collect();
    let want = [
        ("one", names(&["a/Cargo.toml"]), names(&[])),
        ("two_b", names(&[]), names(&["a/Cargo.toml"])),
    ];
    let want: Vec<(String, Cargo)> = (want.into_iter())
        .map(|(id, builds, denies)| (id.to_string(), Cargo { builds, denies }))
        .collect();
    assert_eq!(found, want);
}

/// A policy's lines are read with the table they are in, comments and blank lines dropped.
#[test]
fn a_policy_is_read_line_by_line_with_its_tables() {
    let policy = [
        "# A comment.",
        "[graph]",
        "all-features = true # why",
        "",
        "[licenses]",
        "allow = [",
        "  \"MIT\",",
        "]",
        "[[licenses.exceptions]]",
        "crate = \"x\"",
    ]
    .join("\n");
    let want: BTreeSet<(String, String)> = [
        ("[graph]", "all-features = true"),
        ("[licenses]", "allow = ["),
        ("[licenses]", "\"MIT\","),
        ("[licenses]", "]"),
        ("[[licenses.exceptions]]", "crate = \"x\""),
    ]
    .into_iter()
    .map(|(table, line)| (table.to_string(), line.to_string()))
    .collect();
    assert_eq!(table_lines(&policy), want);
}
