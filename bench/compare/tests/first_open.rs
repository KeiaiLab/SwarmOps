//! The harness run end to end, at sizes that take seconds: what its JSON record says it
//! measured, and in what order.

use std::process::Command;

use serde_json::Value;

#[test]
fn each_seeded_store_s_first_open_is_timed_once_before_w3_opens_it() {
    let base = tempfile::tempdir().expect("a bench dir");
    let out = base.path().join("record.json");
    let run = Command::new(env!("CARGO_BIN_EXE_swarmops-bench-compare"))
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
        .output()
        .expect("run the harness");
    assert!(
        run.status.success(),
        "{}\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
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
