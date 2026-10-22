//! The process runner (§5.4 of the M5 design) of a check and of the bootstrap: argv without a
//! shell, stdin null, a timeout that kills the command's process group on Unix, the last 8 KiB of
//! each stream kept.
//!
//! ```text
//!   run ──spawn──▶ child (own process group on Unix)
//!    │               ├─ stdout ─▶ reader ─┬─▶ log (both streams, as they come)
//!    │               └─ stderr ─▶ reader ─┘    and each stream's last 8 KiB
//!    └─ waits until the exit or the timeout; past it, kills the group
//! ```
//!
//! On Windows only the child is killed: a grandchild holding the output open is left, and the
//! runner stops reading after [`DRAIN`] (a known limitation).

use std::fs::File;
use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
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
/// still writing) before that group is killed too.
const GRACE: Duration = Duration::from_secs(1);

/// How long the readers get to finish once the command's group is killed.
pub const DRAIN: Duration = Duration::from_secs(5);

/// The size of one read from a stream.
const CHUNK: usize = 64 * 1024;

/// What a command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Its exit code; none when it was killed (by its timeout, or by a signal).
    pub exit_code: Option<i32>,
    /// Whether its timeout ended it.
    pub timed_out: bool,
    /// The last [`TAIL_BYTES`] of its stdout, as text (invalid UTF-8 replaced).
    pub stdout_tail: String,
    /// The last [`TAIL_BYTES`] of its stderr, as text.
    pub stderr_tail: String,
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
    // 1. A program to run, and the log made anew.
    let Some((program, rest)) = argv.split_first() else {
        return Err(OpError::new(ErrorCode::InvalidArg, "no command to run"));
    };
    let log_file = new_log(log)?;

    // 2. The child: no shell, no input, both streams piped, its own process group on Unix.
    let mut command = Command::new(program);
    command
        .args(rest)
        .current_dir(cwd)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut command);
    let mut child = command.spawn().map_err(|e| not_started(program, &e))?;

    // 3. Each stream read on a thread of its own, into the log and its tail.
    let shared = Arc::new(Output {
        log: Mutex::new(log_file),
        log_error: Mutex::new(None),
    });
    let (done, finished) = mpsc::channel();
    let stdout = read_out(child.stdout.take(), &shared, &done);
    let stderr = read_out(child.stderr.take(), &shared, &done);
    drop(done);

    // 4. The exit, or the timeout and the group killed.
    let deadline = Instant::now() + timeout;
    let ended = wait(&mut child, deadline).map_err(|e| host_failed("waiting for", log, &e))?;

    // 5. The output to its end: a process left in the group holding it is killed after the
    //    grace, and the readers then get the drain.
    if !drained(&finished, GRACE) {
        kill(&mut child);
        drained(&finished, DRAIN);
    }

    // 6. The log written whole, and the outcome.
    if let Some(e) = shared
        .log_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        return Err(host_failed("writing", log, &e));
    }
    Ok(Run {
        exit_code: ended.status.and_then(|status| status.code()),
        timed_out: ended.timed_out,
        stdout_tail: text(&stdout),
        stderr_tail: text(&stderr),
    })
}

/// How a command ended: its status, unless it was killed past its timeout.
struct Ended {
    /// Its exit status; none when its timeout killed it.
    status: Option<ExitStatus>,
    /// Whether its timeout ended it.
    timed_out: bool,
}

/// What the two readers share: the log, and the first error writing it.
struct Output {
    /// The log both streams go to.
    log: Mutex<File>,
    /// The first error writing the log.
    log_error: Mutex<Option<io::Error>>,
}

/// A stream's last [`TAIL_BYTES`], as its reader keeps them.
type Tail = Arc<Mutex<Vec<u8>>>;

/// Waits for `child` until `deadline`, looking ever less often; past it, kills its group and
/// reaps it.
fn wait(child: &mut Child, deadline: Instant) -> io::Result<Ended> {
    let mut pause = POLL_FIRST;
    loop {
        // 1. Ended on its own.
        if let Some(status) = child.try_wait()? {
            return Ok(Ended {
                status: Some(status),
                timed_out: false,
            });
        }

        // 2. Past its time: killed, then reaped.
        let now = Instant::now();
        if now >= deadline {
            kill(child);
            child.wait()?;
            return Ok(Ended {
                status: None,
                timed_out: true,
            });
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

/// Reads `stream` to its end on a thread of its own, into the shared log and the tail it
/// returns; says so on `done` when it ends.
fn read_out<S>(stream: Option<S>, shared: &Arc<Output>, done: &Sender<()>) -> Tail
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

            // 2. Into the log; the first failure is kept for the caller.
            let written =
                (shared.log.lock().unwrap_or_else(PoisonError::into_inner)).write_all(bytes);
            if let Err(e) = written {
                let mut first = shared
                    .log_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
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
        bytes.iter().take_while(|&&b| b & 0xC0 == 0x80).count()
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

/// Starts the command in a process group of its own, so that a timeout kills what it started.
#[cfg(unix)]
fn own_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Windows: no process group; only the child is killed.
#[cfg(not(unix))]
fn own_group(_command: &mut Command) {}

/// Kills `child`'s process group, which it leads, with every process in it, and `child` itself
/// should it have left the group. One already gone is killed.
#[cfg(unix)]
fn kill(child: &mut Child) {
    use rustix::process::{Pid, Signal, kill_process_group};
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
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
