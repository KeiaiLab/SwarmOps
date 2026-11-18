//! The process runner (§5.4 of the M5 design) of a check and of the bootstrap: argv without a
//! shell, stdin null, a timeout that kills the command's process group on Unix, the last 8 KiB of
//! each stream kept. However the command ends, nothing it left in its group outlives the run.
//!
//! ```text
//!   run_with ──spawn──▶ child (own process group on Unix; env inherited + extra, or only given)
//!    │                    ├─ stdout ─▶ reader ─┬─▶ the Output (RawLog: both streams as they come;
//!    │                    └─ stderr ─▶ reader ─┘    or the caller's), and each stream's last 8 KiB
//!    ├─ waits until the exit, the timeout or the stop flag; past either, kills the group
//!    └─ the output to its end (the grace), then the group killed however it ended, then reaped
//! ```
//!
//! On Unix the command is reaped only after its group is killed: until then it is a zombie that
//! keeps its pid, which is its group's id, from being given to another process.
//!
//! [`run`] is what `ws land --check` and the bootstrap use: the caller's environment plus a few
//! variables, the output as it came into a raw log. [`run_with`] hands the caller the output
//! ([`Output`]), the environment ([`Env`]) and a flag that stops the command (`check run`).
//!
//! On Windows only the child is killed: a grandchild holding the output open is left, and the
//! runner stops reading after [`DRAIN`] (a known limitation).

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};

/// How many bytes of each stream a [`Run`] keeps: its last 8 KiB.
pub const TAIL_BYTES: usize = 8 * 1024;

/// How long the runner first waits between two looks at a running command.
const POLL_FIRST: Duration = Duration::from_millis(5);

/// The longest wait between two looks at a running command.
const POLL_MAX: Duration = Duration::from_millis(50);

/// How long the output may stay open after the command ended (a process it left in its group
/// still writing) before that group is killed, as it then is in any case.
const GRACE: Duration = Duration::from_secs(1);

/// How long the readers get to finish once the command's group is killed.
pub const DRAIN: Duration = Duration::from_secs(5);

/// The size of one read from a stream.
const CHUNK: usize = 64 * 1024;

/// What a command did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Run {
    /// Its exit code; none when it was killed (by its timeout, the stop flag, or a signal).
    pub exit_code: Option<i32>,
    /// The signal that ended it, when one did that the runner did not send.
    pub signal: Option<i32>,
    /// Whether its timeout ended it.
    pub timed_out: bool,
    /// Whether the stop flag ended it.
    pub stopped: bool,
    /// The last [`TAIL_BYTES`] of its stdout, as text (invalid UTF-8 replaced).
    pub stdout_tail: String,
    /// The last [`TAIL_BYTES`] of its stderr, as text.
    pub stderr_tail: String,
}

/// Which stream a chunk of output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stream {
    /// The command's stdout.
    Stdout,
    /// The command's stderr.
    Stderr,
}

/// Where a command's output goes as it comes: each chunk read from either stream, in the order
/// the readers got them. The readers share it, one at a time.
pub trait Output: Send {
    /// Takes `bytes`, read from `stream`.
    ///
    /// # Errors
    ///
    /// A write that failed: the first one is the run's `E_IO`.
    fn write(&mut self, stream: Stream, bytes: &[u8]) -> io::Result<()>;
}

/// The output as it came, both streams together, into a file: the log of `ws land --check` and
/// of the bootstrap.
#[derive(Debug)]
#[non_exhaustive]
pub struct RawLog {
    /// The log.
    file: File,
}

impl RawLog {
    /// The log at `path`, made anew, its directory made if missing.
    ///
    /// # Errors
    ///
    /// `E_IO` naming the path that could not be made.
    pub fn create(path: &Utf8Path) -> Result<RawLog, OpError> {
        new_log(path).map(|file| RawLog { file })
    }
}

impl Output for RawLog {
    fn write(&mut self, _stream: Stream, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }
}

/// The environment a command gets.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Env<'e> {
    /// The caller's environment, with these variables set over it.
    Inherit(&'e [(&'e str, &'e str)]),
    /// These variables alone: the caller's environment is cleared first.
    Only(&'e [(String, OsString)]),
}

/// How a command runs: its environment, its timeout, and the flag that stops it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Spec<'a> {
    /// Its environment.
    pub env: Env<'a>,
    /// The longest it may run.
    pub timeout: Duration,
    /// Set (by anyone, at any time): the command's group is killed and the run says
    /// [`Run::stopped`].
    pub stop: Option<&'a AtomicBool>,
}

impl<'a> Spec<'a> {
    /// A run of at most `timeout`, in the caller's environment, with no stop flag.
    pub fn new(timeout: Duration) -> Spec<'a> {
        Spec {
            env: Env::Inherit(&[]),
            timeout,
            stop: None,
        }
    }
}

