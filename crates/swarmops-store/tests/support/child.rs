//! Running this test binary again as a child process, to crash it at a chosen filesystem call
//! or to race it against others: every child has a deadline, [`CHILD_TIMEOUT`] after it was
//! spawned (or let go) unless a test says otherwise, and one still running then is killed,
//! reaped, and fails the test with what it printed. A child is reaped however the test ends:
//! its handle kills and waits for it when it is dropped, a panic's unwinding included.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a child may run.
pub const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// The signal `std::process::abort` raises: how a `SWARMOPS_TEST_CRASH_AT` crash ends.
pub const SIGABRT: i32 = 6;

/// How often a wait looks whether a child has ended.
const POLL: Duration = Duration::from_millis(1);

/// The test harness's arguments that run test `test` alone, ignored or not (it may be one only
/// the slow suite runs), with its output shown.
pub fn test_args(test: &str) -> [&str; 4] {
    ["--exact", test, "--include-ignored", "--nocapture"]
}

/// This test binary, set to run test `test` alone ([`test_args`]).
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

/// The threads that read a child's stdout and stderr to their ends.
type Readers = (JoinHandle<Vec<u8>>, JoinHandle<Vec<u8>>);

/// A child whose stdout and stderr threads of their own read. Dropped, it is killed if it still
/// runs, and reaped either way.
pub struct Child {
    child: std::process::Child,
    output: Option<Readers>,
}

impl Child {
    /// Spawns `command`, reading nothing from stdin, its stdout and stderr piped.
    pub fn spawn(command: &mut Command) -> Child {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn a child");
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        Child {
            child,
            output: Some((stdout, stderr)),
        }
    }

    /// Waits for the child to end, until `deadline`: one still running then is killed, reaped,
    /// and fails the test with what it printed.
    pub fn wait(mut self, deadline: Instant) -> Output {
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
