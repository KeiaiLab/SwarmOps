//! Runs of the binary: one to its end, racers behind a start barrier, and a store of its own.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::marker::PhantomData;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{self, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use swarmops_store::{Layout, ORDER_CHECK_ENV, StoreFormat};
use tempfile::TempDir;

use crate::child::{Child, SIGABRT, no_core_dumps};

/// The binary under test, as the CLI's tests bind it: Cargo gives its path to them alone
/// (`env!("CARGO_BIN_EXE_swarmops")`), so they implement this for a type of their own and run
/// [`Store`], [`Racers`], [`run`] and the sweeps with it.
pub trait Program {
    /// The binary's path.
    const BIN: &'static str;

    /// The variable that has the binary sign the changesets it commits at the time it holds, in
    /// Unix seconds: `swarmops_ops::NOW_ENV`, which this crate cannot name (it depends on no crate
    /// above the repository: see the crate's documentation).
    const NOW_ENV: &'static str;
}

/// How long [`Racers::race`] waits for every child to reach the barrier.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a child may run, unless a test says otherwise ([`run_within`], [`run_bin_within`],
/// [`Racers::race_within`]).
pub const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// How often [`Racers::race_within`] looks whether a child has reached the barrier.
const POLL: Duration = Duration::from_millis(1);

/// How many times at most [`retry_busy`] runs a setup command again that met `E_BUSY`.
pub const BUSY_RETRIES: usize = 5;

/// The variable that names the directory of the stores' memo keys (`swarmops-cli`'s
/// `context::MEMO_KEY_DIR_ENV`, which this crate cannot name): every run of the binary sets it
/// ([`isolate`]), so that no test makes a key in the user's state directory.
const MEMO_KEY_DIR_ENV: &str = "SWARMOPS_MEMO_KEY_DIR";

/// The memo keys' directory of every run, under the system's temp directory: one for all, so
/// that every command run on a store, through a [`Store`] or not, reads the same key.
const SHARED_MEMO_KEYS: &str = "swarmops-test-memo-keys";

/// The variable that names where `check run --dir` keeps its runs (`swarmops-ops`'
/// `check::run::CHECK_RUNS_DIR_ENV`, which this crate cannot name): every run of the binary sets
/// it ([`isolate`]), so that no test writes a run into the user's state directory.
const CHECK_RUNS_DIR_ENV: &str = "SWARMOPS_CHECK_RUNS_DIR";

/// The runs' directory of every run, under the system's temp directory: one for all, its runs
/// kept apart by the key of the directory each checked.
const SHARED_CHECK_RUNS: &str = "swarmops-test-check-runs";

/// The variable that names the user's login file, which this crate cannot name by its constant:
/// every run of the binary sets it ([`isolate`]), so that no test reads or writes the
/// developer's.
const AUTH_FILE_ENV: &str = "SWARMOPS_AUTH_FILE";

/// The variable that names the `curl` a login runs: every run of the binary sets it
/// ([`isolate`]), so that no test reaches the network through the host's.
const CURL_ENV: &str = crate::curl::CURL_ENV;

/// The variable that turns the binary's automatic update off, which this crate cannot name by
/// its constant: every run of the binary sets it to [`AUTO_UPDATE_OFF`] ([`isolate`]), so that
/// no test asks a release host for a newer version, replaces a binary or leaves a file beside
/// one.
const AUTO_UPDATE_ENV: &str = "SWARMOPS_AUTO_UPDATE";

/// The value of [`AUTO_UPDATE_ENV`] that turns automatic update off.
const AUTO_UPDATE_OFF: &str = "off";

/// The directory, under the system's temp directory, of the login file and the `curl` every
/// run is given: nothing makes it, so neither exists.
const NO_LOGIN: &str = "swarmops-test-no-login";

/// The agent of test child `n`: one identity per child.
pub fn agent(n: usize) -> String {
    format!("test-swarm/worker-{n}")
}

