//! The check file's reader (M30-1) never panics, refuses only for a reason of its closed set at a
//! line the input has, and reads back what it accepted. The first input byte picks the reader,
//! the rest is the file:
//!
//! ```text
//! even ─▶ check::parse(.swarmops/checks.toml)   odd ─▶ check::parse_legacy(workspaces.json)
//! accepted: names unique, sorted, of the grammar; argv, dir and timeout_s inside their rules;
//!           stages (M30-6) never empty, known, none twice, in their own order; each digest the
//!           one its definition gives; written back by this target as TOML, read as the same
//!           checks
//! refused:  E_CHECK_FILE, a known reason, a line from 1 to the input's lines + 1, a message
//!           of one line and at most 512 bytes
//! ```
#![no_main]

use std::fmt::Write as _;

use libfuzzer_sys::fuzz_target;
use swarmops_api::CheckStage;
use swarmops_ops::check::{
    self, CHECK_FILE, Check, LEGACY_CHECK, LEGACY_FILE, MAX_ARGV_BYTES, MAX_ARGV_ITEMS,
    MAX_CHECK_NAME_BYTES, MAX_CHECK_TIMEOUT_S, MAX_CHECKS, ROOT_DIR, Reason, Refusal,
};

fuzz_target!(|data: &[u8]| {
    let Some((&reader, file)) = data.split_first() else {
        return;
    };
    if reader % 2 == 0 {
        match check::parse(file) {
            Ok(read) => {
                assert_eq!(read.version, 1);
                accepted(&read.checks);
                round_trip(&read.checks);
            }
            Err(refusal) => refused(&refusal, file, CHECK_FILE),
        }
        return;
    }
    match check::parse_legacy(file) {
        Ok(Some(default)) => {
            assert_eq!(default.name, LEGACY_CHECK);
            assert_eq!(default.line, None);
            assert_eq!(default.stages, [CheckStage::BeforeLand]);
            accepted(std::slice::from_ref(&default));
        }
        Ok(None) => {}
        Err(refusal) => refused(&refusal, file, LEGACY_FILE),
    }
});

/// `checks` keep version 1's rules, and each digest is its definition's.
fn accepted(checks: &[Check]) {
    assert!(checks.len() <= MAX_CHECKS, "{}", checks.len());
    for pair in checks.windows(2) {
        assert!(pair[0].name < pair[1].name, "{pair:?}");
    }
    for check in checks {
        let name = check.name.as_bytes();
        assert!(
            !name.is_empty() && name.len() <= MAX_CHECK_NAME_BYTES,
            "{check:?}"
        );
        assert!(
            name[0].is_ascii_lowercase() || name[0].is_ascii_digit(),
            "{check:?}"
        );
        let allowed = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(b);
        assert!(name.iter().all(allowed), "{check:?}");

        assert!(
            !check.argv.is_empty() && check.argv.len() <= MAX_ARGV_ITEMS,
            "{check:?}"
        );
        assert!(!check.argv[0].is_empty(), "{check:?}");
        let bytes: usize = check.argv.iter().map(String::len).sum();
        assert!(bytes as u64 <= MAX_ARGV_BYTES, "{check:?}");
        assert!(
            check.argv.iter().all(|arg| !arg.contains('\0')),
            "{check:?}"
        );

        let dir_ok =
            check.dir == ROOT_DIR || swarmops_core::RepoPath::new(check.dir.as_str()).is_ok();
        assert!(dir_ok, "{check:?}");
        assert!(
            (1..=MAX_CHECK_TIMEOUT_S).contains(&check.timeout_s),
            "{check:?}"
        );

        assert!(
            !check.stages.is_empty() && check.stages.len() <= CheckStage::ALL.len(),
            "{check:?}"
        );
        for pair in check.stages.windows(2) {
            assert!(pair[0] < pair[1], "{check:?}");
        }
        assert!(
            check.stages.iter().all(|s| CheckStage::ALL.contains(s)),
            "{check:?}"
        );
        assert_eq!(check.out().stages, check.stages);

        let same = Check::clone(check);
        assert_eq!(same.definition_digest(), check.definition_digest());
        assert_eq!(check.definition_digest().len(), 64, "{check:?}");
        assert_eq!(check.out().definition_digest, check.definition_digest());
    }
}

/// `checks`, written as a version 1 file by this target's own writer, read back as the same
/// checks and digests (the lines aside).
fn round_trip(checks: &[Check]) {
    let mut text = String::from("version = 1\n");
    for check in checks {
        let _ = writeln!(text, "\n[check.{}]", check.name);
        let argv: Vec<String> = check.argv.iter().map(|arg| basic(arg)).collect();
        let _ = writeln!(text, "argv = [{}]", argv.join(", "));
        let _ = writeln!(text, "dir = {}", basic(&check.dir));
        let _ = writeln!(text, "timeout_s = {}", check.timeout_s);
        let stages: Vec<String> = check.stages.iter().map(|s| basic(s.as_str())).collect();
        let _ = writeln!(text, "stages = [{}]", stages.join(", "));
    }
    let again = check::parse(text.as_bytes()).unwrap_or_else(|e| panic!("{text:?}: {e:?}"));
    assert_eq!(again.checks.len(), checks.len(), "{text:?}");
    for (read, check) in again.checks.iter().zip(checks) {
        assert_eq!(
            (&read.name, &read.argv, &read.dir, read.timeout_s),
            (&check.name, &check.argv, &check.dir, check.timeout_s),
            "{text:?}"
        );
        assert_eq!(read.stages, check.stages, "{text:?}");
        assert_eq!(read.definition_digest(), check.definition_digest());
    }
}

/// `value` as a TOML basic string: quoted, with `"`, `\` and every control character escaped.
fn basic(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `refusal` of `file` (`bytes`): a reason of the closed set, a line the input has, and the
/// error it makes `E_CHECK_FILE`, exit 1.
fn refused(refusal: &Refusal, bytes: &[u8], file: &str) {
    assert_eq!(refusal.file, file);
    assert!(Reason::ALL.contains(&refusal.reason), "{refusal:?}");
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64 + 1;
    if let Some(line) = refusal.line {
        assert!(
            (1..=lines + 1).contains(&line),
            "{refusal:?} in {lines} lines"
        );
    }
    let error = refusal.clone().into_error(true);
    assert_eq!(error.code, "E_CHECK_FILE");
    assert_eq!((error.exit, error.retryable), (1, false));
    assert!(!error.message.contains('\n'), "{error:?}");
    assert!(error.message.len() <= 512, "{} bytes", error.message.len());
    assert_eq!(error.details["reason"], refusal.reason.as_str());
    assert_eq!(error.details["vouched"], true);
}
