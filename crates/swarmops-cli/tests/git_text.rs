//! git's text shapes through the binary (the git-familiar command design, §3 "Text shapes"):
//! `--human` prints `ws status`, `log`, `diff`, `ws diff` and `ws commit` as git prints them, a
//! git shape flag asks for that text with no `--human`, and `--json` prints the envelope it
//! printed before.
//!
//! The fixture is a store whose `main` holds two changesets, workspace `w` of it with
//! `README.md` changed and `new.txt` added. Ids differ between runs, so a run of 12 or more hex
//! digits reads `<id>` in the compared text.

use std::path::PathBuf;

use serde_json::Value;

use crate::ws::{Lab, inside, write};

/// The shortest run of hex digits that is read as an id.
const ID_DIGITS: usize = 12;

/// What an id reads as in the compared text.
const ID: &str = "<id>";

/// The keys whose values differ between two runs of one line.
const TIME_KEYS: &[&str] = &["elapsed_ms", "created_at", "at", "secs"];

/// The store, and workspace `w` with one changed and one added file.
struct Fixture {
    /// Held so the store's temp dir outlives the runs.
    _lab: Lab,
    ws: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let lab = Lab::new();
        lab.publish("a.txt", "a\n");
        let ws = lab.workspace("w");
        write(&ws, "README.md", "# acme\nmore\n");
        write(&ws, "new.txt", "new\n");
        Fixture { _lab: lab, ws }
    }

    /// `line` run inside the workspace; its stdout, which must be exit 0's.
    fn text(&self, line: &[&str]) -> String {
        let out = inside(&self.ws, line);
        assert_eq!(out.exit, 0, "{line:?}: {out}");
        out.stdout
    }
}

/// `text` with each run of [`ID_DIGITS`] or more lowercase hex digits read as [`ID`].
fn masked(text: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |out: &mut String, run: &mut String| {
        if run.len() >= ID_DIGITS {
            out.push_str(ID);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run.push(c);
            continue;
        }
        flush(&mut out, &mut run);
        out.push(c);
    }
    flush(&mut out, &mut run);
    out
}

/// The envelope `stdout` holds, its time keys nulled wherever they stand.
fn timeless(stdout: &str) -> Value {
    fn null_times(value: &mut Value) {
        match value {
            Value::Object(members) => {
                for (key, member) in members.iter_mut() {
                    if TIME_KEYS.contains(&key.as_str()) {
                        *member = Value::Null;
                        continue;
                    }
                    null_times(member);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(null_times),
            _ => {}
        }
    }
    let mut envelope: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout is one envelope ({e}): {stdout}"));
    null_times(&mut envelope);
    envelope
}

/// `--human` prints the five operations in git's shape: `ws status` as `git status -sb`, `log` as
/// `git log --oneline`, a diff by what its files carry, `ws commit` as git's summary line.
#[test]
fn human_status_log_diff_commit_are_git_shaped() {
    let fixture = Fixture::new();
    let shaped = |line: &[&str]| masked(&fixture.text(line));

    // 1. The workspace and its changes, one letter a change.
    assert_eq!(
        shaped(&["--human", "ws", "status"]),
        "## w <id>\nM  README.md\nA  new.txt\n"
    );

    // 2. `main`'s changesets, newest first.
    assert_eq!(
        shaped(&["--human", "log", "-n", "2"]),
        format!("{ID} a.txt\n{ID} first\n")
    );

    // 3. A workspace diff: names, numstat, patch.
    assert_eq!(
        shaped(&["--human", "ws", "diff"]),
        "M\tREADME.md\nA\tnew.txt\n"
    );
    assert_eq!(
        shaped(&["--human", "ws", "diff", "--format", "stat"]),
        "1\t0\tREADME.md\n1\t0\tnew.txt\n"
    );
    assert_eq!(
        shaped(&["--human", "ws", "diff", "--format", "unified", "README.md"]),
        "--- a/README.md\n+++ b/README.md\n@@ -1,1 +1,2 @@\n # acme\n+more\n"
    );

    // 4. A store diff between two revisions.
    assert_eq!(shaped(&["--human", "diff", "main~1", "main"]), "A\ta.txt\n");

    // 5. The commit, and the status it leaves.
    assert_eq!(
        shaped(&["--human", "ws", "commit", "-m", "two files"]),
        format!("[{ID}] 2 files changed\n")
    );
    assert_eq!(
        shaped(&["--human", "ws", "status"]),
        "## w <id>\nnothing to commit, workspace clean\n"
    );
}

/// git's own shape flags ask for the text with no `--human`: git's shape, its first line given,
/// and what the target prints with `--human`.
#[test]
fn a_shape_flag_asks_for_text() {
    let fixture = Fixture::new();
    let status: &[&str] = &["--human", "ws", "status"];
    let stat: &[&str] = &["--human", "ws", "diff", "--format", "stat"];
    let cases: &[(&[&str], &str, &[&str])] = &[
        (&["status", "-s"], "## w <id>", status),
        (&["status", "-s", "-b"], "## w <id>", status),
        (&["log", "--oneline"], "<id> a.txt", &["--human", "log"]),
        (
            &["log", "--oneline", "--human"],
            "<id> a.txt",
            &["--human", "log"],
        ),
        (&["diff", "--stat"], "1\t0\tREADME.md", stat),
    ];
    for (line, first, target) in cases {
        let text = masked(&fixture.text(line));
        assert_eq!(text.lines().next(), Some(*first), "{line:?}: {text}");
        assert_eq!(text, masked(&fixture.text(target)), "{line:?}");
    }
}

/// `--json` prints the envelope the line printed before: the alias's equals its target's, and
/// each equals the line run with no format flag, time keys aside.
#[test]
fn json_is_byte_for_byte_what_it_was() {
    let fixture = Fixture::new();
    let cases: &[(&[&str], &[&str])] = &[
        (&["status", "-s", "--json"], &["ws", "status"]),
        (&["log", "--oneline", "--json"], &["log"]),
        (
            &["diff", "--stat", "--json"],
            &["ws", "diff", "--format", "stat"],
        ),
        (&["--json", "ws", "status"], &["ws", "status"]),
        (&["--json", "log", "-n", "2"], &["log", "-n", "2"]),
        (
            &["--json", "diff", "main~1", "main"],
            &["diff", "main~1", "main"],
        ),
        (&["--json", "ws", "diff"], &["ws", "diff"]),
    ];
    for (line, before) in cases {
        let envelope = timeless(&fixture.text(line));
        assert_eq!(envelope["ok"], true, "{line:?}: {envelope}");
        assert_eq!(envelope, timeless(&fixture.text(before)), "{line:?}");
    }
}

/// The masking itself: an id of 12 or more hex digits goes, a shorter run and a word stay.
#[test]
fn the_mask_takes_ids_only() {
    let id = "0123456789abcdef";
    let text = format!("## w {id}\n[{}] a.txt added 1f\n", &id[..12]);
    assert_eq!(masked(&text), "## w <id>\n[<id>] a.txt added 1f\n");
    assert_eq!(masked("README.md"), "README.md");
}