/// Runs `argv` in `cwd`, without a shell, stdin null, its environment the caller's plus `env`,
/// for at most `timeout`; both streams go to `log` (made anew, its directory too) as they
/// come, and the last [`TAIL_BYTES`] of each to the [`Run`]. Past the timeout the command is
/// killed: on Unix with its whole process group, which it was started in.
///
/// `E_INVALID_ARG` for an empty `argv` or a command that cannot start; `E_IO` for a log that
/// cannot be written or a wait that fails.
pub fn run(
    argv: &[String],
    cwd: &Utf8Path,
    env: &[(&str, &str)],
    timeout: Duration,
    log: &Utf8Path,
) -> Result<Run, OpError> {
    if argv.is_empty() {
        return Err(OpError::new(ErrorCode::InvalidArg, "no command to run"));
    }
    let output: Arc<Mutex<dyn Output>> = Arc::new(Mutex::new(RawLog::create(log)?));
    let mut spec = Spec::new(timeout);
    spec.env = Env::Inherit(env);
    run_with(argv, cwd, &spec, &output, log)
}

/// Runs `argv` in `cwd` as `spec` says, without a shell, stdin null, each chunk of both streams
/// to `output` as it comes and the last [`TAIL_BYTES`] of each to the [`Run`]. Past the timeout,
/// or once the stop flag is set, the command is killed: on Unix with its whole process group,
/// which it was started in. A command that ends on its own has its group killed too, once its
/// output has ended or a second (the grace) is over: a process it left running in the group (a
/// server sent to the background, its output sent elsewhere) does not outlive it. `shown` names
/// the output in an `E_IO`.
///
/// `E_INVALID_ARG` for an empty `argv` or a command that cannot start (`details.program`), with
/// nothing written to `output`; `E_IO` for output that cannot be written or a wait that fails.
pub fn run_with(
    argv: &[String],
    cwd: &Utf8Path,
    spec: &Spec<'_>,
    output: &Arc<Mutex<dyn Output>>,
    shown: &Utf8Path,
) -> Result<Run, OpError> {
    // 1. A program to run.
    let Some((program, rest)) = argv.split_first() else {
        return Err(OpError::new(ErrorCode::InvalidArg, "no command to run"));
    };

    // 2. The child: no shell, no input, both streams piped, its own process group on Unix.
    let mut command = Command::new(program);
    command
        .args(rest)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match spec.env {
        Env::Inherit(extra) => {
            command.envs(extra.iter().copied());
        }
        Env::Only(vars) => {
            command
                .env_clear()
                .envs(vars.iter().map(|(name, value)| (name, value)));
        }
    }
    own_group(&mut command);
    let mut child = command.spawn().map_err(|e| not_started(program, &e))?;

    // 3. Each stream read on a thread of its own, into the output and its tail.
    let shared = Arc::new(Shared {
        output: Arc::clone(output),
        error: Mutex::new(None),
    });
    let (done, finished) = mpsc::channel();
    let stdout = read_out(child.stdout.take(), Stream::Stdout, &shared, &done);
    let stderr = read_out(child.stderr.take(), Stream::Stderr, &shared, &done);
    drop(done);

    // 4. The exit, or the timeout or the stop and the group killed; not reaped yet.
    let deadline = Instant::now() + spec.timeout;
    let end =
        wait(&mut child, deadline, spec.stop).map_err(|e| host_failed("waiting for", shown, &e))?;

    // 5. The output to its end within the grace; then the group killed, whatever is left in it,
    //    the readers given the drain if they had not finished, and only then the command reaped.
    let quiet = drained(&finished, GRACE);
    kill(&mut child);
    if !quiet {
        drained(&finished, DRAIN);
    }
    let reaped = child.wait();
    let status = reaped.map_err(|e| host_failed("waiting for", shown, &e))?;
    let status = (end == End::Exited).then_some(status);

    // 6. The output written whole, and the outcome.
    if let Some(e) = shared
        .error
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        return Err(host_failed("writing", shown, &e));
    }
    Ok(Run {
        exit_code: status.and_then(|status| status.code()),
        signal: status.and_then(signal_of),
        timed_out: end == End::TimedOut,
        stopped: end == End::Stopped,
        stdout_tail: text(&stdout),
        stderr_tail: text(&stderr),
    })
}

/// What ended a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// It ended on its own.
    Exited,
    /// Its timeout: the runner killed it.
    TimedOut,
    /// The stop flag: the runner killed it.
    Stopped,
}

/// What the two readers share: the output, and the first error writing it.
struct Shared {
    /// Where both streams go.
    output: Arc<Mutex<dyn Output>>,
    /// The first error writing the output.
    error: Mutex<Option<io::Error>>,
}

/// A stream's last [`TAIL_BYTES`], as its reader keeps them.
type Tail = Arc<Mutex<Vec<u8>>>;

