//! The help screens the CLI lays out itself, through the binaries: the program's own, with its
//! commands in groups. A snapshot pins the screen as a pipe reads it, plain text; the examples
//! it shows are read back out of it and run, in order, against a store of the test's own.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::support::{self, BIN, SO};

/// The store's directory and the caller, as the screen's own first example line sets them.
const SETTING: &str = r#"  $ export SWARMOPS_REPO="$PWD/acme" SWARMOPS_AGENT=person/ada"#;

/// The change list the examples' commit reads.
const CHANGES: &str = r#"[{"path": "README.md", "op": "put", "content": {"inline": "helo\n"}}]"#;

/// What `bin` prints for `words`, a line that asks for help: exit 0, nothing on stderr.
fn help(bin: &str, words: &[&str]) -> String {
    let run = support::run_bin(bin, words, &[], "");
    assert_eq!((run.exit(), run.stderr.as_str()), (0, ""), "{run}");
    run.stdout
}

/// The lines of `screen` under the heading that starts with `title`, to the next blank line.
fn section<'a>(screen: &'a str, title: &str) -> Vec<&'a str> {
    let mut lines = screen.lines().skip_while(|line| !line.starts_with(title));
    assert!(lines.next().is_some(), "no {title} section: {screen}");
    lines.take_while(|line| !line.is_empty()).collect()
}

/// `line`, an example as a screen shows it, as the words after the program's name and the file
/// its stdin reads: split at spaces, a double-quoted word kept whole.
fn words(line: &str, bin: &str) -> (Vec<String>, Option<String>) {
    let start = format!("  $ {bin} ");
    let rest = line
        .strip_prefix(&start)
        .unwrap_or_else(|| panic!("an example of {bin}: {line:?}"));
    let (rest, stdin) = match rest.split_once(" < ") {
        Some((rest, file)) => (rest, Some(file.to_owned())),
        None => (rest, None),
    };
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in rest.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => words.push(std::mem::take(&mut word)),
            _ => word.push(c),
        }
    }
    assert!(!quoted, "an open quote: {line:?}");
    words.push(word);
    (words, stdin)
}

/// A directory to run examples in: what they name is made here.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    /// An empty directory holding the change list the examples commit.
    fn new() -> Scratch {
        let dir = tempfile::tempdir().expect("a temp dir");
        fs::write(dir.path().join("changes.json"), CHANGES).expect("write the change list");
        Scratch { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs `bin` here with `words`, the store and the caller set as [`SETTING`] sets them, and
    /// `stdin` read from that file of this directory; the run's exit code and what it printed.
    fn run(&self, bin: &str, words: &[String], stdin: Option<&str>) -> (i32, String, String) {
        let store = self.path().join("acme");
        let env = [
            ("SWARMOPS_REPO", store.to_str().expect("a UTF-8 temp dir")),
            ("SWARMOPS_AGENT", "person/ada"),
        ];
        let mut command = Command::new(bin);
        for name in support::swarmops_vars() {
            command.env_remove(name);
        }
        support::isolate(&mut command);
        let input = match stdin {
            Some(file) => {
                let file = fs::File::open(self.path().join(file));
                Stdio::from(file.expect("the file an example reads"))
            }
            None => Stdio::null(),
        };
        command
            .args(words)
            .envs(env)
            .current_dir(self.path())
            .stdin(input);
        let done = command.output().expect("run swarmops");
        (
            done.status.code().expect("swarmops exited"),
            String::from_utf8_lossy(&done.stdout).into_owned(),
            String::from_utf8_lossy(&done.stderr).into_owned(),
        )
    }

    /// Runs the example `line` of `bin`'s screen, which must succeed.
    fn example(&self, bin: &str, name: &str, line: &str) -> String {
        let (words, stdin) = words(line, name);
        let (exit, stdout, stderr) = self.run(bin, &words, stdin.as_deref());
        assert_eq!(exit, 0, "{line}\nstdout: {stdout}\nstderr: {stderr}");
        stdout
    }
}

/// The program's screen, as a pipe reads it: plain text, the same for `--help`, `-h` and the
/// `help` subcommand, and by either name but for the name.
#[test]
fn top_level_help() {
    let screen = help(SO, &["--help"]);
    assert_eq!(help(SO, &["-h"]), screen);
    assert_eq!(help(SO, &["help"]), screen);
    assert!(!screen.contains('\x1b'), "plain text to a pipe: {screen:?}");
    assert!(serde_json::from_str::<Value>(&screen).is_err(), "{screen}");

    // By its long name: the usage line, the examples and the first line of Learn more name it.
    let long = help(BIN, &["--help"]);
    let renamed = screen
        .replace("Usage: so ", "Usage: swarmops ")
        .replace("  $ so ", "  $ swarmops ")
        .replace(
            "  so <command> --help   A",
            "  swarmops <command> --help  A",
        )
        .replace(
            "\n  Quick start           ",
            "\n  Quick start                ",
        )
        .replace(
            "\n  Every command         ",
            "\n  Every command              ",
        )
        .replace(
            "\n  Envelope, exit codes  ",
            "\n  Envelope, exit codes       ",
        )
        .replace(
            "\n  Error codes           ",
            "\n  Error codes                ",
        );
    assert_eq!(long, renamed);

    insta::assert_snapshot!(screen);
}

/// The screen's examples run as written, one after another, by either name: a store, a first
/// commit, a workspace, an edit there committed and landed, and the log that shows both.
#[test]
fn top_level_examples_run() {
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        let screen = help(bin, &["--help"]);
        let lines = section(&screen, "Examples");
        assert_eq!(lines[0], SETTING, "{screen}");
        let lines = &lines[1..];
        assert!((4..=6).contains(&lines.len()), "four to six: {lines:?}");

        let scratch = Scratch::new();
        let mut log = String::new();
        for line in lines {
            // The change the workspace's commit records: the one thing no example line does.
            if line.contains(" ws commit ") {
                let file = scratch.path().join("fix-1/README.md");
                fs::write(&file, "hello\n").unwrap_or_else(|e| panic!("edit {file:?}: {e}"));
            }
            log = scratch.example(bin, name, line);
        }
        let subjects: Vec<&str> = log
            .lines()
            .filter_map(|line| line.split_once(' ').map(|(_, subject)| subject))
            .collect();
        assert_eq!(subjects, ["Fix the typo", "first"], "{log}");
    }
}
