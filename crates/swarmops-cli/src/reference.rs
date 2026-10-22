//! The generated reference pages, `docs/reference/cli.md` and `docs/reference/errors.md`:
//! rendered from the command grammar ([`Cli`]'s clap tree) and from the error table
//! ([`ErrorCode`] and its schema), so that the pages cannot drift from the code.
//!
//! `reference_pages_match` fails when a committed page is not its rendering;
//! `SWARMOPS_REGEN_REFERENCE=1` with the ignored `regen_reference` writes them again:
//!
//! ```text
//! SWARMOPS_REGEN_REFERENCE=1 cargo nextest run -p swarmops-cli --run-ignored ignored-only \
//!     -E 'test(regen_reference)'
//! ```

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use clap::{Arg, ArgAction, CommandFactory};
use swarmops_api::{ErrorCode, schema_for_all};

use crate::cli::{Cli, op_name, usage};

/// The variable that makes `regen_reference` write the pages.
const REGEN_VAR: &str = "SWARMOPS_REGEN_REFERENCE";

/// The command that writes the pages again, as the pages and failures name it.
const REGEN_CMD: &str = "SWARMOPS_REGEN_REFERENCE=1 cargo nextest run -p swarmops-cli \
--run-ignored ignored-only -E 'test(regen_reference)'";

/// The first line of every generated page.
const GENERATED: &str = "<!-- Generated from the code by crates/swarmops-cli/src/reference.rs: \
do not edit. Change the code, then run SWARMOPS_REGEN_REFERENCE=1 with the regen_reference \
test. -->";

/// The program name the pages give.
const BIN: &str = "swarmops";

/// Where each command's worked example is, with real output: a command path, the link's text
/// and its target, relative to `docs/reference/`.
const EXAMPLES: &[(&str, &str, &str)] = &[
    (
        "init",
        "Operating SwarmOps: Create a store",
        "../operations.md#create-a-store",
    ),
    (
        "whoami",
        "Operating SwarmOps: Identities",
        "../operations.md#identities",
    ),
    (
        "commit",
        "Operating SwarmOps: Agents commit and land",
        "../operations.md#2-agents-commit-and-land",
    ),
    (
        "land",
        "Operating SwarmOps: Agents commit and land",
        "../operations.md#2-agents-commit-and-land",
    ),
    (
        "resolve",
        "Operating SwarmOps: One writer per trunk",
        "../operations.md#one-writer-per-trunk",
    ),
    (
        "cat",
        "README: First commands",
        "../../README.md#first-commands",
    ),
    (
        "ls",
        "README: First commands",
        "../../README.md#first-commands",
    ),
    (
        "show",
        "README: First commands",
        "../../README.md#first-commands",
    ),
    (
        "log",
        "Operating SwarmOps: Look at what landed",
        "../operations.md#3-look-at-what-landed",
    ),
    (
        "history",
        "Operating SwarmOps: Making room",
        "../operations.md#making-room",
    ),
    (
        "diff",
        "Operating SwarmOps: Look at what landed",
        "../operations.md#3-look-at-what-landed",
    ),
    (
        "ref set",
        "Operating SwarmOps: One writer per trunk",
        "../operations.md#one-writer-per-trunk",
    ),
    (
        "ref delete",
        "Operating SwarmOps: One writer per trunk",
        "../operations.md#one-writer-per-trunk",
    ),
    (
        "op log",
        "Operating SwarmOps: Look at what landed",
        "../operations.md#3-look-at-what-landed",
    ),
    (
        "verify",
        "Operating SwarmOps: Checking a store",
        "../operations.md#checking-a-store",
    ),
    (
        "git import",
        "Operating SwarmOps: Once: bring the repository in",
        "../operations.md#once-bring-the-repository-in",
    ),
    (
        "git export",
        "Operating SwarmOps: Send it to git for review",
        "../operations.md#4-send-it-to-git-for-review",
    ),
    (
        "mcp",
        "Operating SwarmOps: Give agents the store",
        "../operations.md#give-agents-the-store",
    ),
    (
        "derive",
        "Operating SwarmOps: Making room",
        "../operations.md#making-room",
    ),
];

/// What each process exit code means, as the constants of `swarmops_api::error` say.
const EXITS: &[(i32, &str)] = &[
    (0, "Success."),
    (1, "The operation failed."),
    (2, "The request or the command line is malformed."),
    (
        3,
        "The change conflicts with what it lands on: resolve it, then run the operation again.",
    ),
    (4, "The operation can succeed when it is run again."),
    (
        5,
        "An identity or policy requirement refused the operation.",
    ),
    (6, "Something the request names does not exist."),
];

