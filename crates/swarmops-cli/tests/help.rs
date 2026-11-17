//! The help screens the CLI lays out itself, through the binaries: the program's own, with its
//! commands in groups, and each command family's. A snapshot pins each screen as a pipe reads
//! it, plain text; the examples a screen shows are read back out of it and run by `sh`, every
//! line as printed and in order, in a directory of the test's own.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use crate::support::{self, BIN, SO};

/// The store's directory and the caller, as the screen's own first example line sets them.
const SETTING: &str = r#"  $ export SWARMOPS_REPO="$PWD/acme" SWARMOPS_AGENT=person/ada"#;

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

/// What `sh` printed for one line of a script, and the line's exit code.
struct Ran {
    line: String,
    exit: String,
    stdout: String,
}

/// A directory to run examples in, as a reader's shell would: `so` and `swarmops` are found on
/// `PATH`, and what the lines name they make here, or [`Scratch::with_a_store`] does.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    /// An empty working directory, beside a directory that holds the two programs.
    fn new() -> Scratch {
        let dir = tempfile::tempdir().expect("a temp dir");
        let scratch = Scratch { dir };
        fs::create_dir(scratch.path()).expect("make the working directory");
        fs::create_dir(scratch.programs()).expect("make the programs' directory");
        for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
            std::os::unix::fs::symlink(bin, scratch.programs().join(name))
                .unwrap_or_else(|e| panic!("link {name}: {e}"));
        }
        scratch
    }

    /// The directory the lines run in.
    fn path(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    /// The directory `PATH` starts with: the two programs, by their names.
    fn programs(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// A store with a root commit on main, as the program's examples make it, the variables
    /// that name it, and the files of the reader's own the families' examples name: one to
    /// store, and one holding a token.
    fn with_a_store(top: &[&str]) -> (Scratch, String) {
        let scratch = Scratch::new();
        let made: Vec<&str> = top.iter().copied().take(3).collect();
        assert!(made[2].contains(" commit "), "the root commit: {made:?}");
        scratch.examples(&made);
        let here = scratch.path();
        fs::write(here.join("logo.png"), b"\x89PNG\r\n").expect("write a file to store");
        fs::write(
            here.join("token.txt"),
            "ghp_0123456789abcdefghijklmnopqrstuvwxyz\n",
        )
        .expect("write a token");
        (scratch, made[0].to_owned())
    }

    /// Runs `lines`, examples as a screen shows them (`  $ ...`), as one script of `sh`, each
    /// line as printed: with the two programs first on `PATH`, none of this process's
    /// `SWARMOPS_*` variables, a login file of this directory's own and no curl. What each
    /// line printed, in order.
    fn sh(&self, lines: &[&str]) -> Vec<Ran> {
        const MARK: &str = "#--- exit ";
        let mut script = String::new();
        for line in lines {
            let line = line
                .strip_prefix("  $ ")
                .unwrap_or_else(|| panic!("an example: {line:?}"));
            let _ = writeln!(script, "{line}\nprintf '\\n{MARK}%s\\n' $?");
        }

        let mut path = self.programs().into_os_string();
        if let Some(rest) = std::env::var_os("PATH") {
            path.push(":");
            path.push(rest);
        }
        let mut command = Command::new("sh");
        for name in support::swarmops_vars() {
            command.env_remove(name);
        }
        support::isolate(&mut command);
        command
            .args(["-c", &script])
            .env("PATH", path)
            .env("SWARMOPS_AUTH_FILE", self.dir.path().join("auth.toml"))
            .current_dir(self.path())
            .stdin(std::process::Stdio::null());
        let done = command.output().expect("run sh");
        let stdout = String::from_utf8_lossy(&done.stdout);
        let stderr = String::from_utf8_lossy(&done.stderr);

        let mark = format!("\n{MARK}");
        let mut parts = stdout.split(&mark);
        let mut printed = parts.next().unwrap_or_default().to_owned();
        let mut ran = Vec::new();
        for (line, part) in lines.iter().zip(parts) {
            let (exit, rest) = part.split_once('\n').unwrap_or((part, ""));
            ran.push(Ran {
                line: (*line).to_owned(),
                exit: exit.to_owned(),
                stdout: std::mem::replace(&mut printed, rest.to_owned()),
            });
        }
        assert_eq!(ran.len(), lines.len(), "{script}\n{stdout}\n{stderr}");
        ran
    }

    /// Runs `lines` ([`Scratch::sh`]), each of which must succeed; what the last one printed.
    fn examples(&self, lines: &[&str]) -> String {
        let ran = self.sh(lines);
        for Ran { line, exit, stdout } in &ran {
            assert_eq!(exit, "0", "{line}\nstdout: {stdout}");
        }
        ran.last().map_or_default(|ran| ran.stdout.clone())
    }
}

/// The lines of `bin`'s examples that run the program, which every other line must not name.
fn runs<'a>(lines: &[&'a str], name: &str) -> Vec<&'a str> {
    let program = format!(" {name} ");
    let first = format!("  $ {name} ");
    let piped = format!(" | {name} ");
    let (runs, others): (Vec<&str>, Vec<&str>) = lines
        .iter()
        .partition(|line| line.starts_with(&first) || line.contains(&piped));
    for line in others {
        assert!(!line.contains(&program), "a run of {name}: {line:?}");
    }
    runs
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
        .replace(" | so ", " | swarmops ")
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

