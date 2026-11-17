//! The help screens the CLI lays out itself: the program's own (`swarmops --help`), with its
//! commands in named groups, and each command family's (`swarmops ws --help`). Both come from
//! one mechanism, [`shaped`], which gives those commands of the grammar a help template; every
//! other command keeps clap's screen.
//!
//! ```text
//! <what it is>
//!
//! Usage: so [OPTIONS] <COMMAND>
//!
//! <group>:            (a family: Commands:)
//!   <name>  <summary>
//!
//! Examples:
//! Output:             (the program's screen only)
//! Learn more:
//! Options:
//! ```
//!
//! A screen is plain text at most [`WIDTH`] columns wide. Its headings and command names carry
//! the styles clap gives its own (bold, underlined), which clap writes to a terminal only. The
//! summaries are this module's ([`GROUPS`], [`FAMILIES`]): a line each, shorter than the
//! commands' own help, which their own screens and the reference pages keep.
//!
//! [`bare`] is the rule for a line that names no command: a person at a terminal reads the
//! screen, and everything else gets the usage error's envelope as before.

use std::ffi::OsStr;
use std::fmt::Write as _;

use clap::builder::Styles;
use clap::{Arg, ArgAction, Command};

use crate::output::Stdout;

/// The most columns a line of these screens takes.
pub(crate) const WIDTH: usize = 80;

/// The program's name where the line gives none.
const PROGRAM: &str = "swarmops";

/// What the format variable holds when it asks for the envelope.
const JSON: &str = "json";

/// A command's line in a list: its name and what it does, in at most 60 characters, which the
/// tests hold every summary to.
pub(crate) struct Entry {
    /// The command's name, as the grammar has it.
    pub(crate) name: &'static str,
    /// The one line that says what it does.
    pub(crate) summary: &'static str,
}

/// An [`Entry`].
const fn entry(name: &'static str, summary: &'static str) -> Entry {
    Entry { name, summary }
}

/// A named group of the program's commands.
pub(crate) struct Group {
    /// The group's heading, without its colon.
    pub(crate) title: &'static str,
    /// Its commands, in the order shown.
    pub(crate) commands: &'static [Entry],
}

/// Every top-level command, in exactly one group, in the order a newcomer needs them.
pub(crate) const GROUPS: [Group; 7] = [
    Group {
        title: "Start here",
        commands: &[
            entry("init", "Create a store in a directory"),
            entry("clone", "Make a store from a git remote"),
            entry("info", "Describe the store"),
            entry("whoami", "Show the identity your changes carry"),
        ],
    },
    Group {
        title: "Record and land changes",
        commands: &[
            entry("ws", "Work in a directory of your own: edit, commit, land"),
            entry("commit", "Record a changeset from a list of changes"),
            entry("land", "Merge a change onto a ref and publish it there"),
            entry(
                "land-check",
                "Say what landing changes would come to, writing nothing",
            ),
            entry(
                "land-group",
                "Land several changes in order as one operation",
            ),
            entry("rebase", "Merge a change onto a ref's head, moving no ref"),
            entry(
                "revert",
                "Take back a changeset or a range by landing its inverse",
            ),
            entry("blob", "Store content as a blob for a commit to name"),
        ],
    },
    Group {
        title: "Read history",
        commands: &[
            entry("log", "List history along first parents, newest first"),
            entry("show", "Show one changeset"),
            entry("diff", "List the files that differ between two revisions"),
            entry("cat", "Print one file at a revision, or part of it"),
            entry("ls", "List one directory at a revision"),
            entry("history", "List the changesets that changed a path"),
            entry("ancestry", "Compare two revisions' ancestry"),
            entry("merge-base", "Find where a revision forks from another"),
            entry("resolve", "Name the changeset a revision names"),
        ],
    },
    Group {
        title: "Refs and operations",
        commands: &[
            entry("ref", "List refs, or move one by compare-and-swap"),
            entry("op", "List the store's operations, or undo one"),
        ],
    },
    Group {
        title: "Git and remotes",
        commands: &[
            entry("git", "Bring history in from git, or send it back"),
            entry("remote", "Add, remove or list the store's git remotes"),
            entry("fetch", "Fetch a git remote into the store"),
            entry(
                "push",
                "Send a ref to a git remote's branch, never by force",
            ),
            entry(
                "sync",
                "Land main's unpublished changes on a remote, and push",
            ),
            entry(
                "auth",
                "Keep, list or remove your logins, or hand one to git",
            ),
        ],
    },
    Group {
        title: "Store upkeep",
        commands: &[
            entry("verify", "Check the store's consistency"),
            entry("gc", "Remove the store's leftovers, never an object"),
            entry(
                "repair",
                "Rebuild what the journal can rebuild, then verify",
            ),
            entry(
                "upgrade",
                "Move the store to the latest format (irreversible)",
            ),
        ],
    },
    Group {
        title: "Agents and servers",
        commands: &[
            entry("mcp", "Serve the store's operations to an agent over MCP"),
            entry("serve", "Serve stores to stock git over ssh or HTTP"),
            entry(
                "derive",
                "Derive new changesets' derived values ahead of readers",
            ),
            entry(
                "schema",
                "Print the JSON Schema of the request and response types",
            ),
        ],
    },
];