/// One generated page: its file name in `docs/reference/` and its text.
struct Page {
    name: &'static str,
    text: String,
}

/// Every generated page.
fn pages() -> Vec<Page> {
    vec![
        Page {
            name: "cli.md",
            text: cli_page(),
        },
        Page {
            name: "errors.md",
            text: errors_page(),
        },
    ]
}

/// `docs/reference/`, from this crate's directory.
fn reference_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference")
}

// ---- cli.md ----

/// A leaf command: its subcommand path and its clap command.
struct Leaf {
    path: Vec<String>,
    cmd: clap::Command,
}

/// The built grammar, with every global flag propagated to each subcommand.
fn grammar() -> clap::Command {
    let mut cmd = Cli::command();
    cmd.build();
    cmd
}

/// Every command that names an operation, in the grammar's order.
fn leaves(cmd: &clap::Command, prefix: &[String], out: &mut Vec<Leaf>) {
    for sub in cmd.get_subcommands() {
        let mut path = prefix.to_vec();
        path.push(sub.get_name().to_owned());

        // A group (`ref`, `git`, …) names no operation itself: its subcommands do.
        if sub.has_subcommands() {
            leaves(sub, &path, out);
            continue;
        }

        // clap's own `help` subcommand names no operation.
        if op_name(&path).is_none() {
            continue;
        }
        out.push(Leaf {
            path,
            cmd: sub.clone(),
        });
    }
}

/// The commands, in the grammar's order.
fn all_leaves() -> Vec<Leaf> {
    let mut out = Vec::new();
    leaves(&grammar(), &[], &mut out);
    out
}

/// The page's anchor of a command: `swarmops git import` → `swarmops-git-import`.
fn command_anchor(path: &[String]) -> String {
    slug(&format!("{BIN} {}", path.join(" ")))
}

/// GitHub's anchor of a heading: lower case, spaces to `-`, other punctuation dropped.
fn slug(heading: &str) -> String {
    let kept = heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect::<String>();
    kept.replace(' ', "-")
}

/// Renders `cli.md`.
fn cli_page() -> String {
    let root = grammar();
    let leaves = all_leaves();
    let mut out = String::new();

    // 1. Title and how to read the page.
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# CLI reference\n");
    let _ = writeln!(
        out,
        "Every command of `{BIN}` and `so`, its short form: the same program by a shorter name. \
         This page is generated from the command line's own definition, so it says what \
         `{BIN} <command> --help` says. `-h` prints the short help and `--help` the long help \
         that this page gives.\n"
    );
    let _ = writeln!(
        out,
        "Every command but `mcp` prints one JSON envelope on stdout; its `op` names the \
         operation the command runs (`cat` runs `read_file`). [Errors](errors.md) lists every \
         error code an envelope can carry, with its exit code.\n"
    );

    // 2. The commands at a glance.
    let _ = writeln!(out, "## Commands\n");
    let _ = writeln!(out, "| Command | Operation | What it does |");
    let _ = writeln!(out, "| --- | --- | --- |");
    for leaf in &leaves {
        let name = leaf.path.join(" ");
        let about = cell(&sentence(&about_short(&leaf.cmd)));
        let op = op_name(&leaf.path).unwrap_or_default();
        let anchor = command_anchor(&leaf.path);
        let _ = writeln!(out, "| [`{name}`](#{anchor}) | `{op}` | {about} |");
    }
    out.push('\n');

    // 3. The flags every command takes.
    let _ = writeln!(out, "## Global options\n");
    let _ = writeln!(
        out,
        "Every command takes these, before or after its subcommand.\n"
    );
    let globals: Vec<&Arg> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .collect();
    options_table(&mut out, &globals);
    long_helps(&mut out, &globals);
    let _ = writeln!(
        out,
        "`-h` prints a command's short help and `--help` its long help, as plain text, not an \
         envelope. `{BIN} --version` prints the version.\n"
    );

    // 4. One section per command.
    for leaf in &leaves {
        command_section(&mut out, leaf);
    }
    trim_end(out)
}

/// The command's one-line help.
fn about_short(cmd: &clap::Command) -> String {
    cmd.get_about().map(ToString::to_string).unwrap_or_default()
}

