//! One page per command, `docs/reference/commands/<name>.md`, its name the command's path
//! joined by `-` (`blob put` → `blob-put.md`).
//!
//! A page is, in order: the generated marker; the title and the long help; the metadata, a
//! `yaml` block (title, description, group, since, mutates, identity_required, mcp_tool,
//! envelope_op, exit_codes, errors, syntax); Syntax; Required and Optional arguments, `### <arg>` each; Examples, a
//! `### CLI` step of the golden CLI snapshots and a `### MCP` step of the MCP ones; Details,
//! the hand-written note; Return information; Errors; See also.
//!
//! Where each part comes from:
//!
//! ```text
//! clap grammar ─────────────────── syntax, arguments, description
//! META (checked against the code) ─ group, mutates, identity_required, result type
//! tests/snapshots/all__cli__*.snap  CLI example; errors and exit codes the steps show
//! tests/snapshots/all__mcp__*.snap  MCP example; mcp_tool (the tools list)
//! schema_for_all ───────────────── the result's fields
//! notes/commands/<name>.md ──────── Details
//! ```
//!
//! A note is Markdown, spliced as it is under `## Details`; its links are written from its own
//! folder, `docs/reference/notes/commands/`, so each relative one starts with `../`, and one
//! `../` comes off on the page, a folder higher.
//!
//! An MCP transcript snapshot is a JSON array of steps; a step whose `request` is a
//! `tools/call` is the example of the tool it names, its `response` the reply.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use clap::Arg;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use swarmops_api::{ErrorCode, schema_for_all};

use super::{
    BIN, EXAMPLES, EXITS, GENERATED, Leaf, Page, all_leaves, arg_help, cell, code_descriptions,
    default_cell, first_sentence, is_builtin, option_form, paragraphs, reference_dir, sentence,
    slug, trim_end, value_names, yes_no,
};
use crate::cli::{op_name, usage};

/// The command pages' folder in `docs/reference/`.
pub(super) const DIR: &str = "commands";

/// The hand-written notes' folder in `docs/reference/`.
const NOTES_DIR: &str = "notes/commands";

/// The release every command first shipped in, but [`REMOTE_COMMANDS`] and
/// [`WORKSPACE_COMMANDS`].
const SINCE: &str = "0.1.0";

/// The git remotes' commands (M13), and the release they first shipped in.
const REMOTE_COMMANDS: [&str; 8] = [
    "clone",
    "remote add",
    "remote remove",
    "remote list",
    "fetch",
    "push",
    "sync",
    "sync status",
];
const REMOTE_SINCE: &str = "0.2.0";

/// The commands the workspaces brought (M5), and the release they first ship in.
const WORKSPACE_COMMANDS: [&str; 15] = [
    "rebase",
    "blob get",
    "ws create",
    "ws status",
    "ws diff",
    "ws commit",
    "ws rebase",
    "ws resolve",
    "ws land",
    "ws materialize",
    "ws discard",
    "ws list",
    "ws info",
    "ws delete",
    "ws hook",
];
const WORKSPACE_SINCE: &str = "0.3.0";

/// The release the command at `path` first shipped in.
fn since(path: &str) -> &'static str {
    if REMOTE_COMMANDS.contains(&path) {
        return REMOTE_SINCE;
    }
    if WORKSPACE_COMMANDS.contains(&path) {
        return WORKSPACE_SINCE;
    }
    SINCE
}

/// Where insta keeps the integration tests' snapshots, from this crate's root.
const SNAPSHOTS: &str = "tests/snapshots";

/// The start of the CLI tests' snapshot names, and of the MCP tests'.
const CLI_SNAPSHOT: &str = "all__cli__";
const MCP_SNAPSHOT: &str = "all__mcp__";

/// The snapshot of `tools/list`.
const TOOLS_SNAPSHOT: &str = "all__mcp__mcp_tools_list_schema_snapshot.snap";

/// The JSON-RPC method that calls a tool.
const TOOLS_CALL: &str = "tools/call";

/// What a page says where no golden step shows the command yet.
const NO_EXAMPLE: &str = "No recorded example yet.";

/// The errors a command can meet beyond those its steps show: a store it cannot open, and a
/// command line the grammar refuses.
const STORE_ERRORS: [&str; 3] = ["E_NOT_A_REPO", "E_IO", "E_STORE_CORRUPT"];
const USAGE_ERROR: &str = "E_INVALID_ARG";

/// A command's group: the index's sections and the page's `group`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Group {
    Store,
    Write,
    Read,
    Refs,
    Operations,
    Integrity,
    Git,
    Remotes,
    Workspaces,
    Server,
}

impl Group {
    /// Every group, in the index's order.
    pub(super) const ALL: [Group; 10] = [
        Group::Store,
        Group::Write,
        Group::Read,
        Group::Refs,
        Group::Operations,
        Group::Integrity,
        Group::Git,
        Group::Remotes,
        Group::Workspaces,
        Group::Server,
    ];

    /// The group's name in the metadata.
    fn key(self) -> &'static str {
        match self {
            Group::Store => "store",
            Group::Write => "write",
            Group::Read => "read",
            Group::Refs => "refs",
            Group::Operations => "operations",
            Group::Integrity => "integrity",
            Group::Git => "git",
            Group::Remotes => "remotes",
            Group::Workspaces => "workspaces",
            Group::Server => "server",
        }
    }

    /// The group's heading in the index.
    pub(super) fn title(self) -> &'static str {
        match self {
            Group::Store => "Store",
            Group::Write => "Write",
            Group::Read => "Read",
            Group::Refs => "Refs",
            Group::Operations => "Operations",
            Group::Integrity => "Integrity",
            Group::Git => "Git",
            Group::Remotes => "Git remotes",
            Group::Workspaces => "Workspaces",
            Group::Server => "Server",
        }
    }
}