/// A command line a screen shows: one that runs as written, in a directory that holds what it
/// names, with `SWARMOPS_REPO` and `SWARMOPS_AGENT` set as the program's screen sets them.
pub(crate) struct Example {
    /// The line's words after the program's name.
    pub(crate) words: &'static [&'static str],
    /// The file its stdin is read from (`< FILE`), if it reads one.
    pub(crate) stdin: Option<&'static str>,
}

/// An [`Example`] that reads no stdin.
const fn example(words: &'static [&'static str]) -> Example {
    Example { words, stdin: None }
}

/// The line the program's examples start from, which is no run of the program: it names the
/// store and the caller for the lines after it.
pub(crate) const SETTING: &str = r#"export SWARMOPS_REPO="$PWD/acme" SWARMOPS_AGENT=person/ada"#;

/// The program's examples: a store, a first commit, a change made in a workspace and landed,
/// and the history read back. Each runs after the one before it.
pub(crate) const EXAMPLES: [Example; 6] = [
    example(&["init", "acme"]),
    example(&[
        "commit",
        "-m",
        "first",
        "--changes",
        "changes.json",
        "--advance",
        "main",
    ]),
    example(&["ws", "create", "fix-1"]),
    example(&["ws", "commit", "--ws", "fix-1", "-m", "Fix the typo"]),
    example(&["ws", "land", "--ws", "fix-1"]),
    example(&["--human", "log"]),
];

/// What a family's examples need set before they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Needs {
    /// A store named by `SWARMOPS_REPO` and a caller by `SWARMOPS_AGENT`.
    Store,
    /// Nothing: the family uses no store.
    Nothing,
}

/// A command family: a command that only groups subcommands.
pub(crate) struct Family {
    /// The family's name, a top-level command.
    pub(crate) name: &'static str,
    /// Its subcommands, in the grammar's order.
    pub(crate) commands: &'static [Entry],
    /// One or two lines that show it at work.
    pub(crate) examples: &'static [Example],
    /// What those lines need set.
    pub(crate) needs: Needs,
    /// The guide that explains the family, an address of the site.
    pub(crate) guide: &'static str,
}