/// One command's section: what it does, its usage, its arguments and options, its example.
fn command_section(out: &mut String, leaf: &Leaf) {
    let name = leaf.path.join(" ");
    let op = op_name(&leaf.path).unwrap_or_default();
    let _ = writeln!(out, "## {BIN} {name}\n");

    // 1. What it does: the long help when there is one.
    let about = leaf
        .cmd
        .get_long_about()
        .or(leaf.cmd.get_about())
        .map(ToString::to_string)
        .unwrap_or_default();
    paragraphs(out, &sentence(&about));
    let _ = writeln!(out, "Envelope `op`: `{op}`.\n");

    // 2. The usage line clap prints in a usage error's hint.
    let line = usage(Some(BIN), &leaf.path);
    let line = line.strip_prefix("Usage: ").unwrap_or(&line);
    let _ = writeln!(out, "```text\n{line}\n```\n");

    // 3. Positional arguments, then options; the global flags are listed once, above.
    let own: Vec<&Arg> = leaf
        .cmd
        .get_arguments()
        .filter(|arg| !arg.is_global_set() && !is_builtin(arg))
        .collect();
    let positionals: Vec<&Arg> = own.iter().copied().filter(|a| a.is_positional()).collect();
    let options: Vec<&Arg> = own.iter().copied().filter(|a| !a.is_positional()).collect();
    if !positionals.is_empty() {
        let _ = writeln!(out, "Arguments:\n");
        arguments_table(out, &positionals);
    }
    if !options.is_empty() {
        let _ = writeln!(out, "Options:\n");
        options_table(out, &options);
    }
    required_groups(out, &leaf.cmd);
    long_helps(out, &own);

    // 4. Where a worked example with real output is.
    if let Some((_, text, link)) = EXAMPLES.iter().find(|(path, _, _)| *path == name) {
        let _ = writeln!(out, "Example: [{text}]({link}).\n");
    }
}

/// The groups of options a command needs one of: `ref set` takes `--expect` or `--create`.
fn required_groups(out: &mut String, cmd: &clap::Command) {
    for group in cmd.get_groups().filter(|group| group.is_required_set()) {
        let forms: Vec<String> = group
            .get_args()
            .filter_map(|id| cmd.get_arguments().find(|arg| arg.get_id() == id))
            .map(|arg| format!("`{}`", option_form(arg)))
            .collect();
        let _ = writeln!(out, "Exactly one of {} is required.\n", forms.join(" or "));
    }
}

/// Whether `arg` is clap's own `--help` or `--version`.
fn is_builtin(arg: &Arg) -> bool {
    matches!(arg.get_id().as_str(), "help" | "version")
}

/// The table of positional arguments.
fn arguments_table(out: &mut String, args: &[&Arg]) {
    let _ = writeln!(out, "| Argument | Required | Description | Default |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for arg in args {
        let name = format!("`{}`", value_names(arg).join(" "));
        let required = yes_no(arg.is_required_set());
        let help = cell(&arg_help(arg));
        let default = default_cell(arg);
        let _ = writeln!(out, "| {name} | {required} | {help} | {default} |");
    }
    out.push('\n');
}

/// The table of options.
fn options_table(out: &mut String, args: &[&Arg]) {
    let _ = writeln!(out, "| Option | Required | Description | Default |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for arg in args {
        let name = format!("`{}`", option_form(arg));
        let required = yes_no(arg.is_required_set());
        let help = cell(&arg_help(arg));
        let default = default_cell(arg);
        let _ = writeln!(out, "| {name} | {required} | {help} | {default} |");
    }
    out.push('\n');
}

/// The long help of each argument that has one beyond its short help, under its name.
fn long_helps(out: &mut String, args: &[&Arg]) {
    for arg in args {
        let Some(long) = arg.get_long_help() else {
            continue;
        };
        let form = if arg.is_positional() {
            value_names(arg).join(" ")
        } else {
            option_form(arg)
        };
        let _ = writeln!(out, "`{form}`:\n");
        paragraphs(out, &long.to_string());
    }
}

/// An argument's short help, and the values it takes when they are a fixed list.
fn arg_help(arg: &Arg) -> String {
    let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
    let help = sentence(&help);
    let values = possible_values(arg);
    if values.is_empty() {
        return help;
    }
    format!("{help} Values: {}.", values.join("; "))
}

/// The fixed values an argument takes, each with its help: `` `names`: its path and status ``.
/// A flag, which takes no value, has none.
fn possible_values(arg: &Arg) -> Vec<String> {
    if !arg.get_action().takes_values() || is_bool(arg) {
        return Vec::new();
    }
    let values = arg.get_possible_values();
    values
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| match value.get_help() {
            Some(help) => format!(
                "`{}`: {}",
                value.get_name(),
                help.to_string().trim_end_matches('.')
            ),
            None => format!("`{}`", value.get_name()),
        })
        .collect()
}

