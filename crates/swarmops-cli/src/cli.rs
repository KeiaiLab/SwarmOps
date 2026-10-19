//! The command line (operations spec §1): global flags, one subcommand per operation, and the
//! operation's name each subcommand stands for in the envelope.

use std::ffi::OsString;
use std::path::Path;

use clap::error::ErrorKind;
use clap::{ArgGroup, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use swarmops_api::{
    DEFAULT_LOG_LIMIT, DEFAULT_MAX_BYTES, DEFAULT_MAX_CHANGES, DEFAULT_PATH_HISTORY_LIMIT,
    DEFAULT_READ_TREE_LIMIT,
};

use crate::output::printable;

/// The envelope's `op` for a command line that names no operation.
pub const NO_OP: &str = "swarmops";

// The help of `--expect`, as `help` (`-h`) and `long_help` (`--help`) strings rather than doc
// comments, which rustdoc would read `<actual>` in as HTML.

/// `--expect`'s short help.
const EXPECT_HELP: &str = "The changeset the ref holds now: a full id, not a ref name";

/// `ref set --expect`'s long help.
const SET_EXPECT_LONG_HELP: &str = "The changeset the ref holds now.

Give the full changeset id you read (from ref list, show, log or a commit's result). A ref \
name or a relative revision such as main~1 is read from the store when the command runs \
instead of pinning the id you read: the ref's own name always matches (a deleted ref errors \
instead), so the compare-and-swap protects nothing, and main~1 on main never matches. An id \
prefix pins the id you read only until another changeset shares it; then it is E_AMBIGUOUS_ID.

After E_CAS_MISMATCH, re-apply your edits to the content at the error's `actual` and commit \
with --parent <actual>, then retry with --expect <actual>. Do not re-send your old whole-file \
puts: M1 has no merge, so they would revert the winner's edits to shared files.";

/// `ref delete --expect`'s long help.
const DELETE_EXPECT_LONG_HELP: &str = "The changeset the ref holds now.

Give the full changeset id you read (from ref list, show, log or a commit's result). A ref \
name or a relative revision such as main~1 is read from the store when the command runs \
instead of pinning the id you read: the ref's own name always matches (a deleted ref errors \
instead), so the compare-and-swap protects nothing, and main~1 on main never matches. An id \
prefix pins the id you read only until another changeset shares it; then it is E_AMBIGUOUS_ID.

After E_CAS_MISMATCH, the ref holds the error's `actual`, which someone else moved it to: look \
at it (swarmops show <actual>) before you delete it with --expect <actual>.";

/// `commit --advance`'s short help.
const ADVANCE_HELP: &str =
    "Also move REF to the new changeset, in the same transaction, if REF still names --parent";

/// `commit --advance`'s long help.
const ADVANCE_LONG_HELP: &str = "Also move REF (main or release/<name>) to the new changeset, in \
the same transaction: both are recorded, or neither.

The move happens only if REF still names --parent (for a root commit, only if REF does not exist \
yet), so it is always a one-step fast-forward. Give --parent as the full changeset id you read \
REF at (from ref list, show, log or a commit's result): a ref name resolves when the command \
runs, and without --parent the commit builds on REF's head as it is then, so the move catches \
only a race from that moment on.

After E_CAS_MISMATCH nothing was recorded, neither the changeset nor the move: re-apply your \
edits to the content at the error's `actual` and commit with --parent <actual> --advance REF. \
Do not re-send your old whole-file puts: M1 has no merge, so they would revert the winner's \
edits to shared files.";

/// Source control for coding-agent swarms. Every command but mcp prints one JSON envelope
/// on stdout.
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
        /// The repository's name, at most 255 bytes (default: the directory's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Describe the store.
    Info,
    /// Show the identity this caller's changes carry.
    ///
    /// An identified caller gets its agent, its run id when it has one, and auth "trust"; a
    /// caller without an identity gets auth "anonymous" and no agent field at all.
    Whoami,
    /// Print the JSON Schema of every request and response type, or of TYPE's.
    Schema {
        /// One type's name, such as CommitReq.
        #[arg(value_name = "TYPE")]
        name: Option<String>,
    },
    /// Record a changeset from a list of changes; with --advance, also move that ref to it.
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
        #[arg(long, value_name = "REF", help = ADVANCE_HELP, long_help = ADVANCE_LONG_HELP)]
        advance: Option<String>,
    },
    /// Store content as a blob for a commit to name by its id, or ask which blobs the store
    /// holds.
    #[command(subcommand)]
    Blob(BlobCommand),
    /// Print one file at a revision.
    Cat {
        // Split at the first ':' by commands::split_target: M1's revision grammar has no ':'.
        // `ws:<name>` and `cr:CR-n[@k]` arrive later, and the split must become prefix-aware.
        /// The revision and the file's path, split at the first ':'.
        #[arg(value_name = "REV:PATH")]
        target: String,
        /// The most content bytes to print.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_BYTES)]
        max_bytes: u64,
    },
    /// List one directory at a revision (the root without a PATH).
    Ls {
        // Split at the first ':' as `cat`'s target is (see there).
        /// The revision, and the directory's path after the first ':'.
        #[arg(value_name = "REV[:PATH]")]
        target: String,
        /// The most entries to list.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_READ_TREE_LIMIT)]
        limit: usize,
        /// Resume a listing: the previous page's cursor, exactly as it was returned.
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
        /// Resume a log: the previous page's cursor, exactly as it was returned.
        #[arg(long)]
        cursor: Option<String>,
        /// Only the changesets that change PATH, or anything below it; give it again for more.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
    },
    /// List the changesets that changed a path, newest first, along first parents.
    History {
        // Split at the first ':' as `cat`'s target is (see there).
        /// The revision and the path (a file, or a directory), split at the first ':'.
        #[arg(value_name = "REV:PATH")]
        target: String,
        /// The most changesets to list.
        #[arg(short = 'n', long, value_name = "N", default_value_t = DEFAULT_PATH_HISTORY_LIMIT)]
        limit: usize,
        /// Resume a history: the previous page's cursor, exactly as it was returned.
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
    /// Serve the store's operations to an agent as an MCP server on stdin and stdout.
    ///
    /// One tool per operation (repo_info, read_file, read_tree, show, log, path_history, ref_list,
    /// ref_set, ref_delete, commit, blob_put, blob_has, verify), each taking the operation's
    /// request as its arguments and returning the envelope the other commands print. The store and
    /// the identity are the server's, --repo and --as or their variables, never a call's. Speaks
    /// MCP 2026-07-28, and 2025-11-25 and 2025-06-18 to clients that open with initialize. Prints
    /// no envelope: stdout carries the protocol until stdin ends, and a command line it refuses has
    /// its envelope on stderr.
    Mcp,
}

/// `blob`'s operations.
#[derive(Subcommand, Debug)]
pub enum BlobCommand {
    /// Store FILE's content as a blob, outside any commit, and print its id, which a change
    /// names it by: {"blob": ID}.
    Put {
        /// The file whose content to store, or - for stdin.
        #[arg(value_name = "FILE")]
        file: String,
    },
    /// Of the IDs given, list those the store holds blobs under.
    Has {
        /// Blob ids, 64 hex digits each.
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
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
        #[arg(long, value_name = "REV", help = EXPECT_HELP, long_help = SET_EXPECT_LONG_HELP)]
        expect: Option<String>,
        /// Create the ref; if it holds REV already that is a replay (already_applied), at
        /// another target E_CAS_MISMATCH.
        #[arg(long)]
        create: bool,
    },
    /// Delete ref NAME.
    Delete {
        /// The ref.
        name: String,
        #[arg(long, value_name = "REV", help = EXPECT_HELP, long_help = DELETE_EXPECT_LONG_HELP)]
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
        /// clap's error, for the line with each control character in it escaped.
        error: clap::Error,
        /// The name the line invokes the program by, which clap's usage lines give: the file
        /// name of its first word, `so` or `swarmops`; `swarmops` if that has no UTF-8 file name.
        /// `None` leaves clap's own, `swarmops`.
        bin: Option<String>,
        /// The subcommands named, outermost first, as far as they could be read.
        path: Vec<String>,
        /// The global flags, as far as they could be read.
        global: Global,
    },
}

impl Parsed {
    /// Whether the line runs `swarmops mcp`, as far as it could be read: a command to run, or a
    /// usage error in its arguments. Its stdout belongs to the protocol.
    pub fn runs_mcp(&self) -> bool {
        match self {
            Parsed::Run(_, op) => *op == "mcp",
            Parsed::Usage { path, .. } => path.first().is_some_and(|name| name == "mcp"),
            Parsed::Text(_) => false,
        }
    }
}

/// Reads the command line `args`, the program's name first.
pub fn parse(args: &[OsString]) -> Parsed {
    // Parsing gives `grammar` the name the line invokes the program by, for its usage lines: the
    // file name of the first word, if it is UTF-8. Without one, clap's subcommand usage lines and
    // messages name no program (`Usage: ref set …`), so name it `swarmops` here.
    let mut grammar = command();
    if args
        .first()
        .and_then(|first| Path::new(first).file_name()?.to_str())
        .is_none()
    {
        grammar.set_bin_name(grammar.get_name().to_owned());
    }
    let parsed = grammar
        .try_get_matches_from_mut(args)
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
                error: shown(args, error),
                bin: grammar.get_bin_name().map(str::to_owned),
                path,
                global,
            }
        }
    }
}