/// What a command's envelope carries as its `result`.
#[derive(Clone, Copy, Debug)]
enum Returns {
    /// One type of the schema.
    Type(&'static str),
    /// JSON Schemas by type name (`schema`).
    Schemas,
    /// No envelope: the MCP protocol (`mcp`).
    Protocol,
}

/// What the code says of a command beyond its grammar. `mutates`: it can change what the store
/// holds, its changesets and refs, or make the store; a write to the store's caches alone does
/// not count. `identity`: it refuses to run without one (`--as` or `SWARMOPS_AGENT`).
struct Meta {
    path: &'static str,
    group: Group,
    mutates: bool,
    identity: bool,
    returns: Returns,
}

/// Every command, in the grammar's order. `meta_matches_the_code` checks each column it can
/// against the code.
const META: [Meta; 48] = [
    meta("init", Group::Store, true, false, Returns::Type("RepoInfo")),
    meta(
        "info",
        Group::Store,
        false,
        false,
        Returns::Type("RepoInfo"),
    ),
    meta(
        "whoami",
        Group::Store,
        false,
        false,
        Returns::Type("IdentityInfo"),
    ),
    meta("schema", Group::Store, false, false, Returns::Schemas),
    meta(
        "commit",
        Group::Write,
        true,
        true,
        Returns::Type("CommitResp"),
    ),
    meta("land", Group::Write, true, true, Returns::Type("LandResp")),
    meta(
        "rebase",
        Group::Write,
        true,
        true,
        Returns::Type("RebaseResp"),
    ),
    meta(
        "blob put",
        Group::Write,
        true,
        true,
        Returns::Type("BlobPutResp"),
    ),
    meta(
        "blob has",
        Group::Write,
        false,
        false,
        Returns::Type("BlobHasResp"),
    ),
    meta(
        "blob get",
        Group::Read,
        false,
        false,
        Returns::Type("BlobGetResp"),
    ),
    meta(
        "ws create",
        Group::Workspaces,
        true,
        true,
        Returns::Type("WsInfo"),
    ),
    meta(
        "ws status",
        Group::Workspaces,
        false,
        false,
        Returns::Type("WsStatus"),
    ),
    meta(
        "ws diff",
        Group::Workspaces,
        false,
        false,
        Returns::Type("Diff"),
    ),
    meta(
        "ws commit",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsCommitResp"),
    ),
    meta(
        "ws rebase",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsRebaseResp"),
    ),
    meta(
        "ws resolve",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsResolveResp"),
    ),
    meta(
        "ws land",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsLandResp"),
    ),
    meta(
        "ws materialize",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsMaterializeResp"),
    ),
    meta(
        "ws discard",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsDiscardResp"),
    ),
    meta(
        "ws list",
        Group::Workspaces,
        false,
        false,
        Returns::Type("WsList"),
    ),
    meta(
        "ws info",
        Group::Workspaces,
        false,
        false,
        Returns::Type("WsInfo"),
    ),
    meta(
        "ws delete",
        Group::Workspaces,
        true,
        false,
        Returns::Type("WsDeleteResp"),
    ),
    meta("ws hook", Group::Workspaces, true, false, Returns::Protocol),
    meta(
        "resolve",
        Group::Read,
        false,
        false,
        Returns::Type("ResolvedRev"),
    ),
    meta(
        "cat",
        Group::Read,
        false,
        false,
        Returns::Type("FileContent"),
    ),
    meta(
        "ls",
        Group::Read,
        false,
        false,
        Returns::Type("TreeListing"),
    ),
    meta(
        "show",
        Group::Read,
        false,
        false,
        Returns::Type("ChangesetDetail"),
    ),
    meta("log", Group::Read, false, false, Returns::Type("LogPage")),
    meta(
        "history",
        Group::Read,
        false,
        false,
        Returns::Type("PathHistoryPage"),
    ),
    meta("diff", Group::Read, false, false, Returns::Type("Diff")),
    meta(
        "ref list",
        Group::Refs,
        false,
        false,
        Returns::Type("RefListResp"),
    ),
    meta(
        "ref set",
        Group::Refs,
        true,
        true,
        Returns::Type("RefSetResp"),
    ),
    meta(
        "ref delete",
        Group::Refs,
        true,
        true,
        Returns::Type("RefSetResp"),
    ),
    meta(
        "op log",
        Group::Operations,
        false,
        false,
        Returns::Type("OpLogPage"),
    ),
    meta(
        "op undo",
        Group::Operations,
        true,
        true,
        Returns::Type("UndoResp"),
    ),
    meta(
        "verify",
        Group::Integrity,
        false,
        false,
        Returns::Type("VerifyReport"),
    ),
    meta(
        "git import",
        Group::Git,
        true,
        true,
        Returns::Type("GitImportResp"),
    ),
    meta(
        "git export",
        Group::Git,
        false,
        false,
        Returns::Type("GitExportResp"),
    ),
    meta(
        "clone",
        Group::Remotes,
        true,
        true,
        Returns::Type("CloneResp"),
    ),
    meta(
        "remote add",
        Group::Remotes,
        true,
        false,
        Returns::Type("RemoteInfo"),
    ),
    meta(
        "remote remove",
        Group::Remotes,
        true,
        false,
        Returns::Type("RemoteRemoveResp"),
    ),
    meta(
        "remote list",
        Group::Remotes,
        false,
        false,
        Returns::Type("RemoteList"),
    ),
    meta(
        "fetch",
        Group::Remotes,
        true,
        false,
        Returns::Type("FetchResp"),
    ),
    meta(
        "push",
        Group::Remotes,
        true,
        false,
        Returns::Type("PushResp"),
    ),
    meta(
        "sync",
        Group::Remotes,
        true,
        true,
        Returns::Type("SyncResp"),
    ),
    meta(
        "sync status",
        Group::Remotes,
        false,
        false,
        Returns::Type("SyncStatus"),
    ),
    meta("mcp", Group::Server, true, false, Returns::Protocol),
    meta(
        "derive",
        Group::Store,
        false,
        false,
        Returns::Type("DeriveResp"),
    ),
];

/// A row of [`META`].
const fn meta(
    path: &'static str,
    group: Group,
    mutates: bool,
    identity: bool,
    returns: Returns,
) -> Meta {
    Meta {
        path,
        group,
        mutates,
        identity,
        returns,
    }
}

/// The group of the command at `path`, its words joined by spaces.
pub(super) fn group_of(path: &str) -> Group {
    lookup(path).group
}

/// The row of the command at `path`.
fn lookup(path: &str) -> &'static Meta {
    META.iter()
        .find(|meta| meta.path == path)
        .unwrap_or_else(|| panic!("{path}: no row in META"))
}

