//! git's text shapes through the binary (the git-familiar command design, §3 "Text shapes"):
//! `--human` prints `ws status`, `log`, `diff`, `ws diff` and `ws commit` as git prints them, a
//! git shape flag asks for that text with no `--human`, and `--json` prints the envelope it
//! printed before.
//!
//! The fixture is a store whose `main` holds two changesets, workspace `w` of it with
//! `README.md` changed and `new.txt` added. Ids differ between runs, so a run of 12 or more hex
//! digits reads `<id>` in the compared text. `clone` reads a repository the test kit's
//! [`GitServer`] serves on `127.0.0.1`.

use std::path::{Path, PathBuf};

use serde_json::Value;
use swarmops_testkit::remote::{GitServer, HttpAuth};

use crate::support::{BIN, SO, run_bin};
use crate::ws::{AGENT, Lab, inside, write};

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

/// `--json` prints the envelope the line prints with no format flag: the alias's equals its
/// target's, each parsed and its time keys nulled. One build's two runs compared, no recorded
/// envelope.
#[test]
fn json_is_the_envelope_with_no_flag() {
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

/// A diff with no file prints nothing in `--human`, in each format, as git prints nothing.
#[test]
fn an_empty_diff_prints_nothing() {
    let fixture = Fixture::new();
    fixture.text(&["ws", "commit", "-m", "two files"]);
    for format in ["names", "stat", "unified"] {
        let line = ["--human", "ws", "diff", "--format", format];
        assert_eq!(fixture.text(&line), "", "{line:?}");
    }
    assert_eq!(fixture.text(&["--human", "diff", "main", "main"]), "");
}

/// `clone` in `--human` through either binary: the URL, the changesets it imported and `main`,
/// then the next step named with the program run.
#[test]
fn clone_text_names_the_program_run() {
    // 1. A served repository with one commit, and the git environment of the runs.
    let server = GitServer::new();
    server.create(REPO);
    server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
    let http = server.http(HttpAuth::Anonymous);
    let home = tempfile::tempdir().expect("a temp HOME");
    let git = path_text(&real_git(&server));
    let home_path = path_text(home.path());
    let env = [
        ("SWARMOPS_GIT", git.as_str()),
        ("HOME", home_path.as_str()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ];
    let url = http.url(REPO);

    // 2. Each binary clones into a store of its own.
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let into = path_text(&dir.path().join("store"));
        let line = [
            "--as",
            AGENT,
            "--human",
            "clone",
            url.as_str(),
            into.as_str(),
        ];
        let cloned = run_bin(bin, &line, &env, "");
        assert_eq!(cloned.exit(), 0, "{cloned}");
        assert_eq!(
            masked(&cloned.stdout),
            format!(
                "Cloned {url} into a store: 1 changeset, main at {ID}\n\
                 No working tree yet: {name} ws create NAME\n"
            ),
            "{name}"
        );
    }
}

/// The repository [`clone_text_names_the_program_run`] clones.
const REPO: &str = "repo";

/// The real git the server runs, as an absolute path.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// `path` as text.
fn path_text(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// The masking itself: an id of 12 or more hex digits goes, a shorter run and a word stay.
#[test]
fn the_mask_takes_ids_only() {
    let id = "0123456789abcdef";
    let text = format!("## w {id}\n[{}] a.txt added 1f\n", &id[..12]);
    assert_eq!(masked(&text), "## w <id>\n[<id>] a.txt added 1f\n");
    assert_eq!(masked("README.md"), "README.md");
}
