//! The process runner: timeouts, process-group kills and the kept tails of output.
//!
//! The commands are `sh` scripts, so the tests are Unix-only; on Windows the runner kills the
//! child alone (a known limitation), which the Windows lane covers.

#![cfg(unix)]

use std::ffi::OsString;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_ws::run::{Env, Output, Run, Spec, Stream, TAIL_BYTES, run, run_with};

/// `sh -c <script>`, as an argv.
fn sh(script: &str) -> Vec<String> {
    ["sh", "-c", script].map(str::to_owned).to_vec()
}

/// A temporary directory to run in, and its path.
fn workdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = Utf8Path::from_path(tmp.path()).unwrap().to_owned();
    (tmp, dir)
}

/// The output as the readers handed it over: each chunk with its stream.
#[derive(Default)]
struct Kept(Vec<(Stream, Vec<u8>)>);

impl Output for Kept {
    fn write(&mut self, stream: Stream, bytes: &[u8]) -> io::Result<()> {
        self.0.push((stream, bytes.to_vec()));
        Ok(())
    }
}

/// Runs `argv` in `dir` as `spec` says, its output kept: the run, and every byte of each stream.
fn kept(argv: &[String], dir: &Utf8Path, spec: &Spec<'_>) -> (Run, Vec<u8>, Vec<u8>) {
    let output = Arc::new(Mutex::new(Kept::default()));
    let shared: Arc<Mutex<dyn Output>> = output.clone();
    let ran = run_with(argv, dir, spec, &shared, &dir.join("out")).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for (stream, bytes) in &output.lock().unwrap().0 {
        match stream {
            Stream::Stdout => out.extend_from_slice(bytes),
            _ => err.extend_from_slice(bytes),
        }
    }
    (ran, out, err)
}

/// The pid a script wrote into `file` of `dir`, waiting up to 60 s for the file to hold one: a
/// shell's `>` makes the file before it writes the number.
fn pid_in(dir: &Utf8Path, file: &str) -> rustix::process::Pid {
    let path = dir.join(file);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(pid) = text.trim().parse()
        {
            return rustix::process::Pid::from_raw(pid).unwrap();
        }
        assert!(Instant::now() < deadline, "no pid in {path}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits up to 10 s for `group` to be gone, then says whether it is.
fn group_gone(group: rustix::process::Pid) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rustix::process::test_kill_process_group(group).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    rustix::process::test_kill_process_group(group).is_err()
}

/// `Env::Only`: the command gets exactly the variables given, none of the caller's.
#[test]
fn only_the_given_variables_reach_the_command() {
    let (_tmp, dir) = workdir();
    let vars = [
        ("ONE".to_owned(), OsString::from("1")),
        ("TWO".to_owned(), OsString::from("two words")),
    ];
    let mut spec = Spec::new(Duration::from_secs(60));
    spec.env = Env::Only(&vars);
    let argv = vec!["/usr/bin/env".to_owned()];
    let (ran, out, err) = kept(&argv, &dir, &spec);
    assert_eq!(ran.exit_code, Some(0), "{ran:?}");
    assert_eq!(String::from_utf8(out).unwrap(), "ONE=1\nTWO=two words\n");
    assert_eq!(String::from_utf8(err).unwrap(), "");
}

/// A stop flag set while a command's sleeper runs kills the group: the sleeper is gone, and the
/// run says it was stopped, not timed out, with no exit code.
#[test]
fn the_stop_flag_kills_the_group() {
    // 1. A shell that writes its group and its sleeper, and a flag set once both are written:
    //    the group's file is renamed into place whole, so that it never holds a part of the pid.
    let (_tmp, dir) = workdir();
    let script = "sleep 600 & echo $! > sleeper; echo $$ > group.tmp && mv group.tmp group; wait";
    let stop = Arc::new(AtomicBool::new(false));
    let setter = {
        let (stop, dir) = (Arc::clone(&stop), dir.clone());
        std::thread::spawn(move || {
            while !dir.join("group").exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::SeqCst);
        })
    };
    let mut spec = Spec::new(Duration::from_secs(600));
    spec.stop = Some(&stop);
    let started = Instant::now();
    let (ran, _, _) = kept(&sh(script), &dir, &spec);
    setter.join().unwrap();

    // 2. Stopped, at once, not timed out.
    assert!(ran.stopped, "{ran:?}");
    assert!(!ran.timed_out, "{ran:?}");
    assert_eq!((ran.exit_code, ran.signal), (None, None), "{ran:?}");
    assert!(started.elapsed() < Duration::from_secs(60), "{ran:?}");

    // 3. No process of the group is left, the sleeper among them.
    assert!(group_gone(pid_in(&dir, "group")));
    assert!(rustix::process::test_kill_process(pid_in(&dir, "sleeper")).is_err());
}

