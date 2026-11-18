//! The check file's reader, its digest, and `check list` in both forms.

use std::fmt::Write as _;

use camino::Utf8PathBuf;
use proptest::prelude::*;
use serde_json::{Value, json};
use swarmops_api::{
    ChangeInput, ChangeOp, CheckListReq, CheckStage, CommitReq, ContentInput, ErrorCode, Identity,
    ModeInput, OpError, StoreOps,
};

use super::*;
use crate::LocalOps;

/// The fixture: three checks, every key, a default written out.
const FIXTURE: &str = include_str!("../../../swarmops-cli/tests/fixtures/check/checks.toml");

/// The fixture with a key no version 1 takes, `stage`, on line 11.
const REFUSED: &str = include_str!("../../../swarmops-cli/tests/fixtures/check/refused.toml");

/// A check file whose one check names a stage no build knows, `nightly`, on line 8.
const REFUSED_STAGE: &str =
    include_str!("../../../swarmops-cli/tests/fixtures/check/refused-stage.toml");

/// The checks of `text`, or the test fails.
#[track_caller]
fn checks(text: &str) -> Vec<Check> {
    match parse(text.as_bytes()) {
        Ok(file) => file.checks,
        Err(refusal) => panic!("{text:?}: {refusal:?}"),
    }
}

/// The refusal of `bytes`, or the test fails.
#[track_caller]
fn refused(bytes: &[u8]) -> Refusal {
    match parse(bytes) {
        Ok(file) => panic!("{:?} read: {file:?}", String::from_utf8_lossy(bytes)),
        Err(refusal) => refusal,
    }
}

/// `text` is refused for `reason`, at `line`, naming `key`.
#[track_caller]
fn refuses(text: &str, reason: Reason, line: Option<u64>, key: Option<&str>) {
    let refusal = refused(text.as_bytes());
    assert_eq!(
        (refusal.reason, refusal.line, refusal.key.as_deref()),
        (reason, line, key),
        "{text:?}: {refusal:?}"
    );
}

/// `version = 1` and one check `name` whose table holds `body`.
fn one(name: &str, body: &str) -> String {
    format!("version = 1\n[check.{name}]\n{body}\n")
}

/// A check of `argv`, `dir` and `timeout_s`, named `name`.
fn check(name: &str, argv: &[&str], dir: &str, timeout_s: u64) -> Check {
    Check {
        name: name.to_owned(),
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        dir: dir.to_owned(),
        timeout_s,
        stages: vec![CheckStage::BeforeLand],
        line: None,
    }
}

#[test]
fn a_version_alone_lists_nothing() {
    assert_eq!(checks("version = 1\n"), []);
    assert_eq!(checks("version = 1\n[check]\n"), []);
    assert_eq!(checks("# comments only\nversion = 1 # one\n"), []);
}

/// The fixture reads as three checks sorted by name, the defaults filled in, each with the line
/// of its table.
#[test]
fn the_fixture_reads_sorted_with_its_lines() {
    let read = parse(FIXTURE.as_bytes()).unwrap();
    assert_eq!(read.version, 1);
    let shown: Vec<(&str, &str, u64, Option<u64>)> = (read.checks.iter())
        .map(|c| (c.name.as_str(), c.dir.as_str(), c.timeout_s, c.line))
        .collect();
    assert_eq!(
        shown,
        [
            ("docs", ".", 900, Some(12)),
            ("lint", "crates/app", 600, Some(7)),
            ("unit", ".", 900, Some(4)),
        ]
    );
    assert_eq!(read.checks[2].argv, ["cargo", "test", "--locked"]);
    let stages: Vec<&[CheckStage]> = read.checks.iter().map(|c| c.stages.as_slice()).collect();
    assert_eq!(
        stages,
        [
            &[CheckStage::BeforeLand, CheckStage::AfterLand][..],
            &[CheckStage::BeforeLand],
            &[CheckStage::BeforeLand],
        ]
    );
}

// Stages (M30-6).

