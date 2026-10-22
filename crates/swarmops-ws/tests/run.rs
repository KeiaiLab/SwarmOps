//! The process runner: timeouts, process-group kills and the kept tails of output.
//!
//! The commands are `sh` scripts, so the tests are Unix-only; on Windows the runner kills the
//! child alone (a known limitation), which the Windows lane covers.

#![cfg(unix)]

use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_ws::run::{Run, TAIL_BYTES, run};

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
