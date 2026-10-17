//! Running the real `swarmops` binary (`CARGO_BIN_EXE_swarmops`, or by its short name `so`,
//! `CARGO_BIN_EXE_so`) from the multi-process and crash tests: one run to its end, children that
//! race behind a start barrier, and a store in a temp dir of its own with the commands the tests
//! run on it.
//!
//! Every child runs with none of the developer's `SWARMOPS_*` variables, only those the test
//! sets, and `SWARMOPS_TEST_ORDER_CHECK=1` (unless the test sets it otherwise): in a build with
//! the `test-hooks` feature, as the gates build it, every write a child makes is checked against
//! the store's write-ordering rules, and a call that breaks one panics, which the child reports
//! as `E_INTERNAL`. Its envelope is checked against the contract before a test reads it: stdout
//! holds one JSON line, `ok` holds exactly when the exit code is 0, and an error's `exit` is the
//! exit code. Every child has a deadline ([`RUN_TIMEOUT`] unless a test gives another): one still
//! running then is killed and fails the test with what it printed. A child is reaped however
//! the test ends: its handle kills and waits for it when it is dropped, a panic's unwinding
//! included. What the crash and error sweeps share (their runner, dry runs and commit template)
//! is in [`sweep`].

#![allow(dead_code)] // Each test binary uses the part it needs.

/// The store's `FsOps` doubles, for the tests that run a store in this process.
#[path = "../../../swarmops-store/tests/support/fs.rs"]
pub mod doubles;
pub mod sweep;

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use doubles::copy_dir;
use serde_json::{Value, json};
use swarmops_store::{Layout, ORDER_CHECK_ENV};
use tempfile::TempDir;

/// The binary under test.
pub const BIN: &str = env!("CARGO_BIN_EXE_swarmops");

/// The binary under test by its short name, `so`: the same program.
pub const SO: &str = env!("CARGO_BIN_EXE_so");

/// The signal `std::process::abort` raises: how a `SWARMOPS_TEST_CRASH_AT` crash ends.
const SIGABRT: i32 = 6;

/// How long [`Racers::race`] waits for every child to reach the barrier.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a child may run, unless a test says otherwise ([`run_within`],
/// [`Racers::race_within`]).
pub const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// How often a wait looks whether a child has ended.
const POLL: Duration = Duration::from_millis(1);

/// The agent of test child `n`: one identity per child.
pub fn agent(n: usize) -> String {
    format!("test-swarm/worker-{n}")
}

/// The binary with `args`, the order check on, `env` set, and none of the other `SWARMOPS_*`
/// variables of this process's environment.
pub fn swarmops<S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Command {
    named(BIN, args, env)
}

/// [`swarmops`] by the name `bin`: [`BIN`] or [`SO`].
fn named<S: AsRef<str>>(bin: &str, args: &[S], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(bin);
    command.args(args.iter().map(AsRef::as_ref));
    with_env(&mut command, env);
    command
}

/// The names of this process's `SWARMOPS_*` variables: every run of the binary goes without
/// them, but for those its test sets.
pub fn swarmops_vars() -> impl Iterator<Item = OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_string_lossy().starts_with("SWARMOPS_"))
}