/// `error`, which refused the line `args`, as it is to be shown. clap echoes the line's words in
/// its text, and a newline in one would split that text where `commands::usage` reads it by its
/// lines and blank lines. So where a word holds a control character, the error of the line with
/// each such word escaped; `error` itself where none does, or where the escaped line is not
/// refused alike.
fn shown(args: &[OsString], error: clap::Error) -> clap::Error {
    let holds_control = |arg: &OsString| arg.to_string_lossy().chars().any(char::is_control);
    if !args.iter().any(holds_control) {
        return error;
    }
    let escaped = args.iter().map(|arg| {
        if holds_control(arg) {
            OsString::from(printable(&arg.to_string_lossy()))
        } else {
            arg.clone()
        }
    });
    match command().try_get_matches_from(escaped) {
        Err(again) if again.kind() == error.kind() => again,
        _ => error,
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
        ["blob", "put"] => "blob_put",
        ["blob", "has"] => "blob_has",
        ["cat"] => "read_file",
        ["ls"] => "read_tree",
        ["show"] => "show",
        ["log"] => "log",
        ["history"] => "path_history",
        ["ref", "list"] => "ref_list",
        ["ref", "set"] => "ref_set",
        ["ref", "delete"] => "ref_delete",
        ["verify"] => "verify",
        ["mcp"] => "mcp",
        _ => return None,
    })
}

