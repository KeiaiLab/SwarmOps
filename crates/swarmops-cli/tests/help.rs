//! The help screens the CLI lays out itself, through the binaries: the program's own, with its
//! commands in groups, and each command family's. A snapshot pins each screen as a pipe reads
//! it, plain text; the examples a screen shows are read back out of it and run, in order,
//! against a store of the test's own.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::support::{self, BIN, SO};

/// The store's directory and the caller, as the screen's own first example line sets them.
const SETTING: &str = r#"  $ export SWARMOPS_REPO="$PWD/acme" SWARMOPS_AGENT=person/ada"#;

/// The change list the examples' commit reads.
const CHANGES: &str = r#"[{"path": "README.md", "op": "put", "content": {"inline": "helo\n"}}]"#;

/// The command families, in the order the program's screen lists them.
const FAMILIES: [&str; 7] = ["ws", "blob", "ref", "op", "git", "remote", "auth"];

/// The one example no test runs: it asks the remote it names, so it is only read
/// (`the_examples_are_lines_the_grammar_reads`).
const ASKS_A_REMOTE: &str = "  $ so remote add origin https://github.com/OWNER/REPO.git";

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

    /// A store with a first commit on main, as the program's first two examples leave it, and
    /// what the families' examples name: a file to store, an empty bare git repository, and a
    /// file holding a token.
    fn with_a_store() -> Scratch {
        let scratch = Scratch::new();
        for line in [
            "init acme",
            "commit -m first --changes changes.json --advance main",
        ] {
            scratch.example(SO, "so", &format!("  $ so {line}"));
        }
        let here = scratch.path();
        fs::write(here.join("logo.png"), b"\x89PNG\r\n").expect("write a file to store");
        fs::write(
            here.join("token.txt"),
            "ghp_0123456789abcdefghijklmnopqrstuvwxyz\n",
        )
        .expect("write a token");
        let empty = swarmops_testkit::git::empty_repository();
        copy_dir(&empty.dir, &here.join("app.git"));
        scratch
    }

    /// Runs `bin` here with `words`, the store and the caller set as [`SETTING`] sets them, a
    /// login file of this directory's own, and `stdin` read from that file of this directory;
    /// the run's exit code and what it printed.
    fn run(&self, bin: &str, words: &[String], stdin: Option<&str>) -> (i32, String, String) {
        let store = self.path().join("acme");
        let logins = self.path().join("auth.toml");
        let env = [
            ("SWARMOPS_REPO", store.to_str().expect("a UTF-8 temp dir")),
            ("SWARMOPS_AGENT", "person/ada"),
            (
                "SWARMOPS_AUTH_FILE",
                logins.to_str().expect("a UTF-8 temp dir"),
            ),
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

/// Copies the directory `from`, with everything below it, to `to`, which does not exist.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap_or_else(|e| panic!("create {}: {e}", to.display()));
    for entry in fs::read_dir(from).expect("read a directory") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("an entry's type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy a file");
        }
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

/// Each family's screen, as a pipe reads it: the long one (`--help`, which `help <family>`
/// and `<family> help` print too), in the program's shape; the short one (`-h`) differs only
/// where the family's own help has more than its first paragraph, as `ws`'s does; and by its
/// long name the program prints the same but for the name.
#[test]
fn family_help() {
    let mut screens = String::new();
    for family in FAMILIES {
        let long = help(SO, &[family, "--help"]);
        assert_eq!(help(SO, &["help", family]), long, "{family}");
        assert_eq!(help(SO, &[family, "help"]), long, "{family}");
        assert!(!long.contains('\x1b'), "plain text to a pipe: {long:?}");

        let short = help(SO, &[family, "-h"]);
        let from_usage =
            |screen: &str| screen[screen.find("\nUsage: ").expect("usage")..].to_owned();
        assert_eq!(from_usage(&short), from_usage(&long), "{family}");
        assert_eq!(
            short == long,
            family != "ws",
            "{family}: only ws has a long help"
        );

        let renamed = long
            .replace("Usage: so ", "Usage: swarmops ")
            .replace("  $ so ", "  $ swarmops ")
            .replace(
                &format!("  so {family} <command>"),
                &format!("  swarmops {family} <command>"),
            );
        assert_eq!(help(BIN, &[family, "--help"]), renamed, "{family}");

        let _ = writeln!(screens, "$ so {family} --help\n{long}");
    }
    insta::assert_snapshot!(screens);
}

/// Every family's examples run as written on a store with a first commit, by either name, but
/// for the one that asks a remote.
#[test]
fn family_examples_run() {
    let mut asked = 0;
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        let scratch = Scratch::with_a_store();
        for family in FAMILIES {
            let screen = help(bin, &[family, "--help"]);
            let lines = section(&screen, "Examples");
            assert!(
                (1..=2).contains(&lines.len()),
                "{family}: one or two: {lines:?}"
            );
            for line in lines {
                if line.replace("$ swarmops ", "$ so ") == ASKS_A_REMOTE {
                    asked += 1;
                    continue;
                }
                assert!(line.contains(&format!(" {family} ")), "{family}: {line}");
                scratch.example(bin, name, line);
            }
        }

        // What they left: the workspace, the ref, the blob, the branch in git and the login.
        let left = |words: &[&str]| {
            let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            let (exit, stdout, stderr) = scratch.run(bin, &words, None);
            assert_eq!(exit, 0, "{words:?}: {stdout}{stderr}");
            stdout
        };
        assert!(left(&["--human", "ws", "list"]).contains(".name: fix-1\n"));
        let refs = left(&["--human", "ref", "list"]);
        for name in ["from-git", "main", "release/1.0"] {
            assert!(refs.contains(&format!(".name: {name}\n")), "{name}: {refs}");
        }
        assert!(left(&["--human", "auth", "status"]).contains(".host: github.com\n"));
        assert!(scratch.path().join("app.git/refs/heads/main").is_file());
    }
    assert_eq!(asked, 2, "the example that asks a remote, by either name");
}
