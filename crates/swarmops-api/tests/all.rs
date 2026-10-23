//! The API's integration tests, in one test binary (one link): each file of `tests/` is a
//! module here, and its tests are named after it (`schema::<test>`). Cargo builds no other test
//! target from `tests/` (`autotests = false` in `Cargo.toml`): a new test file is a `mod` line
//! here.

mod constructors;
mod identity;
mod m4;
mod maintenance;
mod schema;

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