/// A command's page name in [`DIR`]: `blob put` → `blob-put.md`.
pub(super) fn file_name(path: &[String]) -> String {
    format!("{}.md", path.join("-"))
}

// ---- The golden steps ----

/// One CLI step: a command line and the envelope it printed.
struct Step {
    /// The command line as a shell runs it: the variables it set, `swarmops` and its words.
    line: String,
    /// What it read on stdin, if anything.
    stdin: Option<String>,
    exit: i64,
    /// Its envelope's `op`.
    op: String,
    /// Its envelope as the snapshot holds it, keys in the order printed.
    envelope: String,
    /// Its error's `E_*` code, when it failed.
    code: Option<String>,
}

/// A CLI step as its snapshot holds it.
#[derive(Deserialize)]
struct RawStep<'a> {
    args: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(borrow, default)]
    stdin: Option<&'a RawValue>,
    exit: i64,
    #[serde(borrow)]
    stdout: &'a RawValue,
}

/// One MCP step: a `tools/call` and its reply.
struct Call {
    tool: String,
    request: String,
    response: String,
}

/// An MCP step as its snapshot holds it.
#[derive(Deserialize)]
struct RawCall<'a> {
    #[serde(borrow)]
    request: &'a RawValue,
    #[serde(borrow)]
    response: &'a RawValue,
}

/// Every golden step, what the pages show of them.
struct Golden {
    steps: Vec<Step>,
    calls: Vec<Call>,
    /// Each tool's name and whether it only reads, from the tools list.
    tools: BTreeMap<String, bool>,
}

impl Golden {
    /// The steps of every snapshot, read once.
    fn read() -> Self {
        let mut steps = Vec::new();
        let mut calls = Vec::new();

        // 1. Each transcript, CLI or MCP, step by step.
        for name in snapshot_names() {
            let text = read_snapshot(&name);
            let body = snapshot_body(&name, &text);
            if !body.starts_with('[') {
                continue;
            }
            if name.starts_with(CLI_SNAPSHOT) {
                steps.extend(cli_steps(&name, body));
            }
            if name.starts_with(MCP_SNAPSHOT) {
                calls.extend(mcp_calls(&name, body));
            }
        }

        // 2. The tools, from the tools list.
        let text = read_snapshot(TOOLS_SNAPSHOT);
        let listed: Value = serde_json::from_str(snapshot_body(TOOLS_SNAPSHOT, &text))
            .unwrap_or_else(|e| panic!("{TOOLS_SNAPSHOT}: {e}"));
        let tools = listed["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{TOOLS_SNAPSHOT}: no tools"))
            .iter()
            .map(|tool| {
                let name = tool["name"].as_str().unwrap_or_default().to_owned();
                let reads = tool["annotations"]["readOnlyHint"] == Value::Bool(true);
                (name, reads)
            })
            .collect();
        Self {
            steps,
            calls,
            tools,
        }
    }

    /// The steps whose envelope names operation `op`.
    fn steps_of<'a>(&'a self, op: &'a str) -> impl Iterator<Item = &'a Step> + 'a {
        self.steps.iter().filter(move |step| step.op == op)
    }

    /// The `E_*` codes the steps of `op` show, in the error table's order.
    fn codes_of(&self, op: &str) -> Vec<ErrorCode> {
        ErrorCode::ALL
            .iter()
            .copied()
            .filter(|code| {
                self.steps_of(op)
                    .any(|step| step.code.as_deref() == Some(code.as_str()))
            })
            .collect()
    }
}

/// The names of the CLI and MCP tests' snapshots, in order.
fn snapshot_names() -> Vec<String> {
    let dir = format!("{}/{SNAPSHOTS}", env!("CARGO_MANIFEST_DIR"));
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("{SNAPSHOTS}: {e}"));
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("a directory entry"))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(CLI_SNAPSHOT) || name.starts_with(MCP_SNAPSHOT))
        .filter(|name| Path::new(name).extension() == Some(OsStr::new("snap")))
        .collect();
    names.sort();
    names
}

/// The text of snapshot `name`.
fn read_snapshot(name: &str) -> String {
    let path = format!("{}/{SNAPSHOTS}/{name}", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{SNAPSHOTS}/{name}: {e}"))
}

/// Snapshot `name`'s value: `text` without insta's header.
fn snapshot_body<'a>(name: &str, text: &'a str) -> &'a str {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or_else(
            || panic!("{SNAPSHOTS}/{name}: no insta header"),
            |(_, body)| body,
        )
}

/// The steps of CLI transcript `name`, whose value is `body`.
fn cli_steps(name: &str, body: &str) -> Vec<Step> {
    let raws: Vec<RawStep<'_>> =
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{SNAPSHOTS}/{name}: {e}"));
    raws.iter()
        .map(|raw| {
            // The envelope as a value for its op and code, as text for the order of its keys.
            let envelope: Value = serde_json::from_str(raw.stdout.get())
                .unwrap_or_else(|e| panic!("{SNAPSHOTS}/{name}: a step's stdout: {e}"));
            let code = envelope["error"]["code"].as_str().map(str::to_owned);
            Step {
                line: command_line(&raw.env, &raw.args),
                stdin: raw.stdin.map(stdin_text),
                exit: raw.exit,
                op: envelope["op"].as_str().unwrap_or_default().to_owned(),
                envelope: dedent(raw.stdout.get()),
                code,
            }
        })
        .collect()
}