/// A check without `stages` is `before-land`; one may name several, and any order of the six
/// reads as their own order.
#[test]
fn stages_default_to_before_land_and_read_in_their_own_order() {
    assert_eq!(
        checks(&one("a", "argv = [\"x\"]"))[0].stages,
        [CheckStage::BeforeLand]
    );
    let two = one("a", "argv = [\"x\"]\nstages = [\"manual\", \"after-land\"]");
    assert_eq!(
        checks(&two)[0].stages,
        [CheckStage::AfterLand, CheckStage::Manual]
    );
    let all = "stages = [\"deploy\", \"manual\", \"release\", \"scheduled\", \"after-land\", \
               \"before-land\"]";
    assert_eq!(
        checks(&one("a", &format!("argv = [\"x\"]\n{all}")))[0].stages,
        CheckStage::ALL
    );
    let legacy = parse_legacy(br#"{"check": {"argv": ["make"]}}"#)
        .unwrap()
        .unwrap();
    assert_eq!(legacy.stages, [CheckStage::BeforeLand]);
    assert_eq!(legacy.out().stages, [CheckStage::BeforeLand]);
}

/// Each fault of `stages` is `value`, naming the key and the line of what is at fault: the
/// value, the item, or the second of a stage given twice.
#[test]
fn stages_refuse_each_fault_at_its_line() {
    let key = Some("check.a.stages");
    let at = |body: &str| one("a", &format!("argv = [\"x\"]\n{body}"));
    refuses(&at("stages = \"manual\""), Reason::Value, Some(4), key);
    refuses(&at("stages = []"), Reason::Value, Some(4), key);
    refuses(
        &at("stages = [\n  \"manual\",\n  1,\n]"),
        Reason::Value,
        Some(6),
        key,
    );
    refuses(
        &at("stages = [\n  \"manual\",\n  \"nightly\",\n]"),
        Reason::Value,
        Some(6),
        key,
    );
    refuses(
        &at("stages = [\n  \"manual\",\n  \"manual\",\n]"),
        Reason::Value,
        Some(6),
        key,
    );

    // The unknown stage is named, quoted and cut, and the six are listed.
    let error = refused(REFUSED_STAGE.as_bytes()).into_error(false);
    assert_eq!(
        error.message,
        ".swarmops/checks.toml has check.unit.stages holds \"nightly\", not a stage: one of \
         before-land, after-land, scheduled, release, manual, deploy (line 8)"
    );
    assert_eq!(
        Value::Object(error.details),
        json!({"file": ".swarmops/checks.toml", "reason": "value", "vouched": false,
               "line": 8, "key": "check.unit.stages"})
    );
    let long = "n".repeat(500);
    let refusal = refused(at(&format!("stages = [\"{long}\"]")).as_bytes());
    assert!(refusal.what.len() < 200, "{refusal:?}");
    assert!(refusal.what.contains('…'), "{refusal:?}");
}

/// Stages are not part of the definition: the digest of a check is the same with no `stages`,
/// the default written out, or other stages.
#[test]
fn stages_leave_the_definition_digest_alone() {
    let digest = |stages: &str| {
        checks(&one("a", &format!("argv = [\"make\"]\n{stages}")))[0].definition_digest()
    };
    let none = digest("");
    assert_eq!(none, check("a", &["make"], ".", 900).definition_digest());
    assert_eq!(digest("stages = [\"before-land\"]"), none);
    assert_eq!(digest("stages = [\"manual\", \"deploy\"]"), none);
}

/// `check list` with a stage lists the checks of it, still sorted by name, and echoes it; the
/// same in both forms.
#[test]
fn both_forms_list_one_stage() {
    // 1. The store form.
    let (_dir, ops) = store_with(&[(CHECK_FILE, FIXTURE, ModeInput::File)]);
    let mut req = CheckListReq::default();
    req.stage = Some(CheckStage::AfterLand);
    let after = list(&ops, req).unwrap();
    let names: Vec<&str> = after.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        (names, after.stage),
        (vec!["docs"], Some(CheckStage::AfterLand))
    );
    let mut req = CheckListReq::default();
    req.stage = Some(CheckStage::BeforeLand);
    let before = list(&ops, req).unwrap();
    let names: Vec<&str> = before.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["docs", "lint", "unit"]);
    let mut req = CheckListReq::default();
    req.stage = Some(CheckStage::Deploy);
    assert_eq!(list(&ops, req).unwrap().checks, []);
    assert_eq!(list(&ops, CheckListReq::default()).unwrap().stage, None);

    // 2. The directory form.
    let (_tmp, root) = dir_with(&[(CHECK_FILE, FIXTURE.as_bytes())]);
    let dir_after = list_dir(&root, Some(CheckStage::AfterLand)).unwrap();
    assert_eq!(dir_after.checks, after.checks);
    assert_eq!(dir_after.stage, Some(CheckStage::AfterLand));
}

/// Any table syntax is the same document: dotted keys and inline tables, a check's line the line
/// of its key then.
#[test]
fn every_table_syntax_reads_alike() {
    let dotted = "version = 1\ncheck.a.argv = [\"x\"]\n\ncheck.b = { argv = [\"y\"] }\n";
    let read = checks(dotted);
    assert_eq!(read.len(), 2);
    assert_eq!((read[0].line, read[1].line), (Some(2), Some(4)));
    let headed = "version = 1\n[check]\na = { argv = [\"x\"] }\n[check.b]\nargv = [\"y\"]\n";
    let other = checks(headed);
    assert_eq!(
        other
            .iter()
            .map(Check::definition_digest)
            .collect::<Vec<_>>(),
        read.iter()
            .map(Check::definition_digest)
            .collect::<Vec<_>>()
    );
}

/// The file is TOML 1.1, as the locked `toml` crate reads it: the escapes `\e` and `\xHH`, and an
/// inline table over several lines with a trailing comma, read. A change of the crate that reads
/// another TOML shows here.
#[test]
fn the_file_is_toml_1_1() {
    let text = "version = 1\ncheck = {\n  a = { argv = [\"x\\e[1m\", \"\\xff\"] },\n}\n";
    let read = checks(text);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].argv, ["x\u{1b}[1m", "\u{ff}"]);
    assert_eq!(read[0].line, Some(3));
}

/// Moving a table changes nothing but the lines.
#[test]
fn file_order_does_not_matter() {
    let first = "version = 1\n[check.b]\nargv = [\"b\"]\n[check.a]\nargv = [\"a\"]\n";
    let second = "version = 1\n\n[check.a]\nargv = [\"a\"]\n\n[check.b]\nargv = [\"b\"]\n";
    let unlined = |text: &str| {
        let mut read = checks(text);
        for check in &mut read {
            check.line = None;
        }
        read
    };
    assert_eq!(unlined(first), unlined(second));
    let names: Vec<String> = checks(first).into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["a", "b"]);
}

