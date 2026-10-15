//! Running the real `swarmops` binary (`CARGO_BIN_EXE_swarmops`) from the multi-process and
//! crash tests: one run to its end, children that race behind a start barrier, and a store in a
//! temp dir of its own with the commands the tests run on it.
//!
//! Every child runs with none of the developer's `SWARMOPS_*` variables, only those the test
//! sets, and its envelope is checked against the contract before a test reads it: stdout holds
//! one JSON line, `ok` holds exactly when the exit code is 0, and an error's `exit` is the exit
//! code.

#![allow(dead_code)] // Each test binary uses the part it needs.

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

/// The binary under test.
pub const BIN: &str = env!("CARGO_BIN_EXE_swarmops");

/// The signal `std::process::abort` raises: how a `SWARMOPS_TEST_CRASH_AT` crash ends.
const SIGABRT: i32 = 6;

/// How long [`Racers::race`] waits for every child to reach the barrier.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The agent of test child `n`: one identity per child.
pub fn agent(n: usize) -> String {
    format!("test-swarm/worker-{n}")
}

/// The binary with `args`, `env` set, and none of the other `SWARMOPS_*` variables of this
/// process's environment.
pub fn swarmops<S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(BIN);
    command.args(args.iter().map(AsRef::as_ref));
    with_env(&mut command, env);
    command
}

/// Gives `command` the environment of this process without its `SWARMOPS_*` variables, then
/// `env`; and pipes for its output.
fn with_env(command: &mut Command, env: &[(&str, &str)]) {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SWARMOPS_") {
            command.env_remove(name);
        }
    }
    command
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// Runs the binary with `args` and `env` to its end.
pub fn run<S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Run {
    let output = swarmops(args, env).output().expect("run swarmops");
    Run::new(args, output.status, &output.stdout, &output.stderr)
}

/// One finished run of the binary.
#[derive(Debug)]
pub struct Run {
    /// Its arguments, for messages.
    pub args: Vec<String>,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    fn new<S: AsRef<str>>(args: &[S], status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Run {
        Run {
            args: args.iter().map(|arg| arg.as_ref().to_owned()).collect(),
            status,
            stdout: String::from_utf8_lossy(stdout).into_owned(),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        }
    }

    /// Whether the run died of SIGABRT: a `SWARMOPS_TEST_CRASH_AT` crash.
    pub fn aborted(&self) -> bool {
        self.status.signal() == Some(SIGABRT)
    }

    /// The exit code; a run that a signal ended fails the test.
    pub fn exit(&self) -> i32 {
        self.status
            .code()
            .unwrap_or_else(|| panic!("swarmops did not exit: {self}"))
    }

    /// The envelope the run printed, checked against the contract.
    pub fn envelope(&self) -> Value {
        let exit = self.exit();
        let lines = self.stdout.lines().count();
        assert!(
            self.stdout.ends_with('\n') && lines == 1,
            "one compact line: {self}"
        );
        let envelope: Value = serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is one JSON document ({e}): {self}"));
        assert_eq!(envelope["api"], 1, "{self}");
        assert_eq!(envelope["ok"], exit == 0, "{self}");
        if exit == 0 {
            assert!(envelope.get("error").is_none(), "{self}");
        } else {
            assert_eq!(envelope["error"]["exit"], exit, "{self}");
            assert!(envelope.get("result").is_none(), "{self}");
        }
        envelope
    }

    /// The result of a run that must have succeeded.
    pub fn ok(&self) -> Value {
        assert_eq!(self.exit(), 0, "{self}");
        self.envelope()["result"].take()
    }

    /// The error of a run that must have failed with `code`.
    pub fn err(&self, code: &str) -> Value {
        let mut envelope = self.envelope();
        assert_eq!(envelope["error"]["code"], code, "{self}");
        envelope["error"].take()
    }
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "swarmops {:?} ended {}\n--- stdout ---\n{}--- stderr ---\n{}",
            self.args, self.status, self.stdout, self.stderr
        )
    }
}

/// Children of the binary that start together: each is a shell that makes its ready file,
/// waits until the go file exists, then becomes the binary (`exec`). [`Racers::race`] creates
/// the go file once every child is ready, so that they all run at once and race for real.
/// Children still running when this is dropped (a test failed) are killed.
pub struct Racers {
    /// The barrier's files: `ready-<n>` for each child, and `go`.
    dir: TempDir,
    children: Vec<(Vec<String>, Child)>,
}