/// Whether `arg` takes `true` or `false`, which its help says already.
fn is_bool(arg: &Arg) -> bool {
    let names: Vec<String> = arg
        .get_possible_values()
        .iter()
        .map(|value| value.get_name().to_owned())
        .collect();
    names == ["true", "false"]
}

/// How an option is written: `-n, --limit <N>`, `--pretty[=<BOOL>]`, `--path <P>...`.
fn option_form(arg: &Arg) -> String {
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
fn value_names(arg: &Arg) -> Vec<String> {
    match arg.get_value_names() {
        Some(names) => names.iter().map(ToString::to_string).collect(),
        None => vec![arg.get_id().as_str().to_uppercase()],
    }
}

/// An argument's default, as a table cell: empty when it has none.
fn default_cell(arg: &Arg) -> String {
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| format!("`{}`", value.to_string_lossy()))
        .collect();

    // A flag's implied `false` is no default worth a cell.
    if !arg.get_action().takes_values() {
        return String::new();
    }
    defaults.join(", ")
}

/// Text that ends as a sentence does: clap drops the period of a one-sentence doc comment.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.is_empty() || text.ends_with('.') {
        return text.to_owned();
    }
    format!("{text}.")
}

/// `yes` or `no`.
fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

// ---- errors.md ----

/// Renders `errors.md`.
fn errors_page() -> String {
    let descriptions = code_descriptions();
    let mut out = String::new();

    // 1. Title and how to read the page.
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# Error codes\n");
    let _ = writeln!(
        out,
        "A failed command prints an envelope whose `error` names a stable `E_*` code, the \
         process exit code (`exit`) and whether running the operation again can succeed \
         (`retryable`), with machine-readable `details` and a `hint`. This page is generated \
         from the one table in the code that gives each code its exit code and `retryable` \
         flag; `{BIN} schema ErrorCode` prints the same codes and descriptions. Some codes are \
         defined before any operation raises them: [Known limitations](../known-limitations.md) \
         names them.\n"
    );

    // 2. Exit codes.
    let _ = writeln!(out, "## Exit codes\n");
    let _ = writeln!(out, "| Exit | Meaning | Codes |");
    let _ = writeln!(out, "| --- | --- | --- |");
    for (exit, meaning) in EXITS {
        let codes: Vec<String> = ErrorCode::ALL
            .iter()
            .filter(|code| code.exit() == *exit)
            .map(|code| format!("[`{0}`](#{1})", code.as_str(), slug(code.as_str())))
            .collect();
        let _ = writeln!(out, "| {exit} | {meaning} | {} |", codes.join(", "));
    }
    out.push('\n');

    // 3. Every code at a glance.
    let _ = writeln!(out, "## Codes\n");
    let _ = writeln!(out, "| Code | Exit | Retryable | Description |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for code in ErrorCode::ALL {
        let name = code.as_str();
        let description = descriptions.get(name).map_or("", String::as_str);
        let _ = writeln!(
            out,
            "| [`{name}`](#{}) | {} | {} | {} |",
            slug(name),
            code.exit(),
            yes_no(code.retryable()),
            cell(first_sentence(description))
        );
    }
    out.push('\n');

    // 4. One section per code, with its whole description.
    for code in ErrorCode::ALL {
        let name = code.as_str();
        let _ = writeln!(out, "## {name}\n");
        let _ = writeln!(
            out,
            "Exit {}. Retryable: {}.\n",
            code.exit(),
            yes_no(code.retryable())
        );
        paragraphs(&mut out, descriptions.get(name).map_or("", String::as_str));
    }
    trim_end(out)
}

