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
//! The container images are read the same way: every image the automation runs is pinned by
//! digest, each has one digest, the one `packaging/entrance/Dockerfile` names (the file Dependabot
//! moves), and the workflows install one rustup-init release. And `release.yml` gives a write
//! permission to no job that runs for anything but a pushed tag.

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

/// The file whose `FROM` lines are the one source of every image digest: Dependabot's `docker`
/// entry reads it.
const IMAGES: &str = "packaging/entrance/Dockerfile";

/// The stages of `IMAGES` that build nothing: each names an image a script or a workflow runs.
const NAMED_IMAGES: [&str; 4] = ["install-deb", "install-rpm", "package-build", "qemu-binfmt"];

/// The marker of a digest in an image reference, `name:tag@sha256:<hex>`.
const DIGEST: &str = "@sha256:";

/// The image references pinned by digest in `text`, each `(name:tag, digest)`: a word that holds
/// `@sha256:`, quotes and brackets around it dropped. Comment lines are read too, so that a
/// comment cannot name another digest than the code under it.
fn pins(text: &str) -> BTreeSet<(String, String)> {
    let outside = |c: char| !(c.is_ascii_alphanumeric() || "@:./_-".contains(c));
    text.split_whitespace()
        .map(|word| word.trim_matches(outside))
        .filter_map(|word| word.split_once(DIGEST))
        .filter(|(name, digest)| is_image_name(name) && is_sha256(digest))
        .map(|(name, digest)| (name.to_string(), digest.to_string()))
        .collect()
}

/// Whether `name` reads as an image's `name:tag`, not as a pattern or a placeholder that
/// mentions the digest marker (`(?P<image>\S+@sha256:...`).
fn is_image_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":./_-".contains(c))
}

/// Whether `digest` is a SHA-256 in hex: 64 lowercase hex digits.
fn is_sha256(digest: &str) -> bool {
    digest.len() == 64 && digest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The images a Dockerfile's `FROM` lines name, each with its stage (`AS <stage>`, `""`
/// without one), in order.
fn from_lines(dockerfile: &str) -> Vec<(String, String)> {
    let mut images = Vec::new();
    for line in dockerfile.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["FROM", image] => images.push(((*image).to_string(), String::new())),
            ["FROM", image, "AS" | "as", stage] => {
                images.push(((*image).to_string(), (*stage).to_string()));
            }
            _ => {}
        }
    }
    images
}

/// The images a workflow's jobs and services run in: the value of each `image:` key.
fn job_images(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("image: "))
        .map(|image| image.trim().to_string())
        .collect()
}

/// What keeps the images of `texts` (each a file's path and text) from having one source, one
/// line each; empty when they do. Every `FROM` of the source is pinned by digest and no image
/// has two digests there; a job's `image:` is pinned; and a pinned reference anywhere is one the
/// source names, with its digest.
fn image_problems(source: &str, texts: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();

    // 1. The source: every base pinned, one digest for each image.
    let named = pins(source);
    for (image, _) in from_lines(source) {
        if !image.contains(DIGEST) {
            problems.push(format!("{IMAGES}: `FROM {image}` is not pinned by digest"));
        }
    }
    for (name, digest) in &named {
        if named.iter().any(|(n, d)| n == name && d != digest) {
            problems.push(format!("{IMAGES} names two digests of {name}"));
        }
    }

    // 2. Every other file: a workflow job's image is pinned (a script's `image: str` is a type,
    //    not a key), and each pin is the source's.
    for (path, text) in texts {
        let is_yml = Path::new(path).extension().is_some_and(|ext| ext == "yml");
        let images = if is_yml { job_images(text) } else { Vec::new() };
        for image in images {
            if !image.contains(DIGEST) {
                problems.push(format!("{path}: `image: {image}` is not pinned by digest"));
            }
        }
        for (name, digest) in pins(text) {
            if !named.contains(&(name.clone(), digest.clone())) {
                problems.push(format!(
                    "{path}: {name}{DIGEST}{digest} is not a digest {IMAGES} names"
                ));
            }
        }
    }
    problems
}