/// The `tools/call` steps of MCP transcript `name`, whose value is `body`; other steps, and
/// other arrays, hold none.
fn mcp_calls(name: &str, body: &str) -> Vec<Call> {
    let Ok(raws) = serde_json::from_str::<Vec<Box<RawValue>>>(body) else {
        return Vec::new();
    };
    raws.iter()
        .filter_map(|raw| serde_json::from_str::<RawCall<'_>>(raw.get()).ok())
        .filter_map(|raw| {
            let request: Value = serde_json::from_str(raw.request.get())
                .unwrap_or_else(|e| panic!("{SNAPSHOTS}/{name}: a request: {e}"));
            if request["method"] != TOOLS_CALL {
                return None;
            }
            Some(Call {
                tool: request["params"]["name"].as_str()?.to_owned(),
                request: dedent(raw.request.get()),
                response: dedent(raw.response.get()),
            })
        })
        .collect()
}

/// A step's command line: `env`'s assignments, `swarmops`, then `args`, which the snapshot
/// holds quoted for a shell already.
fn command_line(env: &BTreeMap<String, String>, args: &str) -> String {
    let mut words: Vec<String> = env
        .iter()
        .map(|(name, value)| format!("{name}={}", shell_word(value)))
        .collect();
    words.push(BIN.to_owned());
    if !args.is_empty() {
        words.push(args.to_owned());
    }
    words.join(" ")
}

/// `word` as a shell word: as it is when made of `[A-Za-z0-9-_./:=~[]@]` alone, else quoted.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=~[]@".contains(&b));
    if plain {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// What a step read on stdin: text as it was, JSON as the snapshot indents it.
fn stdin_text(raw: &RawValue) -> String {
    serde_json::from_str::<String>(raw.get()).unwrap_or_else(|_| dedent(raw.get()))
}

/// `json`, a value cut from inside an indented document, with its last line's indentation
/// taken off each line after the first.
fn dedent(json: &str) -> String {
    let last = json.lines().last().unwrap_or_default();
    let indent = " ".repeat(last.len() - last.trim_start().len());
    let lines: Vec<&str> = json
        .lines()
        .enumerate()
        .map(|(i, line)| match i {
            0 => line,
            _ => line.strip_prefix(&indent).unwrap_or(line),
        })
        .collect();
    lines.join("\n")
}

// ---- The pages ----

/// Every command's page.
pub(super) fn pages() -> Vec<Page> {
    let golden = Golden::read();
    let leaves = all_leaves();
    leaves
        .iter()
        .map(|leaf| Page {
            name: format!("{DIR}/{}", file_name(&leaf.path)),
            text: render(leaf, &leaves, &golden),
        })
        .collect()
}

/// What a page needs of one command, gathered once.
struct Entry<'a> {
    leaf: &'a Leaf,
    name: String,
    meta: &'static Meta,
    op: &'static str,
    /// Its MCP tool's name, when it has one.
    tool: Option<&'a str>,
    codes: Vec<ErrorCode>,
    exits: Vec<i32>,
    syntax: String,
}

impl<'a> Entry<'a> {
    /// The command of `leaf`, with what the golden steps show of it.
    fn new(leaf: &'a Leaf, golden: &'a Golden) -> Self {
        let name = leaf.path.join(" ");
        let op = op_name(&leaf.path).unwrap_or_default();
        let codes = golden.codes_of(op);

        // Success, then the exits of the errors its steps show.
        let mut exits: Vec<i32> = codes.iter().map(ErrorCode::exit).collect();
        exits.push(0);
        exits.sort_unstable();
        exits.dedup();

        let line = usage(Some(BIN), &leaf.path);
        let tool = golden
            .tools
            .get_key_value(op)
            .map(|(tool, _)| tool.as_str());
        Self {
            leaf,
            meta: lookup(&name),
            name,
            op,
            tool,
            codes,
            exits,
            syntax: line.strip_prefix("Usage: ").unwrap_or(&line).to_owned(),
        }
    }
}

/// Renders one command's page.
fn render(leaf: &Leaf, leaves: &[Leaf], golden: &Golden) -> String {
    let command = Entry::new(leaf, golden);
    let mut out = String::new();

    // 1. Marker, title, what the command does, and its metadata.
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# {BIN} {}\n", command.name);
    let about = leaf
        .cmd
        .get_long_about()
        .or(leaf.cmd.get_about())
        .map(ToString::to_string)
        .unwrap_or_default();
    paragraphs(&mut out, &sentence(&about));
    metadata(&mut out, &command);

    // 2. Syntax and arguments, from the grammar.
    let _ = writeln!(out, "## Syntax\n");
    let _ = writeln!(out, "```text\n{}\n```\n", command.syntax);
    arguments(&mut out, &leaf.cmd);

    // 3. Examples, from the golden steps.
    examples(&mut out, &command, golden);

    // 4. The hand-written note, then what the command returns and how it fails.
    details(&mut out, &command.leaf.path);
    returns(&mut out, &command);
    errors(&mut out, &command);
    see_also(&mut out, &command, leaves);
    trim_end(out)
}

/// The page's metadata, a `yaml` block whose strings are JSON strings. Not front matter: that
/// must open the file, where the generated marker is, and GitHub and the site read no front
/// matter after it.
fn metadata(out: &mut String, command: &Entry<'_>) {
    let about = command
        .leaf
        .cmd
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    let codes: Vec<&str> = command.codes.iter().map(ErrorCode::as_str).collect();
    let exits: Vec<String> = command.exits.iter().map(ToString::to_string).collect();

    // One `key: value` line each, in the contract's order.
    let fields = [
        ("title", quoted(&format!("{BIN} {}", command.name))),
        ("description", quoted(&sentence(&about))),
        ("group", quoted(command.meta.group.key())),
        ("since", quoted(since(&command.name))),
        ("mutates", command.meta.mutates.to_string()),
        ("identity_required", command.meta.identity.to_string()),
        ("mcp_tool", quoted(command.tool.unwrap_or_default())),
        ("envelope_op", quoted(command.op)),
        ("exit_codes", format!("[{}]", exits.join(", "))),
        ("errors", format!("[{}]", codes.join(", "))),
        ("syntax", quoted(&command.syntax)),
    ];
    let _ = writeln!(out, "```yaml");
    for (key, value) in fields {
        let _ = writeln!(out, "{key}: {value}");
    }
    let _ = writeln!(out, "```\n");
}

/// `text` as a double-quoted YAML scalar, which a JSON string is.
fn quoted(text: &str) -> String {
    Value::from(text).to_string()
}

// ---- Arguments ----

/// The required arguments, then the optional ones, one `### <arg>` each.
fn arguments(out: &mut String, cmd: &clap::Command) {
    let own: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|arg| !arg.is_global_set() && !is_builtin(arg))
        .collect();

    // 1. A group the command needs one of (`ref set`: `--expect` or `--create`) is required.
    let grouped: Vec<&Arg> = own
        .iter()
        .copied()
        .filter(|arg| in_required_group(cmd, arg))
        .collect();
    let required: Vec<&Arg> = own
        .iter()
        .copied()
        .filter(|arg| arg.is_required_set() && !grouped.contains(arg))
        .collect();
    let optional: Vec<&Arg> = own
        .iter()
        .copied()
        .filter(|arg| !arg.is_required_set() && !grouped.contains(arg))
        .collect();

    // 2. Required: each argument, then each group's.
    let _ = writeln!(out, "## Required arguments\n");
    if required.is_empty() && grouped.is_empty() {
        let _ = writeln!(out, "None.\n");
    }
    for arg in &required {
        argument(out, arg);
    }
    required_groups(out, cmd);
    for arg in &grouped {
        argument(out, arg);
    }

    // 3. Optional, and the global options every command takes.
    let _ = writeln!(out, "## Optional arguments\n");
    let _ = writeln!(
        out,
        "The command also takes the [global options](../cli.md#global-options).\n"
    );
    for arg in &optional {
        argument(out, arg);
    }
}

