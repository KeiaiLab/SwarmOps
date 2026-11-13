//! Running this test binary again as a child process, to crash it at a chosen filesystem call
//! or to race it against others: every child has a deadline, [`CHILD_TIMEOUT`] after it was
//! spawned (or let go) unless a test says otherwise, and one still running then is killed,
//! reaped, and fails the test with what it printed. A child is reaped however the test ends:
//! its handle kills and waits for it when it is dropped, a panic's unwinding included.
//!
//! [`Child`] is the testkit's one runner of children: the runs of the binary ([`crate::cli`])
//! wait for theirs through it too. The contention tests and the MCP tests' server spawn their
//! children themselves, each with a deadline and a kill and reap on drop of its own.

use std::io::Read;
use std::process::{self, Command, Output, Stdio};
use std::sync::Once;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a child may run.
pub const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// The signal `std::process::abort` raises: how a `SWARMOPS_TEST_CRASH_AT` crash ends.
pub const SIGABRT: i32 = 6;

/// The signal a kill sends: how a child that [`Child::finish`] killed at its deadline ends, and
/// one that `kill -KILL` hit.
pub const SIGKILL: i32 = 9;

/// How often a wait looks whether a child has ended.
const POLL: Duration = Duration::from_millis(1);

/// The core limit [`no_core_dumps`] asks `prlimit` for: a soft limit of 1 byte, the hard limit
/// as it is.
const CORE_LIMIT: &str = "--core=1:";

/// The name the test harness knows a test of the module this expands in by, given the test's
/// own: the module's path past the test binary's crate name, then the test's name
/// (`journal::a_test` for `a_test` in the `journal` module of a `tests/all.rs`, `a_test` at a
/// test binary's root). [`this_test`] and [`test_args`] take it, to run that test again.
#[macro_export]
macro_rules! test_name {
    ($test:expr) => {
        $crate::child::test_path(module_path!(), $test)
    };
}

/// The name the test harness knows test `test` of module `module` by, `module` being its path
/// as `module_path!` gives it: `module` past its first segment (the test binary's crate name),
/// then `test`. [`test_name!`](crate::test_name!) passes it the module it expands in.
pub fn test_path(module: &str, test: &str) -> String {
    match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    }
}

/// The test harness's arguments that run the test it knows as `test` ([`test_name!`]) alone,
/// ignored or not (it may be one only the slow suite runs), with its output shown.
pub fn test_args(test: &str) -> [&str; 4] {
    ["--exact", test, "--include-ignored", "--nocapture"]
}

/// This test binary, set to run the test it knows as `test` ([`test_name!`]) alone
/// ([`test_args`]).
pub fn this_test(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("the test binary"));
    command.args(test_args(test));
    command
}

/// Runs `command` to its end, within [`CHILD_TIMEOUT`], and returns what it printed and how it
/// ended, as [`Command::output`] does.
pub fn run(command: &mut Command) -> Output {
    Child::spawn(command).wait(Instant::now() + CHILD_TIMEOUT)
}