/// The refused fixture: `stage` is no key of version 1, at its line.
#[test]
fn the_refused_fixture_names_its_key_and_line() {
    refuses(
        REFUSED,
        Reason::UnknownKey,
        Some(11),
        Some("check.lint.stage"),
    );
    let error = refused(REFUSED.as_bytes()).into_error(true);
    assert_eq!(error.code, "E_CHECK_FILE");
    assert_eq!((error.exit, error.retryable), (1, false));
    assert_eq!(
        error.message,
        ".swarmops/checks.toml has an unknown key check.lint.stage (line 11)"
    );
    assert_eq!(
        Value::Object(error.details),
        json!({"file": ".swarmops/checks.toml", "reason": "unknown_key", "vouched": true,
               "line": 11, "key": "check.lint.stage"})
    );
    assert!(error.hint.unwrap().contains("a newer swarmops may read"));
}

/// The words the later units of M30 add to a check, and keys at the top, are refused as
/// unknown, so that a build that does not read them never lists a file without them.
#[test]
fn a_key_version_1_does_not_take_is_refused() {
    for key in [
        "stage",
        "needs",
        "inputs",
        "variants",
        "labels",
        "parameters",
    ] {
        let text = one("a", &format!("argv = [\"x\"]\n{key} = []"));
        let path = format!("check.a.{key}");
        refuses(&text, Reason::UnknownKey, Some(4), Some(&path));
    }
    refuses(
        "version = 1\ninclude = [\"other.toml\"]\n",
        Reason::UnknownKey,
        Some(2),
        Some("include"),
    );
    refuses(
        "version = 1\n[check.a.inputs]\npaths = []\n",
        Reason::UnknownKey,
        Some(2),
        Some("check.a.inputs"),
    );
    refuses(
        "version = 1\n\"a.b\" = 1\n",
        Reason::UnknownKey,
        Some(2),
        Some("\"a.b\""),
    );
}

#[test]
fn the_version_is_the_integer_1() {
    refuses("[check.a]\nargv = [\"x\"]\n", Reason::Version, None, None);
    for (text, line) in [
        ("version = 2\n", 1),
        ("\nversion = \"1\"\n", 2),
        ("version = 1.0\n", 1),
        ("version = true\n", 1),
        ("version = -1\n", 1),
    ] {
        refuses(text, Reason::Version, Some(line), Some("version"));
    }
    // The version first: a version 2 file's new keys are not this build's to judge.
    refuses(
        "version = 2\nnew = 1\n",
        Reason::Version,
        Some(1),
        Some("version"),
    );
    assert_eq!(checks("version = 0x1\n"), []);
}

#[test]
fn a_check_name_keeps_its_grammar() {
    let long = "a".repeat(65);
    for name in [
        "Build",
        "\"-x\"",
        "\"a b\"",
        "\"\"",
        "\"_x\"",
        "\"a.b\"",
        long.as_str(),
    ] {
        let text = one(name, "argv = [\"x\"]");
        let refusal = refused(text.as_bytes());
        assert_eq!(
            (refusal.reason, refusal.line),
            (Reason::Name, Some(2)),
            "{name}"
        );
    }
    let longest = "a".repeat(64);
    for name in ["a", "0", "a-b_c", "9-lives", longest.as_str()] {
        assert_eq!(checks(&one(name, "argv = [\"x\"]")).len(), 1, "{name}");
    }
    refuses(
        "version = 1\n[check]\nx = 1\n",
        Reason::Value,
        Some(3),
        Some("check.x"),
    );
    refuses(
        "version = 1\ncheck = 1\n",
        Reason::Value,
        Some(2),
        Some("check"),
    );
    refuses(
        "version = 1\n[[check]]\nargv = [\"x\"]\n",
        Reason::Value,
        Some(2),
        Some("check"),
    );
}

#[test]
fn argv_is_one_to_1024_strings() {
    let argv = |body: &str, line: u64| {
        refuses(
            &one("a", body),
            Reason::Value,
            Some(line),
            Some("check.a.argv"),
        );
    };
    argv("argv = []", 3);
    argv("argv = \"make\"", 3);
    argv("argv = [1]", 3);
    argv("argv = [\"make\",\n  2]", 4);
    argv("argv = [\"a\\u0000\"]", 3);
    argv("argv = [\"\"]", 3);
    let many = vec!["\"x\""; MAX_ARGV_ITEMS + 1].join(", ");
    argv(&format!("argv = [{many}]"), 3);
    let fits = vec!["\"x\""; MAX_ARGV_ITEMS].join(", ");
    assert_eq!(
        checks(&one("a", &format!("argv = [{fits}]")))[0].argv.len(),
        1024
    );
    let half = usize::try_from(MAX_ARGV_BYTES / 2).unwrap();
    let big = format!(
        "argv = [\"{}\", \"{}\"]",
        "x".repeat(half),
        "y".repeat(half + 1)
    );
    argv(&big, 3);
    let exact = format!(
        "argv = [\"{}\", \"{}\"]",
        "x".repeat(half),
        "y".repeat(half)
    );
    assert_eq!(checks(&one("a", &exact)).len(), 1);
    refuses(
        &one("a", "dir = \"src\""),
        Reason::Value,
        Some(2),
        Some("check.a.argv"),
    );
    // An empty argument after the program is an argument.
    assert_eq!(
        checks(&one("a", "argv = [\"echo\", \"\"]"))[0].argv,
        ["echo", ""]
    );
}