/// Gives `command` the environment of this process without its `SWARMOPS_*` variables, then
/// `SWARMOPS_TEST_ORDER_CHECK=1`, then `env`; and pipes for its output.
fn with_env(command: &mut Command, env: &[(&str, &str)]) {
    for name in swarmops_vars() {
        command.env_remove(name);
    }
    command
        .env(ORDER_CHECK_ENV, "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// Runs the binary with `args` and `env` to its end, within [`RUN_TIMEOUT`].
pub fn run<S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Run {
    run_within(args, env, RUN_TIMEOUT)
}

/// Runs the binary with `args` and `env` to its end; one still running after `timeout` is
/// killed and fails the test.
pub fn run_within<S: AsRef<str>>(args: &[S], env: &[(&str, &str)], timeout: Duration) -> Run {
    let child = swarmops(args, env).spawn().expect("run swarmops");
    Running::start(args, child).finish(Instant::now() + timeout)
}

/// Runs the binary by the name `bin` ([`BIN`] or [`SO`]) with `args` and `env` to its end,
/// within [`RUN_TIMEOUT`], `input` all that it reads on stdin.
pub fn run_bin<S: AsRef<str>>(bin: &str, args: &[S], env: &[(&str, &str)], input: &str) -> Run {
    run_bin_within(bin, args, env, input, RUN_TIMEOUT)
}

/// [`run_bin`] within `timeout`: a child still running then is killed and fails the test. The
/// input is written from a thread of its own ([`feed`]) while the deadline runs, so a child that
/// leaves it unread cannot hold the test past its deadline, however much of it there is.
pub fn run_bin_within<S: AsRef<str>>(
    bin: &str,
    args: &[S],
    env: &[(&str, &str)],
    input: &str,
    timeout: Duration,
) -> Run {
    let mut child = named(bin, args, env)
        .stdin(Stdio::piped())
        .spawn()
        .expect("run swarmops");
    let stdin = child.stdin.take().expect("a piped stdin");
    let mut running = Running::start(args, child);
    let feeding = feed(stdin, input);
    let run = running.finish(Instant::now() + timeout);
    // The child is reaped, so the write is over. Past the deadline, `finish` fails the test
    // before this, and the thread ends by itself: the killed child's end of the pipe is closed.
    feeding.join().expect("write a child's stdin");
    run
}

/// The threads that read a child's stdout and stderr to their ends.
type Readers = (JoinHandle<Vec<u8>>, JoinHandle<Vec<u8>>);

/// A child whose stdout and stderr threads of their own read. Dropped, it is killed if it still
/// runs, and reaped either way.
struct Running {
    args: Vec<String>,
    child: Child,
    output: Option<Readers>,
}

impl Running {
    fn start<S: AsRef<str>>(args: &[S], mut child: Child) -> Running {
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        Running {
            args: args.iter().map(|arg| arg.as_ref().to_owned()).collect(),
            child,
            output: Some((stdout, stderr)),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the child to end, until `deadline`: one still running then is killed, reaped,
    /// and fails the test with what it printed.
    fn finish(&mut self, deadline: Instant) -> Run {
        let mut killed = false;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("look whether a child ended") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                killed = true;
                break self
                    .child
                    .wait()
                    .expect("reap a child killed at its deadline");
            }
            thread::sleep(POLL);
        };
        let (stdout, stderr) = self.output.take().expect("finished once");
        let stdout = stdout.join().expect("read a child's stdout");
        let stderr = stderr.join().expect("read a child's stderr");
        let run = Run::new(&self.args, status, &stdout, &stderr);
        assert!(!killed, "past its deadline, so killed: {run}");
        run
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // An error is a child reaped already.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A thread that reads `pipe` to its end.
fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    let mut pipe = pipe.expect("a piped output");
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

/// A thread that writes `input` to `pipe`, a child's stdin, then closes it. The write ends once
/// the child has read all of it, or once the child has ended, which closes its end of the pipe.
fn feed(mut pipe: ChildStdin, input: &str) -> JoinHandle<()> {
    let input = input.to_owned();
    thread::spawn(move || {
        // A child that exits before it reads all of its input shows that in its run.
        let _ = pipe.write_all(input.as_bytes());
    })
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
/// then reads a line from its stdin, a pipe from the parent, then becomes the binary (`exec`).
/// [`Racers::race`] writes each child its line once every child is ready, so that they all run
/// at once and race for real. Every child is reaped however the test ends ([`Running`]), and a
/// child whose pipe closes without its line (the parent gave up) exits without running.
pub struct Racers {
    /// The children's ready files, `ready-<n>`.
    dir: TempDir,
    children: Vec<Running>,
    /// Each child's stdin, until its line is written.
    gates: Vec<ChildStdin>,
}

/// What each child's shell runs: `$1` its ready file, the rest the command.
const BARRIER: &str = r#"
: > "$1"
read -r _ || exit 125
shift
exec "$@"
"#;

impl Racers {
    /// Racers whose ready files go in a fresh directory under `dir`.
    pub fn new(dir: &Path) -> Racers {
        Racers {
            dir: tempfile::tempdir_in(dir).expect("make a barrier directory"),
            children: Vec::new(),
            gates: Vec::new(),
        }
    }

    /// Spawns the binary with `args` and `env` as one more child; it waits at the barrier.
    pub fn spawn<S: AsRef<str>>(&mut self, args: &[S], env: &[(&str, &str)]) {
        let ready = self
            .dir
            .path()
            .join(format!("ready-{}", self.children.len()));
        let mut shell = Command::new("/bin/sh");
        shell
            .args(["-c", BARRIER, "sh"])
            .arg(ready)
            .arg(BIN)
            .args(args.iter().map(AsRef::as_ref));
        with_env(&mut shell, env);
        shell.stdin(Stdio::piped());
        let mut child = shell.spawn().expect("spawn a child");
        self.gates.push(child.stdin.take().expect("a piped stdin"));
        self.children.push(Running::start(args, child));
    }

    /// The process ids of the children.
    pub fn pids(&self) -> Vec<u32> {
        self.children.iter().map(Running::id).collect()
    }

    /// [`Racers::race_within`] [`RUN_TIMEOUT`].
    pub fn race(self) -> Vec<Run> {
        self.race_within(RUN_TIMEOUT)
    }

    /// Lets every child go at once, once all of them wait at the barrier, then waits for each
    /// to end; their runs, in the order they were spawned. Children still running `timeout`
    /// after they were let go are killed, and fail the test.
    pub fn race_within(mut self, timeout: Duration) -> Vec<Run> {
        let deadline = Instant::now() + READY_TIMEOUT;
        for at in 0..self.children.len() {
            let ready = self.dir.path().join(format!("ready-{at}"));
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "child {at} never reached the barrier"
                );
                thread::sleep(POLL);
            }
        }
        for mut gate in self.gates.drain(..) {
            // A child that died before reading its line shows in its run.
            let _ = gate.write_all(b"\n");
        }
        let deadline = Instant::now() + timeout;
        self.children
            .iter_mut()
            .map(|child| child.finish(deadline))
            .collect()
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

    /// The store's paths.
    pub fn layout(&self) -> Layout {
        Layout::new(Utf8PathBuf::from(self.path()))
    }

    /// `meta/journal.ckpt`: how far the checkpoint is.
    pub fn checkpoint(&self) -> u64 {
        let bytes = fs::read(self.layout().journal_ckpt()).expect("read meta/journal.ckpt");
        u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
    }

    /// The length of `meta/journal.log`.
    pub fn journal_len(&self) -> u64 {
        let log = self.layout().journal_log();
        fs::metadata(log).expect("meta/journal.log").len()
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