/// Asks for no core dump of the children this process spawns from now on, once per process: a
/// soft core limit of 1 byte on this process, which its children inherit. A crash sweep ends
/// each child by `abort`, and a host whose `kernel.core_pattern` is a pipe hands every such core
/// to its handler under a limit of 0 too; only a limit of 1 stops it (`core(5)`). With hundreds
/// of aborts queued for one handler, a child's end took seconds instead of milliseconds. The
/// child still ends by SIGABRT. The hard limit stays, so the soft one can be raised again.
/// Linux only, and where there is no `prlimit` (util-linux) nothing changes.
pub(crate) fn no_core_dumps() {
    static ASKED: Once = Once::new();

    ASKED.call_once(|| {
        if !cfg!(target_os = "linux") {
            return;
        }

        // A failure leaves the limit as it was: the children are slower to end, no less right.
        let pid = process::id().to_string();
        let _ = Command::new("prlimit")
            .args(["--pid", &pid, CORE_LIMIT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// The threads that read a child's stdout and stderr to their ends.
type Readers = (JoinHandle<Vec<u8>>, JoinHandle<Vec<u8>>);

/// A child whose stdout and stderr threads of their own read. Dropped, it is killed if it still
/// runs, and reaped either way.
pub struct Child {
    /// The process.
    child: process::Child,
    /// Its stdout's and stderr's readers, until it is waited for.
    output: Option<Readers>,
}

impl Child {
    /// Spawns `command`, reading nothing from stdin, its stdout and stderr piped.
    pub fn spawn(command: &mut Command) -> Child {
        no_core_dumps();

        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn a child");
        Child::adopt(child)
    }

    /// `child`, spawned with its stdout and stderr piped: from now on threads of their own read
    /// them, and it is killed and reaped when dropped.
    pub fn adopt(mut child: process::Child) -> Child {
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        Child {
            child,
            output: Some((stdout, stderr)),
        }
    }

    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the child to end, until `deadline`: one still running then is killed and
    /// reaped. What it printed and how it ended, and whether it was killed. A child is waited
    /// for once.
    pub fn finish(&mut self, deadline: Instant) -> (Output, bool) {
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
        let (stdout, stderr) = self.output.take().expect("waited for once");
        let output = Output {
            status,
            stdout: stdout.join().expect("read a child's stdout"),
            stderr: stderr.join().expect("read a child's stderr"),
        };
        (output, killed)
    }

    /// Waits for the child to end, until `deadline`: one still running then is killed, reaped,
    /// and fails the test with what it printed.
    pub fn wait(mut self, deadline: Instant) -> Output {
        let (output, killed) = self.finish(deadline);
        assert!(
            !killed,
            "a child past its deadline, so killed: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }
}

impl Drop for Child {
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// This process's soft and hard core limits, as `/proc/self/limits` words them.
    fn core_limits() -> (String, String) {
        let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
        let line = limits
            .lines()
            .find_map(|line| line.strip_prefix("Max core file size"))
            .expect("a core limit line");
        let mut words = line.split_whitespace();
        let soft = words.next().unwrap().to_owned();
        let hard = words.next().unwrap().to_owned();
        (soft, hard)
    }

    /// Whether [`no_core_dumps`] can change the limit here: there is a `prlimit`, and the hard
    /// limit leaves room for a soft one of 1.
    fn can_limit() -> bool {
        let found = Command::new("prlimit")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        let (_soft, hard) = core_limits();
        let can = found && hard != "0";
        if !can {
            eprintln!("skipped: no prlimit here, or a hard core limit of 0 ({hard})");
        }
        can
    }

    /// A child the kit runs dumps no core when it ends by SIGABRT, as a crash sweep's does. It
    /// tells a kit that asks for the limit from one that does not only on a host that hands
    /// cores to a pipe; where `kernel.core_pattern` names a file and the soft limit is 0, no
    /// core is dumped either way.
    #[test]
    fn an_aborted_child_dumps_no_core() {
        if !can_limit() {
            return;
        }

        let child = run(Command::new("/bin/sh").args(["-c", "kill -ABRT $$"]));

        assert_eq!(child.status.signal(), Some(SIGABRT), "{:?}", child.status);
        assert!(!child.status.core_dumped(), "{:?}", child.status);
    }

    /// A child still running at its deadline ends by SIGKILL, and is reported as killed.
    #[test]
    fn a_child_past_its_deadline_ends_by_sigkill() {
        let mut child = Child::spawn(Command::new("sleep").arg("60"));

        let (output, killed) = child.finish(Instant::now());

        assert!(killed, "{:?}", output.status);
        assert_eq!(output.status.signal(), Some(SIGKILL), "{:?}", output.status);
    }

    /// The limit asked for is the soft one, 1 byte; the hard limit is left as it was.
    #[test]
    fn the_core_limit_is_soft_and_1() {
        if !can_limit() {
            return;
        }
        let (_soft, hard_before) = core_limits();

        no_core_dumps();

        let (soft, hard) = core_limits();
        assert_eq!(soft, "1");
        assert_eq!(hard, hard_before);
    }
}