/// The files that run or name a container image: the workflows and the Python scripts.
fn image_users() -> Vec<(String, String)> {
    let mut users = files(".github/workflows", ".yml");
    users.extend(files("scripts", ".py"));
    users
        .into_iter()
        .map(|path| {
            let text = read(&path);
            (path, text)
        })
        .collect()
}

/// Every image a workflow or a script runs is pinned by a digest `packaging/entrance/Dockerfile`
/// names: the build image of build.yml and of scheduled.yml's git matrix is the entrance's base,
/// and a digest moved in one file alone fails here, naming the file left behind.
#[test]
fn every_image_digest_is_the_dockerfiles() {
    let problems = image_problems(&read(IMAGES), &image_users());
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Dependabot moves the digests: its `docker` entry reads the directory of the one source, which
/// names the images scripts/packages.py runs as stages, and that script holds no digest of its
/// own to move by hand.
#[test]
fn dependabot_moves_every_image_digest() {
    // 1. The entry, for the source's directory.
    let dependabot = read(".github/dependabot.yml");
    let directory = Path::new(IMAGES).parent().unwrap().to_str().unwrap();
    let entries: Vec<&str> = dependabot.split("  - package-ecosystem: ").collect();
    let tracked = entries.iter().any(|entry| {
        entry.starts_with("docker\n") && entry.contains(&format!("    directory: /{directory}\n"))
    });
    assert!(
        tracked,
        "dependabot.yml has no docker entry for /{directory}"
    );

    // 2. The stages the script reads.
    let source = read(IMAGES);
    let stages: Vec<String> = (from_lines(&source).into_iter())
        .map(|(_, stage)| stage)
        .collect();
    for stage in NAMED_IMAGES {
        assert!(
            stages.iter().any(|s| s == stage),
            "{IMAGES} has no `FROM <image> AS {stage}`"
        );
    }

    // 3. No digest Dependabot cannot read.
    let script = read("scripts/packages.py");
    assert_eq!(
        pins(&script),
        BTreeSet::new(),
        "scripts/packages.py holds a digest of its own"
    );
}

/// The check finds an image that is not pinned, a digest the source does not name, a second
/// digest of one image, and a digest in a comment; and it passes files that agree.
#[test]
fn the_image_check_finds_each_gap() {
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let source =
        format!("FROM debian:12{DIGEST}{a} AS install-deb\nFROM ubuntu:24.04{DIGEST}{b}\n");
    let file = |text: String| vec![("x.yml".to_string(), text)];

    // 1. Files that agree, read through quotes and from a comment.
    let agreeing = format!(
        "    container:\n      image: ubuntu:24.04{DIGEST}{b}\nX = \"debian:12{DIGEST}{a}\"\n\
         # debian:12{DIGEST}{a}\nLINE = r\"FROM (?P<image>\\S+{DIGEST}[0-9a-f]{{64}}) AS\"\n\
         # 'FROM <image>{DIGEST}<digest> AS'\n"
    );
    assert_eq!(
        image_problems(&source, &file(agreeing)),
        Vec::<String>::new()
    );

    // A script's `image: str` is a type annotation, not a job's image.
    let script = vec![("x.py".to_string(), "    image: str = \"\"\n".to_string())];
    assert_eq!(image_problems(&source, &script), Vec::<String>::new());

    // 2. Each gap is found.
    let gaps = [
        (source.clone(), "      image: ubuntu:24.04\n".to_string()),
        (
            source.clone(),
            format!("      image: ubuntu:24.04{DIGEST}{a}\n"),
        ),
        (source.clone(), format!("# ubuntu:24.04{DIGEST}{a}\n")),
        (source.clone(), format!("X = 'almalinux:9{DIGEST}{a}'\n")),
        (
            format!("{source}FROM debian:12{DIGEST}{b} AS other\n"),
            String::new(),
        ),
        (format!("{source}FROM python:3.12\n"), String::new()),
    ];
    for (source, text) in gaps {
        let problems = image_problems(&source, &file(text.clone()));
        assert!(!problems.is_empty(), "{source}{text}");
    }
}

/// The variables that name the rustup-init release a workflow installs and its SHA-256.
const RUSTUP: [&str; 2] = ["RUSTUP_RELEASE: ", "RUSTUP_SHA256: "];

/// The values a workflow gives each `RUSTUP` variable, by variable.
fn rustup_pins(workflow: &str) -> BTreeSet<(String, String)> {
    let mut found = BTreeSet::new();
    for line in workflow.lines() {
        for key in RUSTUP {
            if let Some(value) = line.trim_start().strip_prefix(key) {
                found.insert((key.to_string(), value.trim().to_string()));
            }
        }
    }
    found
}

/// The workflows that install rustup-init install one release by one SHA-256: build.yml's, the
/// release build's. A release moved in one workflow alone fails here.
#[test]
fn the_workflows_install_one_rustup_init() {
    let source = rustup_pins(&read(".github/workflows/build.yml"));
    assert_eq!(source.len(), RUSTUP.len(), "build.yml: {source:?}");
    let mut installers = 0;
    for workflow in files(".github/workflows", ".yml") {
        let found = rustup_pins(&read(&workflow));
        if found.is_empty() {
            continue;
        }
        installers += 1;
        assert_eq!(found, source, "{workflow} installs another rustup-init");
    }
    assert!(installers >= 2, "the scan found one workflow or none");

    let other = "      RUSTUP_RELEASE: 1.0.0\n      RUSTUP_SHA256: 00\n";
    assert_ne!(rustup_pins(other), source);
}

/// The one condition under which a job of release.yml may hold a write permission: the run is a
/// tag's push, or the push of a release branch, which stands for its tag (RELEASE.md).
const TAG_PUSH_ONLY: &str = "    if: github.event_name == 'push' && (github.ref_type == 'tag' || startsWith(github.ref, 'refs/heads/release/'))";

/// The push filters that may start release.yml: `v*` tags, and `release/*` branches.
const RELEASE_PUSH: [&str; 2] = ["    tags: [\"v*\"]", "    branches: [\"release/*\"]"];

/// What lets a run of `workflow` that is no tag's or release branch's push write or displace a
/// release, one line each; empty when nothing does. Every job with a `: write` permission
/// carries `TAG_PUSH_ONLY`, the workflow starts from no permission, a push starts it for `v*`
/// tags and `release/*` branches alone, and its concurrency group names the event, so that a run
/// started by hand on a tag's ref shares no group with that tag's release.
fn release_problems(workflow: &str) -> Vec<String> {
    let mut problems = Vec::new();

    // 1. The jobs that write.
    let mut writers = 0;
    for (id, lines) in jobs(workflow) {
        let grants_write = shell_lines(&lines).iter().any(|line| {
            line.split('#')
                .next()
                .unwrap_or_default()
                .trim_end()
                .ends_with(": write")
        });
        if !grants_write {
            continue;
        }
        writers += 1;
        if !lines.lines().any(|line| line == TAG_PUSH_ONLY) {
            problems.push(format!(
                "job `{id}` writes without `{}`",
                TAG_PUSH_ONLY.trim()
            ));
        }
    }
    if writers == 0 {
        problems.push("no job holds a write permission: is the scan reading it?".to_string());
    }

    // 2. What the jobs do not set.
    if !workflow.lines().any(|line| line == "permissions: {}") {
        problems.push("the workflow does not start from `permissions: {}`".to_string());
    }
    let on: Vec<&str> = (workflow.lines())
        .skip_while(|line| *line != "on:")
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with(' '))
        .collect();
    let filters: Vec<&&str> = (on.iter())
        .filter(|line| line.starts_with("    ") && !line.starts_with("     "))
        .collect();
    if !on.contains(&RELEASE_PUSH[0])
        || filters.iter().any(|line| !RELEASE_PUSH.contains(line))
    {
        problems.push(
            "a push starts the workflow for more than `v*` tags and `release/*` branches"
                .to_string(),
        );
    }

    // 3. The group.
    let group = (workflow.lines())
        .skip_while(|line| *line != "concurrency:")
        .find_map(|line| line.strip_prefix("  group: "));
    match group {
        Some(group) if group.contains("github.event_name") && group.contains("github.ref") => {}
        _ => problems.push("the concurrency group does not name the event and the ref".to_string()),
    }
    problems
}

