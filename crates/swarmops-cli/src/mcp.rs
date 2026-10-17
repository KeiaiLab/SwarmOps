//! `swarmops mcp`: the store's operations as an MCP server on stdin and stdout, for the agent
//! harnesses that speak the Model Context Protocol.
//!
//! # The protocol
//!
//! MCP over stdio: JSON-RPC 2.0, one message per line. The server reads a message from stdin,
//! answers it on stdout if it is a request, and reads the next: one at a time, in order. stdout
//! carries nothing but answers. stderr gets a line for a message that has no answer although it
//! is neither a request nor a notification (a response from the client), a line for the I/O
//! error that stops the server, and the panic hook's report of a bug; nothing else. The server
//! exits 0 when stdin ends, and 1 if reading stdin or writing stdout fails.
//!
//! It serves the protocol's revisions each the way its clients open:
//!
//! - [`LATEST`], 2026-07-28, which has no handshake: a request whose `params._meta` names
//!   `io.modelcontextprotocol/protocolVersion` 2026-07-28 or later is served on its own, by that
//!   revision's rules, and must carry `io.modelcontextprotocol/clientCapabilities` too. Its
//!   methods are `server/discover`, `tools/list` and `tools/call`. Each result has
//!   `resultType: "complete"` and the server's identity in
//!   `_meta["io.modelcontextprotocol/serverInfo"]`, and the results of `server/discover` and
//!   `tools/list` say how long a client may keep them (`ttlMs`, an hour; `cacheScope:
//!   "public"`): they change only with the binary.
//! - The revisions with a handshake ([`HANDSHAKE`]): `initialize` opens a session that lasts as
//!   long as the process, at the version the client asks for when it is 2025-11-25 ([`PREVIOUS`])
//!   or 2025-06-18, the first with `structuredContent` and `outputSchema`, and else at 2025-11-25,
//!   which a client that cannot speak it refuses. Both get the same tools and results. From then
//!   on, a request that names no version of 2026-07-28 or later in `_meta` is served by the
//!   session's rules: `tools/list` and `tools/call`, and `ping`, which is also answered before
//!   `initialize`. Before it, a request that names a version with a handshake in `_meta` is
//!   told to send `initialize` (`-32022`).
//!
//! Notifications get no answer, `notifications/initialized` included; a
//! `notifications/cancelled` finds nothing to cancel, since a request is answered before the
//! next message is read.
//!
//! # Errors
//!
//! A message that is not a request the server can serve gets a JSON-RPC error. One whose id
//! cannot be read is answered without an `id`, as MCP allows (its schema types an error's `id`
//! as an optional string or integer, so a null one would not validate); a request's id is a
//! string or an integer.
//!
//! | Code | Name | For |
//! |---|---|---|
//! | `-32700` | Parse error | a line that is not JSON, or not UTF-8 |
//! | `-32600` | Invalid Request | a batch, or anything else that is not one object; an object whose `jsonrpc` is not `"2.0"`, whose `id` is not a string or an integer (null, a fraction, an object, an array, a boolean), or that has neither a string `method` nor a response's `result` or `error` |
//! | `-32601` | Method not found | a method its revision does not have (`ping` in 2026-07-28, `initialize` in a request that names 2026-07-28) |
//! | `-32602` | Invalid params | `params`, or a `params._meta`, that is not an object, or a protocol version in it that is not a string; before `initialize`, a request that names no version in `_meta` (but `ping`), and a `server/discover` that names none at any time; a 2026-07-28 `_meta` without the client's capabilities; an `initialize` without the client's `protocolVersion`; a `tools/call` without a known tool's name, or with `arguments` that are not an object; a `tools/list` cursor (there is one page) |
//! | `-32603` | Internal error | a panic outside an operation (a bug) |
//! | `-32022` | Unsupported protocol version | a `_meta` version from 2026-07-28 on that is not 2026-07-28; before `initialize`, a `_meta` version with a handshake; `data` gives the `supported` versions ([`SUPPORTED`]: 2026-07-28, and the ones `initialize` opens) and the `requested` one |
//!
//! An operation that fails is not a protocol error: it is a tool result, below.
//!
//! # The tools
//!
//! One tool per [`StoreOps`] operation, named as the envelope's `op` is: `repo_info`,
//! `read_file`, `read_tree`, `show`, `log`, `ref_list`, `ref_set`, `ref_delete`, `commit` and
//! `verify`, which `tools/list` gives in that order. A tool's `inputSchema` is its request
//! type's JSON Schema, as `swarmops schema <type>` prints it, with each `$ref` replaced by the
//! definition it names so that the schema stands alone: a keyword beside the reference wins
//! over the definition's, but for `description`, where the reference's comes first and the
//! definition's after it. Its annotations say whether it changes the store and how; none
//! reaches beyond the store (`openWorldHint: false`).
//!
//! A call runs as one command of the CLI does. Its arguments are the request: a field the type
//! does not know is ignored, and arguments that do not fit the type are `E_INVALID_ARG`, as is
//! a request that breaks this server's one rule of its own ([`Refuse`]): a `ref_set` or a
//! `ref_delete` must name what it expects the ref to hold by its full changeset id, since a ref
//! name or a relative revision resolves when the call runs and so defeats the compare-and-swap
//! (the CLI takes any revision for `--expect`, and its help warns why not to). A mutation then
//! needs the server's identity, and last the store is opened and the operation runs; a
//! `verify` that finds problems is `E_STORE_CORRUPT`, as `swarmops verify` is, and a panic in
//! the operation is `E_INTERNAL`. The tool result holds the envelope the CLI prints for the
//! same request, as its `structuredContent` and as the text of its one content block, and
//! `isError` is true exactly when the envelope's `ok` is false.
//!
//! Clients cut long tool results (Claude Code at 25,000 tokens by default), and a result holds
//! its envelope twice, so the descriptions and [`INSTRUCTIONS`] ask the model for small reads
//! (`max_bytes` of 32 KiB or less, `limit` and `max_changes` of 100 or less, paging with the
//! `cursor`); the API's defaults stay as the operations spec gives them.
//!
//! # The store and the identity
//!
//! Both are the server's, never a call's. The store is `--repo`, else `SWARMOPS_REPO`, opened
//! anew for each call, so that a call sees what other processes committed; a store that is not
//! there is each call's `E_NOT_A_REPO`, and the server serves on. The identity is `--as`, else
//! `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID`, as for the CLI; a mutation without one is
//! `E_IDENTITY_REQUIRED`. The client starts the server as its own user, in its own trust
//! domain, so `{"file": <path>}` content is read here, as the CLI reads it, a relative path from
//! the server's working directory. The output flags (`--json`, `--human`, `--pretty`) do not
//! apply. A command line that parses as `swarmops mcp` but is refused (a usage error in what
//! follows the word `mcp`, a `SWARMOPS_FORMAT` that is neither `json` nor `human`), and a panic
//! before the server serves, start no server: their envelope goes to stderr, not to stdout,
//! which belongs to the protocol, and the process exits with its code. Of the flags, only those
//! after `mcp` are refused that way: a flag refused before the subcommand (`swarmops --frob
//! mcp`) is a plain usage error of the CLI, whose envelope goes to stdout with exit code 2, and
//! no server starts either.