/// Waits for `child` until `deadline` or until `stop` is set, looking ever less often; past
/// it, or once stopped, kills its group. The child is not reaped: the caller does that.
fn wait(child: &mut Child, deadline: Instant, stop: Option<&AtomicBool>) -> io::Result<End> {
    let mut pause = POLL_FIRST;
    loop {
        // 1. Ended on its own.
        if exited(child)? {
            return Ok(End::Exited);
        }

        // 2. Past its time, or stopped: killed.
        let now = Instant::now();
        let stopped = stop.is_some_and(|stop| stop.load(Ordering::SeqCst));
        if now >= deadline || stopped {
            kill(child);
            return Ok(if stopped { End::Stopped } else { End::TimedOut });
        }

        // 3. Another look later.
        thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(POLL_MAX);
    }
}

/// Whether both readers finished within `limit`.
fn drained(finished: &Receiver<()>, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match finished.recv_timeout(left) {
            Ok(()) => {}
            // Both senders gone: every reader has finished.
            Err(mpsc::RecvTimeoutError::Disconnected) => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => return false,
        }
    }
    true
}

/// Reads `stream`, which is `which`, to its end on a thread of its own, into the shared output
/// and the tail it returns; says so on `done` when it ends.
fn read_out<S>(stream: Option<S>, which: Stream, shared: &Arc<Shared>, done: &Sender<()>) -> Tail
where
    S: Read + Send + 'static,
{
    let tail: Tail = Arc::default();
    let Some(mut stream) = stream else {
        let _ = done.send(());
        return tail;
    };
    let (kept, shared, done) = (Arc::clone(&tail), Arc::clone(shared), done.clone());
    thread::spawn(move || {
        let mut chunk = vec![0; CHUNK];
        loop {
            // 1. The next bytes, until the stream ends (or breaks).
            let n = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let bytes = &chunk[..n];

            // 2. Into the output; the first failure is kept for the caller.
            let written =
                (shared.output.lock().unwrap_or_else(PoisonError::into_inner)).write(which, bytes);
            if let Err(e) = written {
                let mut first = shared.error.lock().unwrap_or_else(PoisonError::into_inner);
                first.get_or_insert(e);
            }

            // 3. Into the tail, cut to its last TAIL_BYTES.
            let mut tail = kept.lock().unwrap_or_else(PoisonError::into_inner);
            tail.extend_from_slice(bytes);
            let over = tail.len().saturating_sub(TAIL_BYTES);
            tail.drain(..over);
        }
        let _ = done.send(());
    });
    tail
}

/// A tail as text: from its first whole UTF-8 character, invalid bytes replaced.
fn text(tail: &Tail) -> String {
    let bytes = tail.lock().unwrap_or_else(PoisonError::into_inner);
    let start = if bytes.len() == TAIL_BYTES {
        // A cut can fall inside a character: its continuation bytes (0b10xx_xxxx) go.
        bytes.iter().take_while(|&&b| b & 0xc0 == 0x80).count()
    } else {
        0
    };
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// The log at `path`, made anew, its directory made if missing.
fn new_log(path: &Utf8Path) -> Result<File, OpError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| host_failed("making", dir, &e))?;
    }
    File::create(path).map_err(|e| host_failed("making", path, &e))
}

/// The signal that ended a command with `status`, if one did.
#[cfg(unix)]
fn signal_of(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Windows: no signals.
#[cfg(not(unix))]
fn signal_of(_status: ExitStatus) -> Option<i32> {
    None
}

/// Starts the command in a process group of its own, so that a timeout kills what it started.
#[cfg(unix)]
fn own_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Windows: no process group; only the child is killed.
#[cfg(not(unix))]
fn own_group(_command: &mut Command) {}

/// Whether `child` has exited, looked at without reaping it: its pid, and so its group's id,
/// stays its own until the caller waits for it.
#[cfg(unix)]
fn exited(child: &Child) -> io::Result<bool> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let how = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    Ok(waitid(WaitId::Pid(Pid::from_child(child)), how)?.is_some())
}

/// Whether `child` has exited. Windows has no process group whose id could be given away.
#[cfg(not(unix))]
fn exited(child: &mut Child) -> io::Result<bool> {
    Ok(child.try_wait()?.is_some())
}

/// Kills `child`'s process group, which it leads, with every process in it, and `child` itself
/// should it have left the group. One already gone is killed.
#[cfg(unix)]
fn kill(child: &mut Child) {
    use rustix::process::{Pid, Signal};
    let _ = rustix::process::kill_process_group(Pid::from_child(child), Signal::KILL);
    let _ = child.kill();
}

/// Kills `child` alone: Windows has no process group to kill.
#[cfg(not(unix))]
fn kill(child: &mut Child) {
    let _ = child.kill();
}

/// `E_INVALID_ARG` for `program`, which did not start.
fn not_started(program: &str, e: &io::Error) -> OpError {
    OpError::new(
        ErrorCode::InvalidArg,
        format!("cannot run {program:?}: {e}"),
    )
    .with_details(Map::from_iter([(
        "program".to_owned(),
        Value::from(program),
    )]))
}

/// `E_IO` for `doing` (making, writing, waiting for) `path`, which failed with `e`.
fn host_failed(doing: &str, path: &Utf8Path, e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("{doing} {path}: {e}")).with_details(Map::from_iter([(
        "file".to_owned(),
        Value::from(path.as_str()),
    )]))
}