/// Whether `arg` belongs to a group of options `cmd` requires one of.
fn in_required_group(cmd: &clap::Command, arg: &Arg) -> bool {
    cmd.get_groups()
        .filter(|group| group.is_required_set())
        .any(|group| group.get_args().any(|id| id == arg.get_id()))
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

/// One argument: its name as the heading, how it is written, its default, its help.
fn argument(out: &mut String, arg: &Arg) {
    let (heading, form) = if arg.is_positional() {
        let names = value_names(arg).join(" ");
        let form = format!("<{names}>");
        (names, form)
    } else {
        (arg_name(arg), option_form(arg))
    };
    let _ = writeln!(out, "### `{heading}`\n");

    // 1. How it is written, and its default when it has one.
    let _ = writeln!(out, "Syntax: `{form}`.");
    let default = default_cell(arg);
    if !default.is_empty() {
        let _ = writeln!(out, "Default: {default}.");
    }
    out.push('\n');

    // 2. Its help: the long one when it has one, else the short one with its values.
    match arg.get_long_help() {
        Some(long) => paragraphs(out, &long.to_string()),
        None => paragraphs(out, &arg_help(arg)),
    }
}

/// An option's name as a heading: `--message`, or `-m` for one with no long form.
fn arg_name(arg: &Arg) -> String {
    match (arg.get_long(), arg.get_short()) {
        (Some(long), _) => format!("--{long}"),
        (None, Some(short)) => format!("-{short}"),
        (None, None) => arg.get_id().as_str().to_owned(),
    }
}

// ---- Examples ----

/// The sentence under every recorded example: what the bracketed names stand for.
const PLACEHOLDERS: &str = "Recorded by the CLI tests. A name in square brackets, such as \
`[tmp]` or `[c1]`, stands for what differs from run to run: the test's temporary directory, an \
id, a time.";

/// A CLI step of the command, and an MCP call of its tool.
fn examples(out: &mut String, command: &Entry<'_>, golden: &Golden) {
    let _ = writeln!(out, "## Examples\n");

    // 1. CLI: the first step that succeeded, else the first that ran it.
    let _ = writeln!(out, "### CLI\n");
    let chosen = golden
        .steps_of(command.op)
        .find(|step| step.exit == 0)
        .or_else(|| golden.steps_of(command.op).next());
    match chosen {
        Some(step) => cli_example(out, step),
        None => {
            let _ = writeln!(out, "{NO_EXAMPLE}\n");
        }
    }

    // 2. MCP, for a command with a tool.
    let Some(tool) = command.tool else {
        return;
    };
    let _ = writeln!(out, "### MCP\n");
    match golden.calls.iter().find(|call| call.tool == tool) {
        Some(call) => {
            let _ = writeln!(out, "```json\n{}\n```\n", call.request);
            let _ = writeln!(out, "```json\n{}\n```\n", call.response);
        }
        None => {
            let _ = writeln!(out, "{NO_EXAMPLE}\n");
        }
    }
}

/// One CLI step: the command line, with stdin as a here-document, then what it printed.
fn cli_example(out: &mut String, step: &Step) {
    match &step.stdin {
        Some(stdin) => {
            let _ = writeln!(out, "```sh\n{} <<'EOF'\n{stdin}\nEOF\n```\n", step.line);
        }
        None => {
            let _ = writeln!(out, "```sh\n{}\n```\n", step.line);
        }
    }
    let _ = writeln!(out, "```json\n{}\n```\n", step.envelope);
    let _ = writeln!(out, "Exit code {}. {PLACEHOLDERS}\n", step.exit);
}

// ---- Details ----

/// The hand-written note of the command at `path`, when it has one.
fn details(out: &mut String, path: &[String]) {
    let file = reference_dir().join(NOTES_DIR).join(file_name(path));
    let Ok(note) = fs::read_to_string(&file) else {
        return;
    };
    let _ = writeln!(out, "## Details\n");
    let _ = writeln!(out, "{}\n", relink(&file_name(path), note.trim()));
}

/// A note's text as the page shows it, a folder higher: one `../` off each relative link.
fn relink(note: &str, text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        // 1. The next inline link `](` or reference definition `]: `, whichever comes first.
        let next = [
            rest.find("](").map(|at| at + 2),
            rest.find("]: ").map(|at| at + 3),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(start) = next else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start..];

        // 2. Fragments and URLs stay; a relative link climbs out of the notes folder.
        let target = rest.split([')', ' ', '\n']).next().unwrap_or_default();
        if target.starts_with('#') || target.contains("://") || target.starts_with("mailto:") {
            continue;
        }
        let Some(kept) = rest.strip_prefix("../") else {
            panic!(
                "{NOTES_DIR}/{note}: a relative link starts with ../, as from the note's folder"
            );
        };
        rest = kept;
    }
    out.push_str(rest);
    out
}

