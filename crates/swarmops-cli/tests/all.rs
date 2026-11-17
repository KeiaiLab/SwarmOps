//! The CLI's integration tests, in one test binary (one link): each file of `tests/` is a module
//! here, and its tests are named after it (`multiprocess::<test>`); `support` binds the binaries
//! under test. `crash.rs`, `faults.rs` and `serve_lock.rs` drive the test hooks, so they are here
//! only with the `test-hooks` feature, which the workspace's gates turn on (`--all-features`).
//! The release proof, `test_hooks.rs`, is a binary of its own, built without the feature. Cargo
//! builds no other test target from `tests/` (`autotests = false` in `Cargo.toml`): a new test
//! file is a `mod` line here.

mod support;

mod auth;
mod cli;
mod coming_from_git;
mod contention;
mod counted_work;
#[cfg(feature = "test-hooks")]
mod crash;
mod damaged_index;
#[cfg(feature = "test-hooks")]
mod faults;
mod format2;
mod format3_perf;
mod git;
mod git_text;
mod github_live;
mod gitlike;
mod help;
mod intake_perf;
mod landing;
mod mcp;
mod mcp_e2e;
mod mcp_parity;
mod mcp_ws;
mod merge_table;
mod multiprocess;
mod pilot_ws;
mod power_loss;
mod remote_acceptance;
mod remote_fetch;
mod remote_matrix;
mod remote_native;
mod remote_perf;
mod remote_push;
mod remote_serve;
mod remote_sync;
mod remote_url;
mod repo_path;
mod revert_limits;
mod scale_perf;
mod seeded;
mod serve_differential;
mod serve_fetch;
mod serve_fetch_pins;
mod serve_http;
mod serve_interop;
mod serve_land;
mod serve_load;
#[cfg(feature = "test-hooks")]
mod serve_lock;
mod serve_native;
mod serve_perf;
mod serve_pins;
mod serve_prepare;
mod serve_push;
mod serve_records;
mod sigkill;
mod so;
mod sync_load;
mod undo_kinds;
mod ws;
mod ws_processes;
mod ws_sigkill;

/// Every test file is a module here: with `autotests = false` Cargo builds no target from a file
/// of `tests/` that no `mod` line names, and its tests would never run (jj's
/// `no_forgotten_test_files`). Directories (`support/`, `snapshots/`, …) are passed over, and so
/// are the targets of their own that `Cargo.toml`'s `[[test]]` entries name, `all.rs` itself
/// among them.
#[test]
fn every_test_file_is_a_module_of_all_rs() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let names: Vec<String> = std::fs::read_dir(tests)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let files: Vec<&str> = names.iter().map(String::as_str).collect();
    let forgotten = undeclared(
        &files,
        include_str!("all.rs"),
        include_str!("../Cargo.toml"),
    );
    assert!(
        forgotten.is_empty(),
        "no `mod` line of tests/all.rs names {forgotten:?} in tests/, so Cargo never builds them"
    );
}

/// The files of `files`, the names in `tests/`, that Cargo builds no test from: those ending in
/// `.rs` that no `mod` line of `all_rs` names and no `[[test]]` entry of `manifest` builds on
/// its own (an entry without a `path` builds `tests/<name>.rs`).
fn undeclared<'a>(files: &[&'a str], all_rs: &str, manifest: &str) -> Vec<&'a str> {
    let declared: Vec<&str> = all_rs
        .lines()
        .filter_map(|line| line.strip_prefix("mod ")?.strip_suffix(';'))
        .collect();
    let mut own = Vec::new();
    let mut in_test = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_test = line == "[[test]]";
        } else if in_test
            && let Some(name) = line
                .strip_prefix("name = \"")
                .and_then(|n| n.strip_suffix('"'))
        {
            own.push(name);
        }
    }
    let forgotten = |module: &str| !declared.contains(&module) && !own.contains(&module);
    (files.iter().copied())
        .filter(|file| file.strip_suffix(".rs").is_some_and(forgotten))
        .collect()
}