use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use swarmops_api::{
    CommitReq, Envelope, ErrorCode, Identity, InfoReq, LogReq, OpError, ReadFileReq, ReadTreeReq,
    RefDeleteReq, RefListReq, RefSetReq, Rev, ShowReq, StoreOps, VerifyReq, schema_for_all,
};

use crate::cli::Global;
use crate::commands::{self, Ctx, Used};
use crate::context::{self, Env};
use crate::output::{self, Format, Printed};

/// MCP's latest revision, served without a handshake.
pub const LATEST: &str = "2026-07-28";

/// The revision before it, the last with a handshake: served to clients that open with
/// `initialize`, also to those that ask for a version the handshake does not serve.
pub const PREVIOUS: &str = "2025-11-25";

/// The revisions with a handshake that `initialize` serves, newest first: [`PREVIOUS`], and
/// 2025-06-18, the first with `structuredContent` and `outputSchema`. Both get the same tools
/// and results; 2025-03-26 and older are not served.
pub const HANDSHAKE: [&str; 2] = [PREVIOUS, "2025-06-18"];

/// Every revision the server serves, newest first: [`LATEST`], which a request names in
/// `params._meta`, then the [`HANDSHAKE`] revisions, which `initialize` opens. `server/discover`
/// lists them, and so does a `-32022` error, so that a client that cannot speak [`LATEST`] sees
/// that it can open with `initialize` instead.
pub const SUPPORTED: [&str; 3] = {
    let [previous, earliest] = HANDSHAKE;
    [LATEST, previous, earliest]
};

/// The `_meta` key under which a 2026-07-28 request names its protocol version.
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";

/// The `_meta` key of the client's capabilities, which a 2026-07-28 request must carry.
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";

/// How long a client may keep the results of `server/discover` and `tools/list`, in
/// milliseconds: an hour.
const TTL_MS: u64 = 60 * 60 * 1000;

/// The most bytes the line buffer keeps between messages: a commit's line can be hundreds of
/// MiB, which a long-lived server should not hold on to.
const LINE_KEEP: usize = 64 * 1024;

/// JSON-RPC's error codes, and MCP's for a protocol version the server does not serve.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// Who the server is, in each revision's `serverInfo`.
const SERVER: Implementation = Implementation {
    name: "swarmops",
    title: "SwarmOps",
    version: env!("CARGO_PKG_VERSION"),
};

/// What the server tells a client's model about using it.
const INSTRUCTIONS: &str = "SwarmOps is source control for coding-agent swarms: many agents commit \
to one store at once. Each tool is one store operation, and its result is the SwarmOps envelope \
{ok, op, api, result | error, meta}, as structuredContent and again as JSON text. A failed \
operation sets isError and gives error {code, message, retryable, exit, details, hint}: \
retryable says whether the same call can succeed again, and a hint is often a swarmops command, \
whose operation is the tool of the same name (ref list is ref_list, cat read_file, ls \
read_tree, info repo_info). A revision is main, release/<name>, a changeset id or 12 or more of \
its first hex digits, then optionally ~N for the N-th first parent. commit records a changeset \
and moves no ref. ref_set and ref_delete change a ref by compare-and-swap, so their \
expected_old is the full 64-hex changeset id the ref held when you read it (from ref_list, \
show, log or a commit's result). A ref name, a relative revision or an id prefix is read when \
the call runs instead of pinning the id you read: the ref's own name always matches, so the \
compare-and-swap protects nothing, and main~1 on ref main never matches; the server refuses \
them. After \
E_CAS_MISMATCH, commit your change again on top of details.actual, re-applying your edits to \
the content there, then retry with expected_old set to it. Clients cut long tool results \
(Claude Code at 25,000 tokens by default) and each result carries its envelope twice, so ask \
for little: max_bytes of 32768 or less for read_file (less for a binary file, which comes back \
in base64), a limit of 100 or less for read_tree and log, paging on with the cursor a result \
returns, and max_changes of 100 or less for show. Every change is recorded for the agent the \
server runs as.";

/// The tools, in the order `tools/list` gives them.
static TOOLS: [Spec; 10] = [
    Spec {
        tool: Tool::RepoInfo,
        name: "repo_info",
        request: "InfoReq",
        title: "Describe the store",
        description: "What the store is and holds: its id and name, the changeset main points \
                      at, and how many changesets and refs it has.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::ReadFile,
        name: "read_file",
        request: "ReadFileReq",
        title: "Read a file",
        description: "Reads one file at a revision, without a checkout, as UTF-8 text or \
                      base64; pass max_bytes of 32768 or less (less for a binary file), since \
                      clients cut long tool results and this one holds the content twice.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::ReadTree,
        name: "read_tree",
        request: "ReadTreeReq",
        title: "List a directory",
        description: "Lists one directory at a revision, without a checkout, a page of entries \
                      at a time; pass a limit of 100 or less and page on with the cursor, since \
                      clients cut long tool results.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::Show,
        name: "show",
        request: "ShowReq",
        title: "Show a changeset",
        description: "One changeset in full: its parents, author, message, changes and \
                      signatures; pass max_changes of 100 or less, since clients cut long tool \
                      results.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::Log,
        name: "log",
        request: "LogReq",
        title: "List history",
        description: "History along first parents, newest first, from main unless rev says \
                      otherwise; pass a limit of 100 or less and page on with the cursor, since \
                      clients cut long tool results.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::RefList,
        name: "ref_list",
        request: "RefListReq",
        title: "List refs",
        description: "Every ref, the changeset it points at, and how many moves it has had.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::RefSet,
        name: "ref_set",
        request: "RefSetReq",
        title: "Move a ref",
        description: "Moves a ref, or creates one, by compare-and-swap: give create, or \
                      expected_old, the full 64-hex changeset id the ref held when you read it; a \
                      ref name, a relative revision or an id prefix is refused, since it is read \
                      when the call runs instead of pinning the id you read.",
        effect: Effect::Moves,
    },
    Spec {
        tool: Tool::RefDelete,
        name: "ref_delete",
        request: "RefDeleteReq",
        title: "Delete a ref",
        description: "Removes a ref by compare-and-swap, if it still holds expected_old, the \
                      full 64-hex changeset id it held when you read it; a ref name, a relative \
                      revision or an id prefix is refused, since it is read when the call runs \
                      instead of pinning the id you read.",
        effect: Effect::Moves,
    },
    Spec {
        tool: Tool::Commit,
        name: "commit",
        request: "CommitReq",
        title: "Commit a changeset",
        description: "Records a changeset from an explicit change list, on parent or else on \
                      main's head; it moves no ref.",
        effect: Effect::Adds,
    },
    Spec {
        tool: Tool::Verify,
        name: "verify",
        request: "VerifyReq",
        title: "Verify the store",
        description: "Checks the whole store's consistency; deep re-hashes every object too.",
        effect: Effect::Reads,
    },
];