/// A command a signal ends, one the runner did not send, reports that signal and no exit code.
#[test]
fn a_command_killed_by_its_own_signal_reports_it() {
    let (_tmp, dir) = workdir();
    let (ran, out, _) = kept(
        &sh("echo before; kill -TERM $$"),
        &dir,
        &Spec::new(Duration::from_secs(60)),
    );
    assert_eq!((ran.exit_code, ran.signal), (None, Some(15)), "{ran:?}");
    assert!(!ran.timed_out && !ran.stopped, "{ran:?}");
    assert_eq!(out, b"before\n");
}

/// A command that forks a sleeper and waits past its timeout is killed with the sleeper: no
/// process of its group survives, and the run says it timed out, without an exit code.
#[test]
fn a_run_past_its_timeout_is_killed_with_its_children() {
    // 1. A shell that writes its pid (its group's id) and its sleeper's, then waits.
    let (_tmp, dir) = workdir();
    let script = "echo $$ > group; sleep 600 & echo $! > sleeper; wait";
    let started = Instant::now();
    let ran = run(
        &sh(script),
        &dir,
        &[],
        Duration::from_millis(500),
        &dir.join("checks/1.log"),
    )
    .unwrap();

    // 2. Timed out, well before the sleeper would have ended.
    assert!(ran.timed_out, "{ran:?}");
    assert_eq!(ran.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(60), "{ran:?}");

    // 3. No process of the group is left, the sleeper among them.
    let pid = |file: &str| -> i32 {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        text.trim().parse().unwrap()
    };
    let group = rustix::process::Pid::from_raw(pid("group")).unwrap();
    let sleeper = rustix::process::Pid::from_raw(pid("sleeper")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while rustix::process::test_kill_process_group(group).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(rustix::process::test_kill_process_group(group).is_err());
    assert!(rustix::process::test_kill_process(sleeper).is_err());
}

/// A command's output: the last 8 KiB of each stream are kept, the log holds both streams
/// whole, and the exit code is the command's; its environment has what the caller adds, and
/// it runs where it is told with no input.
#[test]
fn a_run_keeps_the_last_8_kib_of_each_stream_and_the_log() {
    // 1. 20,000 bytes and a mark on stdout, a line and a mark on stderr, exit 3.
    let (_tmp, dir) = workdir();
    let script = "head -c 20000 /dev/zero | tr '\\0' o; printf END; \
                  echo \"to stderr $SWARMOPS_WS\" >&2; cat; pwd >&2; exit 3";
    let log = dir.join("checks/2.log");
    let ran: Run = run(
        &sh(script),
        &dir,
        &[("SWARMOPS_WS", "worker")],
        Duration::from_secs(60),
        &log,
    )
    .unwrap();

    // 2. The exit code; the tails, stdout's cut to its last 8 KiB.
    assert_eq!(ran.exit_code, Some(3), "{ran:?}");
    assert!(!ran.timed_out);
    assert_eq!(ran.stdout_tail.len(), TAIL_BYTES);
    assert_eq!(TAIL_BYTES, 8 * 1024);
    assert!(
        ran.stdout_tail.ends_with("ooEND"),
        "{}",
        &ran.stdout_tail[8000..]
    );
    let real = std::fs::canonicalize(&dir).unwrap();
    let want = format!("to stderr worker\n{}\n", real.display());
    assert_eq!(ran.stderr_tail, want);

    // 3. The log holds every byte of both streams.
    let logged = std::fs::read_to_string(&log).unwrap();
    assert_eq!(logged.len(), 20_000 + 3 + want.len());
    assert!(logged.contains("to stderr worker"));
    assert_eq!(
        logged.matches('o').count(),
        20_000 + want.matches('o').count()
    );

    // 4. A command that cannot start is refused, naming it.
    let missing = vec!["swarmops-no-such-command".to_owned()];
    let refused = run(&missing, &dir, &[], Duration::from_secs(5), &log).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(refused.message.contains("swarmops-no-such-command"));
    let empty = run(&[], &dir, &[], Duration::from_secs(5), &log).unwrap_err();
    assert_eq!(empty.code, "E_INVALID_ARG");
}