#[test]
fn dir_is_the_root_or_a_repository_path() {
    for dir in ["/abs", "a/../b", "a/", "", "a//b", "./a", "..", "a\\u0000"] {
        let body = format!("argv = [\"x\"]\ndir = \"{dir}\"");
        refuses(
            &one("a", &body),
            Reason::Value,
            Some(4),
            Some("check.a.dir"),
        );
    }
    refuses(
        &one("a", "argv = [\"x\"]\ndir = 1"),
        Reason::Value,
        Some(4),
        Some("check.a.dir"),
    );
    for (dir, read) in [(".", "."), ("a", "a"), ("crates/app", "crates/app")] {
        let body = format!("argv = [\"x\"]\ndir = \"{dir}\"");
        assert_eq!(checks(&one("a", &body))[0].dir, read, "{dir}");
    }
}

#[test]
fn timeout_is_1_to_86400_seconds() {
    for value in [
        "0",
        "86401",
        "-1",
        "1.5",
        "\"10\"",
        "true",
        "99999999999999999999",
    ] {
        let body = format!("argv = [\"x\"]\ntimeout_s = {value}");
        refuses(
            &one("a", &body),
            Reason::Value,
            Some(4),
            Some("check.a.timeout_s"),
        );
    }
    for (value, seconds) in [("1", 1), ("86400", 86_400), ("0x10", 16), ("1_000", 1_000)] {
        let body = format!("argv = [\"x\"]\ntimeout_s = {value}");
        assert_eq!(checks(&one("a", &body))[0].timeout_s, seconds, "{value}");
    }
}

#[test]
fn at_most_1000_checks() {
    let file = |count: usize| {
        let mut text = String::from("version = 1\n");
        for i in 0..count {
            let _ = write!(text, "[check.c{i:04}]\nargv = [\"x\"]\n");
        }
        text
    };
    assert_eq!(checks(&file(MAX_CHECKS)).len(), MAX_CHECKS);
    refuses(
        &file(MAX_CHECKS + 1),
        Reason::TooMany,
        Some(2002),
        Some("check.c1000"),
    );
}

#[test]
fn a_file_over_1_mib_is_too_large() {
    let limit = usize::try_from(MAX_CHECK_FILE_BYTES).unwrap();
    let mut fits = String::from("version = 1\n");
    fits.push_str(&"#".repeat(limit - fits.len()));
    assert_eq!(checks(&fits), []);
    fits.push('#');
    let refusal = refused(fits.as_bytes());
    assert_eq!(
        (refusal.reason, refusal.line, refusal.size),
        (
            Reason::TooLarge,
            None,
            Some((MAX_CHECK_FILE_BYTES + 1, MAX_CHECK_FILE_BYTES))
        )
    );
    let error = refusal.into_error(false);
    assert_eq!(error.details["bytes"], MAX_CHECK_FILE_BYTES + 1);
    assert_eq!(error.details["limit"], MAX_CHECK_FILE_BYTES);
    assert_eq!(error.details["vouched"], false);
}

#[test]
fn bad_text_and_bad_toml_name_their_line() {
    let bytes = b"version = 1\n# fine\n# \xff not UTF-8\n";
    let refusal = refused(bytes);
    assert_eq!((refusal.reason, refusal.line), (Reason::NotText, Some(3)));
    refuses(
        "version = 1\n[check.a]\nargv = [\"x\"]\n[check.a]\nargv = [\"y\"]\n",
        Reason::Syntax,
        Some(4),
        None,
    );
    refuses(
        "version = 1\n[check.a]\nargv = [\"x]\n",
        Reason::Syntax,
        Some(3),
        None,
    );
    refuses("version = 1\nversion = 1\n", Reason::Syntax, Some(2), None);
    refuses("version 1\n", Reason::Syntax, Some(1), None);
}

/// A refusal's message is one line, whatever the file holds.
#[test]
fn a_refusal_is_one_line() {
    let text = "version = 1\n[check.\"a\\nb\"]\nargv = [\"x\"]\n";
    let error = refused(text.as_bytes()).into_error(false);
    assert!(!error.message.contains('\n'), "{}", error.message);
    assert_eq!(error.details["key"], "check.\"a\\nb\"");
}

/// A refusal repeats at most the first 64 characters of a number, a string or a key, however
/// long the file writes it: the message stays short whatever the 1 MiB holds.
#[test]
fn a_refusal_cuts_what_it_repeats() {
    let long = |c: &str| c.repeat(1 << 19);
    let texts = [
        format!("version = 1{}\n", long("0")),
        format!("version = 1.{}\n", long("0")),
        format!("version = 1\ncheck.a.timeout_s = 1{}\n", long("0")),
        format!("version = 1\ncheck.a.argv = 1.{}\n", long("5")),
        format!("version = 1\ncheck.a.dir = \"{}\"\n", long("\\u001b")),
        format!("version = 1\n{} = 1\n", long("k")),
        format!("version = 1\ncheck.a.{} = 1\n", long("k")),
        format!("version = 1\ncheck.{} = {{ argv = [\"x\"] }}\n", long("a")),
        format!(
            "version = 1\ncheck.\"{}\".argv = [\"x\"]\n",
            long("\\u0007")
        ),
        format!("version = 1\n{k} = 1\n{k} = 2\n", k = long("k")),
        format!("version = 1\nx = \"{}\n", long("y")),
    ];
    for text in texts {
        let error = refused(text.as_bytes()).into_error(true);
        assert!(
            error.message.len() < 300,
            "{} bytes: {}",
            error.message.len(),
            error.message
        );
        let details = serde_json::to_string(&error.details).unwrap();
        assert!(details.len() < 400, "{} bytes", details.len());
    }
    let error = refused(format!("version = 1{}\n", long("0")).as_bytes()).into_error(true);
    assert_eq!(
        error.message,
        format!(
            ".swarmops/checks.toml has version 1{}…, not 1 (line 1)",
            "0".repeat(63)
        )
    );
}