/// The binary with `args`, the order check on, `env` set, and none of the other `SWARMOPS_*`
/// variables of this process's environment.
pub fn swarmops<P: Program, S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Command {
    named(P::BIN, args, env)
}

/// [`swarmops`] by the name `bin`: the binary's path by either of its names.
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

/// What keeps a run of the binary away from the user's login, from the network, from the
/// binary's own file and from the user's state directory: the login file's variable
/// (`SWARMOPS_AUTH_FILE`) and curl's (`SWARMOPS_CURL`), each naming an absolute path under the
/// system's temp directory where nothing is and nothing is made, `SWARMOPS_AUTO_UPDATE=off`,
/// the memo keys' directory (`SWARMOPS_MEMO_KEY_DIR`) and the check runs' directory
/// (`SWARMOPS_CHECK_RUNS_DIR`), the ones under the system's temp directory that every run
/// shares. The run then finds no login and no curl, whatever the developer's home directory and
/// `PATH` hold, neither looks for a newer release nor installs one, and makes a store's memo key
/// and keeps a `check run --dir`'s run there, not in the developer's state directory. A test of
/// a login names a file of its own, one of the device flow a stand-in for curl, one of
/// automatic update turns it on for a copy of the binary in a directory of its own, and one of
/// the memo keys or of the check runs names a directory of its own or none.
pub fn isolation() -> [(&'static str, OsString); 5] {
    let temp = std::env::temp_dir();
    let nowhere = temp.join(NO_LOGIN);
    [
        (AUTH_FILE_ENV, nowhere.join("auth.toml").into_os_string()),
        (CURL_ENV, nowhere.join("curl").into_os_string()),
        (AUTO_UPDATE_ENV, OsString::from(AUTO_UPDATE_OFF)),
        (
            MEMO_KEY_DIR_ENV,
            temp.join(SHARED_MEMO_KEYS).into_os_string(),
        ),
        (
            CHECK_RUNS_DIR_ENV,
            temp.join(SHARED_CHECK_RUNS).into_os_string(),
        ),
    ]
}

/// Sets [`isolation`] on `command`, a run of the binary. Every place that spawns the binary
/// calls this, or sets [`isolation`] itself: after it has removed this process's `SWARMOPS_*`
/// variables ([`swarmops_vars`]), which would unset these again, and before the test's own
/// variables, which may name a login file, a curl or a memo keys' directory of the test's, or
/// turn automatic update on.
pub fn isolate(command: &mut Command) {
    command.envs(isolation());
}

/// Gives `command` the environment of this process without its `SWARMOPS_*` variables, then
/// `SWARMOPS_TEST_ORDER_CHECK=1` and no login, curl or automatic update, with a memo keys'
/// directory under the system's temp directory ([`isolate`]), then `env`; and pipes for its
/// output.
fn with_env(command: &mut Command, env: &[(&str, &str)]) {
    for name in swarmops_vars() {
        command.env_remove(name);
    }
    isolate(command);
    command
        .env(ORDER_CHECK_ENV, "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// Runs the binary with `args` and `env` to its end, within [`RUN_TIMEOUT`].
pub fn run<P: Program, S: AsRef<str>>(args: &[S], env: &[(&str, &str)]) -> Run {
    run_within::<P, S>(args, env, RUN_TIMEOUT)
}

/// Runs the binary with `args` and `env` to its end; one still running after `timeout` is
/// killed and fails the test.
pub fn run_within<P: Program, S: AsRef<str>>(
    args: &[S],
    env: &[(&str, &str)],
    timeout: Duration,
) -> Run {
    no_core_dumps();

    let child = swarmops::<P, S>(args, env).spawn().expect("run swarmops");
    Running::start(args, child).finish(Instant::now() + timeout)
}

/// Runs the binary by the name `bin` (its path by either of its names, `swarmops` or `so`) with
/// `args` and `env` to its end, within [`RUN_TIMEOUT`], `input` all that it reads on stdin.
pub fn run_bin<S: AsRef<str>>(bin: &str, args: &[S], env: &[(&str, &str)], input: &str) -> Run {
    run_bin_within(bin, args, env, input, RUN_TIMEOUT)
}

/// [`run_bin`] within `timeout`: a child still running then is killed and fails the test. The
/// input is written from a thread of its own (`feed`) while the deadline runs, so a child that
/// leaves it unread cannot hold the test past its deadline, however much of it there is.
pub fn run_bin_within<S: AsRef<str>>(
    bin: &str,
    args: &[S],
    env: &[(&str, &str)],
    input: &str,
    timeout: Duration,
) -> Run {
    no_core_dumps();

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

/// A run of the binary, its child waited for by [`Child`]: killed if it still runs when dropped,
/// and reaped either way.
struct Running {
    /// Its arguments, for its [`Run`].
    args: Vec<String>,
    /// The child.
    child: Child,
}

impl Running {
    /// The run of `child`, the binary with `args`, spawned with its stdout and stderr piped.
    fn start<S: AsRef<str>>(args: &[S], child: process::Child) -> Running {
        Running {
            args: args.iter().map(|arg| arg.as_ref().to_owned()).collect(),
            child: Child::adopt(child),
        }
    }

    /// The child's process id.
    fn id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the child to end, until `deadline`: one still running then is killed, reaped,
    /// and fails the test with what it printed.
    fn finish(&mut self, deadline: Instant) -> Run {
        let (output, killed) = self.child.finish(deadline);
        let run = Run::new(&self.args, output.status, &output.stdout, &output.stderr);
        assert!(!killed, "past its deadline, so killed: {run}");
        run
    }
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
    /// How it ended.
    pub status: ExitStatus,
    /// What it printed on stdout.
    pub stdout: String,
    /// What it printed on stderr.
    pub stderr: String,
}

impl Run {
    /// The run of the binary with `args` that ended with `status`, having printed `stdout` and
    /// `stderr`.
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

    /// Whether the run failed with `E_BUSY` (exit 4), of either of its causes: the store's lock
    /// stayed taken for the whole of its timeout, or a read snapshot met a commit landing in each
    /// of its rounds and gave up, which waits for no lock and so can come at any time (its
    /// message names the snapshot). Its envelope is not checked against the contract here;
    /// [`Run::ok`] and [`Run::err`] judge a run that ended otherwise.
    pub fn busy(&self) -> bool {
        self.status.code() == Some(4)
            && serde_json::from_str::<Value>(&self.stdout)
                .is_ok_and(|envelope| envelope["error"]["code"] == "E_BUSY")
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

/// `run`, a test's setup command; or, if it met `E_BUSY`, the command run again by `again` until
/// it does not, [`BUSY_RETRIES`] times at most: what an agent does with the contract's
/// retryable lock timeout, which a host under heavy load can stretch a short queue past. For
/// setup, not for the operation a test measures: what such a wait does may be its subject, and a
/// run again is not the run it asserts on (a commit run again finds the objects its first run
/// wrote). `fifty_processes_race_ref_set_exactly_one_wins_per_round` runs its racers' commits,
/// the setup of each ref race, through it. One race runs the operation it measures through it:
/// in `fifty_processes_race_commit_advance_exactly_one_wins_per_round` a racer's publish that
/// met `E_BUSY` recorded nothing, and its run again, which names the parent the racer read,
/// races as a racer that read late does.
pub fn retry_busy(mut run: Run, mut again: impl FnMut() -> Run) -> Run {
    for _ in 0..BUSY_RETRIES {
        if !run.busy() {
            break;
        }
        run = again();
    }
    run
}

/// Children of the binary that start together: each is a shell that makes its ready file,
/// then reads a line from its stdin, a pipe from the parent, then becomes the binary (`exec`).
/// [`Racers::race`] writes each child its line once every child is ready, so that they all run
/// at once and race for real. Every child is reaped however the test ends ([`Child`]), and a
/// child whose pipe closes without its line (the parent gave up) exits without running.
pub struct Racers<P> {
    /// The children's ready files, `ready-<n>`.
    dir: TempDir,
    /// The children, in the order they were spawned.
    children: Vec<Running>,
    /// Each child's stdin, until its line is written.
    gates: Vec<ChildStdin>,
    /// The binary they run.
    program: PhantomData<fn() -> P>,
}

/// What each child's shell runs: `$1` its ready file, the rest the command.
const BARRIER: &str = r#"
: > "$1"
read -r _ || exit 125
shift
exec "$@"
"#;

impl<P: Program> Racers<P> {
    /// Racers whose ready files go in a fresh directory under `dir`.
    pub fn new(dir: &Path) -> Racers<P> {
        Racers {
            dir: tempfile::tempdir_in(dir).expect("make a barrier directory"),
            children: Vec::new(),
            gates: Vec::new(),
            program: PhantomData,
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
            .arg(P::BIN)
            .args(args.iter().map(AsRef::as_ref));
        with_env(&mut shell, env);
        shell.stdin(Stdio::piped());
        no_core_dumps();

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
pub struct Store<P> {
    /// The temp dir: the store and the files the tests write.
    dir: TempDir,
    /// The store's path.
    path: String,
    /// How many change lists [`Store::commit`] has written, for their names.
    commits: AtomicU64,
    /// Variables every command run on the store sets ([`Store::with_env`]).
    env: Vec<(String, String)>,
    /// The binary the commands run.
    program: PhantomData<fn() -> P>,
}

impl<P: Program> Store<P> {
    /// A fresh store, made with `swarmops init`: of this build's default format.
    pub fn init() -> Store<P> {
        let store = Store::empty();
        let init = run::<P, _>(&["init", store.path()], &[]);
        assert_eq!(init.ok()["changesets"], 0, "{init}");
        store
    }

    /// A fresh store of `format`, made with `swarmops init --format <n>`.
    pub fn init_as(format: StoreFormat) -> Store<P> {
        let store = Store::empty();
        let version = format.version().to_string();
        let init = run::<P, _>(&["init", "--format", &version, store.path()], &[]);
        let made = init.ok();
        assert_eq!(made["changesets"], 0, "{init}");
        assert_eq!(made["store_version"], format.version(), "{init}");
        store
    }

    /// A temp dir with no store in it yet: [`Store::path`] does not exist.
    pub fn empty() -> Store<P> {
        let dir = tempfile::tempdir().expect("make a temp dir");
        let path = format!("{}/store", dir.path().to_str().expect("UTF-8 temp paths"));
        Store {
            dir,
            path,
            commits: AtomicU64::new(0),
            env: Vec::new(),
            program: PhantomData,
        }
    }

    /// This store, every command run on it setting `env` too (before the command's own).
    #[must_use]
    pub fn with_env(mut self, env: &[(&str, &str)]) -> Store<P> {
        let env = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()));
        self.env.extend(env);
        self
    }

    /// A copy of `template`'s store, and of the variables its commands set, in a temp dir of
    /// its own, nothing synced: what a test crashes a run in, from the same starting point
    /// each time.
    pub fn copy_of(template: &Store<P>) -> Store<P> {
        let mut store = Store::empty();
        crate::fs::copy_dir(Path::new(template.path()), Path::new(store.path()));
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
        crate::store::layout_of(self.path())
    }

    /// `meta/journal.ckpt`: how far the checkpoint is.
    pub fn checkpoint(&self) -> u64 {
        let bytes = fs::read(self.layout().journal_ckpt()).expect("read meta/journal.ckpt");
        u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"))
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
        run::<P, _>(&self.args(agent, args), &self.env_with(env))
    }

    /// Runs the read `args` on the store (no identity), with the store's variables set.
    pub fn read(&self, args: &[&str]) -> Run {
        let args: Vec<&str> = ["--repo", self.path()]
            .iter()
            .chain(args)
            .copied()
            .collect();
        run::<P, _>(&args, &self.env_with(&[]))
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A [`Program`] whose variables are read and whose binary is never run.
    struct Any;

    impl Program for Any {
        const BIN: &'static str = "swarmops";
        const NOW_ENV: &'static str = "";
    }

    /// What `command` sets variable `name` to.
    fn set(command: &Command, name: &str) -> Option<OsString> {
        let (_, value) = command.get_envs().find(|(n, _)| *n == name)?;
        value.map(OsString::from)
    }

    #[test]
    fn every_run_is_given_no_login_and_no_curl() {
        let command = swarmops::<Any, &str>(&["info"], &[]);

        for name in [AUTH_FILE_ENV, CURL_ENV] {
            // Set, not left to the developer's environment or home directory.
            let path = set(&command, name).unwrap_or_else(|| panic!("{name} is not set"));
            let path = PathBuf::from(path);
            assert!(path.is_absolute(), "{name}: {path:?}");

            // Nothing is there, nor the directory it would be made in.
            let dir = path.parent().expect("a file's path");
            assert_eq!(dir, std::env::temp_dir().join(NO_LOGIN), "{name}");
            assert!(!dir.exists(), "{name}: {dir:?} exists");
        }
        assert_ne!(set(&command, AUTH_FILE_ENV), set(&command, CURL_ENV));
    }

    #[test]
    fn every_run_is_given_automatic_update_off() {
        let command = swarmops::<Any, &str>(&["info"], &[]);

        assert_eq!(
            set(&command, "SWARMOPS_AUTO_UPDATE"),
            Some(OsString::from("off")),
            "a run that could look for a release, or replace the binary under test"
        );
    }

    /// The five names, spelled out: a variable dropped from [`isolation`], or renamed there
    /// and not in the binary, fails here. A run that [`isolate`] alone prepares, as the CLI's
    /// tests of the help's examples and of the error codes do, then makes no memo key and keeps
    /// no check run in the developer's state directory.
    #[test]
    fn isolation_names_the_login_file_curl_automatic_update_the_memo_keys_and_the_runs() {
        let named = isolation();

        assert_eq!(
            named.clone().map(|(name, _)| name),
            [
                "SWARMOPS_AUTH_FILE",
                "SWARMOPS_CURL",
                "SWARMOPS_AUTO_UPDATE",
                "SWARMOPS_MEMO_KEY_DIR",
                "SWARMOPS_CHECK_RUNS_DIR"
            ]
        );
        let runs = std::env::temp_dir().join(SHARED_CHECK_RUNS);
        assert_eq!(named[4].1, runs.clone().into_os_string());
        assert!(runs.is_absolute(), "{}", runs.display());
        let keys = std::env::temp_dir().join(SHARED_MEMO_KEYS);
        assert_eq!(named[3].1, keys.clone().into_os_string());
        assert!(keys.is_absolute(), "{}", keys.display());

        // Every run of the kit's reads the same key as one that is only isolated.
        let command = swarmops::<Any, &str>(&["info"], &[]);
        assert_eq!(
            set(&command, "SWARMOPS_MEMO_KEY_DIR"),
            Some(keys.into_os_string())
        );
        assert_eq!(
            set(&command, "SWARMOPS_CHECK_RUNS_DIR"),
            Some(runs.into_os_string())
        );
    }

    #[test]
    fn a_test_s_own_login_file_curl_and_automatic_update_win() {
        let own = [
            (AUTH_FILE_ENV, "/own/auth.toml"),
            (CURL_ENV, "/own/curl"),
            (AUTO_UPDATE_ENV, "on"),
        ];
        let command = swarmops::<Any, &str>(&["info"], &own);

        for (name, value) in own {
            assert_eq!(set(&command, name), Some(OsString::from(value)), "{name}");
        }
    }

    #[test]
    fn isolating_a_command_sets_what_isolation_names() {
        let mut command = Command::new("swarmops");
        isolate(&mut command);

        let named = isolation();
        assert_eq!(command.get_envs().count(), named.len());
        for (name, value) in named {
            assert_eq!(set(&command, name), Some(value), "{name}");
        }
    }
}