/// What each child's shell runs: `$1` its ready file, `$2` the go file, the rest the command.
const BARRIER: &str = r#"
: > "$1"
while [ ! -e "$2" ]; do sleep 0.001; done
shift 2
exec "$@"
"#;

impl Racers {
    /// Racers whose barrier files go in a fresh directory under `dir`.
    pub fn new(dir: &Path) -> Racers {
        Racers {
            dir: tempfile::tempdir_in(dir).expect("make a barrier directory"),
            children: Vec::new(),
        }
    }

    /// Spawns the binary with `args` and `env` as one more child; it waits at the barrier.
    pub fn spawn<S: AsRef<str>>(&mut self, args: &[S], env: &[(&str, &str)]) {
        let dir = self.dir.path();
        let mut shell = Command::new("/bin/sh");
        shell
            .args(["-c", BARRIER, "sh"])
            .arg(dir.join(format!("ready-{}", self.children.len())))
            .arg(dir.join("go"))
            .arg(BIN)
            .args(args.iter().map(AsRef::as_ref));
        with_env(&mut shell, env);
        let child = shell.spawn().expect("spawn a child");
        let args = args.iter().map(|arg| arg.as_ref().to_owned()).collect();
        self.children.push((args, child));
    }

    /// Lets every child go at once, once all of them wait at the barrier, then waits for each
    /// to end; their runs, in the order they were spawned.
    pub fn race(mut self) -> Vec<Run> {
        let deadline = Instant::now() + READY_TIMEOUT;
        for at in 0..self.children.len() {
            let ready = self.dir.path().join(format!("ready-{at}"));
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "child {at} never reached the barrier"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        fs::write(self.dir.path().join("go"), b"").expect("open the barrier");
        let children = std::mem::take(&mut self.children);
        children
            .into_iter()
            .map(|(args, child)| {
                let output = child.wait_with_output().expect("wait for a child");
                Run::new(&args, output.status, &output.stdout, &output.stderr)
            })
            .collect()
    }
}

impl Drop for Racers {
    fn drop(&mut self) {
        for (_, child) in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A store in a temp dir of its own (`<tmp>/store`), and the commands the tests run on it.
pub struct Store {
    dir: TempDir,
    path: String,
    /// How many change lists [`Store::commit`] has written, for their names.
    commits: AtomicU64,
    /// Variables every command run on the store sets ([`Store::with_env`]).
    env: Vec<(String, String)>,
}

impl Store {
    /// A fresh store, made with `swarmops init`.
    pub fn init() -> Store {
        let store = Store::empty();
        let init = run(&["init", store.path()], &[]);
        assert_eq!(init.ok()["changesets"], 0, "{init}");
        store
    }

    /// A temp dir with no store in it yet: [`Store::path`] does not exist.
    pub fn empty() -> Store {
        let dir = tempfile::tempdir().expect("make a temp dir");
        let path = format!("{}/store", dir.path().to_str().expect("UTF-8 temp paths"));
        Store {
            dir,
            path,
            commits: AtomicU64::new(0),
            env: Vec::new(),
        }
    }

    /// This store, every command run on it setting `env` too (before the command's own).
    #[must_use]
    pub fn with_env(mut self, env: &[(&str, &str)]) -> Store {
        let env = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()));
        self.env.extend(env);
        self
    }

    /// A copy of `template`'s store, and of the variables its commands set, in a temp dir of
    /// its own, nothing synced: what a test crashes a run in, from the same starting point
    /// each time.
    pub fn copy_of(template: &Store) -> Store {
        let mut store = Store::empty();
        copy_dir(Path::new(template.path()), Path::new(store.path()));
        store.env.clone_from(&template.env);
        store
    }

    /// The store's path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The temp dir: the store and the files the tests write.
    pub fn tmp(&self) -> &Path {
        self.dir.path()
    }

    /// Writes a change list that puts `content` at `path` to the temp dir as `<name>.json`;
    /// the file's path.
    pub fn changes(&self, name: &str, path: &str, content: &str) -> String {
        self.changes_of(name, &[(path, content)])
    }