/// release.yml publishes for a pushed tag or release branch only, and a run started by hand
/// cannot replace a pending release run.
#[test]
fn only_a_pushed_tag_publishes() {
    let problems = release_problems(&read(".github/workflows/release.yml"));
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The check finds a publishing job without its condition or with a weaker one, a write
/// permission granted to every job, a branch other than `release/*` that starts the workflow, a
/// push of any path, and a group by ref alone.
#[test]
fn the_release_check_finds_each_gap() {
    let workflow = [
        "on:",
        "  push:",
        "    tags: [\"v*\"]",
        "    branches: [\"release/*\"]",
        "  workflow_dispatch:",
        "",
        "permissions: {}",
        "",
        "concurrency:",
        "  group: release-${{ github.event_name }}-${{ github.ref }}",
        "",
        "jobs:",
        "  build:",
        "    permissions:",
        "      contents: read",
        "  publish:",
        TAG_PUSH_ONLY,
        "    permissions:",
        "      contents: write # why",
        "",
    ]
    .join("\n");
    assert_eq!(release_problems(&workflow), Vec::<String>::new());

    let condition = TAG_PUSH_ONLY.trim_start();
    let gaps = [
        workflow.replace(&format!("    {condition}\n"), ""),
        workflow.replace(condition, "if: github.event_name == 'push'"),
        workflow.replace(
            condition,
            "if: github.event_name == 'push' && (github.ref_type == 'tag' || github.ref_type == 'branch')",
        ),
        workflow.replace("permissions: {}", "permissions:\n  contents: write"),
        workflow.replace("    branches: [\"release/*\"]", "    branches: [stable]"),
        workflow.replace(
            "    branches: [\"release/*\"]",
            "    branches: [\"release/*\", stable]",
        ),
        workflow.replace(
            "    branches: [\"release/*\"]",
            "    branches: [\"release/*\"]\n    paths: [\"**\"]",
        ),
        workflow.replace("${{ github.event_name }}-", ""),
        workflow.replace("contents: write # why", "contents: read"),
    ];
    for gap in &gaps {
        assert_eq!(release_problems(gap).len(), 1, "{gap}");
    }
}

/// How a workflow makes the packages: `scripts/packages.py build`, run inside the
/// `package-build` image `IMAGES` names, so that the bytes depend on that image's Python and
/// liblzma and not on the runner's.
const PACKAGE_BUILD: &str = "scripts/packages.py build";

/// The flag of `PACKAGE_BUILD` that runs it in the pinned image.
const IN_IMAGE: &str = "--in-image";

/// The shell lines of `workflow` that make packages without `IN_IMAGE`, one line each.
fn unpinned_builds(workflow: &str) -> Vec<String> {
    shell_lines(workflow)
        .into_iter()
        .filter(|line| line.contains(PACKAGE_BUILD))
        .filter(|line| !line.contains(&format!("{PACKAGE_BUILD} {IN_IMAGE} ")))
        .map(|line| line.trim().to_string())
        .collect()
}

/// release.yml makes its packages in the pinned image, never with the runner's Python.
#[test]
fn the_packages_are_built_in_the_pinned_image() {
    let release = read(".github/workflows/release.yml");
    let builds = shell_lines(&release)
        .into_iter()
        .filter(|line| line.contains(PACKAGE_BUILD))
        .count();
    assert!(builds > 0, "release.yml runs no `{PACKAGE_BUILD}`");
    assert_eq!(unpinned_builds(&release), Vec::<String>::new());

    // The check itself: a build on the runner is found, one in the image is not.
    let runner = "          python3 -I scripts/packages.py build \"archive/$a\" dist\n";
    let image = "          python3 -I scripts/packages.py build --in-image \"archive/$a\" dist\n";
    assert_eq!(unpinned_builds(runner).len(), 1, "{runner}");
    assert_eq!(unpinned_builds(image), Vec::<String>::new());
}