/// Serves MCP on stdin and stdout until stdin ends, with the store and the identity that
/// `global` and `env` name. It prints no envelope: what it returns has nothing for stdout, and
/// the exit code.
pub fn serve(global: &Global, env: Env<'_>) -> Printed {
    let mut server = Server::new(global, env);
    let exit = match server.serve(io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("stopped: {e}"));
            1
        }
    };
    Printed {
        stdout: String::new(),
        exit,
    }
}

/// One process's server: the session `initialize` opened, and the tools.
struct Server<'a> {
    /// The global flags: the store and the identity.
    global: &'a Global,
    /// How environment variables are read.
    env: Env<'a>,
    /// The tools, as `tools/list` gives them.
    tools: Vec<ToolDef>,
    /// The revision of the session `initialize` opened, one of [`HANDSHAKE`]; `None` before
    /// it.
    session: Option<&'static str>,
}

/// The rules a request is served by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    /// 2026-07-28's: the request names that version in `_meta`.
    Latest,
    /// The session's: the request names no version of 2026-07-28 or later.
    Previous,
}

/// What a request's `params._meta` says of its protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Claim {
    /// Nothing: the request is the session's.
    None,
    /// A revision with a handshake (before 2026-07-28), which only a session serves.
    Handshake(String),
    /// 2026-07-28, with the client's capabilities.
    Latest,
}

/// How to answer request `id`, served by `era`'s rules.
#[derive(Debug, Clone, Copy)]
struct Reply<'a> {
    id: &'a Value,
    era: Era,
}

impl<'a> Server<'a> {
    fn new(global: &'a Global, env: Env<'a>) -> Server<'a> {
        let all = schema_for_all();
        let tools = TOOLS
            .iter()
            .map(|spec| ToolDef {
                name: spec.name,
                title: spec.title,
                description: spec.description,
                input_schema: standalone(&all[spec.request]),
                annotations: spec.effect.annotations(),
            })
            .collect();
        Server {
            global,
            env,
            tools,
            session: None,
        }
    }

    /// Answers each message `input` holds, a line each, on `output`, until `input` ends.
    fn serve(&mut self, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        let mut line = Vec::new();
        loop {
            line.clear();
            line.shrink_to(LINE_KEEP);
            if input.read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            if let Some(answer) = self.answer(&line) {
                output.write_all(answer.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
    }

    /// The answer to the message on `line`: a response to a request, an error for what is not
    /// a valid message, nothing for a notification or a blank line.
    fn answer(&mut self, line: &[u8]) -> Option<String> {
        let line = line.trim_ascii();
        if line.is_empty() {
            return None;
        }
        let mut message = match serde_json::from_slice::<Value>(line) {
            Ok(Value::Object(message)) => message,
            Ok(_) => {
                return Some(error(
                    None,
                    INVALID_REQUEST,
                    "Invalid Request: a message is one JSON object; batches are not served",
                ));
            }
            Err(e) => return Some(error(None, PARSE_ERROR, format!("Parse error: {e}"))),
        };
        let id = match message.remove("id") {
            None => None,
            Some(id @ Value::String(_)) => Some(id),
            Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Some(Value::Number(n)),
            Some(_) => {
                return Some(error(
                    None,
                    INVALID_REQUEST,
                    "Invalid Request: an id is a string or an integer",
                ));
            }
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(error(
                id.as_ref(),
                INVALID_REQUEST,
                "Invalid Request: a message has \"jsonrpc\": \"2.0\"",
            ));
        }
        let method = match message.remove("method") {
            Some(Value::String(method)) => method,
            Some(_) => {
                return Some(error(
                    id.as_ref(),
                    INVALID_REQUEST,
                    "Invalid Request: a method is a string",
                ));
            }
            None if message.contains_key("result") || message.contains_key("error") => {
                let to = id.map_or_else(|| "no request".to_owned(), |id| id.to_string());
                log(&format!(
                    "ignored a message that is not a request or a notification: a response to \
                     {to}, and the server sends no requests"
                ));
                return None;
            }
            None => {
                return Some(error(
                    id.as_ref(),
                    INVALID_REQUEST,
                    "Invalid Request: a request has a method",
                ));
            }
        };
        // A notification gets no answer, and none of them asks the server to do anything.
        let id = id?;
        let params = match message.remove("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(params)) => params,
            Some(_) => {
                return Some(error(
                    Some(&id),
                    INVALID_PARAMS,
                    "Invalid params: params are an object",
                ));
            }
        };
        let answer = panic::catch_unwind(AssertUnwindSafe(|| self.request(&id, &method, params)));
        Some(answer.unwrap_or_else(|payload| {
            let bug = commands::panicked(&*payload);
            error(
                Some(&id),
                INTERNAL_ERROR,
                format!("Internal error: {}", bug.message),
            )
        }))
    }

    /// The answer to request `id`, of `method` with `params`.
    fn request(&mut self, id: &Value, method: &str, params: Map<String, Value>) -> String {
        let claim = match claim(&params) {
            Ok(claim) => claim,
            Err(rejected) => return rejected.answer(id),
        };
        if claim == Claim::Latest {
            let reply = Reply {
                id,
                era: Era::Latest,
            };
            return match method {
                "server/discover" => reply.result(Discovered {
                    supported_versions: &SUPPORTED,
                    capabilities: Capabilities { tools: Empty {} },
                    instructions: INSTRUCTIONS,
                    ttl_ms: TTL_MS,
                    cache_scope: "public",
                }),
                "tools/list" => self.list_tools(reply, &params),
                "tools/call" => self.call_tool(reply, params),
                _ => not_found(id, method),
            };
        }
        if method == "initialize" {
            return self.initialize(id, &params);
        }
        if let (Claim::Handshake(requested), None) = (claim, self.session) {
            return Rejected::Handshake(requested).answer(id);
        }
        let reply = Reply {
            id,
            era: Era::Previous,
        };
        match method {
            "ping" => reply.result(Empty {}),
            "server/discover" => error(
                Some(id),
                INVALID_PARAMS,
                format!(
                    "Invalid params: server/discover is {LATEST}'s, whose requests name that \
                     version in params._meta ({VERSION_KEY})"
                ),
            ),
            "tools/list" | "tools/call" if self.session.is_none() => error(
                Some(id),
                INVALID_PARAMS,
                format!(
                    "Invalid params: {method} names no protocol version in params._meta \
                     ({VERSION_KEY}), and no initialize has opened a session"
                ),
            ),
            "tools/list" => self.list_tools(reply, &params),
            "tools/call" => self.call_tool(reply, params),
            _ => not_found(id, method),
        }
    }

    /// `initialize`: opens a session at the version the client asks for when it is one of
    /// [`HANDSHAKE`], else at [`PREVIOUS`].
    fn initialize(&mut self, id: &Value, params: &Map<String, Value>) -> String {
        let Some(asked) = params.get("protocolVersion").and_then(Value::as_str) else {
            return error(
                Some(id),
                INVALID_PARAMS,
                "Invalid params: initialize names the protocolVersion the client speaks",
            );
        };
        let version = HANDSHAKE
            .into_iter()
            .find(|version| *version == asked)
            .unwrap_or(PREVIOUS);
        // A second initialize opens the session anew, at the version it negotiates: tolerated on
        // purpose, since a session holds nothing but its revision, and every revision with a
        // handshake gets the same tools and results.
        self.session = Some(version);
        let reply = Reply {
            id,
            era: Era::Previous,
        };
        reply.result(Initialized {
            protocol_version: version,
            capabilities: Capabilities { tools: Empty {} },
            server_info: SERVER,
            instructions: INSTRUCTIONS,
        })
    }

    /// `tools/list`: every tool, on one page.
    fn list_tools(&self, reply: Reply<'_>, params: &Map<String, Value>) -> String {
        if params.get("cursor").is_some_and(|cursor| !cursor.is_null()) {
            return error(
                Some(reply.id),
                INVALID_PARAMS,
                "Invalid params: tools/list has one page, so no cursor to resume from",
            );
        }
        let (ttl_ms, cache_scope) = match reply.era {
            Era::Latest => (Some(TTL_MS), Some("public")),
            Era::Previous => (None, None),
        };
        reply.result(ToolList {
            tools: &self.tools,
            ttl_ms,
            cache_scope,
        })
    }

    /// `tools/call`: runs the tool `params` names on its arguments.
    fn call_tool(&self, reply: Reply<'_>, mut params: Map<String, Value>) -> String {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return error(
                Some(reply.id),
                INVALID_PARAMS,
                "Invalid params: tools/call names the tool to call",
            );
        };
        let Some(spec) = TOOLS.iter().find(|spec| spec.name == name) else {
            return error(
                Some(reply.id),
                INVALID_PARAMS,
                format!("Unknown tool: {name}"),
            );
        };
        let arguments = match params.remove("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(arguments @ Value::Object(_)) => arguments,
            Some(_) => {
                return error(
                    Some(reply.id),
                    INVALID_PARAMS,
                    "Invalid params: a tool's arguments are an object",
                );
            }
        };
        let call = Call {
            server: self,
            spec,
            reply,
            arguments,
        };
        operate(spec.tool, call)
    }
}