// ---- Return information ----

/// The result's type and fields, the envelope's `op` and the exit codes.
fn returns(out: &mut String, command: &Entry<'_>) {
    let _ = writeln!(out, "## Return information\n");

    // 1. The result.
    match command.meta.returns {
        Returns::Type(name) => result_fields(out, name),
        Returns::Schemas => {
            let _ = writeln!(
                out,
                "`result` is an object of JSON Schemas by type name: TYPE's alone, or every \
                 type's.\n"
            );
        }
        Returns::Protocol => {
            let _ = writeln!(
                out,
                "`{BIN} mcp` prints no envelope: stdout carries the MCP protocol until stdin \
                 ends, and a command line it refuses has its envelope on stderr.\n"
            );
        }
    }

    // 2. The envelope's `op`, and each exit code with its meaning.
    let _ = writeln!(out, "Envelope `op`: `{}`.\n", command.op);
    let _ = writeln!(out, "Exit codes:\n");
    for exit in &command.exits {
        let meaning = EXITS
            .iter()
            .find(|(code, _)| code == exit)
            .map_or("", |(_, meaning)| meaning);
        let _ = writeln!(out, "- `{exit}`: {meaning}");
    }
    out.push('\n');
}

/// `result`'s type, and a table of its fields from the type's JSON Schema.
fn result_fields(out: &mut String, name: &str) {
    let all = schema_for_all();
    let schema = &all[name];
    let _ = writeln!(
        out,
        "`result` is a `{name}` (`{BIN} schema {name}` prints its JSON Schema).\n"
    );

    // One row per field, in the schema's order.
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let _ = writeln!(out, "| Field | Type | Required | Description |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for (field, property) in properties {
        let kind = type_name(property);
        let needed = yes_no(required.contains(&field.as_str()));
        let text = cell(property["description"].as_str().unwrap_or_default());
        let _ = writeln!(out, "| `{field}` | `{kind}` | {needed} | {text} |");
    }
    out.push('\n');
}

