//! The harness run end to end, at sizes that take seconds: what its JSON record says it
//! measured, and in what order.

use std::fs::File;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long the harness may run. Its quick run takes seconds.
const DEADLINE: Duration = Duration::from_secs(300);

/// How often a run is asked whether it has ended.
const POLL: Duration = Duration::from_millis(50);

/// `command` run to its end: its exit status, or `None` if it was still running at `deadline`
/// and was killed. The caller gives the command's output a file, not a pipe: nobody reads a
/// pipe while this waits, and a child of the killed process could hold one open.
fn run_within(command: &mut Command, deadline: Duration) -> Option<ExitStatus> {
    let mut child = command.spawn().expect("start the run");
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait().expect("ask the run") {
            return Some(status);
        }
        if started.elapsed() >= deadline {
            break;
        }
        thread::sleep(POLL);
    }

    // Past the deadline: kill it, and reap it so that no zombie is left.
    child.kill().expect("kill the run");
    child.wait().expect("reap the run");
    None
}

#[test]
fn a_run_still_going_at_its_deadline_is_killed() {
    let mut sleeper = Command::new("sleep");
    sleeper.arg("20");

    let ended = run_within(&mut sleeper, Duration::from_millis(200));

    assert_eq!(ended, None, "the sleeper was left to finish");
}

#[test]
fn each_seeded_store_s_first_open_is_timed_once_before_w3_opens_it() {
    let base = tempfile::tempdir().expect("a bench dir");
    let out = base.path().join("record.json");
    let stderr = base.path().join("stderr.log");
    let mut harness = Command::new(env!("CARGO_BIN_EXE_swarmops-bench-compare"));
    harness
        .args([
            "--quick",
            "--runs",
            "1",
            "--only",
            "w3",
            "--engines",
            "native,sqlite",
        ])
        .args(["--sizes", "1000,2000", "--out"])
        .arg(&out)
        .env("SWARMOPS_BENCH_DIR", base.path())
        .stdout(Stdio::null())
        .stderr(File::create(&stderr).expect("a file for its progress"));

    let status = run_within(&mut harness, DEADLINE);

    let progress = std::fs::read_to_string(&stderr).expect("its progress");
    let status = status.unwrap_or_else(|| panic!("killed after {DEADLINE:?}\n{progress}"));
    assert!(status.success(), "{status}\n{progress}");
    let record: Value =
        serde_json::from_slice(&std::fs::read(&out).expect("the JSON record")).expect("JSON");
    let records = record["records"].as_array().expect("its records");
    let at = |workload: &str, engine: &str, size: &str| -> Vec<usize> {
        let of =
            |r: &Value| r["workload"] == workload && r["engine"] == engine && r["variant"] == size;
        (0..records.len()).filter(|&i| of(&records[i])).collect()
    };
    for engine in ["native", "sqlite"] {
        for size in ["1k", "2k"] {
            let (first, w3) = (at("w4-first-open", engine, size), at("w3", engine, size));
            assert_eq!(first.len(), 1, "{engine} {size}: first opens at {first:?}");
            assert_eq!(w3.len(), 1, "{engine} {size}: W3 at {w3:?}");
            assert!(
                first[0] < w3[0],
                "{engine} {size}: the first open at {}, W3 at {}",
                first[0],
                w3[0]
            );
            let ms = records[first[0]]["values"]["first_open_ms"].as_f64();
            assert!(ms.is_some_and(|ms| ms > 0.0), "{engine} {size}: {ms:?}");
        }
    }
}