/// Each code's description, from `ErrorCode`'s JSON Schema: its variants' doc comments.
fn code_descriptions() -> std::collections::BTreeMap<String, String> {
    let all = schema_for_all();
    let variants = all["ErrorCode"]["oneOf"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    variants
        .iter()
        .filter_map(|variant| {
            let code = variant["const"].as_str()?;
            let description = variant["description"].as_str()?;
            Some((code.to_owned(), description.to_owned()))
        })
        .collect()
}

/// The text up to the end of its first sentence: `". "` outside a code span, or the end.
fn first_sentence(text: &str) -> &str {
    let mut in_code = false;
    for (at, c) in text.char_indices() {
        if c == '`' {
            in_code = !in_code;
            continue;
        }
        let ends = c == '.' && !in_code && text[at + 1..].starts_with([' ', '\n']);
        if ends {
            return &text[..=at];
        }
    }
    text
}

// ---- Markdown ----

/// Escapes text for Markdown outside code spans, so that `<name>` stays text rather than HTML
/// and `main~1` no strikethrough; inside a table cell, `|` too.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for (i, part) in text.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }

        // Odd parts are code spans: only a table's `|` needs escaping there.
        if i % 2 == 1 {
            out.push_str(&part.replace('|', "\\|"));
            continue;
        }
        for c in part.chars() {
            match c {
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '\\' | '|' | '*' | '_' | '~' | '[' | ']' => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    out
}

/// Text as one table cell: its lines joined, escaped.
fn cell(text: &str) -> String {
    escape(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Text as Markdown paragraphs: blank lines split them, other line breaks are spaces.
fn paragraphs(out: &mut String, text: &str) {
    for paragraph in text.split("\n\n") {
        let joined = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
        if joined.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{}\n", escape(&joined));
    }
}

/// The page with exactly one newline at its end.
fn trim_end(mut text: String) -> String {
    let kept = text.trim_end().len();
    text.truncate(kept);
    text.push('\n');
    text
}

// ---- Tests ----

/// The first line where `want` and `got` differ, numbered from 1, with both versions.
fn first_difference(want: &str, got: &str) -> String {
    let mut want_lines = want.lines();
    let mut got_lines = got.lines();
    for number in 1.. {
        let (w, g) = (want_lines.next(), got_lines.next());
        if w.is_none() && g.is_none() {
            break;
        }
        if w != g {
            return format!(
                "line {number}:\n  generated: {}\n  committed: {}",
                w.unwrap_or("<end>"),
                g.unwrap_or("<end>")
            );
        }
    }
    "only line endings differ".to_owned()
}

/// Each committed page is what the code renders.
#[test]
fn reference_pages_match() {
    let dir = reference_dir();
    for page in pages() {
        let path = dir.join(page.name);
        let committed = fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == page.text,
            "docs/reference/{} is not what the code renders; run `{REGEN_CMD}`, review the diff \
             and commit it. First difference, {}",
            page.name,
            first_difference(&page.text, &committed)
        );
    }
}

/// Writes the pages again. Writes nothing unless `SWARMOPS_REGEN_REFERENCE=1` is set, so that
/// `--run-ignored all` never rewrites them.
#[test]
#[ignore = "writes docs/reference when SWARMOPS_REGEN_REFERENCE=1; run on purpose"]
fn regen_reference() {
    if std::env::var_os(REGEN_VAR).is_none() {
        eprintln!("regen_reference: {REGEN_VAR} not set; nothing written");
        return;
    }
    let dir = reference_dir();
    fs::create_dir_all(&dir).unwrap();
    for page in pages() {
        fs::write(dir.join(page.name), page.text).unwrap();
    }
}

/// Every command has a section, and every example names a command and a heading that exists.
#[test]
fn examples_name_real_targets() {
    let names: Vec<String> = all_leaves()
        .iter()
        .map(|leaf| leaf.path.join(" "))
        .collect();
    for (path, _, link) in EXAMPLES {
        assert!(
            names.iter().any(|name| name == path),
            "{path} is no command"
        );

        // The target file holds a heading whose anchor the link names.
        let (file, anchor) = link.split_once('#').unwrap();
        let text = fs::read_to_string(reference_dir().join(file)).unwrap();
        let found = text
            .lines()
            .filter(|line| line.starts_with('#'))
            .any(|line| slug(line.trim_start_matches('#').trim()) == anchor);
        assert!(found, "{link}: no such heading");
    }
}

/// Every exit code a code gives has a meaning on the page, and every code a description.
#[test]
fn every_code_is_described() {
    let descriptions = code_descriptions();
    for code in ErrorCode::ALL {
        let exit = code.exit();
        assert!(EXITS.iter().any(|(e, _)| *e == exit), "exit {exit}");
        assert!(
            descriptions.contains_key(code.as_str()),
            "{}",
            code.as_str()
        );
    }
}

/// Markdown escaping keeps code spans and table cells intact.
#[test]
fn escape_keeps_code_spans() {
    assert_eq!(
        escape("main~1 <name> `a_b|c`"),
        "main\\~1 &lt;name&gt; `a_b\\|c`"
    );
    assert_eq!(first_sentence("A `x. y` b. C."), "A `x. y` b.");
    assert_eq!(
        slug("2. Agents commit and land"),
        "2-agents-commit-and-land"
    );
}