/// The screen's examples run as printed, every line of them and nothing else, one after
/// another in an empty directory, by either name: a store, a root commit, a workspace, a file
/// made there, committed and landed, and the log that shows both.
#[test]
fn top_level_examples_run() {
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        let screen = help(bin, &["--help"]);
        let lines = section(&screen, "Examples");
        assert_eq!(lines[0], SETTING, "{screen}");
        let runs = runs(&lines, name);
        assert!((4..=6).contains(&runs.len()), "four to six: {runs:?}");

        let log = Scratch::new().examples(&lines);
        let subjects: Vec<&str> = log
            .lines()
            .filter_map(|line| line.split_once(' ').map(|(_, subject)| subject))
            .collect();
        assert_eq!(subjects, ["Add a readme", "root"], "{log}");
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

/// Every family's examples run as printed on a store with a root commit, by either name, but
/// for the one that asks a remote.
#[test]
fn family_examples_run() {
    let mut asked = 0;
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        let top = help(bin, &["--help"]);
        let (scratch, setting) = Scratch::with_a_store(&section(&top, "Examples"));
        for family in FAMILIES {
            let screen = help(bin, &[family, "--help"]);
            let lines = section(&screen, "Examples");
            let runs = runs(&lines, name);
            assert!(
                (1..=2).contains(&runs.len()),
                "{family}: one or two: {runs:?}"
            );
            for line in &runs {
                assert!(line.contains(&format!(" {family} ")), "{family}: {line}");
            }

            // In a shell that has the variables the heading names.
            let mut script = vec![setting.as_str()];
            for line in lines {
                if line.replace("$ swarmops ", "$ so ") == ASKS_A_REMOTE {
                    asked += 1;
                } else {
                    script.push(line);
                }
            }
            scratch.examples(&script);
        }

        // What they left: the workspace, the ref, the blob, the branch in git and the login.
        let left = |words: &str| scratch.examples(&[&setting, &format!("  $ {name} {words}")]);
        assert!(left("--human ws list").contains(".name: fix-1\n"));
        let refs = left("--human ref list");
        for name in ["from-git", "main", "release/1.0"] {
            assert!(refs.contains(&format!(".name: {name}\n")), "{name}: {refs}");
        }
        assert!(left("--human auth status").contains(".host: github.com\n"));
        assert!(scratch.path().join("app.git/refs/heads/main").is_file());
    }
    assert_eq!(asked, 2, "the example that asks a remote, by either name");
}

/// Off a terminal, a line that names no command prints what it printed before there were help
/// screens to print: its usage error's envelope on stdout (with `SWARMOPS_FORMAT=human` or
/// `--human`, that error as text), exit 2, and no help. The tests' stdout is a pipe; the
/// terminal's side of the rule is `entry`'s and `help`'s unit tests, and a run by hand.
#[test]
fn a_bare_line_keeps_its_envelope_off_a_terminal() {
    // `remote` alone is no bare line: it lists the remotes, as `git remote` does.
    let bare = ["", "ws", "blob", "ref", "op", "git", "auth"];
    for (bin, name) in [(SO, "so"), (BIN, "swarmops")] {
        for family in bare {
            let words: Vec<&str> = family.split_whitespace().collect();
            let named = format!("{name} {family}");
            let missing = format!("'{}' requires a subcommand", named.trim_end());

            // 1. The envelope, by default and when it is asked for.
            let json: Vec<&str> = words.iter().copied().chain(["--json"]).collect();
            for (line, env) in [
                (&words, &[][..]),
                (&words, &[("SWARMOPS_FORMAT", "json")]),
                (&json, &[]),
                (&json, &[("SWARMOPS_FORMAT", "human")]),
            ] {
                let run = support::run_bin(bin, line, env, "");
                assert_eq!((run.exit(), run.stderr.as_str()), (2, ""), "{run}");
                let error = run.err("E_INVALID_ARG");
                let message = error["message"].as_str().expect("a message");
                assert!(message.starts_with(&missing), "{run}");
                let usage = format!("Usage: {} [OPTIONS] <COMMAND>", named.trim_end());
                assert!(
                    error["hint"].as_str().expect("a hint").starts_with(&usage),
                    "{run}"
                );
            }

            // 2. The same error as text, where text is asked for: never the help.
            let human: Vec<&str> = words.iter().copied().chain(["--human"]).collect();
            for (line, env) in [
                (&words, &[("SWARMOPS_FORMAT", "human")][..]),
                (&human, &[]),
                (&human, &[("SWARMOPS_FORMAT", "json")]),
            ] {
                let run = support::run_bin(bin, line, env, "");
                assert_eq!((run.exit(), run.stderr.as_str()), (2, ""), "{run}");
                let start = format!("error: E_INVALID_ARG: {missing}");
                assert!(run.stdout.starts_with(&start), "{run}");
                assert!(!run.stdout.contains("Learn more"), "{run}");
            }
        }
    }
}