/// What the `_meta` of a request with `params` claims: no version, a version with a handshake
/// (before 2026-07-28), or 2026-07-28 with the client's capabilities. A version of 2026-07-28
/// or later that is not 2026-07-28, and a 2026-07-28 `_meta` without the client's
/// capabilities, are refused.
fn claim(params: &Map<String, Value>) -> Result<Claim, Rejected> {
    let meta = match params.get("_meta") {
        None => return Ok(Claim::None),
        Some(Value::Object(meta)) => meta,
        Some(_) => return Err(Rejected::params("params._meta is an object")),
    };
    let version = match meta.get(VERSION_KEY) {
        None => return Ok(Claim::None),
        Some(Value::String(version)) => version,
        Some(_) => return Err(Rejected::params(&format!("{VERSION_KEY} is a string"))),
    };
    // Revisions are dates, so they sort as strings.
    if version.as_str() < LATEST {
        return Ok(Claim::Handshake(version.clone()));
    }
    if version != LATEST {
        return Err(Rejected::Version(version.clone()));
    }
    if !meta.get(CAPABILITIES_KEY).is_some_and(Value::is_object) {
        return Err(Rejected::params(&format!(
            "a {LATEST} request carries {CAPABILITIES_KEY} in params._meta"
        )));
    }
    Ok(Claim::Latest)
}

/// A request refused before its method is looked at.
enum Rejected {
    /// Its params break a rule: the message says which.
    Params(String),
    /// It names a version from 2026-07-28 on that is not served.
    Version(String),
    /// Before any session, it names a version with a handshake, which only a session serves.
    Handshake(String),
}

impl Rejected {
    fn params(why: &str) -> Rejected {
        Rejected::Params(format!("Invalid params: {why}"))
    }

    /// The error answering request `id`.
    fn answer(self, id: &Value) -> String {
        match self {
            Rejected::Params(message) => error(Some(id), INVALID_PARAMS, message),
            Rejected::Version(requested) => error_with(
                Some(id),
                UNSUPPORTED_PROTOCOL_VERSION,
                &format!(
                    "Unsupported protocol version: {requested}; this server serves {LATEST} in \
                     params._meta, and {} through initialize",
                    HANDSHAKE.join(" and ")
                ),
                Some(json!({"supported": SUPPORTED, "requested": requested})),
            ),
            Rejected::Handshake(requested) => error_with(
                Some(id),
                UNSUPPORTED_PROTOCOL_VERSION,
                &format!(
                    "Unsupported protocol version: {requested} has a handshake, so send \
                     initialize first (this server serves {} that way), or name {LATEST} in \
                     params._meta",
                    HANDSHAKE.join(" and ")
                ),
                Some(json!({"supported": SUPPORTED, "requested": requested})),
            ),
        }
    }
}

/// How a tool runs its operation: as a read, or as a mutation, which acts as the server's
/// identity. [`operate`] ties each tool to the [`StoreOps`] method it stands for through it.
trait Operate {
    /// What running the tool gives.
    type Done;

    /// Runs a read: the request `R`, then `call` on the store.
    fn read<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn StoreOps, R) -> Result<T, OpError>,
    ) -> Self::Done;

    /// Runs a mutation: the request `R`, then `call` on the store as the server's identity.
    fn mutate<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn StoreOps, &Identity, R) -> Result<T, OpError>,
    ) -> Self::Done;
}