/// The check itself: it finds a file that no `mod` line names, however nearly one does (in a
/// comment) and whatever else builds from the name (a bench, the package), and passes over a
/// module, one under a `cfg`, a target of its own and a directory.
#[test]
fn the_test_file_check_finds_a_forgotten_file() {
    let all_rs = "mod a;\n#[cfg(feature = \"x\")]\nmod b;\n// mod c;\n";
    let manifest = "[package]\nname = \"pkg\"\n\n[[test]]\nname = \"all\"\n\n[[test]]\n\
                    name = \"proof\"\n\n[[bench]]\nname = \"d\"\n";
    let files = [
        "all.rs",
        "a.rs",
        "b.rs",
        "c.rs",
        "d.rs",
        "pkg.rs",
        "proof.rs",
        "support",
        "snapshots",
    ];
    assert_eq!(
        undeclared(&files, all_rs, manifest),
        ["c.rs", "d.rs", "pkg.rs"]
    );
}

/// Every test file that spawns the binary on a command of its own, without the test kit's
/// runner, gives it no login file, no curl and no automatic update (`support::isolate`,
/// `support::isolation`): the kit's runner does, and a run without them would read the
/// developer's login, and could reach the network through the host's curl or look for a newer
/// release to replace the binary with.
#[test]
fn every_run_of_the_binary_is_isolated() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut open: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(tests).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        // This file runs nothing, and holds the check's own examples.
        if name != "all.rs" && path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            open.extend(unisolated(&text).map(|why| format!("{name}: {why}")));
        }
    }
    assert!(
        open.is_empty(),
        "a run of the binary without support::isolate or support::isolation: {open:#?}"
    );
}

/// What is wrong with `text`, a test file, if it runs the binary without the kit's isolation.
/// A command of the binary's is made from one of its paths (`BIN`, `SO`, `bin`, or Cargo's
/// variable); each place that drops this process's `SWARMOPS_*` variables from one
/// (`swarmops_vars()`) isolates it too, and a file that makes one has such a place.
fn unisolated(text: &str) -> Option<&'static str> {
    const OWN: [&str; 5] = [
        "Command::new(support::BIN)",
        "Command::new(BIN)",
        "Command::new(SO)",
        "Command::new(bin)",
        "Command::new(env!(\"CARGO_BIN_EXE_",
    ];
    let count = |needle: &str| text.matches(needle).count();
    let dropped = count("swarmops_vars()");
    let isolated = count("support::isolate(") + count("support::isolation()");
    if dropped != isolated {
        return Some("a command drops the SWARMOPS_* variables and is not isolated");
    }
    let makes = OWN.iter().any(|needle| text.contains(needle));
    (makes && isolated == 0).then_some("a command of the binary's is never isolated")
}

/// The check itself: it finds a command that is not isolated, where another of the file is, and
/// one that keeps the developer's variables whole; and passes a file that runs no command.
#[test]
fn the_isolation_check_finds_a_bare_run() {
    let isolated = "let mut c = Command::new(support::BIN);\n\
                    for v in support::swarmops_vars() { c.env_remove(v); }\n\
                    support::isolate(&mut c);\n";
    assert_eq!(unisolated(isolated), None);
    assert_eq!(unisolated("support::run(&[\"info\"], &[]);\n"), None);

    let bare = "let mut c = Command::new(SO);\n\
                for v in support::swarmops_vars() { c.env_remove(v); }\n";
    assert!(unisolated(bare).is_some(), "{bare}");
    let one_of_two = format!("{isolated}{bare}");
    assert!(unisolated(&one_of_two).is_some(), "{one_of_two}");
    let whole = "Command::new(env!(\"CARGO_BIN_EXE_swarmops\")).arg(\"info\");\n";
    assert!(unisolated(whole).is_some(), "{whole}");
}

/// The isolation turns automatic update off in the words the program reads: the test kit, which
/// names no crate above the repository, spells the variable and its value itself.
#[test]
fn the_isolation_s_switch_is_the_one_the_program_reads() {
    use swarmops_ops::update::{AUTO_UPDATE_ENV, Switch};

    let isolation = support::isolation();
    let switch = isolation.iter().find(|(name, _)| *name == AUTO_UPDATE_ENV);
    let (_, value) = switch.expect("the isolation names the program's variable");
    assert_eq!(Switch::of(Some(value.as_os_str())), Switch::Off);
}