/// Every command family, with a summary of each of its subcommands.
pub(crate) const FAMILIES: [Family; 7] = [
    Family {
        name: "ws",
        commands: &[
            entry("create", "Make a workspace in a directory of its own"),
            entry(
                "status",
                "What changed in a workspace, and how far its target moved",
            ),
            entry("diff", "A workspace's edits against its head"),
            entry("commit", "Record a workspace's edits as one changeset"),
            entry("rebase", "Move a workspace's chain onto its target's head"),
            entry("resolve", "Mark conflicting paths of a rebase resolved"),
            entry("land", "Land a workspace's chain on its target"),
            entry("materialize", "Write out more of a workspace's tree"),
            entry("discard", "Restore paths from the head: the edits are lost"),
            entry("snapshot", "Record the edits as a changeset, moving no ref"),
            entry("list", "List the store's workspaces"),
            entry("info", "Describe one workspace"),
            entry("delete", "Remove a workspace's directory, then its entry"),
            entry("hook", "The agent hosts' hooks, on stdin and stdout"),
        ],
        examples: &[
            example(&["ws", "create", "fix-1"]),
            example(&["--human", "ws", "status", "--ws", "fix-1"]),
        ],
        needs: Needs::Store,
        guide: "https://swarmops.cc/docs/guides/parallel-agents",
    },
    Family {
        name: "ref",
        commands: &[
            entry("list", "List refs, each with its kind"),
            entry("set", "Point a ref at a revision, by compare-and-swap"),
            entry("delete", "Delete a ref"),
            entry("log", "List a ref's moves, newest first"),
            entry("txn", "Move several refs as one operation: all or none"),
        ],
        examples: &[
            example(&["--human", "ref", "list"]),
            example(&["ref", "set", "release/1.0", "main", "--create"]),
        ],
        needs: Needs::Store,
        guide: "https://swarmops.cc/docs/guides/history-and-undo",
    },
    Family {
        name: "op",
        commands: &[
            entry("log", "List the store's operations, newest first"),
            entry(
                "undo",
                "Move every ref an operation moved back, all or none",
            ),
        ],
        examples: &[example(&["--human", "op", "log"])],
        needs: Needs::Store,
        guide: "https://swarmops.cc/docs/guides/history-and-undo",
    },
    Family {
        name: "blob",
        commands: &[
            entry("put", "Store a file's content as a blob, and print its id"),
            entry("has", "Of the ids given, list those the store holds"),
            entry("get", "Print the blobs the ids name"),
        ],
        examples: &[example(&["blob", "put", "logo.png"])],
        needs: Needs::Store,
        guide: "https://swarmops.cc/docs/guides/commit-and-land",
    },
    Family {
        name: "git",
        commands: &[
            entry("import", "Bring a git repository's history in"),
            entry("export", "Send the store's history to a git repository"),
        ],
        examples: &[
            example(&[
                "git", "export", "app.git", "--ref", "main", "--branch", "main",
            ]),
            example(&[
                "git", "import", "app.git", "--ref", "main", "--into", "from-git",
            ]),
        ],
        needs: Needs::Store,
        guide: "https://swarmops.cc/docs/guides/git-round-trip",
    },
    Family {
        name: "remote",
        commands: &[
            entry("add", "Record a git remote in the store"),
            entry("remove", "Forget a git remote, and the mirror's refs of it"),
            entry("list", "List the store's git remotes"),
        ],
        examples: &[
            example(&[
                "remote",
                "add",
                "origin",
                "https://github.com/OWNER/REPO.git",
            ]),
            example(&["--human", "remote", "list"]),
        ],
        needs: Needs::Store,
        guide: "https://swarmops.cc/docs/guides/github-remote",
    },
    Family {
        name: "auth",
        commands: &[
            entry("login", "Log in to a provider, and keep its token"),
            entry("status", "List your logins, without their tokens"),
            entry("logout", "Remove a provider's login"),
            entry("git-credential", "Answer git's credential-helper protocol"),
        ],
        examples: &[
            Example {
                words: &["auth", "login", "--gh", "--with-token"],
                stdin: Some("token.txt"),
            },
            example(&["--human", "auth", "status"]),
        ],
        needs: Needs::Nothing,
        guide: "https://swarmops.cc/docs/guides/github-remote",
    },
];

/// What the program's screen says of its output, the rule of [`bare`] included.
const OUTPUT: &str = "Every command prints one JSON envelope on stdout, or with --human a short \
text, and exits 0 or with its error's code. mcp, serve, derive --follow and auth \
git-credential print something else there. At a terminal, a line that names no command prints \
help like this instead of its usage error's envelope, unless --json or SWARMOPS_FORMAT=json \
asks for the envelope; it exits 2 either way.";

/// What `<command> --help` gives.
const COMMAND_HELP: &str = "A command's arguments, options and long help";

/// The site's pages the program's screen names.
const PAGES: [(&str, &str); 4] = [
    ("Quick start", "https://swarmops.cc/docs/guides/quick-start"),
    ("Every command", "https://swarmops.cc/docs/reference/cli"),
    (
        "Envelope, exit codes",
        "https://swarmops.cc/docs/develop/cli-scripting",
    ),
    ("Error codes", "https://swarmops.cc/docs/reference/errors"),
];

/// The page every family's screen names beside its guide.
const REFERENCE: &str = "https://swarmops.cc/docs/reference/cli";

/// The name the line `first` starts invokes the program by, as clap's usage lines give it: its
/// file name, or `swarmops` when it has none that is UTF-8.
pub(crate) fn invoked(first: Option<&OsStr>) -> &str {
    first
        .and_then(|first| std::path::Path::new(first).file_name()?.to_str())
        .unwrap_or(PROGRAM)
}

