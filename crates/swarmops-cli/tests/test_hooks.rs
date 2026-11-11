//! A build without the `test-hooks` feature ignores the test hooks (storage spec §8), by either
//! name of the binary, `swarmops` or `so`: `SWARMOPS_TEST_CRASH_AT=<k>`, which aborts a build
//! with the feature at its k-th filesystem call, `SWARMOPS_TEST_FS_CALLS=<file>`, which has it
//! write how many calls it made, `SWARMOPS_TEST_FAIL_AT=<k>[:<mode>[:<errno>]]`, which fails its
//! k-th call (and with `persistent` every mutating call after it), `SWARMOPS_TEST_ORDER_CHECK=1`,
//! which checks the order of its writes (the test kit's runner sets it for every run),
//! `SWARMOPS_TEST_NOW=<unix seconds>`, which fixes the time it signs changesets at, and
//! `SWARMOPS_TEST_GITHUB_HOSTS=<host[:port],…>`, which makes a push count those hosts as
//! GitHub's and check GitHub's file limits there, `SWARMOPS_TEST_SERVE_CACHE=bound=<bytes>`
//! or `=no-reach-check`, which bounds `serve`'s pack cache or skips its reachability check, and
//! `SWARMOPS_TEST_LEAD_WAIT=<n>`, which makes a writer that leads the write queue wait for n other
//! requests (and panics on a value that is no number), and `SWARMOPS_TEST_PHASE_LOG=<file>`, which
//! makes an import, a sync and a push append their phases' times to the file.
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
use swarmops_store::{CRASH_AT_ENV, FAIL_AT_ENV, FS_CALLS_ENV, LEAD_WAIT_ENV};

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

        // SWARMOPS_TEST_FS_CALLS, SWARMOPS_TEST_FAIL_AT, SWARMOPS_TEST_NOW and
        // SWARMOPS_TEST_LEAD_WAIT, on a commit: the failure hook would fail its first call and
        // every mutating one after it, and the lead-wait hook would panic on its value.
        let calls = store.tmp().join("fs-calls");
        let now = NOW.to_string();
        let env = [
            (FS_CALLS_ENV, calls.to_str().unwrap()),
            (FAIL_AT_ENV, "1:persistent"),
            (NOW_ENV, now.as_str()),
            (LEAD_WAIT_ENV, "not-a-number"),
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

/// `SWARMOPS_TEST_RAW_OBJECTS=1`, the scale fixtures' uncompressed-write seam: a build with the
/// test hooks writes every object file raw; this build compresses a compressible blob as it
/// would without the variable (storage spec §2, M21 plan task M21-2).
#[test]
fn the_release_build_has_no_raw_seam() {
    use std::fs;

    /// The encoding byte of an object file: 1 is zstd.
    const ZSTD: u8 = 1;

    for bin in [BIN, SO] {
        let store = Store::empty();
        let init = run_bin(bin, &["init", store.path()], &[], "");
        init.ok();

        // A blob of repeated text, which zstd shrinks.
        let text = "the quick brown fox jumps over the lazy dog\n".repeat(64);
        let changes = store.changes("raw", "raw.txt", &text);
        let commit = ["commit", "-m", "raw", "--changes", &changes];
        let env = [("SWARMOPS_TEST_RAW_OBJECTS", "1")];
        let done = run_bin(bin, &store.args(&agent(0), &commit), &env, "");
        done.ok();

        let objects = std::path::Path::new(store.path()).join("objects");
        let mut compressed = 0;
        for fanout in fs::read_dir(objects).unwrap() {
            let fanout = fanout.unwrap().path();
            if !fanout.is_dir() {
                continue;
            }
            for file in fs::read_dir(fanout).unwrap() {
                let bytes = fs::read(file.unwrap().path()).unwrap();
                compressed += usize::from(bytes[5] == ZSTD);
            }
        }
        assert!(
            compressed > 0,
            "{bin}: the raw-objects hook is ignored: the blob is zstd"
        );
    }
}

/// GitHub's warning limit, 50 MiB, and one byte: a file a push to GitHub sends with a warning.
const OVER_WARNED: u64 = 50 * 1024 * 1024 + 1;

/// `SWARMOPS_TEST_GITHUB_HOSTS` naming the loopback server: a build with the test hooks pushes a
/// file over 50 MiB there with a warning (`remote_acceptance::limits_warn_from_50_mib`); this
/// build pushes it with none, as to any host but GitHub's. And `SWARMOPS_TEST_PHASE_LOG` on the
/// import and the push: a build with the hooks writes their phases
/// (`remote_sync::a_phase_log_names_every_phase`); this build writes no file.
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
    let phases = home.path().join("phases.log");
    let phases_text = path(&phases);
    let phase_hook = [("SWARMOPS_TEST_PHASE_LOG", phases_text.as_str())];
    let import = ["git", "import", &path(&big), "--ref", "refs/heads/main"];
    store.run_as(&agent(0), &import, &phase_hook).ok();

    // 3. Pushed with the hooks naming the server and the phase log: sent, and no warning.
    let host = http.host();
    let hook = [("SWARMOPS_TEST_GITHUB_HOSTS", host.as_str()), phase_hook[0]];
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
    assert!(!phases.exists(), "the phase log hook is ignored: no file");
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

/// `SWARMOPS_TEST_SERVE_CACHE=bound=0` on `serve`: a build with the test hooks keeps no pack
/// (`serve_fetch::the_cache_stays_under_its_bound`); this build keeps a clone's under
/// `MAX_PACK_CACHE_BYTES`, as it would without the hook.
#[test]
fn a_build_without_the_feature_ignores_the_serve_cache_hook() {
    use std::fs;

    use swarmops_testkit::git::fixture;
    use swarmops_testkit::serve::ServeSite;

    // 1. A served store holding the `linear` fixture, which person/ada reads.
    let site =
        ServeSite::new::<support::Swarmops>().with_env(&[("SWARMOPS_TEST_SERVE_CACHE", "bound=0")]);
    let store = site.init("acme");
    let access = "version = 1\n\n[[grant]]\nwho = [\"person/ada\"]\nread = true\n";
    fs::write(store.join("access.toml"), access).unwrap();
    let linear = fixture("linear");
    let source = linear.repo.as_deref().unwrap().to_str().unwrap();
    let import = ["git", "import", source, "--ref", "HEAD", "--into", "main"];
    let line = [
        &["--repo", store.to_str().unwrap(), "--as", "agent/worker-1"][..],
        &import,
    ]
    .concat();
    run_bin(BIN, &line, &[], "").ok();

    // 2. A clone through serve: its pack kept all the same.
    let work = tempfile::tempdir().unwrap();
    let cloned = site
        .command()
        .env("GIT_SSH_COMMAND", site.ssh_command("person/ada"))
        .current_dir(work.path())
        .args(["clone", "--quiet", &site.ssh_url("acme"), "clone"])
        .output()
        .unwrap();
    assert!(cloned.status.success(), "{cloned:?}");
    let kept = fs::read_dir(store.join("git/serve-cache"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("pack".as_ref()))
        .count();
    assert_eq!(kept, 1, "the hook is ignored: the clone's pack is kept");
}
