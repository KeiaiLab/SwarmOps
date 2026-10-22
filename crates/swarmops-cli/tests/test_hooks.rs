//! A build without the `test-hooks` feature ignores the test hooks (storage spec §8), by either
//! name of the binary, `swarmops` or `so`: `SWARMOPS_TEST_CRASH_AT=<k>`, which aborts a build
//! with the feature at its k-th filesystem call, `SWARMOPS_TEST_FS_CALLS=<file>`, which has it
//! write how many calls it made, `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]`, which fails its
//! k-th call (and with `persistent` every mutating call after it), `SWARMOPS_TEST_ORDER_CHECK=1`,
//! which checks the order of its writes (the test kit's runner sets it for every run),
//! `SWARMOPS_TEST_NOW=<unix seconds>`, which fixes the time it signs changesets at, and
//! `SWARMOPS_TEST_GITHUB_HOSTS=<host[:port],…>`, which makes a push count those hosts as
//! GitHub's and check GitHub's file limits there.
//!
//! The test exists only in a build without the feature, as `cargo build --release` is. `just
//! check` and `scripts/check.sh` run it in a release build and require it to run
//! (`--no-tests=fail`), so a build that turns the feature on all the same (a stray default, say)
//! fails the gate rather than passing it with nothing tested. The crash tests (`crash.rs`) show
//! the hooks acting when the feature is on.
//!
//! That release run is also the proof that a release build holds none of the store's fault
//! doubles (`CrashAtFs`, `FailAtFs`, `OrderCheckFs`), and the proof is the compiler's: the doubles
//! compile only with `swarmops-store`'s `test-hooks` feature, and the run builds the store, with
//! the `swarmops` and `so` it tests, without it, so code a release build holds that names one
//! does not compile, and fails the gate. (A scan of the binary's symbols would prove less:
//! inlining and stripping change them.) The test kit's own `test-hooks` is not a default for the
//! same reason: as a dev-dependency of this crate, a default would turn the store's on here.

#![cfg(not(feature = "test-hooks"))]

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use support::{BIN, SO, Store, agent, run_bin};
use swarmops_ops::NOW_ENV;
use swarmops_store::{CRASH_AT_ENV, FAIL_AT_ENV, FS_CALLS_ENV};

/// The time the commit asks to be signed at: September 2001, in Unix seconds.
const NOW: u64 = 1_000_000_000;

#[test]
fn a_build_without_the_feature_ignores_the_test_hooks() {
    for bin in [BIN, SO] {
        // SWARMOPS_TEST_CRASH_AT=1 would abort init at its first filesystem call.
        let store = Store::empty();
        let init = run_bin(bin, &["init", store.path()], &[(CRASH_AT_ENV, "1")], "");
        assert_eq!(
            init.ok()["changesets"],
            0,
            "{bin}: the crash hook is ignored: {init}"
        );

        // SWARMOPS_TEST_FS_CALLS, SWARMOPS_TEST_FAIL_AT and SWARMOPS_TEST_NOW, on a commit: the
        // failure hook would fail its first call and every mutating one after it.
        let calls = store.tmp().join("fs-calls");
        let now = NOW.to_string();
        let env = [
            (FS_CALLS_ENV, calls.to_str().unwrap()),
            (FAIL_AT_ENV, "1:persistent"),
            (NOW_ENV, now.as_str()),
        ];
        let changes = store.changes("hooks", "hooks.txt", "the test hooks\n");
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let commit = ["commit", "-m", "hooks", "--changes", &changes];
        let commit = run_bin(bin, &store.args(&agent(0), &commit), &env, "");
        // The failure hook is ignored: the commit succeeded.
        let id = commit.ok()["changeset"].as_str().unwrap().to_owned();
        let secs = store.show(&id)["signatures"]["committer"]["secs"]
            .as_u64()
            .unwrap();
        assert!(
            !calls.exists(),
            "{bin}: the call-count hook is ignored: nothing written"
        );
        assert!(
            secs >= before,
            "{bin}: signed at the clock's time, not SWARMOPS_TEST_NOW's: {secs} < {before}"
        );
        store.assert_clean(&format!("{bin}: after the commit"));
    }
}

/// GitHub's warning limit, 50 MiB, and one byte: a file a push to GitHub sends with a warning.
const OVER_WARNED: u64 = 50 * 1024 * 1024 + 1;

/// `SWARMOPS_TEST_GITHUB_HOSTS` naming the loopback server: a build with the test hooks pushes a
/// file over 50 MiB there with a warning (`remote_acceptance::limits_warn_from_50_mib`); this
/// build pushes it with none, as to any host but GitHub's.
#[test]
fn a_build_without_the_feature_ignores_the_github_hosts_hook() {
    use std::fs;
    use std::path::Path;

    use swarmops_testkit::remote::{GitServer, HttpAuth};

    let server = GitServer::new();
    server.create("repo");
    let big = server.create("big");
    let http = server.http(HttpAuth::Anonymous);
    let home = tempfile::tempdir().unwrap();
    let path = |path: &Path| path.to_str().unwrap().to_owned();
    let git = |args: &[&str]| {
        let output = server
            .command()
            .arg("--git-dir")
            .arg(&big)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };

    // 1. A repository whose main holds the file, as one commit.
    let file = home.path().join("huge.bin");
    fs::File::create(&file)
        .unwrap()
        .set_len(OVER_WARNED)
        .unwrap();
    let blob = git(&["hash-object", "-w", &path(&file)]);
    let entry = format!("100644 blob {blob}\thuge.bin\n");
    let tree_file = home.path().join("tree");
    fs::write(&tree_file, entry).unwrap();
    let tree = {
        let input = fs::File::open(&tree_file).unwrap();
        let output = server
            .command()
            .arg("--git-dir")
            .arg(&big)
            .arg("mktree")
            .stdin(input)
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let commit = git(&["commit-tree", &tree, "-m", "large"]);
    git(&["update-ref", "refs/heads/main", &commit]);

    // 2. A store that imported it and follows the empty repository.
    let env = [
        ("SWARMOPS_GIT", path(&absolute(server.git_program()))),
        ("HOME", path(home.path())),
        ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_owned()),
    ];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let store = Store::init().with_env(&env);
    let url = http.url("repo");
    store
        .read(&["remote", "add", "origin", &url, "--branch", "main"])
        .ok();
    let import = ["git", "import", &path(&big), "--ref", "refs/heads/main"];
    store.run_as(&agent(0), &import, &[]).ok();

    // 3. Pushed with the hook naming the server: sent, and no warning.
    let host = http.host();
    let hook = [("SWARMOPS_TEST_GITHUB_HOSTS", host.as_str())];
    let pushed = run_bin(
        BIN,
        &store.args(&agent(0), &["push", "origin"]),
        &[&env[..], &hook].concat(),
        "",
    );
    let result = pushed.ok();
    assert_eq!(result["status"], "created", "{pushed}");
    assert!(
        result.get("warnings").is_none(),
        "the hook is ignored: {pushed}"
    );
}

/// `program` as an absolute path: itself, or the first of its name on `PATH`.
fn absolute(program: &std::path::Path) -> std::path::PathBuf {
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}
