//! The command line (operations spec §1): global flags, one subcommand per operation, and the
//! operation's name each subcommand stands for in the envelope.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{ArgGroup, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use swarmops_api::{DEFAULT_LOG_LIMIT, DEFAULT_LS_LIMIT, DEFAULT_MAX_BYTES, DEFAULT_MAX_CHANGES};

/// The envelope's `op` for a command line that names no operation.
pub const NO_OP: &str = "swarmops";

/// Source control for coding-agent swarms. Every command prints one JSON envelope on stdout.
#[derive(Parser, Debug)]
#[command(name = "swarmops", version)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

/// The flags every command takes, before or after its subcommand.
#[derive(Args, Debug, Default, Clone)]
pub struct Global {
    /// The store to use (default: $SWARMOPS_REPO).
    #[arg(long, global = true, value_name = "STORE")]
    pub repo: Option<String>,
    // Help given as `help` rather than a doc comment: rustdoc would take `<family>` for HTML.
    #[arg(
        long = "as",
        global = true,
        value_name = "AGENT",
        help = "The agent to act as, <family>/<instance> (default: $SWARMOPS_AGENT)"
    )]
    pub agent: Option<String>,
    /// Print the JSON envelope: the default, unless $SWARMOPS_FORMAT is human.
    #[arg(long, global = true, overrides_with = "human")]
    pub json: bool,
    /// Print a short text rendering instead of JSON.
    #[arg(long, global = true, overrides_with = "json")]
    pub human: bool,
    /// Pretty-print the JSON (default: when stdout is a terminal).
    #[arg(
        long,
        global = true,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    pub pretty: Option<bool>,
}