/// `grammar`, the program's, with the screens of this module: its own help in groups, and each
/// family's in the same shape. The examples name the program `bin`. The grammar reads every
/// line as before: only the help text changes, and the subcommands keep their order, which
/// clap's messages list them in.
pub(crate) fn shaped(grammar: Command, bin: &str) -> Command {
    let styles = grammar.get_styles().clone();
    let options = options(&grammar, &styles);

    // 1. The program's own screen. Its long help is its short one: the screen says the rest.
    let template = program_template(bin, &styles, &options);
    let mut grammar = grammar.long_about(None).help_template(template);

    // 2. The families, in the order the grammar has them: `mut_subcommand` moves each to the
    //    end.
    let order: Vec<&Family> = grammar
        .get_subcommands()
        .filter_map(|sub| FAMILIES.iter().find(|family| family.name == sub.get_name()))
        .collect();
    for family in order {
        let template = family_template(family, bin, &styles, &options);
        grammar = grammar.mut_subcommand(family.name, |cmd| refilled(cmd).help_template(template));
    }
    grammar
}

/// `cmd` with its help's first lines, short and long, wrapped at [`WIDTH`] columns: clap
/// writes them as they are.
fn refilled(cmd: Command) -> Command {
    let about = cmd.get_about().map(|about| wrapped(&about.to_string(), 0));
    let long = cmd
        .get_long_about()
        .map(|about| wrapped(&about.to_string(), 0));
    let cmd = match about {
        Some(about) => cmd.about(about),
        None => cmd,
    };
    match long {
        Some(long) => cmd.long_about(long),
        None => cmd,
    }
}

/// Whether a line that names no command (the program alone, or a family alone) prints its help
/// instead of its usage error: when `stdout` is a terminal and no format is forced, neither by
/// `--json` (`json`: the line settles on it, the last of `--json` and `--human`) nor by a
/// `SWARMOPS_FORMAT` (`format`) of `json`, whatever flag is given with it. Whatever is not a
/// person at a terminal gets what it got before there were screens to print: the envelope, or
/// with `--human` the error as text.
pub(crate) fn bare(stdout: Stdout, json: bool, format: Option<&OsStr>) -> bool {
    stdout == Stdout::Terminal && !json && format != Some(OsStr::new(JSON))
}

/// The program's help template: clap fills in the about and the usage, the rest is text.
fn program_template(bin: &str, styles: &Styles, options: &str) -> String {
    let mut out = String::from("{about}\n\n{usage-heading} {usage}\n");

    // 1. The commands, by group.
    for group in &GROUPS {
        heading(&mut out, styles, group.title);
        entries(&mut out, styles, group.commands, name_width(&GROUPS));
    }

    // 2. The examples, after the line that names the store and the caller.
    heading(&mut out, styles, "Examples");
    let _ = writeln!(out, "  $ {SETTING}");
    examples(&mut out, bin, &EXAMPLES);

    // 3. What a command prints, and where to read more.
    heading(&mut out, styles, "Output");
    let _ = writeln!(out, "{}", wrapped(OUTPUT, 2));
    heading(&mut out, styles, "Learn more");
    let mut rows = vec![(format!("{bin} <command> --help"), COMMAND_HELP)];
    rows.extend(
        PAGES
            .iter()
            .map(|(label, page)| ((*label).to_owned(), *page)),
    );
    table(&mut out, &rows);

    // 4. The options, last.
    heading(&mut out, styles, "Options");
    out.push_str(options);
    out
}

/// A family's help template, in the program's shape.
fn family_template(family: &Family, bin: &str, styles: &Styles, options: &str) -> String {
    let mut out = String::from("{about}\n\n{usage-heading} {usage}\n");

    // 1. The subcommands.
    heading(&mut out, styles, "Commands");
    let width = family.commands.iter().map(|entry| entry.name.len()).max();
    entries(&mut out, styles, family.commands, width.unwrap_or(0));

    // 2. The examples.
    let title = match family.needs {
        Needs::Store => "Examples (SWARMOPS_REPO names the store, SWARMOPS_AGENT you)",
        Needs::Nothing => "Examples",
    };
    heading(&mut out, styles, title);
    examples(&mut out, bin, family.examples);

    // 3. Where to read more.
    heading(&mut out, styles, "Learn more");
    let name = family.name;
    let _ = writeln!(out, "  {bin} {name} <command> --help  {COMMAND_HELP}");
    let pages = [("Guide", family.guide), ("Reference", REFERENCE)];
    table(
        &mut out,
        &pages.map(|(label, page)| (label.to_owned(), page)),
    );

    // 4. The options, last. `--version` is the program's alone.
    heading(&mut out, styles, "Options");
    for line in options.lines().filter(|line| !line.contains("--version")) {
        let _ = writeln!(out, "{line}");
    }
    out
}