/// The usage line of the subcommand `path` names, or of the deepest part of it that names
/// one, with the program named `bin` as clap's own usage lines name it (see [`Parsed::Usage`]):
/// `Usage: so <path> …`.
pub fn usage(bin: Option<&str>, path: &[String]) -> String {
    let mut at = command();
    if let Some(bin) = bin {
        at.set_bin_name(bin);
    }
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
        .mut_subcommand("blob", |cmd| cmd.arg_required_else_help(false))
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
        assert_eq!(ops.len(), 17);
        ops.sort_unstable();
        ops.dedup();
        assert_eq!(ops.len(), 17, "two subcommands name one op");
        assert_eq!(op_name(&["ref"]), None);
        assert_eq!(op_name(&["blob"]), None);
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
        for line in [&[][..], &["ref"], &["blob"]] {
            let Parsed::Usage { error, .. } = parse(&args(line)) else {
                panic!("{line:?}: a usage error, not a help screen");
            };
            assert_eq!(error.kind(), ErrorKind::MissingSubcommand);
        }
        assert!(matches!(parse(&args(&["--version"])), Parsed::Text(_)));
        assert!(matches!(parse(&args(&["help", "log"])), Parsed::Text(_)));
        // The name the program was invoked by: the file name of the line's first word.
        for (first, name) in [
            ("swarmops", "swarmops"),
            ("so", "so"),
            ("/usr/bin/so", "so"),
        ] {
            let mut line = args(&["log", "-n", "many"]);
            line[0] = first.into();
            let Parsed::Usage { bin, .. } = parse(&line) else {
                panic!("{first}: a usage error");
            };
            assert_eq!(bin.as_deref(), Some(name), "{first}");
        }
        // A first word with no UTF-8 file name (empty, the root, or not UTF-8) leaves the program
        // `swarmops`, in the subcommands' usage lines and messages too.
        let firsts = [
            OsString::new(),
            OsString::from("/"),
            #[cfg(unix)]
            std::os::unix::ffi::OsStringExt::from_vec(vec![0xff]),
        ];
        for first in firsts {
            for (rest, shown) in [
                (&["ref", "set"][..], "Usage: swarmops ref set"),
                (&["ref"], "'swarmops ref' requires a subcommand"),
                (&["info", "--frob"], "Usage: swarmops info"),
            ] {
                let mut line = args(rest);
                line[0] = first.clone();
                let Parsed::Usage { error, bin, .. } = parse(&line) else {
                    panic!("{first:?} {rest:?}: a usage error");
                };
                assert_eq!(bin.as_deref(), Some("swarmops"), "{first:?} {rest:?}");
                assert!(
                    error.render().to_string().contains(shown),
                    "{first:?} {rest:?}"
                );
            }
        }
    }

    #[test]
    fn a_line_that_runs_mcp_is_known_as_one() {
        for line in [
            &["mcp"][..],
            &["--repo", "s", "mcp"],
            &["mcp", "--frob"],
            &["mcp", "x"],
        ] {
            assert!(parse(&args(line)).runs_mcp(), "{line:?}");
        }
        for line in [&["info"][..], &["mcp", "--help"], &["frobnicate"], &[]] {
            assert!(!parse(&args(line)).runs_mcp(), "{line:?}");
        }
    }

    #[test]
    fn usage_lines_name_the_subcommand() {
        assert_eq!(
            usage(None, &["log".to_owned()]),
            "Usage: swarmops log [OPTIONS] [REV]"
        );
        assert!(usage(None, &[]).starts_with("Usage: swarmops [OPTIONS] <COMMAND>"));
        let set = usage(None, &["ref".to_owned(), "set".to_owned(), "x".to_owned()]);
        assert!(set.starts_with("Usage: swarmops ref set "), "{set}");
        // The program by the name the line invoked it by.
        assert_eq!(
            usage(Some("so"), &["log".to_owned()]),
            "Usage: so log [OPTIONS] [REV]"
        );
        let set = usage(Some("so"), &["ref".to_owned(), "set".to_owned()]);
        assert!(set.starts_with("Usage: so ref set "), "{set}");
    }
}
