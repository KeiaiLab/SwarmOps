//! The store's path as the command line gives it, through the binary: relative to the working
//! directory for the commands that run git in the store's mirror, and through a working
//! directory whose name is not UTF-8.
//!
//! ```text
//!   <tmp>/            working directory
//!     store/          --repo store  ──  push, sync  ──  git, run inside store/git/mirror.git
//!
//!   <tmp>/cwd-\xff/   working directory, not UTF-8
//!     store           init store, --repo store info  ──  E_IO {fs_op: resolve}, no host path
//! ```

use std::path::{Path, PathBuf};

use swarmops_testkit::remote::{GitServer, HttpAuth};

use crate::support::{Run, Store, swarmops};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository the relative store pushes to and syncs with.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The binary with `args` and `env`, run in the working directory `cwd`, to its end.
fn run_in(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let output = swarmops(args, env)
        .current_dir(cwd)
        .output()
        .expect("run swarmops");
    Run {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The real git the server runs, as an absolute path.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// `--repo` relative to the working directory: the git of a push and of a sync runs in the
/// mirror, and still finds it. Each moves the remote, so its git ran.
#[test]
fn a_relative_repo_pushes_and_syncs() {
    // 1. A served repository with one commit, and git's environment for the binary.
    let server = GitServer::new();
    server.create(REPO);
    server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
    let http = server.http(HttpAuth::Anonymous);
    let home = tempfile::tempdir().expect("a temp HOME");
    let git = real_git(&server);
    let env = [
        ("SWARMOPS_GIT", git.to_str().expect("a UTF-8 git")),
        ("HOME", home.path().to_str().expect("UTF-8 temp paths")),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ];
    let head = || server.rev_parse(REPO, "refs/heads/main").expect("main");

    // 2. A store with main at the remote's head, as a clone leaves it.
    let store = Store::init().with_env(&env);
    let url = http.url(REPO);
    let line = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
    store.read(&line).ok();
    let fetched = store.read(&["fetch", ORIGIN]).ok();
    let first = fetched["head"]["changeset"].as_str().expect("a head");
    store
        .run_as(AGENT, &["ref", "set", "main", first, "--create"], &[])
        .ok();

    // 3. The store named from its parent directory, for the commands under test.
    let path = Path::new(store.path());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a UTF-8 store name");
    let parent = path.parent().expect("the store's parent");
    let relative = |args: &[&str]| {
        let line = [&["--repo", name, "--as", AGENT], args].concat();
        run_in(parent, &line, &env)
    };
    let land = |change: &str, onto: &str| {
        let line = ["ref", "set", "main", change, "--expect", onto];
        store.run_as(AGENT, &line, &[]).ok();
    };

    // 4. A change on main, pushed.
    let one = store.commit(AGENT, "one.txt", "one\n", Some(first));
    land(&one, first);
    let before = head();
    let pushed = relative(&["push", ORIGIN, "main"]).ok();
    assert_eq!(pushed["status"], "pushed", "{pushed}");
    assert_ne!(head(), before);

    // 5. Another, synced.
    let two = store.commit(AGENT, "two.txt", "two\n", Some(&one));
    land(&two, &one);
    let before = head();
    let synced = relative(&["sync", ORIGIN]).ok();
    assert_eq!(synced["status"], "pushed", "{synced}");
    assert_eq!(synced["remote_head"]["old"], before.as_str(), "{synced}");
    assert_eq!(synced["remote_head"]["new"], head().as_str(), "{synced}");
    assert_ne!(head(), before);
}

/// A relative store path below a working directory whose name is not UTF-8: `init` and a command
/// that opens the store fail at working out where the path leads, `E_IO` with `fs_op` `resolve`
/// and no store file, not as damage (`E_STORE_CORRUPT`), and nothing printed names the working
/// directory. Nothing is made there.
#[cfg(unix)]
#[test]
fn a_non_utf8_store_path_names_no_host_path() {
    use std::os::unix::ffi::OsStrExt as _;

    // 1. A working directory whose name is not UTF-8.
    let dir = tempfile::tempdir().expect("a temp dir");
    let base = dir.path().canonicalize().expect("the temp dir");
    let cwd = base.join(std::ffi::OsStr::from_bytes(b"cwd-\xff"));
    std::fs::create_dir(&cwd).expect("make the working directory");
    let base = base.to_str().expect("UTF-8 temp paths");

    // 2. Both refuse the relative path, by no path.
    for line in [&["init", "store"][..], &["--repo", "store", "info"]] {
        let failed = run_in(&cwd, line, &[]);
        let error = failed.err("E_IO");
        assert_eq!(error["details"]["fs_op"], "resolve", "{failed}");
        assert_eq!(error["details"].get("store_file"), None, "{failed}");
        for stream in [&failed.stdout, &failed.stderr] {
            assert!(!stream.contains(base), "{failed}");
            assert!(!stream.contains("cwd-"), "{failed}");
        }
    }

    // 3. Nothing was made in the working directory.
    let made = std::fs::read_dir(&cwd)
        .expect("the working directory")
        .count();
    assert_eq!(made, 0, "nothing was created here");
}