/// One operation.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a store in DIR, and describe it.
    Init {
        /// The directory to create the store in.
        dir: String,
        /// The repository's name (default: the directory's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Describe the store.
    Info,
    /// Show the identity this caller's changes carry.
    Whoami,
    /// Print the JSON Schema of every request and response type, or of TYPE's.
    Schema {
        /// One type's name, such as CommitReq.
        #[arg(value_name = "TYPE")]
        name: Option<String>,
    },
    /// Record a changeset from a list of changes; it moves no ref.
    Commit {
        /// The commit message.
        #[arg(short, long)]
        message: String,
        /// A JSON file holding an array of changes, or - for stdin.
        #[arg(long, value_name = "FILE")]
        changes: String,
        /// The changeset to commit on top of (default: main's head, if main exists).
        #[arg(long, value_name = "REV")]
        parent: Option<String>,
        /// Record the changeset even if it changes nothing.
        #[arg(long)]
        allow_empty: bool,
    },
    /// Print one file at a revision.
    Cat {
        /// The revision and the file's path, split at the first ':'.
        #[arg(value_name = "REV:PATH")]
        target: String,
        /// The most content bytes to print.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_BYTES)]
        max_bytes: u64,
    },
    /// List one directory at a revision (the root without a PATH).
    Ls {
        /// The revision, and the directory's path after the first ':'.
        #[arg(value_name = "REV[:PATH]")]
        target: String,
        /// The most entries to list.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_LS_LIMIT)]
        limit: usize,
        /// Resume after this entry name: the previous page's cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Show one changeset.
    Show {
        /// The changeset.
        rev: String,
        /// The most changes to list.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_CHANGES)]
        max_changes: usize,
    },
    /// List history along first parents, newest first.
    Log {
        /// Where to start (default: main).
        rev: Option<String>,
        /// The most changesets to list.
        #[arg(short = 'n', long, value_name = "N", default_value_t = DEFAULT_LOG_LIMIT)]
        limit: usize,
        /// Resume after this changeset: the previous page's cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// List refs, or move one by compare-and-swap.
    #[command(subcommand)]
    Ref(RefCommand),
    /// Check the store's consistency.
    Verify {
        /// Re-hash every object too.
        #[arg(long)]
        deep: bool,
    },
}

/// `ref`'s operations.
#[derive(Subcommand, Debug)]
pub enum RefCommand {
    /// List every ref.
    List,
    /// Point ref NAME at REV: move it from --expect, or --create it.
    #[command(group(ArgGroup::new("from").required(true).args(["expect", "create"])))]
    Set {
        #[arg(help = "main or release/<name>")]
        name: String,
        /// The revision to point it at.
        rev: String,
        /// The changeset the ref holds now. Give its id: a ref name is read when the command
        /// runs, so it guards against no move in between, and a replay cannot read it once
        /// that ref is gone.
        #[arg(long, value_name = "REV")]
        expect: Option<String>,
        /// Create the ref; it must not exist.
        #[arg(long)]
        create: bool,
    },
    /// Delete ref NAME.
    Delete {
        /// The ref.
        name: String,
        /// The changeset the ref holds now. Give its id: a ref name is read when the command
        /// runs, so it guards against no move in between, and a replay cannot read it once
        /// that ref is gone.
        #[arg(long, value_name = "REV")]
        expect: String,
    },
}

/// What a command line asks for.
#[derive(Debug)]
pub enum Parsed {
    /// A command to run, and the operation it names.
    Run(Cli, &'static str),
    /// `--help` or `--version`: clap's text to print, which is not an envelope.
    Text(clap::Error),
    /// A usage error, with as much of the line as could be read despite it.
    Usage {
        /// clap's error.
        error: clap::Error,
        /// The subcommands named, outermost first, as far as they could be read.
        path: Vec<String>,
        /// The global flags, as far as they could be read.
        global: Global,
    },
}

/// Reads the command line `args`, the program's name first.
pub fn parse(args: &[OsString]) -> Parsed {
    let parsed = command()
        .try_get_matches_from(args)
        .and_then(|matches| Ok((Cli::from_arg_matches(&matches)?, path(&matches))));
    match parsed {
        Ok((mut cli, path)) => {
            settle_format(&mut cli.global, args);
            // Every leaf names an operation (a unit test checks), so NO_OP is never used here.
            Parsed::Run(cli, op_name(&path).unwrap_or(NO_OP))
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Parsed::Text(error)
        }
        Err(error) => {
            // Read the line again, ignoring errors, for what it names despite the one found.
            let partial = command().ignore_errors(true).try_get_matches_from(args);
            let (path, mut global) = partial.map_or_else(
                |_| (Vec::new(), Global::default()),
                |matches| {
                    let global = Global::from_arg_matches(innermost(&matches)).unwrap_or_default();
                    (path(&matches), global)
                },
            );
            settle_format(&mut global, args);
            Parsed::Usage {
                error,
                path,
                global,
            }
        }
    }
}

/// The operation the subcommand path `path` names, as the envelope's `op` gives it: the
/// operations spec's library name (`cat` runs `read_file`), so that a local and a remote
/// store name it alike. `None` for a path that names no operation.
pub fn op_name<S: AsRef<str>>(path: &[S]) -> Option<&'static str> {
    let path: Vec<&str> = path.iter().map(AsRef::as_ref).collect();
    Some(match path[..] {
        ["init"] => "repo_init",
        ["info"] => "repo_info",
        ["whoami"] => "whoami",
        ["schema"] => "schema",
        ["commit"] => "commit",
        ["cat"] => "read_file",
        ["ls"] => "read_tree",
        ["show"] => "show",
        ["log"] => "log",
        ["ref", "list"] => "ref_list",
        ["ref", "set"] => "ref_set",
        ["ref", "delete"] => "ref_delete",
        ["verify"] => "verify",
        _ => return None,
    })
}

/// The usage line of the subcommand `path` names, or of the deepest part of it that names
/// one: `Usage: swarmops <path> …`.
pub fn usage(path: &[String]) -> String {
    let mut at = command();
    at.build();
    for name in path {
        match at.find_subcommand(name) {
            Some(sub) => at = sub.clone(),
            None => break,
        }
    }
    at.render_usage().to_string()
}

/// The grammar of [`Cli`], with a missing subcommand a usage error rather than a help screen,
/// so that it too gets an envelope.
fn command() -> clap::Command {
    Cli::command()
        .arg_required_else_help(false)
        .mut_subcommand("ref", |cmd| cmd.arg_required_else_help(false))
}

/// Of `--json` and `--human`, keeps the one given last: clap's `overrides_with` settles them
/// within one level of the line (before the subcommand, or after it), not across levels.
/// Before a `--`, each such word is the flag itself, since no option takes a value that
/// starts with `--`.
fn settle_format(global: &mut Global, args: &[OsString]) {
    if global.json && global.human {
        let mut human = false;
        for arg in args.iter().skip(1) {
            match arg.to_str() {
                Some("--") => break,
                Some("--human") => human = true,
                Some("--json") => human = false,
                _ => {}
            }
        }
        global.human = human;
        global.json = !human;
    }
}

/// The names of the subcommands `matches` holds, outermost first.
fn path(matches: &ArgMatches) -> Vec<String> {
    let mut path = Vec::new();
    let mut at = matches;
    while let Some((name, sub)) = at.subcommand() {
        path.push(name.to_owned());
        at = sub;
    }
    path
}

/// The matches of the innermost subcommand, which hold the global flags given anywhere.
fn innermost(matches: &ArgMatches) -> &ArgMatches {
    let mut at = matches;
    while let Some((_, sub)) = at.subcommand() {
        at = sub;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Vec<OsString> {
        std::iter::once("swarmops")
            .chain(line.iter().copied())
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn the_grammar_is_consistent() {
        command().debug_assert();
    }

    /// Every subcommand without subcommands of its own names an operation, and no two name
    /// the same one.
    #[test]
    fn every_leaf_names_one_operation() {
        fn leaves(cmd: &clap::Command, prefix: &[String], out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands().filter(|sub| sub.get_name() != "help") {
                let mut path = prefix.to_vec();
                path.push(sub.get_name().to_owned());
                if sub.has_subcommands() {
                    leaves(sub, &path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut all = Vec::new();
        leaves(&command(), &[], &mut all);
        let mut ops: Vec<&str> = all
            .iter()
            .map(|path| op_name(path).unwrap_or_else(|| panic!("{path:?} names no op")))
            .collect();
        assert_eq!(ops.len(), 13);
        ops.sort_unstable();
        ops.dedup();
        assert_eq!(ops.len(), 13, "two subcommands name one op");
        assert_eq!(op_name(&["ref"]), None);
    }

    #[test]
    fn a_command_line_parses_to_its_operation() {
        let Parsed::Run(cli, op) = parse(&args(&["ref", "set", "main", "abc", "--create"])) else {
            panic!("a valid line");
        };
        assert_eq!(op, "ref_set");
        assert!(matches!(
            cli.command,
            Command::Ref(RefCommand::Set { create: true, .. })
        ));
        // Global flags go before or after the subcommand; --pretty takes its value after `=`.
        let Parsed::Run(cli, _) = parse(&args(&["log", "--repo", "s", "--as", "a/b", "--pretty"]))
        else {
            panic!("a valid line");
        };
        assert_eq!(cli.global.repo.as_deref(), Some("s"));
        assert_eq!(cli.global.agent.as_deref(), Some("a/b"));
        assert_eq!(cli.global.pretty, Some(true));
        // Of --json and --human, the later wins, before the subcommand or after it.
        for (line, human) in [
            (&["--json", "--human", "info"][..], true),
            (&["--human", "--json", "info"], false),
            (&["--json", "--human", "info", "--json"], false),
            (&["--human", "info", "--json"], false),
            (&["--json", "info", "--human"], true),
            (&["info", "--human", "--json"], false),
        ] {
            let Parsed::Run(cli, _) = parse(&args(line)) else {
                panic!("a valid line");
            };
            assert_eq!(
                (cli.global.human, cli.global.json),
                (human, !human),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_usage_error_keeps_what_the_line_names() {
        let Parsed::Usage { path, global, .. } = parse(&args(&["--human", "ref", "set", "main"]))
        else {
            panic!("a usage error");
        };
        assert_eq!(path, ["ref", "set"]);
        assert!(global.human);
        let Parsed::Usage { path, .. } = parse(&args(&["frobnicate"])) else {
            panic!("a usage error");
        };
        assert_eq!(path, Vec::<String>::new());
        for line in [&[][..], &["ref"]] {
            let Parsed::Usage { error, .. } = parse(&args(line)) else {
                panic!("{line:?}: a usage error, not a help screen");
            };
            assert_eq!(error.kind(), ErrorKind::MissingSubcommand);
        }
        assert!(matches!(parse(&args(&["--version"])), Parsed::Text(_)));
        assert!(matches!(parse(&args(&["help", "log"])), Parsed::Text(_)));
    }

    #[test]
    fn usage_lines_name_the_subcommand() {
        assert_eq!(
            usage(&["log".to_owned()]),
            "Usage: swarmops log [OPTIONS] [REV]"
        );
        assert!(usage(&[]).starts_with("Usage: swarmops [OPTIONS] <COMMAND>"));
        let set = usage(&["ref".to_owned(), "set".to_owned(), "x".to_owned()]);
        assert!(set.starts_with("Usage: swarmops ref set "), "{set}");
    }
}