/// The longest command name of `groups`.
fn name_width(groups: &[Group]) -> usize {
    let names = groups.iter().flat_map(|group| group.commands);
    names.map(|entry| entry.name.len()).max().unwrap_or(0)
}

/// A blank line, then `title` as a heading, in the style clap gives its own.
fn heading(out: &mut String, styles: &Styles, title: &str) {
    let style = styles.get_header();
    let _ = writeln!(out, "\n{style}{title}:{style:#}");
}

/// A line per entry: its name, padded to `width`, then its summary.
fn entries(out: &mut String, styles: &Styles, entries: &[Entry], width: usize) {
    let style = styles.get_literal();
    for entry in entries {
        let pad = width.saturating_sub(entry.name.len());
        let _ = writeln!(
            out,
            "  {style}{}{style:#}{:pad$}  {}",
            entry.name, "", entry.summary
        );
    }
}

/// A line per example, as a shell takes it.
fn examples(out: &mut String, bin: &str, examples: &[Example]) {
    for example in examples {
        let _ = writeln!(out, "  $ {}", shell_line(bin, example));
    }
}

/// `example` as a line to type: the program `bin`, its words, quoted where a shell would split
/// them, and its stdin's file.
fn shell_line(bin: &str, example: &Example) -> String {
    let mut line = bin.to_owned();
    for word in example.words {
        if word.contains(' ') {
            let _ = write!(line, " \"{word}\"");
        } else {
            let _ = write!(line, " {word}");
        }
    }
    if let Some(file) = example.stdin {
        let _ = write!(line, " < {file}");
    }
    line
}

/// Two columns: each row's label, padded to the longest, then its text.
fn table(out: &mut String, rows: &[(String, &str)]) {
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, text) in rows {
        let _ = writeln!(out, "  {label:width$}  {text}");
    }
}