// The legacy file.

/// `check.argv` of `workspaces.json` is the one check `default`, at the root.
#[test]
fn the_legacy_argv_is_the_check_default() {
    let read = |text: &str| parse_legacy(text.as_bytes()).unwrap();
    let default = read(r#"{"check": {"argv": ["./check.sh", "--all"]}}"#).unwrap();
    assert_eq!(
        default,
        check(LEGACY_CHECK, &["./check.sh", "--all"], ".", 900)
    );
    let timed = read(r#"{"check": {"argv": ["x"], "timeout_s": 60}, "other": 1}"#).unwrap();
    assert_eq!(timed.timeout_s, 60);
    assert_eq!(read(r#"{"check": {"argv": []}}"#), None);
    assert_eq!(read(r#"{"prefixes": ["src"]}"#), None);
    assert_eq!(
        read(r#"{"check": null, "bootstrap": {"argv": ["b"]}}"#),
        None
    );
    assert_eq!(read("{}"), None);
}

#[test]
fn a_legacy_file_that_does_not_read_is_refused() {
    let refuses = |text: &str, reason: Reason, line: Option<u64>| {
        let refusal = parse_legacy(text.as_bytes()).unwrap_err();
        assert_eq!(refusal.file, LEGACY_FILE, "{text}");
        assert_eq!((refusal.reason, refusal.line), (reason, line), "{text}");
    };
    refuses("{\n\"check\": ", Reason::Syntax, Some(2));
    refuses("not json", Reason::Syntax, Some(1));
    refuses("{\n\"check\": {\"argv\": \"x\"}}", Reason::Value, Some(2));
    refuses("{\"check\": {}}", Reason::Value, Some(1));
    refuses(
        "{\"check\": {\"argv\": [\"x\"], \"timeout_s\": -1}}",
        Reason::Value,
        Some(1),
    );
    refuses("{\"prefixes\": \"src\"}", Reason::Value, Some(1));
    refuses(
        "{\"check\": {\"argv\": [\"x\"], \"timeout_s\": 0}}",
        Reason::Value,
        None,
    );
    refuses("{\"check\": {\"argv\": [\"\"]}}", Reason::Value, None);
    refuses("\u{0}", Reason::Syntax, Some(1));
    let big = format!("{{\"x\": \"{}\"}}", "y".repeat(64 * 1024));
    refuses(&big, Reason::TooLarge, None);
    let big = format!(
        "{{\"check\": {{\"argv\": \"{}\"}}}}",
        "\\u001b".repeat(9_000)
    );
    refuses(&big, Reason::Value, Some(1));
    let refusal = parse_legacy(b"{\"x\": \"\xff\"}").unwrap_err();
    assert_eq!(refusal.reason, Reason::NotText);
    let error = refusal.into_error(false);
    assert_eq!(error.details["file"], ".swarmops/workspaces.json");
    assert!(error.hint.unwrap().contains("check.argv"));
}

/// A legacy file that does not read is named by where it fails, never by repeating what it
/// holds: serde's wording quotes the string at fault whole.
#[test]
fn a_legacy_refusal_repeats_nothing_of_the_file() {
    let message = |text: &str| {
        parse_legacy(text.as_bytes())
            .unwrap_err()
            .into_error(true)
            .message
    };
    let long = format!(
        "{{\"check\": {{\"argv\": \"\\u001b[31m{}\"}}}}",
        "A".repeat(60_000)
    );
    assert_eq!(
        message(&long),
        ".swarmops/workspaces.json has a value of the wrong type or lacks a key, at column \
         60031 (line 1)"
    );
    assert_eq!(
        message("{\n\"check\": {\"argv\": [\"x\"], \"timeout_s\": \"60\"}}"),
        ".swarmops/workspaces.json has a value of the wrong type or lacks a key, at column 42 \
         (line 2)"
    );
    assert_eq!(
        message("{\"check\": {}}"),
        ".swarmops/workspaces.json has a value of the wrong type or lacks a key, at column 12 \
         (line 1)"
    );
    assert_eq!(
        message("{\n  \"check\" 1}"),
        ".swarmops/workspaces.json is not JSON, at column 11 (line 2)"
    );
    assert_eq!(
        message("{\"check\": "),
        ".swarmops/workspaces.json is not JSON: it ends before its last value (line 1)"
    );
}

// The digest.

/// The name is no part of the digest; a default written out is the default; the legacy check
/// is the same definition as a table of the same values.
#[test]
fn a_definition_digest_ignores_names_and_written_defaults() {
    let written = one(
        "other",
        "argv = [\"./check.sh\"]\ndir = \".\"\ntimeout_s = 900",
    );
    let from_toml = &checks(&written)[0];
    let legacy = parse_legacy(br#"{"check": {"argv": ["./check.sh"]}}"#)
        .unwrap()
        .unwrap();
    assert_eq!(from_toml.definition_digest(), legacy.definition_digest());
    let read = parse(FIXTURE.as_bytes()).unwrap();
    let docs = &read.checks[0];
    let unwritten = one(
        "x",
        "argv = [\"python3\", \"-I\", \"scripts/check-docs.py\"]",
    );
    assert_eq!(
        docs.definition_digest(),
        checks(&unwritten)[0].definition_digest()
    );
}

/// Any change of an argument, their order or split, the directory or the timeout changes it.
#[test]
fn a_definition_digest_changes_with_the_definition() {
    let base = check("a", &["make", "test"], ".", 900);
    let others = [
        check("a", &["make", "tests"], ".", 900),
        check("a", &["test", "make"], ".", 900),
        check("a", &["make test"], ".", 900),
        check("a", &["make", "te", "st"], ".", 900),
        check("a", &["make", "test", ""], ".", 900),
        check("a", &["make", "test"], "src", 900),
        check("a", &["make", "test"], ".", 901),
    ];
    let mut digests = vec![base.definition_digest()];
    digests.extend(others.iter().map(Check::definition_digest));
    let unique: std::collections::BTreeSet<&String> = digests.iter().collect();
    assert_eq!(unique.len(), digests.len(), "{digests:#?}");
}

/// Two digests pinned: the recipe of `v0`, changed only in the open until a release fixes it.
#[test]
fn two_definition_digests_are_pinned() {
    assert_eq!(
        check("a", &["make"], ".", 900).definition_digest(),
        "25fb129d52047b2d4fdc6f5c464153fd633dd6a40d8abee361f8a845211f3019"
    );
    assert_eq!(
        check("a", &["cargo", "test"], "crates/app", 600).definition_digest(),
        "2aa3a3895d0e2eb77750f1b1a909a97866c751047576c45bcbc77affe9a0329e"
    );
}

/// The recipe is the one the digest module documents, byte for byte: its domain, then argv,
/// dir and timeout_s, each a tag, a length and the value; a default left out.
#[test]
fn a_definition_digest_hashes_the_documented_bytes() {
    let le = |n: u64| n.to_le_bytes().to_vec();
    let mut bytes = b"swarmops:check-definition:v0\0".to_vec();
    let argv = [le(2), le(5), b"cargo".to_vec(), le(4), b"test".to_vec()].concat();
    bytes.extend([vec![1], le(33), argv].concat());
    bytes.extend([vec![2], le(10), b"crates/app".to_vec()].concat());
    bytes.extend([vec![3], le(8), le(600)].concat());
    assert_eq!(
        check("a", &["cargo", "test"], "crates/app", 600).definition_digest(),
        blake3::hash(&bytes).to_hex().to_string()
    );
    let root = [b"swarmops:check-definition:v0\0".to_vec(), vec![1], le(20)].concat();
    let argv = [le(1), le(4), b"make".to_vec()].concat();
    assert_eq!(
        check("b", &["make"], ".", 900).definition_digest(),
        blake3::hash(&[root, argv].concat()).to_hex().to_string()
    );
}

// The two forms of `check list`.

/// A store whose main holds `files` (path, content), one of them maybe a link.
fn store_with(files: &[(&str, &str, ModeInput)]) -> (tempfile::TempDir, LocalOps) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let (ops, _) = LocalOps::init(&root, "test").unwrap();
    let who = Identity::parse("agent/test").unwrap();
    let mut changes = vec![{
        let mut readme = ChangeInput::new("README".to_owned(), ChangeOp::Put);
        readme.content = Some(ContentInput::Inline("hi\n".to_owned()));
        readme
    }];
    for (path, content, mode) in files {
        let mut change = ChangeInput::new((*path).to_owned(), ChangeOp::Put);
        change.content = Some(ContentInput::Inline((*content).to_owned()));
        change.mode = Some(*mode);
        changes.push(change);
    }
    let mut req = CommitReq::new(changes, "root".to_owned());
    req.advance = Some("main".to_owned());
    ops.commit(&who, req).unwrap();
    (dir, ops)
}

/// The error `result` holds, or the test fails.
#[track_caller]
fn error_of<T: std::fmt::Debug>(result: Result<T, OpError>) -> OpError {
    result.expect_err("an error")
}

#[test]
fn the_store_form_reads_the_tree_s_file() {
    let (_dir, ops) = store_with(&[(CHECK_FILE, FIXTURE, ModeInput::File)]);
    let resp = list(&ops, CheckListReq::default()).unwrap();
    assert!(!resp.vouched);
    assert_eq!(resp.file.as_deref(), Some(CHECK_FILE));
    assert_eq!(resp.version, Some(1));
    assert_eq!(resp.tree.as_ref().map(String::len), Some(64));
    let names: Vec<&str> = resp.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["docs", "lint", "unit"]);
    assert_eq!(resp.checks[1].line, Some(7));
}

#[test]
fn the_store_form_without_a_file_lists_nothing() {
    let (_dir, ops) = store_with(&[]);
    let resp = list(&ops, CheckListReq::default()).unwrap();
    assert_eq!(
        (resp.file, resp.version, resp.checks.len()),
        (None, None, 0)
    );
    assert!(resp.tree.is_some());

    let legacy = r#"{"check": {"argv": ["./check.sh"]}}"#;
    let (_dir, ops) = store_with(&[(LEGACY_FILE, legacy, ModeInput::File)]);
    let resp = list(&ops, CheckListReq::default()).unwrap();
    assert_eq!(resp.file.as_deref(), Some(LEGACY_FILE));
    assert_eq!(resp.version, None);
    assert_eq!(resp.checks[0].name, "default");
    assert_eq!(resp.checks[0].line, None);
}

/// A check file present makes `workspaces.json` irrelevant, even one that does not parse.
#[test]
fn a_check_file_replaces_the_legacy_one() {
    let (_dir, ops) = store_with(&[
        (CHECK_FILE, "version = 1\n", ModeInput::File),
        (LEGACY_FILE, "not json", ModeInput::File),
    ]);
    let resp = list(&ops, CheckListReq::default()).unwrap();
    assert_eq!(resp.file.as_deref(), Some(CHECK_FILE));
    assert_eq!(resp.checks, []);
}

#[test]
fn the_store_form_refuses_a_link_or_a_bad_file() {
    let (_dir, ops) = store_with(&[(CHECK_FILE, "other.toml", ModeInput::Symlink)]);
    let error = error_of(list(&ops, CheckListReq::default()));
    assert_eq!(error.code, ErrorCode::CheckFile.as_str());
    assert_eq!(error.details["reason"], "not_a_file");
    assert_eq!(error.details["vouched"], false);

    let (_dir, ops) = store_with(&[(".swarmops/checks.toml/x", "x", ModeInput::File)]);
    let error = error_of(list(&ops, CheckListReq::default()));
    assert_eq!(error.details["reason"], "not_a_file", "{error:?}");

    let (_dir, ops) = store_with(&[(CHECK_FILE, REFUSED, ModeInput::File)]);
    let error = error_of(list(&ops, CheckListReq::default()));
    assert_eq!(error.details["line"], 11);
    assert_eq!(error.details["vouched"], false);

    let (_dir, ops) = store_with(&[(CHECK_FILE, FIXTURE, ModeInput::File)]);
    let error = error_of(list(&ops, CheckListReq::new("release/none")));
    assert_eq!(error.code, ErrorCode::RevNotFound.as_str());
}

/// `.swarmops` as a symbolic link is refused in both forms alike, by a message that names it:
/// neither follows it, and neither lists the tree as one without checks.
#[test]
fn both_forms_refuse_swarmops_as_a_link() {
    let said = ".swarmops/checks.toml cannot be read: .swarmops is a symbolic link, which is not \
                followed";
    let (_dir, ops) = store_with(&[
        ("real/checks.toml", FIXTURE, ModeInput::File),
        (".swarmops", "real", ModeInput::Symlink),
    ]);
    let error = error_of(list(&ops, CheckListReq::default()));
    assert_eq!(error.code, ErrorCode::CheckFile.as_str());
    assert_eq!(error.message, said);
    assert_eq!(
        Value::Object(error.details),
        json!({"file": ".swarmops/checks.toml", "reason": "not_a_file", "vouched": false})
    );

    let (_dir, root) = dir_with(&[("real/checks.toml", FIXTURE.as_bytes())]);
    std::os::unix::fs::symlink("real", root.join(".swarmops")).unwrap();
    let error = error_of(list_dir(&root, None));
    assert_eq!(error.message, said);
    assert_eq!(
        Value::Object(error.details),
        json!({"file": ".swarmops/checks.toml", "reason": "not_a_file", "vouched": true})
    );
}

/// A file over the limit is refused by its size, never read whole.
#[test]
fn the_store_form_refuses_a_file_over_the_limit() {
    let big = "#".repeat(usize::try_from(MAX_CHECK_FILE_BYTES).unwrap() + 1);
    let (_dir, ops) = store_with(&[(CHECK_FILE, &big, ModeInput::File)]);
    let error = error_of(list(&ops, CheckListReq::default()));
    assert_eq!(error.details["reason"], "too_large");
    assert_eq!(error.details["bytes"], MAX_CHECK_FILE_BYTES + 1);
}

/// A directory holding `files` (path, content), below a temp directory.
fn dir_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("tree")).unwrap();
    std::fs::create_dir(&root).unwrap();
    for (path, content) in files {
        let at = root.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, content).unwrap();
    }
    (dir, root)
}

#[test]
fn the_directory_form_is_vouched_for() {
    let (_dir, root) = dir_with(&[(CHECK_FILE, FIXTURE.as_bytes())]);
    let resp = list_dir(&root, None).unwrap();
    assert!(resp.vouched);
    assert_eq!(resp.tree, None);
    let (_store, ops) = store_with(&[(CHECK_FILE, FIXTURE, ModeInput::File)]);
    let stored = list(&ops, CheckListReq::default()).unwrap();
    assert_eq!(resp.checks, stored.checks);

    let (_dir, root) = dir_with(&[]);
    let resp = list_dir(&root, None).unwrap();
    assert_eq!(
        (resp.vouched, resp.file, resp.checks.len()),
        (true, None, 0)
    );

    let (_dir, root) = dir_with(&[(CHECK_FILE, REFUSED.as_bytes())]);
    let error = error_of(list_dir(&root, None));
    assert_eq!(error.details["vouched"], true);
    assert_eq!(error.details["line"], 11);

    let (_dir, root) = dir_with(&[(LEGACY_FILE, br#"{"check": {"argv": ["make"]}}"#)]);
    assert_eq!(list_dir(&root, None).unwrap().checks[0].name, "default");
}

#[test]
fn the_directory_form_follows_no_link() {
    let (_dir, root) = dir_with(&[("elsewhere.toml", FIXTURE.as_bytes())]);
    std::fs::create_dir(root.join(".swarmops")).unwrap();
    std::os::unix::fs::symlink("../elsewhere.toml", root.join(CHECK_FILE)).unwrap();
    let error = error_of(list_dir(&root, None));
    assert_eq!(error.details["reason"], "not_a_file");
    assert_eq!(error.details["vouched"], true);

    let (_dir, root) = dir_with(&[("real/checks.toml", FIXTURE.as_bytes())]);
    std::os::unix::fs::symlink("real", root.join(".swarmops")).unwrap();
    assert_eq!(
        error_of(list_dir(&root, None)).details["reason"],
        "not_a_file"
    );

    let (_dir, root) = dir_with(&[]);
    std::fs::create_dir_all(root.join(CHECK_FILE)).unwrap();
    assert_eq!(
        error_of(list_dir(&root, None)).details["reason"],
        "not_a_file"
    );
}

#[test]
fn the_directory_form_needs_a_directory() {
    let (_dir, root) = dir_with(&[("file", b"x")]);
    for dir in [root.join("missing"), root.join("file")] {
        let error = error_of(list_dir(&dir, None));
        assert_eq!(error.code, ErrorCode::InvalidArg.as_str());
        assert_eq!(error.details["reason"], "dir");
    }
    let error = error_of(list_dir(camino::Utf8Path::new(""), None));
    assert_eq!(error.message, "--dir \"\" names no directory");
    let (_dir, root) = dir_with(&[(".swarmops", b"a file")]);
    assert_eq!(list_dir(&root, None).unwrap().checks, []);
}

/// A file the directory form cannot read is `E_IO`, named below the directory and vouched for
/// like every answer of that form.
#[test]
fn the_directory_form_s_io_error_is_vouched_for() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_dir, root) = dir_with(&[(CHECK_FILE, FIXTURE.as_bytes())]);
    let file = root.join(CHECK_FILE);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&file).is_ok() {
        return; // A user that reads every file, such as root: nothing to refuse.
    }
    let error = error_of(list_dir(&root, None));
    assert_eq!(error.code, ErrorCode::Io.as_str());
    assert_eq!(
        error.message,
        ".swarmops/checks.toml cannot be read: permission denied"
    );
    assert_eq!(
        Value::Object(error.details),
        json!({"path": ".swarmops/checks.toml", "vouched": true})
    );
}

#[test]
fn the_directory_form_refuses_a_file_over_the_limit() {
    let big = vec![b'#'; usize::try_from(MAX_CHECK_FILE_BYTES).unwrap() + 1];
    let (_dir, root) = dir_with(&[(CHECK_FILE, &big)]);
    let error = error_of(list_dir(&root, None));
    assert_eq!(error.details["reason"], "too_large");
    assert_eq!(error.details["bytes"], MAX_CHECK_FILE_BYTES + 1);
    assert!(!error.message.contains(root.as_str()), "{}", error.message);
}

// The round trip.

/// A check's definition that version 1 takes.
fn definition() -> impl Strategy<Value = (Vec<String>, String, u64)> {
    let arg = "[ -~]{0,12}";
    let argv = ("[a-z./][ -~]{0,8}", proptest::collection::vec(arg, 0..4)).prop_map(
        |(program, mut rest)| {
            rest.insert(0, program);
            rest
        },
    );
    let dir = prop_oneof![Just(".".to_owned()), "[a-z]{1,4}(/[a-z0-9_]{1,4}){0,2}",];
    (argv, dir, 1..=MAX_CHECK_TIMEOUT_S)
}

proptest! {
    /// Definitions written with `toml`'s own writer read back as the same checks, sorted, with
    /// the same digests.
    #[test]
    fn written_checks_read_back(
        defs in proptest::collection::btree_map("[a-z0-9][a-z0-9_-]{0,10}", definition(), 0..6)
    ) {
        let mut table = toml::Table::new();
        table.insert("version".to_owned(), toml::Value::Integer(1));
        let mut all = toml::Table::new();
        for (name, (argv, dir, timeout_s)) in &defs {
            let mut one = toml::Table::new();
            let argv = argv.iter().cloned().map(toml::Value::String).collect();
            one.insert("argv".to_owned(), toml::Value::Array(argv));
            one.insert("dir".to_owned(), toml::Value::String(dir.clone()));
            one.insert(
                "timeout_s".to_owned(),
                toml::Value::Integer(i64::try_from(*timeout_s).unwrap()),
            );
            all.insert(name.clone(), toml::Value::Table(one));
        }
        table.insert("check".to_owned(), toml::Value::Table(all));
        let text = toml::to_string(&table).unwrap();

        let read = checks(&text);
        prop_assert_eq!(read.len(), defs.len());
        for (check, (name, (argv, dir, timeout_s))) in read.iter().zip(&defs) {
            prop_assert_eq!(&check.name, name);
            prop_assert_eq!(&check.argv, argv);
            prop_assert_eq!(&check.dir, dir);
            prop_assert_eq!(check.timeout_s, *timeout_s);
            let fresh = digest::definition(argv, dir, *timeout_s);
            prop_assert_eq!(check.definition_digest(), fresh);
        }
    }
}

proptest! {
    /// The line index, found once, gives every offset the line a count from the file's start
    /// gives, an offset past the end included.
    #[test]
    fn the_line_index_counts_as_line_of(
        bytes in proptest::collection::vec(prop_oneof![Just(b'\n'), any::<u8>()], 0..64)
    ) {
        let lines = file::Lines::of(&bytes);
        for at in 0..=bytes.len() + 1 {
            prop_assert_eq!(lines.line(at), crate::error::line_of(&bytes, at), "at {}", at);
        }
    }
}

/// Nesting past the reader's depth is a syntax refusal, never a crash. Arrays and inline tables
/// name their line; dotted keys, which the parser gives no span for, name none.
#[test]
fn deep_nesting_is_refused() {
    for depth in [1_000, 100_000] {
        let arrays = format!(
            "version = 1\nx = {}{}\n",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        refuses(&arrays, Reason::Syntax, Some(2), None);
        let tables = format!(
            "version = 1\nx = {}1{}\n",
            "{a=".repeat(depth),
            "}".repeat(depth)
        );
        refuses(&tables, Reason::Syntax, Some(2), None);
    }
    let keys = format!("version = 1\n{}a = 1\n", "a.".repeat(300_000));
    refuses(&keys, Reason::Syntax, None, None);
}