/// Runs `tool`'s operation as `how` runs operations: the one place where a tool meets the
/// [`StoreOps`] method it stands for, and so the request type it reads, which the method's
/// signature fixes.
fn operate<O: Operate>(tool: Tool, how: O) -> O::Done {
    match tool {
        Tool::RepoInfo => how.read(|ops, req| ops.repo_info(req)),
        Tool::ReadFile => how.read(|ops, req| ops.read_file(req)),
        Tool::ReadTree => how.read(|ops, req| ops.read_tree(req)),
        Tool::Show => how.read(|ops, req| ops.show(req)),
        Tool::Log => how.read(|ops, req| ops.log(req)),
        Tool::RefList => how.read(|ops, req| ops.ref_list(req)),
        Tool::RefSet => how.mutate(|ops, who, req| ops.ref_set(who, req)),
        Tool::RefDelete => how.mutate(|ops, who, req| ops.ref_delete(who, req)),
        Tool::Commit => how.mutate(|ops, who, req| ops.commit(who, req)),
        Tool::Verify => how.read(|ops, req: VerifyReq| {
            let deep = req.deep;
            ops.verify(req)
                .and_then(|report| commands::clean(report, deep))
        }),
    }
}

/// One `tools/call` of the tool `spec` on `arguments`, answered as `reply`.
struct Call<'s, 'a> {
    server: &'s Server<'a>,
    spec: &'static Spec,
    reply: Reply<'s>,
    arguments: Value,
}

impl Operate for Call<'_, '_> {
    type Done = String;

    /// The arguments as the request, then `call` on the store.
    fn read<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn StoreOps, R) -> Result<T, OpError>,
    ) -> String {
        let Call {
            server,
            spec,
            reply,
            arguments,
        } = self;
        run(server, spec, reply, || request_of(spec, arguments), call)
    }

    /// The arguments as the request, then the server's identity, then `call` on the store as
    /// that identity.
    fn mutate<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn StoreOps, &Identity, R) -> Result<T, OpError>,
    ) -> String {
        let Call {
            server,
            spec,
            reply,
            arguments,
        } = self;
        let (agent, env) = (server.global.agent.as_deref(), server.env);
        run(
            server,
            spec,
            reply,
            || {
                let req = request_of(spec, arguments)?;
                Ok((context::mutator(agent, env)?, req))
            },
            |ops, (who, req)| call(ops, &who, req),
        )
    }
}

/// Runs tool `spec` for `server` as a command runs ([`Ctx::envelope_on_store`]): `prepare`
/// builds its input, then `call` runs on the store, opened; a panic in either is `E_INTERNAL`.
/// The tool result that holds the envelope, answered as `reply`.
fn run<P, T: Serialize>(
    server: &Server<'_>,
    spec: &Spec,
    reply: Reply<'_>,
    prepare: impl FnOnce() -> Result<P, OpError>,
    call: impl FnOnce(&dyn StoreOps, P) -> Result<T, OpError>,
) -> String {
    let ctx = Ctx {
        global: server.global,
        env: server.env,
        format: Format::Json { pretty: false },
        started: Instant::now(),
        used: Used::default(),
    };
    let op = spec.name;
    match panic::catch_unwind(AssertUnwindSafe(|| {
        ctx.envelope_on_store(op, prepare, call)
    })) {
        Ok(envelope) => reply.tool_result(&envelope),
        Err(payload) => {
            let bug = commands::panicked(&*payload);
            let envelope = commands::failed_envelope(op, bug, &ctx.used, ctx.started);
            reply.tool_result(&envelope)
        }
    }
}

/// The request `arguments` stand for, of tool `spec`'s type: `E_INVALID_ARG` when they do not
/// fit it, or break a rule of this server's ([`Refuse`]). Fields the type does not know are
/// ignored, as the API's types always ignore them.
fn request_of<R: Refuse>(spec: &Spec, arguments: Value) -> Result<R, OpError> {
    let req: R = serde_json::from_value(arguments).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            output::printable(&format!("the arguments are not a {}: {e}", spec.request)),
        )
        .with_hint(format!(
            "the {} tool's inputSchema gives its arguments, as swarmops schema {} does",
            spec.name, spec.request
        ))
    })?;
    req.refuse()?;
    Ok(req)
}

/// This server's own rules for a request, which its type cannot say: a request type with one
/// refuses what breaks it, the others nothing.
trait Refuse: DeserializeOwned {
    /// `E_INVALID_ARG` for a request that breaks the rule.
    fn refuse(&self) -> Result<(), OpError> {
        Ok(())
    }
}

impl Refuse for InfoReq {}
impl Refuse for ReadFileReq {}
impl Refuse for ReadTreeReq {}
impl Refuse for ShowReq {}
impl Refuse for LogReq {}
impl Refuse for RefListReq {}
impl Refuse for CommitReq {}
impl Refuse for VerifyReq {}

/// A move names the target it expects by its full id ([`concrete_expectation`]), unless it
/// creates the ref.
impl Refuse for RefSetReq {
    fn refuse(&self) -> Result<(), OpError> {
        match &self.expected_old {
            Some(expected) => concrete_expectation(&self.name, expected),
            None => Ok(()),
        }
    }
}

/// A delete names the target it expects by its full id ([`concrete_expectation`]).
impl Refuse for RefDeleteReq {
    fn refuse(&self) -> Result<(), OpError> {
        concrete_expectation(&self.name, &self.expected_old)
    }
}

/// `E_INVALID_ARG` unless `expected`, the target a move or a delete of ref `name` expects, is a
/// full changeset id: 64 hex digits, as the resolver takes them. Anything else (a ref name, a
/// relative revision, an id's prefix) is read from the store when the call runs instead of
/// pinning the id the caller read. The ref's own name always matches, so the compare-and-swap
/// protects nothing: a move computed on a stale read would silently drop what another agent
/// moved the ref to, and a move to `main~1` made twice would move the ref twice. `main~1` on
/// `main` names main's parent and never matches, and a prefix stops resolving
/// (`E_AMBIGUOUS_ID`) once another changeset shares it. The CLI takes any revision for
/// `--expect`, whose help warns of this; an agent through this server gets the rule enforced.
fn concrete_expectation(name: &str, expected: &Rev) -> Result<(), OpError> {
    let rev = expected.as_str();
    if rev.len() == 64 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::InvalidArg,
        output::printable(&format!(
            "expected_old {rev:?} is not a full changeset id, which a compare-and-swap needs: a \
             ref name, a relative revision or an id prefix is read when the call runs instead \
             of pinning the id you read ({name} itself always matches, so the compare-and-swap \
             would protect nothing, and {name}~1 never matches); pass the 64-hex id the ref \
             held when you read it, from ref_list, show, log or a commit's result"
        )),
    )
    .with_details(Map::from_iter([
        ("name".to_owned(), Value::from(name)),
        ("expected_old".to_owned(), Value::from(rev)),
    ]))
    .with_hint("swarmops ref list"))
}

