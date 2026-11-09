//! FORMAT 3's golden vectors (the FORMAT 3 design §6), written by the encoder once and pinned
//! here: the marker, so far. FORMAT 1 and 2 keep their own vectors untouched.
//!
//! Each file is lines of lowercase hex.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_store::{
    Layout, RealFs, StoreError, StoreFormat, StoreFs, UPGRADE_POISON_V3, read_format,
};

#[test]
fn format3_marker_round_trips_and_v1_v2_are_unchanged() {
    let v3 = StoreFormat::V3;
    assert_eq!(v3.version(), 3);
    assert_eq!(v3.marker(), b"swarmops-store 3\n");
    assert_eq!(StoreFormat::from_marker(v3.marker()), Some(v3));
    assert_eq!(StoreFormat::from_version(3), Some(v3));
    assert_eq!(UPGRADE_POISON_V3, v3.marker());
    assert_eq!(StoreFormat::V1.marker(), b"swarmops-store 1\n");
    assert_eq!(StoreFormat::V2.marker(), b"swarmops-store 2\n");
    for format in [StoreFormat::V1, StoreFormat::V2] {
        assert_ne!(format.marker(), v3.marker());
    }

    // The golden: the marker, then the poison; 17 bytes each.
    let lines = read_golden("marker");
    assert_eq!(lines, [v3.marker().to_vec(), UPGRADE_POISON_V3.to_vec()]);
    assert!(lines.iter().all(|line| line.len() == 17));
}

/// A marker that names a later format, in the 17 bytes a marker has, is a store too new for this
/// build; any other bytes name no store format.
#[test]
fn a_marker_past_3_is_too_new() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let layout = Layout::with_format(root.clone(), StoreFormat::V3);
    let fs = StoreFs::new(Arc::new(RealFs::new()), &root);
    let read = |marker: &[u8]| {
        fs::write(layout.format_file(), marker).unwrap();
        read_format(&layout, &fs)
    };

    // 1. Too new: the format it names, and no features.
    for (marker, format) in [(&b"swarmops-store 4\n"[..], 4), (b"swarmops-store 9\n", 9)] {
        match read(marker) {
            Err(StoreError::TooNew {
                format: got,
                features,
            }) => assert_eq!((got, features), (format, Vec::<String>::new())),
            other => panic!("{marker:?}: expected TooNew, got {other:?}"),
        }
    }

    // 2. Not a marker: no digit, a trailing byte, a leading zero, or 18 bytes.
    for marker in [
        &b"swarmops-store 3x\n"[..],
        b"swarmops-store 17\n",
        b"swarmops-store 04\n",
        b"swarmops-store x\n",
        b"swarmops-store 4",
    ] {
        assert!(
            matches!(read(marker), Err(StoreError::Format(_))),
            "{marker:?}"
        );
    }
    assert_eq!(read(b"swarmops-store 3\n").unwrap(), StoreFormat::V3);
}

/// Writes `tests/golden3/` from the values above. Writes nothing unless
/// `SWARMOPS_REGEN_GOLDEN=1` is set, so `--run-ignored all` never rewrites the pin. The files are
/// new layouts' (FORMAT 3); FORMAT 1 and 2 have their own, which this never touches.
#[test]
#[ignore = "writes tests/golden3 when SWARMOPS_REGEN_GOLDEN=1; run on purpose"]
fn regenerate_golden3() {
    if std::env::var_os("SWARMOPS_REGEN_GOLDEN").is_none() {
        eprintln!("regenerate_golden3: SWARMOPS_REGEN_GOLDEN not set; nothing written");
        return;
    }
    fs::create_dir_all(golden_dir()).unwrap();
    let write = |name: &str, lines: &[Vec<u8>]| {
        let text: String = lines.iter().map(|line| hex(line) + "\n").collect();
        fs::write(golden_dir().join(format!("{name}.hex")), text).unwrap();
    };

    // The marker, then the poison.
    write(
        "marker",
        &[
            StoreFormat::V3.marker().to_vec(),
            UPGRADE_POISON_V3.to_vec(),
        ],
    );
}

// ---- the files ----

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden3")
}

/// The lines of golden `name`, in hex.
fn read_golden(name: &str) -> Vec<Vec<u8>> {
    let text = fs::read_to_string(golden_dir().join(format!("{name}.hex"))).unwrap();
    text.lines().map(unhex).collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, b| {
        write!(text, "{b:02x}").expect("a String takes every write");
        text
    })
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}