/// A property's type as the page writes it: `string or null`, `array of ChangesetSummary`.
fn type_name(schema: &Value) -> String {
    // 1. A reference names its type.
    if let Some(reference) = schema["$ref"].as_str() {
        return reference.rsplit('/').next().unwrap_or(reference).to_owned();
    }

    // 2. A choice, by its variants, each named once.
    let variants = schema["anyOf"].as_array().or(schema["oneOf"].as_array());
    if let Some(variants) = variants {
        let mut names: Vec<String> = Vec::new();
        for name in variants.iter().map(type_name) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        return names.join(" or ");
    }

    // 3. A JSON type, or several; an array by its items.
    let names: Vec<String> = match &schema["type"] {
        Value::String(name) => vec![name.clone()],
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => return "any".to_owned(),
    };
    names
        .iter()
        .map(|name| match name.as_str() {
            "array" => format!("array of {}", type_name(&schema["items"])),
            _ => name.clone(),
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

// ---- Errors and See also ----

/// The errors the golden steps show the command failing with, then those any command meets.
fn errors(out: &mut String, command: &Entry<'_>) {
    let descriptions = code_descriptions();
    let _ = writeln!(out, "## Errors\n");

    // 1. What the steps show.
    if command.codes.is_empty() {
        let _ = writeln!(out, "No golden step shows this command failing yet.\n");
    }
    for code in &command.codes {
        let name = code.as_str();
        let description = descriptions.get(name).map_or("", String::as_str);
        let _ = writeln!(
            out,
            "- {} (exit {}): {}",
            code_link(name),
            code.exit(),
            cell(first_sentence(description))
        );
    }
    if !command.codes.is_empty() {
        out.push('\n');
    }

    // 2. What any command can meet.
    let links: Vec<String> = STORE_ERRORS.iter().map(|code| code_link(code)).collect();
    let (last, first) = links.split_last().expect("store errors");
    let _ = writeln!(
        out,
        "Any command that opens a store may also fail with a store error, such as {} or {last}; \
         a command line the grammar refuses is {} (exit 2).\n",
        first.join(", "),
        code_link(USAGE_ERROR)
    );
}

/// A link to code `name`'s entry on the errors page.
fn code_link(name: &str) -> String {
    format!("[`{name}`](../errors.md#{})", slug(name))
}

/// The worked example, the commands of the same group, and the index pages.
fn see_also(out: &mut String, command: &Entry<'_>, leaves: &[Leaf]) {
    let _ = writeln!(out, "## See also\n");

    // 1. Where a guide runs the command.
    let example = EXAMPLES.iter().find(|(path, _, _)| *path == command.name);
    if let Some((_, text, link)) = example {
        let _ = writeln!(out, "- [{text}](../{link})");
    }

    // 2. The group's other commands.
    let siblings = leaves.iter().filter(|leaf| {
        let name = leaf.path.join(" ");
        name != command.name && group_of(&name) == command.meta.group
    });
    for leaf in siblings {
        let _ = writeln!(
            out,
            "- [`{BIN} {}`]({})",
            leaf.path.join(" "),
            file_name(&leaf.path)
        );
    }

    // 3. The index pages.
    let _ = writeln!(out, "- [CLI reference](../cli.md)");
    let _ = writeln!(out, "- [Error codes](../errors.md)");
}

// ---- Tests ----

/// The source of the crates' code a test reads, from this crate's directory.
fn source(path: &str) -> String {
    let full = format!("{}/{path}", env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(&full).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Where each result type is returned: the file, and the function whose signature names it.
const RETURN_SITES: [(&str, &str, &str); 45] = [
    ("init", "src/commands.rs", "fn init("),
    ("info", "../swarmops-api/src/ops.rs", "fn repo_info("),
    (
        "whoami",
        "../swarmops-ops/src/identity.rs",
        "fn identity_info(",
    ),
    ("commit", "../swarmops-api/src/ops.rs", "fn commit("),
    ("land", "../swarmops-api/src/ops.rs", "fn land("),
    ("rebase", "../swarmops-api/src/ops.rs", "fn rebase("),
    ("blob put", "../swarmops-api/src/ops.rs", "fn blob_put("),
    ("blob has", "../swarmops-api/src/ops.rs", "fn blob_has("),
    ("blob get", "../swarmops-api/src/ops.rs", "fn blob_get("),
    (
        "ws create",
        "../swarmops-ws/src/create.rs",
        "pub fn create(",
    ),
    (
        "ws status",
        "../swarmops-ws/src/status.rs",
        "pub fn status(",
    ),
    ("ws diff", "../swarmops-ws/src/diff.rs", "pub fn diff("),
    (
        "ws commit",
        "../swarmops-ws/src/commit.rs",
        "pub fn commit(",
    ),
    (
        "ws rebase",
        "../swarmops-ws/src/rebase.rs",
        "pub fn rebase(",
    ),
    (
        "ws resolve",
        "../swarmops-ws/src/resolve.rs",
        "pub fn resolve(",
    ),
    ("ws land", "../swarmops-ws/src/land.rs", "pub fn land("),
    (
        "ws materialize",
        "../swarmops-ws/src/materialize.rs",
        "pub fn materialize(",
    ),
    (
        "ws discard",
        "../swarmops-ws/src/discard.rs",
        "pub fn discard(",
    ),
    ("ws list", "../swarmops-ws/src/list.rs", "pub fn list("),
    ("ws info", "../swarmops-ws/src/info.rs", "pub fn info("),
    (
        "ws delete",
        "../swarmops-ws/src/delete.rs",
        "pub fn delete(",
    ),
    ("resolve", "../swarmops-api/src/ops.rs", "fn resolve_rev("),
    ("cat", "../swarmops-api/src/ops.rs", "fn read_file("),
    ("ls", "../swarmops-api/src/ops.rs", "fn read_tree("),
    ("show", "../swarmops-api/src/ops.rs", "fn show("),
    ("log", "../swarmops-api/src/ops.rs", "fn log("),
    ("history", "../swarmops-api/src/ops.rs", "fn path_history("),
    ("diff", "../swarmops-api/src/ops.rs", "fn diff("),
    ("ref list", "../swarmops-api/src/ops.rs", "fn ref_list("),
    ("ref set", "../swarmops-api/src/ops.rs", "fn ref_set("),
    ("ref delete", "../swarmops-api/src/ops.rs", "fn ref_delete("),
    ("op log", "../swarmops-api/src/ops.rs", "fn op_log("),
    ("op undo", "../swarmops-api/src/ops.rs", "fn undo("),
    ("verify", "../swarmops-api/src/ops.rs", "fn verify("),
    ("git import", "../swarmops-ops/src/git.rs", "fn git_import("),
    ("git export", "../swarmops-ops/src/git.rs", "fn git_export("),
    ("clone", "src/commands.rs", "fn clone("),
    (
        "remote add",
        "../swarmops-ops/src/remote.rs",
        "fn remote_add(",
    ),
    (
        "remote remove",
        "../swarmops-ops/src/remote.rs",
        "fn remote_remove(",
    ),
    (
        "remote list",
        "../swarmops-ops/src/remote.rs",
        "fn remote_list(",
    ),
    ("fetch", "../swarmops-ops/src/remote.rs", "fn fetch("),
    ("push", "../swarmops-ops/src/push.rs", "fn push("),
    ("sync", "../swarmops-ops/src/sync.rs", "fn sync("),
    (
        "sync status",
        "../swarmops-ops/src/sync.rs",
        "fn sync_status(",
    ),
    ("derive", "../swarmops-api/src/ops.rs", "fn derive("),
];

/// The calls that refuse a command without an identity.
const IDENTITY_CALL: &str = "context::mutator(";

/// The code's rule that ties `mutates` to `identity`: a command that changes the store needs an
/// identity, so the two columns agree but for [`STORE_MAKER`] and [`SERVER`].
const CHANGE_RULE: (&str, &str) = (
    "../swarmops-ops/src/identity.rs",
    "this operation changes the store, so it needs the caller's identity",
);

/// The commands that write only the store's caches: no change under [`CHANGE_RULE`].
const CACHE_ONLY: [(&str, &str); 2] = [
    (
        "derive",
        "It writes only the store's cache of derived values, so it needs",
    ),
    (
        "git export",
        "It needs no identity: it writes no journal frame, only",
    ),
];

/// Where [`CACHE_ONLY`]'s phrases are.
const CACHE_ONLY_SOURCE: &str = "../swarmops-ops/src/local.rs";

/// The git remotes' commands that change the store's files with no identity (M13): each, the
/// operation's file, and the phrase of its doc comment that says why it needs none. They are
/// exceptions to [`CHANGE_RULE`]: a fetch's import moves no ref and records no agent, a push is
/// an export, and `remote add|remove` write the remote configuration and the mirror.
const NO_IDENTITY: [(&str, &str, &str); 4] = [
    (
        "fetch",
        "../swarmops-ops/src/remote.rs",
        "it moves no ref and records no agent",
    ),
    (
        "push",
        "../swarmops-ops/src/push.rs",
        "It needs no identity: it is an export, as git export is",
    ),
    (
        "remote add",
        "../swarmops-ops/src/remote.rs",
        "it writes remotes.toml and the mirror",
    ),
    (
        "remote remove",
        "../swarmops-ops/src/remote.rs",
        "it writes remotes.toml and the mirror",
    ),
];

/// The workspace commands that change the store or the workspace with no identity of their own:
/// a mutation without one acts as the workspace's `created_by` (M5 design §2.6), where the
/// workspace is opened. Exceptions to [`CHANGE_RULE`], but `ws create`, which records who made
/// the workspace and so needs one.
const WORKSPACE_ACTS_AS: [&str; 8] = [
    "ws commit",
    "ws rebase",
    "ws resolve",
    "ws land",
    "ws materialize",
    "ws discard",
    "ws delete",
    "ws hook",
];

/// Where [`WORKSPACE_ACTS_AS`]'s rule is, and its phrase.
const WORKSPACE_ACTS_SOURCE: (&str, &str) = (
    "../swarmops-ws/src/workspace.rs",
    "its identity `ctx.who` or else its `created_by`",
);

/// The command that makes a store, so changes it with no identity, and where its code says so.
const STORE_MAKER: (&str, &str, &str) = ("init", "src/commands.rs", "`init`: a store in");

/// The command that serves the MCP tools: it mutates when one of them does.
const SERVER: &str = "mcp";

/// META names each command once, in the grammar's order, and each column the code can tell
/// agrees with it.
#[test]
fn meta_matches_the_code() {
    let golden = Golden::read();

    // 1. One row per command, in order.
    let names: Vec<String> = all_leaves().iter().map(|l| l.path.join(" ")).collect();
    let rows: Vec<&str> = META.iter().map(|meta| meta.path).collect();
    assert_eq!(names, rows, "META's rows are not the grammar's commands");

    // 2. A command with a tool mutates exactly when the tool does not only read.
    for meta in &META {
        let op = op_name(&meta.path.split(' ').collect::<Vec<_>>()).unwrap();
        if let Some(reads) = golden.tools.get(op) {
            assert_eq!(meta.mutates, !reads, "{}: mutates", meta.path);
        }
    }

    // 3. The identity column: one mutator call per command that needs one, and every
    //    command a step shows refused for want of one.
    let calls = source("src/commands.rs").matches(IDENTITY_CALL).count()
        + source("src/commands/ws.rs").matches(IDENTITY_CALL).count();
    let needs = META.iter().filter(|meta| meta.identity).count();
    assert_eq!(calls, needs, "{IDENTITY_CALL} calls against identity rows");
    for step in &golden.steps {
        if step.code.as_deref() != Some(ErrorCode::IdentityRequired.as_str()) {
            continue;
        }
        let meta = META
            .iter()
            .find(|meta| {
                op_name(&meta.path.split(' ').collect::<Vec<_>>()) == Some(step.op.as_str())
            })
            .unwrap();
        assert!(meta.identity, "{}: refused without an identity", meta.path);
    }

    // 4. The mutates column of the rows no tool checks: the change rule, cache writes aside.
    let (rule_file, rule) = CHANGE_RULE;
    assert!(source(rule_file).contains(rule), "{rule_file}: no {rule:?}");
    let local = source(CACHE_ONLY_SOURCE);
    for (path, phrase) in CACHE_ONLY {
        assert!(local.contains(phrase), "{CACHE_ONLY_SOURCE}: no {phrase:?}");
        assert!(!lookup(path).mutates, "{path}: writes only caches");
    }
    let (maker, maker_file, maker_doc) = STORE_MAKER;
    assert!(
        source(maker_file).contains(maker_doc),
        "{maker_file}: no {maker_doc:?}"
    );
    assert!(lookup(maker).mutates, "{maker}: makes a store");
    let serves_writes = golden.tools.values().any(|reads| !reads);
    assert_eq!(lookup(SERVER).mutates, serves_writes, "{SERVER}: mutates");
    for (path, file, phrase) in NO_IDENTITY {
        assert!(source(file).contains(phrase), "{file}: no {phrase:?}");
        let meta = lookup(path);
        assert!(
            meta.mutates && !meta.identity,
            "{path}: changes with no identity"
        );
    }
    let (acts_file, acts_phrase) = WORKSPACE_ACTS_SOURCE;
    assert!(
        source(acts_file).contains(acts_phrase),
        "{acts_file}: no {acts_phrase:?}"
    );
    for path in WORKSPACE_ACTS_AS {
        let meta = lookup(path);
        assert!(
            meta.mutates && !meta.identity,
            "{path}: changes with no identity"
        );
    }
    for meta in &META {
        let exempt = NO_IDENTITY.iter().any(|(path, _, _)| *path == meta.path)
            || WORKSPACE_ACTS_AS.contains(&meta.path);
        if meta.path == maker || meta.path == SERVER || exempt {
            continue;
        }
        assert_eq!(meta.mutates, meta.identity, "{}: mutates", meta.path);
    }
}

/// Each result type is the one its operation's signature returns, and in the schema.
#[test]
fn result_types_match_the_code() {
    let all = schema_for_all();
    for meta in &META {
        let Returns::Type(name) = meta.returns else {
            continue;
        };
        assert!(all.get(name).is_some(), "{name}: not in the schema");

        // The signature runs from `fn name(` to the body or the `;`.
        let (_, file, function) = RETURN_SITES
            .iter()
            .find(|(path, _, _)| *path == meta.path)
            .unwrap_or_else(|| panic!("{}: no return site", meta.path));
        let text = source(file);
        let at = text
            .find(function)
            .unwrap_or_else(|| panic!("{file}: no {function}"));
        let end = text[at..]
            .find(['{', ';'])
            .map_or(text.len(), |end| at + end);
        let signature = &text[at..end];
        let returned = signature.contains(&format!("Result<{name},"))
            || signature.contains(&format!("-> {name}"));
        assert!(
            returned,
            "{}: {signature} does not return {name}",
            meta.path
        );
    }
}

/// The codes every page names exist, and a note's links move a folder up.
#[test]
fn page_parts_are_sound() {
    for name in STORE_ERRORS.iter().chain([&USAGE_ERROR]) {
        assert!(
            ErrorCode::ALL.iter().any(|code| code.as_str() == *name),
            "{name}"
        );
    }
    assert_eq!(
        relink(
            "x.md",
            "[a](../../guides/a.md) [b](#b) [c](https://c.example/)\n[d]: ../d.md"
        ),
        "[a](../guides/a.md) [b](#b) [c](https://c.example/)\n[d]: d.md"
    );
    assert_eq!(
        type_name(&serde_json::json!({"type": ["string", "null"]})),
        "string or null"
    );
}