impl Reply<'_> {
    /// The response carrying `result`; in 2026-07-28, with `resultType` and the server's
    /// identity.
    fn result<R: Serialize>(&self, result: R) -> String {
        match self.era {
            Era::Previous => response(self.id, result),
            Era::Latest => response(
                self.id,
                LatestResult {
                    result_type: "complete",
                    result,
                    meta: ResultMeta {
                        server_info: SERVER,
                    },
                },
            ),
        }
    }

    /// The response carrying a tool's result: `envelope`, as structured content and as the
    /// text of the one content block, and `isError` when the envelope is a failure's.
    fn tool_result<T: Serialize>(&self, envelope: &Envelope<T>) -> String {
        // Every API type serializes: their maps have string keys, their impls never fail.
        let text = serde_json::to_string(envelope).expect("an envelope serializes");
        self.result(ToolResult {
            content: [TextContent {
                kind: "text",
                text: &text,
            }],
            structured_content: envelope,
            is_error: !envelope.ok,
        })
    }
}

/// `schema` with each `$ref` into its `$defs` replaced by the definition it names, and the
/// `$defs` gone: a schema that stands alone, for the clients that do not follow references. A
/// keyword beside a reference wins over the definition's, but for `description`, where the
/// one beside the reference comes first and the definition's after it, so that neither is
/// lost.
fn standalone(schema: &Value) -> Value {
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    inline(schema, &defs, &mut Vec::new())
}

/// [`standalone`] of `schema`, with `defs` the definitions and `expanding` the ones being
/// replaced, around it: a definition that names itself has no schema that stands alone.
fn inline(schema: &Value, defs: &Map<String, Value>, expanding: &mut Vec<String>) -> Value {
    match schema {
        Value::Object(members) => {
            let mut out = match members.get("$ref").and_then(Value::as_str) {
                Some(reference) => {
                    let name = reference
                        .strip_prefix("#/$defs/")
                        .unwrap_or_else(|| panic!("{reference} is not a reference into $defs"));
                    assert!(
                        !expanding.iter().any(|open| open == name),
                        "{name} refers to itself, so its schema cannot stand alone"
                    );
                    let definition = defs
                        .get(name)
                        .unwrap_or_else(|| panic!("$defs has no {name}"));
                    expanding.push(name.to_owned());
                    let inlined = inline(definition, defs, expanding);
                    expanding.pop();
                    match inlined {
                        Value::Object(definition) => definition,
                        _ => panic!("the definition of {name} is not a schema object"),
                    }
                }
                None => Map::new(),
            };
            for (key, value) in members {
                match key.as_str() {
                    "$ref" | "$defs" => {}
                    "description" => {
                        let description = match (value.as_str(), out.get("description")) {
                            (Some(beside), Some(Value::String(defined))) => {
                                Value::from(format!("{beside} {defined}"))
                            }
                            _ => value.clone(),
                        };
                        out.insert(key.clone(), description);
                    }
                    _ => {
                        out.insert(key.clone(), inline(value, defs, expanding));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| inline(item, defs, expanding))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Writes `message` to stderr as one line of the server's.
fn log(message: &str) {
    // stderr is for the person who reads the logs; a write that fails has nobody to tell.
    let _ = writeln!(io::stderr(), "swarmops mcp: {}", output::printable(message));
}

/// The response to request `id` carrying `result`, as one line.
fn response<R: Serialize>(id: &Value, result: R) -> String {
    let response = Response {
        jsonrpc: "2.0",
        id,
        result,
    };
    serde_json::to_string(&response).expect("a response serializes")
}

/// The error response with `code` and `message` to request `id` (`None`: a message whose id
/// could not be read, answered without one), as one line.
fn error(id: Option<&Value>, code: i64, message: impl AsRef<str>) -> String {
    error_with(id, code, message.as_ref(), None)
}

/// `Method not found` for request `id` of `method`.
fn not_found(id: &Value, method: &str) -> String {
    error(
        Some(id),
        METHOD_NOT_FOUND,
        format!("Method not found: {method}"),
    )
}

/// [`error`] with `data`.
fn error_with(id: Option<&Value>, code: i64, message: &str, data: Option<Value>) -> String {
    let response = ErrorResponse {
        jsonrpc: "2.0",
        id,
        error: RpcError {
            code,
            message: output::printable(message),
            data,
        },
    };
    serde_json::to_string(&response).expect("an error response serializes")
}

/// One tool: which operation it runs, and what `tools/list` says of it.
struct Spec {
    tool: Tool,
    /// The tool's name: the operation's, as the envelope's `op` gives it.
    name: &'static str,
    /// The operation's request type, as `schema_for_all` names it.
    request: &'static str,
    title: &'static str,
    /// One line.
    description: &'static str,
    effect: Effect,
}

/// The operations, one tool each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    RepoInfo,
    ReadFile,
    ReadTree,
    Show,
    Log,
    RefList,
    RefSet,
    RefDelete,
    Commit,
    Verify,
}

/// What a tool does to the store, which its annotations say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    /// It only reads.
    Reads,
    /// It adds to the store and changes nothing there (`commit`); run again, it adds again,
    /// another changeset signed at its own time.
    Adds,
    /// It moves or deletes a ref by compare-and-swap, changing what the ref names; run again
    /// with the same arguments, it finds the ref as it asked (`already_applied`) or not as it
    /// expected (`E_CAS_MISMATCH`), and changes nothing more.
    Moves,
}

impl Effect {
    /// The tool annotations of a tool with this effect.
    fn annotations(self) -> Value {
        let (read_only, destructive, idempotent) = match self {
            Effect::Reads => (true, false, true),
            Effect::Adds => (false, false, false),
            // idempotentHint rests on concrete_expectation: the server refuses an expected_old
            // that is not a full id, so the same move again compares against the same changeset
            // and finds its work done (already_applied, nothing written) or the ref elsewhere
            // (E_CAS_MISMATCH). The ref's own name would match anew at every call and move the
            // ref again.
            Effect::Moves => (false, true, true),
        };
        json!({
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        })
    }
}

/// A JSON-RPC response with a result.
#[derive(Serialize)]
struct Response<'a, R> {
    jsonrpc: &'static str,
    id: &'a Value,
    result: R,
}

/// A JSON-RPC error response.
#[derive(Serialize)]
struct ErrorResponse<'a> {
    jsonrpc: &'static str,
    /// The request's id; omitted when it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a Value>,
    error: RpcError,
}

/// A JSON-RPC error object.
#[derive(Serialize)]
struct RpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// A 2026-07-28 result: its type, its own fields, then the server's identity in `_meta`.
#[derive(Serialize)]
struct LatestResult<R> {
    #[serde(rename = "resultType")]
    result_type: &'static str,
    #[serde(flatten)]
    result: R,
    #[serde(rename = "_meta")]
    meta: ResultMeta,
}

/// The `_meta` of a 2026-07-28 result.
#[derive(Serialize)]
struct ResultMeta {
    #[serde(rename = "io.modelcontextprotocol/serverInfo")]
    server_info: Implementation,
}

/// An MCP `Implementation`: who the server is.
#[derive(Serialize, Clone, Copy)]
struct Implementation {
    name: &'static str,
    title: &'static str,
    version: &'static str,
}

/// The server's capabilities: tools, whose list never changes while it runs.
#[derive(Serialize)]
struct Capabilities {
    tools: Empty,
}

/// An empty object: `{}`.
#[derive(Serialize)]
struct Empty {}

/// The result of `initialize`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Initialized {
    protocol_version: &'static str,
    capabilities: Capabilities,
    server_info: Implementation,
    instructions: &'static str,
}