/// The options every command takes, with clap's own two, a line each, wrapped: the global
/// flags of `grammar`, which is not built yet, so `--help` and `--version` are written here as
/// clap words them.
fn options(grammar: &Command, styles: &Styles) -> String {
    let mut rows: Vec<(String, String)> = grammar
        .get_arguments()
        .filter(|arg| arg.is_global_set() && !arg.is_hide_set())
        .map(|arg| {
            let help = arg.get_help().map_or_default(ToString::to_string);
            (format!("    {}", option_form(arg)), help)
        })
        .collect();
    rows.push(("-h, --help".to_owned(), "Print help".to_owned()));
    rows.push(("-V, --version".to_owned(), "Print version".to_owned()));

    let style = styles.get_literal();
    let width = rows.iter().map(|(form, _)| form.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (form, help) in &rows {
        // The help starts after the form's column, and its further lines below its start.
        let indent = 2 + width + 2;
        let help = wrapped(help, indent);
        let pad = width.saturating_sub(form.len());
        let name = form.trim_start();
        let lead = form.len() - name.len();
        let _ = writeln!(
            out,
            "  {:lead$}{style}{name}{style:#}{:pad$}  {}",
            "",
            "",
            help.trim_start()
        );
    }
    out
}

/// How an option is written: `-n, --limit <N>`, `--pretty[=<BOOL>]`, `--path <P>...`.
pub(crate) fn option_form(arg: &Arg) -> String {
    let mut form = String::new();
    if let Some(short) = arg.get_short() {
        let _ = write!(form, "-{short}, ");
    }
    if let Some(long) = arg.get_long() {
        let _ = write!(form, "--{long}");
    }

    // A flag takes no value.
    if !arg.get_action().takes_values() {
        return form;
    }
    let value = format!("<{}>", value_names(arg).join(" "));
    let optional = arg.get_num_args().is_some_and(|n| n.min_values() == 0);
    match (optional, arg.is_require_equals_set()) {
        (true, true) => {
            let _ = write!(form, "[={value}]");
        }
        (true, false) => {
            let _ = write!(form, " [{value}]");
        }
        (false, _) => {
            let _ = write!(form, " {value}");
        }
    }
    if matches!(arg.get_action(), ArgAction::Append) {
        form.push_str("...");
    }
    form
}

/// An argument's value names, or its id in capitals when it names none.
pub(crate) fn value_names(arg: &Arg) -> Vec<String> {
    match arg.get_value_names() {
        Some(names) => names.iter().map(ToString::to_string).collect(),
        None => vec![arg.get_id().as_str().to_uppercase()],
    }
}

/// `text` with each of its paragraphs wrapped at [`WIDTH`] columns, every line after `indent`
/// spaces. A word longer than a line stays whole.
fn wrapped(text: &str, indent: usize) -> String {
    let mut out = String::new();
    for (at, paragraph) in text.split("\n\n").enumerate() {
        if at > 0 {
            out.push_str("\n\n");
        }
        let mut column = 0;
        for word in paragraph.split_whitespace() {
            if column == 0 {
                let _ = write!(out, "{:indent$}{word}", "");
                column = indent + word.len();
            } else if column + 1 + word.len() > WIDTH {
                let _ = write!(out, "\n{:indent$}{word}", "");
                column = indent + word.len();
            } else {
                let _ = write!(out, " {word}");
                column += 1 + word.len();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    use clap::CommandFactory as _;

    use super::*;
    use crate::cli::{self, Cli, Parsed};

    /// The most characters a summary takes.
    const SUMMARY_WIDTH: usize = 60;

    /// The program's screen, as `bin --help` prints it where no terminal reads it.
    fn screen(bin: &str) -> String {
        shaped(Cli::command(), bin).render_help().to_string()
    }

    /// The names of `cmd`'s subcommands but clap's own `help`, in the grammar's order.
    fn subcommands(cmd: &Command) -> Vec<String> {
        cmd.get_subcommands()
            .map(|sub| sub.get_name().to_owned())
            .filter(|name| name != "help")
            .collect()
    }

    /// A new top-level command fails here until a group lists it, and so does a group that
    /// lists a command twice or one the grammar does not have.
    #[test]
    fn every_command_is_in_exactly_one_group() {
        let listed: Vec<&str> = GROUPS
            .iter()
            .flat_map(|group| group.commands)
            .map(|entry| entry.name)
            .collect();
        let once: BTreeSet<&str> = listed.iter().copied().collect();
        assert_eq!(once.len(), listed.len(), "a command in two groups");

        let grammar = subcommands(&Cli::command());
        let grammar: BTreeSet<&str> = grammar.iter().map(String::as_str).collect();
        assert_eq!(
            once, grammar,
            "the groups and the grammar's commands differ"
        );
    }

    /// A summary is one line of plain words, without the period clap drops from a command's
    /// own help.
    fn check_summaries(of: &str, entries: &[Entry]) {
        for entry in entries {
            let summary = entry.summary;
            assert!(
                !summary.is_empty() && summary.len() <= SUMMARY_WIDTH,
                "{of} {}: 1 to {SUMMARY_WIDTH} characters: {summary:?}",
                entry.name
            );
            assert!(
                summary.is_ascii() && !summary.contains(['\n', '{', '}']),
                "{of} {}: plain ASCII on one line: {summary:?}",
                entry.name
            );
            assert!(!summary.ends_with('.'), "{of} {}: no period", entry.name);
        }
    }

    #[test]
    fn summaries_fit_their_column() {
        for group in &GROUPS {
            check_summaries(group.title, group.commands);
        }
        for family in &FAMILIES {
            check_summaries(family.name, family.commands);
        }
    }

    /// The families are the grammar's commands that only group subcommands, and each lists
    /// its subcommands in the grammar's order: a new family, or a new subcommand of one, fails
    /// here until it has its summary.
    #[test]
    fn every_family_lists_its_subcommands() {
        let grammar = Cli::command();
        let groups: BTreeSet<&str> = grammar
            .get_subcommands()
            .filter(|sub| sub.has_subcommands() && cli::op_name(&[sub.get_name()]).is_none())
            .map(Command::get_name)
            .filter(|name| *name != "help")
            .collect();
        let families: BTreeSet<&str> = FAMILIES.iter().map(|family| family.name).collect();
        assert_eq!(families, groups, "the families and the grammar's differ");
        assert_eq!(families.len(), FAMILIES.len(), "a family listed twice");

        for family in &FAMILIES {
            let sub = grammar.find_subcommand(family.name).expect("a command");
            let listed: Vec<&str> = family.commands.iter().map(|entry| entry.name).collect();
            assert_eq!(listed, subcommands(sub), "{}", family.name);
            assert!(
                (1..=2).contains(&family.examples.len()),
                "{}: an example or two",
                family.name
            );
        }
    }

    /// A family's screen, short (`-h`) or long (`--help`), with the program named `bin`.
    fn family_screen(bin: &str, family: &str, long: bool) -> String {
        let mut grammar = shaped(Cli::command(), bin);
        grammar.set_bin_name(bin);
        grammar.build();
        let mut sub = grammar.find_subcommand(family).expect("a command").clone();
        let screen = if long {
            sub.render_long_help()
        } else {
            sub.render_help()
        };
        screen.to_string()
    }

    #[test]
    fn the_families_screens_fit_80_columns_by_either_name() {
        for family in &FAMILIES {
            let name = family.name;
            for bin in ["so", "swarmops"] {
                for long in [false, true] {
                    let screen = family_screen(bin, name, long);
                    check_screen(&format!("{bin} {name} (long: {long})"), &screen);

                    // The shape of the program's screen: usage, commands, examples, more, options.
                    let usage =
                        format!("\n\nUsage: {bin} {name} [OPTIONS] <COMMAND>\n\nCommands:\n");
                    let mut at = screen.find(&usage).unwrap_or_else(|| panic!("{screen}"));
                    for title in ["\n\nExamples", "\n\nLearn more:\n", "\n\nOptions:\n"] {
                        let found = screen[at..].find(title);
                        at += found.unwrap_or_else(|| panic!("{title:?}: {screen}"));
                    }
                    assert!(
                        screen.ends_with("  -h, --help             Print help\n"),
                        "{screen}"
                    );
                    let more = format!("  {bin} {name} <command> --help  ");
                    assert!(screen.contains(&more), "{screen}");
                }
            }
        }
    }

    /// The first lines of a family's screen are its command's own help, word for word: only
    /// the line breaks are this module's.
    #[test]
    fn a_family_keeps_its_own_first_lines() {
        let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let plain = Cli::command();
        for family in &FAMILIES {
            let sub = plain.find_subcommand(family.name).expect("a command");
            let about = sub.get_about().expect("an about").to_string();
            let short = family_screen("so", family.name, false);
            let (first, _) = short.split_once("\n\nUsage: ").expect("a usage line");
            assert_eq!(words(first), words(&about), "{}", family.name);

            let long_about = sub.get_long_about().map_or(about, ToString::to_string);
            let long = family_screen("so", family.name, true);
            let (first, _) = long.split_once("\n\nUsage: ").expect("a usage line");
            assert_eq!(words(first), words(&long_about), "{}", family.name);
        }
    }

    /// Every line of the screen fits [`WIDTH`] columns by either name, with no tag left
    /// unfilled and nothing a terminal would read as more than text.
    fn check_screen(of: &str, screen: &str) {
        for line in screen.lines() {
            assert!(line.len() <= WIDTH, "{of}: over {WIDTH} columns: {line:?}");
            assert_eq!(line, line.trim_end(), "{of}: trailing spaces: {line:?}");
        }
        assert!(screen.is_ascii(), "{of}: ASCII alone");
        assert!(!screen.contains(['{', '}', '\x1b']), "{of}: {screen}");
    }

    #[test]
    fn the_screen_fits_80_columns_by_either_name() {
        for bin in ["so", "swarmops"] {
            let screen = screen(bin);
            check_screen(bin, &screen);
            assert!(
                screen.starts_with("Source control for coding-agent swarms\n\nUsage: "),
                "{screen}"
            );
            // The sections, in order, the options last.
            let mut at = 0;
            let titles = GROUPS.iter().map(|group| group.title);
            for title in titles.chain(["Examples", "Output", "Learn more", "Options"]) {
                let heading = format!("\n\n{title}:\n");
                let found = screen[at..].find(&heading);
                at += found.unwrap_or_else(|| panic!("{title} after byte {at}: {screen}"));
            }
            assert!(screen.ends_with("  -V, --version          Print version\n"));
            assert!(
                screen.contains(&format!("  $ {bin} init acme\n")),
                "{screen}"
            );
        }
    }

    /// The grammar reads each example as a command to run.
    fn check_examples(examples: &[Example]) {
        for example in examples {
            let line: Vec<OsString> = std::iter::once("so")
                .chain(example.words.iter().copied())
                .map(OsString::from)
                .collect();
            let parsed = cli::parse(&line);
            assert!(matches!(parsed, Parsed::Run(..)), "{line:?}: {parsed:?}");
        }
    }

    #[test]
    fn the_examples_are_lines_the_grammar_reads() {
        check_examples(&EXAMPLES);
        for family in &FAMILIES {
            check_examples(family.examples);
            for example in family.examples {
                assert!(example.words.contains(&family.name), "{}", family.name);
            }
        }
        assert_eq!(
            shell_line("so", &FAMILIES[6].examples[0]),
            "so auth login --gh --with-token < token.txt"
        );
        assert_eq!(
            shell_line("so", &EXAMPLES[3]),
            r#"so ws commit --ws fix-1 -m "Fix the typo""#
        );
    }

    /// The pages a screen names are addresses the site serves (`web/addresses.txt`).
    fn check_addresses(pages: &[&str]) {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/addresses.txt");
        let served = std::fs::read_to_string(file).expect("read web/addresses.txt");
        for page in pages {
            let path = page
                .strip_prefix("https://swarmops.cc")
                .unwrap_or_else(|| panic!("{page}: not the site's"));
            assert!(
                served.lines().any(|line| line == path),
                "{page}: not served"
            );
        }
    }

    #[test]
    fn the_pages_named_are_the_site_s() {
        let pages: Vec<&str> = PAGES.iter().map(|(_, page)| *page).collect();
        check_addresses(&pages);
        let guides: Vec<&str> = FAMILIES.iter().map(|family| family.guide).collect();
        check_addresses(&guides);
        check_addresses(&[REFERENCE]);
    }

    /// Shaping changes the help text alone: the commands and their order, which clap's
    /// message for a missing subcommand lists, stay those of the grammar the CLI reads a line
    /// with (`cli::command`).
    #[test]
    fn shaping_keeps_the_commands_and_their_order() {
        let plain = cli::command();
        let shaped = shaped(cli::command(), "so");
        assert_eq!(subcommands(&shaped), subcommands(&plain));
        for (shaped, plain) in shaped.get_subcommands().zip(plain.get_subcommands()) {
            assert_eq!(
                subcommands(shaped),
                subcommands(plain),
                "{}",
                plain.get_name()
            );
        }
    }

    #[test]
    fn the_program_is_named_as_the_line_invokes_it() {
        for (first, name) in [
            (Some("so"), "so"),
            (Some("/usr/local/bin/swarmops"), "swarmops"),
            (Some("./so"), "so"),
            (Some(""), "swarmops"),
            (Some("/"), "swarmops"),
            (None, "swarmops"),
        ] {
            assert_eq!(invoked(first.map(OsStr::new)), name, "{first:?}");
        }
    }

    /// The help only for a terminal where no format is forced; what it printed before for a
    /// pipe or a file, whatever else is set.
    #[test]
    fn a_bare_line_reads_help_at_a_terminal_alone() {
        let json = Some(OsStr::new("json"));
        let human = Some(OsStr::new("human"));
        let other = Some(OsStr::new("JSON"));
        let empty = Some(OsStr::new(""));
        for (flag, format, at_a_terminal) in [
            (false, None, true),
            (false, human, true),
            (false, other, true),
            (false, empty, true),
            (false, json, false),
            (true, None, false),
            (true, human, false),
            (true, json, false),
        ] {
            let said = format!("--json: {flag}, {format:?}");
            assert_eq!(
                bare(Stdout::Terminal, flag, format),
                at_a_terminal,
                "{said}"
            );
            assert!(!bare(Stdout::Other, flag, format), "a pipe: {said}");
        }
    }

    #[test]
    fn text_wraps_at_the_width_under_its_indent() {
        let text = "word ".repeat(40);
        let wrapped = wrapped(&text, 4);
        assert!(wrapped.lines().count() > 1, "{wrapped}");
        for line in wrapped.lines() {
            assert!(
                line.starts_with("    word") && line.len() <= WIDTH,
                "{line:?}"
            );
        }
        assert_eq!(super::wrapped("one\n\ntwo", 0), "one\n\ntwo");
    }
}