    /// Writes a change list of one put per `(path, content)` to the temp dir as
    /// `<name>.json`; the file's path.
    pub fn changes_of(&self, name: &str, puts: &[(&str, &str)]) -> String {
        let changes: Vec<Value> = puts
            .iter()
            .map(|(path, content)| {
                json!({"path": path, "op": "put", "content": {"inline": content}})
            })
            .collect();
        let file = self.dir.path().join(format!("{name}.json"));
        fs::write(&file, Value::from(changes).to_string()).expect("write a change list");
        file.to_str().expect("UTF-8 temp paths").to_owned()
    }

    /// `--repo <store> --as <agent>`, then `args`.
    pub fn args(&self, agent: &str, args: &[&str]) -> Vec<String> {
        ["--repo", self.path(), "--as", agent]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
    }

    /// Runs `args` on the store as `agent`, with the store's variables and `env` set.
    pub fn run_as(&self, agent: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
        run(&self.args(agent, args), &self.env_with(env))
    }

    /// Runs the read `args` on the store (no identity), with the store's variables set.
    pub fn read(&self, args: &[&str]) -> Run {
        let args: Vec<&str> = ["--repo", self.path()]
            .iter()
            .chain(args)
            .copied()
            .collect();
        run(&args, &self.env_with(&[]))
    }

    /// The store's variables, then `env`.
    fn env_with<'a>(&'a self, env: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let own = self
            .env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()));
        own.chain(env.iter().copied()).collect()
    }

    /// Commits `content` at `path` as `agent`, on `parent` (default: main, if it exists);
    /// the new changeset's id.
    pub fn commit(&self, agent: &str, path: &str, content: &str, parent: Option<&str>) -> String {
        let n = self.commits.fetch_add(1, Ordering::Relaxed);
        let changes = self.changes(&format!("commit-{n}"), path, content);
        let mut args = vec!["commit", "-m", path, "--changes", &changes];
        if let Some(parent) = parent {
            args.extend(["--parent", parent]);
        }
        let result = self.run_as(agent, &args, &[]).ok();
        assert!(result.get("warnings").is_none(), "{result}");
        result["changeset"]
            .as_str()
            .expect("a changeset id")
            .to_owned()
    }

    /// `show <rev>`'s result.
    pub fn show(&self, rev: &str) -> Value {
        self.read(&["show", rev]).ok()
    }

    /// `info`'s result.
    pub fn info(&self) -> Value {
        self.read(&["info"]).ok()
    }

    /// What `verify --deep` finds, through the binary: a clean store exits 0 with the report
    /// as its result, a damaged one exits 1 with `E_STORE_CORRUPT` and the report in
    /// `details`.
    pub fn problems(&self) -> Vec<String> {
        let verify = self.read(&["verify", "--deep"]);
        let report = match verify.exit() {
            0 => verify.ok(),
            _ => verify.err("E_STORE_CORRUPT")["details"].take(),
        };
        let problems = report["problems"].as_array().expect("a problem list");
        problems
            .iter()
            .map(|problem| problem.as_str().expect("one line").to_owned())
            .collect()
    }

    /// Asserts that `verify --deep` finds nothing wrong; `what` says when.
    #[track_caller]
    pub fn assert_clean(&self, what: &str) {
        assert_eq!(self.problems(), Vec::<String>::new(), "{what}");
    }

    /// The files in the store's `objects/tmp/`: what crashed writes leave behind.
    pub fn temp_files(&self) -> usize {
        fs::read_dir(Path::new(self.path()).join("objects/tmp"))
            .expect("objects/tmp")
            .count()
    }

    /// The object files in the store's fan-out directories.
    pub fn object_files(&self) -> usize {
        let objects = Path::new(self.path()).join("objects");
        let mut count = 0;
        for fanout in fs::read_dir(objects).expect("objects/") {
            let fanout = fanout.expect("an entry of objects/");
            if fanout.file_name() != "tmp" {
                count += fs::read_dir(fanout.path()).expect("a fan-out dir").count();
            }
        }
        count
    }
}

/// Copies directory `from` to `to`, files and subdirectories, without syncing anything.
pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("make a directory");
    for entry in fs::read_dir(from).expect("read a directory") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("its type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy a file");
        }
    }
}