/// The result of `server/discover`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Discovered {
    supported_versions: &'static [&'static str],
    capabilities: Capabilities,
    instructions: &'static str,
    ttl_ms: u64,
    cache_scope: &'static str,
}

/// The result of `tools/list`; in 2026-07-28, with how long it stays fresh.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolList<'a> {
    tools: &'a [ToolDef],
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_scope: Option<&'static str>,
}

/// One tool, as `tools/list` gives it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolDef {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Value,
    annotations: Value,
}

/// The result of `tools/call`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolResult<'a, T> {
    content: [TextContent<'a>; 1],
    structured_content: &'a Envelope<T>,
    is_error: bool,
}

/// A text content block.
#[derive(Serialize)]
struct TextContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    use camino::Utf8Path;
    use swarmops_api::{
        ChangeInput, ChangeOp, ChangesetDetail, CommitResp, ContentInput, FileContent, LogPage,
        RefListResp, RefSetResp, RepoInfo, TreeListing, VerifyReport,
    };
    use swarmops_ops::LocalOps;

    use super::*;

    /// Each operation's request type has exactly one tool, and each tool runs exactly the
    /// `StoreOps` method of its name, on the request type it advertises: a new operation, or a
    /// tool wired to the wrong one, fails here.
    #[test]
    fn the_tools_are_the_store_operations() {
        // `init` runs on the client and is no `StoreOps` operation, so it has no tool.
        let all = schema_for_all();
        let requests: Vec<&str> = all
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|name| name.ends_with("Req") && *name != "InitReq")
            .collect();
        let mut advertised: Vec<&str> = TOOLS.iter().map(|spec| spec.request).collect();
        advertised.sort_unstable();
        assert_eq!(
            advertised, requests,
            "one tool per operation's request type"
        );

        for spec in &TOOLS {
            let called = RefCell::new(Vec::new());
            operate(spec.tool, Probe { called: &called });
            assert_eq!(
                called.into_inner(),
                [(spec.name, spec.request)],
                "the {} tool",
                spec.name
            );
        }
        let names: BTreeSet<&str> = TOOLS.iter().map(|spec| spec.name).collect();
        assert_eq!(names.len(), TOOLS.len(), "two tools share a name");
    }

    /// Runs a tool's operation on a [`Recorder`], with a request read from [`sample`].
    struct Probe<'a> {
        called: &'a RefCell<Vec<(&'static str, &'static str)>>,
    }

    /// Arguments that every operation's request type reads: each takes the fields it knows,
    /// these fill the ones it needs, and it ignores the rest.
    fn sample() -> Value {
        json!({
            "rev": "main",
            "path": "a",
            "name": "main",
            "new": "main",
            "expected_old": "0".repeat(64),
            "message": "m",
            "changes": [],
        })
    }

    impl Operate for Probe<'_> {
        type Done = ();

        fn read<R: Refuse, T: Serialize>(
            self,
            call: impl FnOnce(&dyn StoreOps, R) -> Result<T, OpError>,
        ) {
            let req = serde_json::from_value(sample()).expect("every request reads the sample");
            let _ = call(&Recorder(self.called), req);
        }

        fn mutate<R: Refuse, T: Serialize>(
            self,
            call: impl FnOnce(&dyn StoreOps, &Identity, R) -> Result<T, OpError>,
        ) {
            let req = serde_json::from_value(sample()).expect("every request reads the sample");
            let who = Identity::parse("test/probe").unwrap();
            let _ = call(&Recorder(self.called), &who, req);
        }
    }

    /// A store that records which operation was called, by its name and the type of request
    /// it took (which its signature fixes), and fails it.
    struct Recorder<'a>(&'a RefCell<Vec<(&'static str, &'static str)>>);

    impl Recorder<'_> {
        fn called<T>(&self, op: &'static str, request: &'static str) -> Result<T, OpError> {
            self.0.borrow_mut().push((op, request));
            Err(OpError::new(ErrorCode::Internal, "a probe"))
        }
    }

    impl StoreOps for Recorder<'_> {
        fn repo_info(&self, _: InfoReq) -> Result<RepoInfo, OpError> {
            self.called("repo_info", "InfoReq")
        }

        fn commit(&self, _: &Identity, _: CommitReq) -> Result<CommitResp, OpError> {
            self.called("commit", "CommitReq")
        }

        fn read_file(&self, _: ReadFileReq) -> Result<FileContent, OpError> {
            self.called("read_file", "ReadFileReq")
        }

        fn read_tree(&self, _: ReadTreeReq) -> Result<TreeListing, OpError> {
            self.called("read_tree", "ReadTreeReq")
        }

        fn show(&self, _: ShowReq) -> Result<ChangesetDetail, OpError> {
            self.called("show", "ShowReq")
        }

        fn log(&self, _: LogReq) -> Result<LogPage, OpError> {
            self.called("log", "LogReq")
        }

        fn ref_list(&self, _: RefListReq) -> Result<RefListResp, OpError> {
            self.called("ref_list", "RefListReq")
        }

        fn ref_set(&self, _: &Identity, _: RefSetReq) -> Result<RefSetResp, OpError> {
            self.called("ref_set", "RefSetReq")
        }

        fn ref_delete(&self, _: &Identity, _: RefDeleteReq) -> Result<RefSetResp, OpError> {
            self.called("ref_delete", "RefDeleteReq")
        }

        fn verify(&self, _: VerifyReq) -> Result<VerifyReport, OpError> {
            self.called("verify", "VerifyReq")
        }
    }

    /// A move's `expected_old` must be 64 hex digits, of either case as the resolver takes
    /// them; anything else is resolved when the move runs, and is refused.
    #[test]
    fn an_expected_target_must_be_a_full_id() {
        let id = "0123456789abcdef".repeat(4);
        assert!(concrete_expectation("main", &Rev::from(id.as_str())).is_ok());
        assert!(concrete_expectation("main", &Rev::from(id.to_uppercase())).is_ok());
        for refused in [
            "main".to_owned(),
            "release/1.0".to_owned(),
            "main~1".to_owned(),
            id[..12].to_owned(),
            id[..63].to_owned(),
            format!("{id}0"),
            format!("{id}~1"),
            format!("{}g", &id[..63]),
            String::new(),
        ] {
            let error = concrete_expectation("release/1.0", &Rev::from(refused.as_str()))
                .expect_err(&refused);
            assert_eq!((error.code.as_str(), error.exit), ("E_INVALID_ARG", 2));
            assert_eq!(
                Value::Object(error.details),
                json!({"name": "release/1.0", "expected_old": refused})
            );
            assert!(!error.message.contains('\n'), "{}", error.message);
        }
        // The request types that carry one check it, and a create carries none.
        let set = |expected: Option<&str>| RefSetReq {
            name: "main".to_owned(),
            new: "main".into(),
            expected_old: expected.map(Rev::from),
            create: expected.is_none(),
        };
        assert!(set(None).refuse().is_ok());
        assert!(set(Some(&id)).refuse().is_ok());
        assert!(set(Some("main")).refuse().is_err());
        let delete = |expected: &str| RefDeleteReq {
            name: "main".to_owned(),
            expected_old: expected.into(),
        };
        assert!(delete(&id).refuse().is_ok());
        assert!(delete("main~1").refuse().is_err());
    }

    /// A panic in a tool's operation is that call's `E_INTERNAL` envelope, with the store the
    /// server names and what the operation resolved before it panicked, as a command's is.
    #[test]
    fn a_panic_in_an_operation_is_e_internal_with_what_it_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = format!("{}/store", dir.path().to_str().unwrap());
        let (ops, _) = LocalOps::init(Utf8Path::new(&store), "acme").unwrap();
        let who = Identity::parse("agent/worker-1").unwrap();
        let change = ChangeInput {
            path: "a.txt".to_owned(),
            op: ChangeOp::Put,
            content: Some(ContentInput::Inline("a".to_owned())),
            mode: None,
        };
        let req = CommitReq {
            parent: None,
            changes: vec![change],
            message: "m".to_owned(),
            allow_empty: false,
        };
        let c1 = ops.commit(&who, req).unwrap().changeset;
        let main = RefSetReq {
            name: "main".to_owned(),
            new: c1.as_str().into(),
            expected_old: None,
            create: true,
        };
        ops.ref_set(&who, main).unwrap();
        drop(ops);

        let global = Global {
            repo: Some(store.clone()),
            ..Global::default()
        };
        let env = |_: &str| -> Option<OsString> { None };
        let server = Server::new(&global, &env);
        let id = json!(7);
        let reply = Reply {
            id: &id,
            era: Era::Previous,
        };
        let spec = TOOLS.iter().find(|spec| spec.name == "show").unwrap();
        let answer = run(
            &server,
            spec,
            reply,
            || Ok(()),
            |ops, ()| -> Result<(), OpError> {
                let rev = "main".into();
                ops.show(ShowReq {
                    rev,
                    max_changes: 1,
                })?;
                panic!("boom after a read")
            },
        );
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["id"], 7);
        let result = &answer["result"];
        assert_eq!(result["isError"], true);
        let envelope = &result["structuredContent"];
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), envelope);
        assert_eq!(
            (&envelope["op"], &envelope["error"]["code"]),
            (&json!("show"), &json!("E_INTERNAL"))
        );
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.ends_with(": boom after a read"), "{message}");
        assert_eq!(envelope["meta"]["repo"], store.as_str());
        assert_eq!(envelope["meta"]["resolved"], json!({"main": c1}));
    }

    /// Each tool's description is one line, without a run of spaces that a broken string
    /// continuation leaves; `the_tools_are_the_store_operations` checks the names and the
    /// request types.
    #[test]
    fn every_tool_has_a_description_of_one_line() {
        for spec in &TOOLS {
            assert!(!spec.description.contains('\n'), "{}", spec.name);
            assert!(!spec.description.contains("  "), "{}", spec.name);
        }
    }

    #[test]
    fn a_schema_stands_alone_with_both_descriptions() {
        let schema = json!({
            "$defs": {
                "Rev": {"type": "string", "description": "A revision."},
                "Pair": {"type": "object", "properties": {"rev": {"$ref": "#/$defs/Rev"}}},
            },
            "type": "object",
            "properties": {
                "at": {"$ref": "#/$defs/Rev", "description": "Where to read."},
                "maybe": {"anyOf": [{"$ref": "#/$defs/Pair"}, {"type": "null"}]},
                "plain": {"$ref": "#/$defs/Rev", "minLength": 1},
            },
        });
        assert_eq!(
            standalone(&schema),
            json!({
                "type": "object",
                "properties": {
                    "at": {"type": "string", "description": "Where to read. A revision."},
                    "maybe": {"anyOf": [
                        {"type": "object", "properties": {
                            "rev": {"type": "string", "description": "A revision."},
                        }},
                        {"type": "null"},
                    ]},
                    "plain": {"type": "string", "description": "A revision.", "minLength": 1},
                },
            })
        );
    }

    #[test]
    #[should_panic(expected = "refers to itself")]
    fn a_schema_that_refers_to_itself_cannot_stand_alone() {
        let schema = json!({
            "$defs": {"Tree": {"type": "array", "items": {"$ref": "#/$defs/Tree"}}},
            "$ref": "#/$defs/Tree",
        });
        standalone(&schema);
    }

    #[test]
    fn a_request_claims_its_version() {
        let meta = |meta: Value| {
            let mut params = Map::new();
            params.insert("_meta".to_owned(), meta);
            params
        };
        assert_eq!(claim(&Map::new()).ok(), Some(Claim::None));
        assert_eq!(claim(&meta(json!({"other": 1}))).ok(), Some(Claim::None));
        for version in HANDSHAKE.into_iter().chain(["2024-11-05"]) {
            assert_eq!(
                claim(&meta(json!({VERSION_KEY: version}))).ok(),
                Some(Claim::Handshake(version.to_owned())),
                "an earlier version is the session's, or wants one"
            );
        }
        assert_eq!(
            claim(&meta(json!({VERSION_KEY: LATEST, CAPABILITIES_KEY: {}}))).ok(),
            Some(Claim::Latest)
        );
        assert!(matches!(
            claim(&meta(json!({VERSION_KEY: "2027-01-01", CAPABILITIES_KEY: {}}))),
            Err(Rejected::Version(version)) if version == "2027-01-01"
        ));
        for refused in [
            json!({VERSION_KEY: LATEST}),
            json!({VERSION_KEY: LATEST, CAPABILITIES_KEY: []}),
            json!({VERSION_KEY: 20_260_728}),
            json!("not an object"),
        ] {
            assert!(
                matches!(claim(&meta(refused.clone())), Err(Rejected::Params(_))),
                "{refused}"
            );
        }
    }
}
