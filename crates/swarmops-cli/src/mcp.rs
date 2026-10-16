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
//! It serves two revisions of the protocol, each the way its clients open:
//!
//! - [`LATEST`], 2026-07-28, which has no handshake: a request whose `params._meta` names
//!   `io.modelcontextprotocol/protocolVersion` 2026-07-28 or later is served on its own, by that
//!   revision's rules, and must carry `io.modelcontextprotocol/clientCapabilities` too. Its
//!   methods are `server/discover`, `tools/list` and `tools/call`. Each result has
//!   `resultType: "complete"` and the server's identity in
//!   `_meta["io.modelcontextprotocol/serverInfo"]`, and the results of `server/discover` and
//!   `tools/list` say how long a client may keep them (`ttlMs`, an hour; `cacheScope:
//!   "public"`): they change only with the binary.
//! - [`PREVIOUS`], 2025-11-25, the last revision with a handshake: `initialize` opens a session
//!   that lasts as long as the process, and answers with 2025-11-25 whatever version the client
//!   asks for (one that cannot speak it disconnects). From then on, a request that names no
//!   version of 2026-07-28 or later in `_meta` is served by 2025-11-25's rules: `tools/list`
//!   and `tools/call`, and `ping`, which is also answered before `initialize`.
//!
//! Notifications get no answer, `notifications/initialized` included; a
//! `notifications/cancelled` finds nothing to cancel, since a request is answered before the
//! next message is read.
//!
//! # Errors
//!
//! A message that is not a request the server can serve gets a JSON-RPC error:
//!
//! | Code | Name | For |
//! |---|---|---|
//! | `-32700` | Parse error | a line that is not JSON, or not UTF-8; answered without an `id` |
//! | `-32600` | Invalid Request | a batch, or anything else that is not one object; an object whose `jsonrpc` is not `"2.0"`, whose `id` is not a string or a number (answered without an `id`), or that has neither a string `method` nor a response's `result` or `error` |
//! | `-32601` | Method not found | a method its revision does not have (`ping` in 2026-07-28, `initialize` in a request that names 2026-07-28) |
//! | `-32602` | Invalid params | `params`, or a `params._meta`, that is not an object, or a protocol version in it that is not a string; before `initialize`, a request that names no version in `_meta` (but `ping`), and a `server/discover` that names none at any time; a 2026-07-28 `_meta` without the client's capabilities; an `initialize` without the client's `protocolVersion`; a `tools/call` without a known tool's name, or with `arguments` that are not an object; a `tools/list` cursor (there is one page) |
//! | `-32603` | Internal error | a panic outside an operation (a bug) |
//! | `-32022` | Unsupported protocol version | a `_meta` version from 2026-07-28 on that is not 2026-07-28; `data` gives the `supported` versions and the `requested` one |
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
//! does not know is ignored, and arguments that do not fit the type are `E_INVALID_ARG`. A
//! mutation then needs the server's identity, and last the store is opened and the operation
//! runs; a `verify` that finds problems is `E_STORE_CORRUPT`, as `swarmops verify` is, and a
//! panic in the operation is `E_INTERNAL`. The tool result holds the envelope the CLI prints
//! for the same request, as its `structuredContent` and as the text of its one content block,
//! and `isError` is true exactly when the envelope's `ok` is false.
//!
//! # The store and the identity
//!
//! Both are the server's, never a call's. The store is `--repo`, else `SWARMOPS_REPO`, opened
//! anew for each call, so that a call sees what other processes committed; a store that is not
//! there is each call's `E_NOT_A_REPO`, and the server serves on. The identity is `--as`, else
//! `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID`; a mutation without one is `E_IDENTITY_REQUIRED`.
//! The client starts the server as its own user, in its own trust domain, so `{"file": <path>}`
//! content is read here, as the CLI reads it, a relative path from the server's working
//! directory. The output flags (`--json`, `--human`, `--pretty`) do not apply. A command line
//! the CLI refuses (a usage error, a `SWARMOPS_FORMAT` that is neither `json` nor `human`)
//! starts no server: it prints its envelope and exits with its code, as any command does.

use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use swarmops_api::{Envelope, ErrorCode, Identity, OpError, StoreOps, VerifyReq, schema_for_all};

use crate::cli::Global;
use crate::commands::{self, Ctx, Used};
use crate::context::{self, Env};
use crate::output::{self, Format, Printed};

/// MCP's latest revision, served without a handshake.
pub const LATEST: &str = "2026-07-28";

/// The revision before it, the last with a handshake: served to clients that open with
/// `initialize`.
pub const PREVIOUS: &str = "2025-11-25";

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
{ok, op, api, result | error, meta}, as structuredContent and as JSON text. A failed operation \
sets isError and gives error {code, message, retryable, exit, details, hint}: retryable says \
whether the same call can succeed again, and a hint is often a swarmops command, whose operation \
is the tool of the same name (ref list is ref_list, cat read_file, ls read_tree, info repo_info). \
A revision is main, release/<name>, a changeset id or 12 or more of its first hex digits, then \
optionally ~N for the N-th first parent. commit records a changeset and moves no ref; ref_set \
moves a ref by compare-and-swap, and after E_CAS_MISMATCH you commit your change again on top of \
details.actual, re-applying your edits to the content there, then retry with expected_old set to \
it. Every change is recorded for the agent the server runs as.";

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
        description: "Reads one file at a revision, without a checkout: UTF-8 text, or base64, \
                      at most max_bytes of it.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::ReadTree,
        name: "read_tree",
        request: "ReadTreeReq",
        title: "List a directory",
        description: "Lists one directory at a revision, without a checkout, a page of entries \
                      at a time.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::Show,
        name: "show",
        request: "ShowReq",
        title: "Show a changeset",
        description: "One changeset in full: its parents, author, message, changes and \
                      signatures.",
        effect: Effect::Reads,
    },
    Spec {
        tool: Tool::Log,
        name: "log",
        request: "LogReq",
        title: "List history",
        description: "History along first parents, newest first, a page at a time; from main \
                      unless rev says otherwise.",
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
        description: "Moves a ref, or creates one, by compare-and-swap: give expected_old, the \
                      target you last saw, or create.",
        effect: Effect::Moves,
    },
    Spec {
        tool: Tool::RefDelete,
        name: "ref_delete",
        request: "RefDeleteReq",
        title: "Delete a ref",
        description: "Removes a ref by compare-and-swap, if it still points at expected_old.",
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
    /// Whether `initialize` has opened a 2025-11-25 session.
    in_session: bool,
}

/// The revision whose rules a request is served by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    /// 2026-07-28: the request names it in `_meta`.
    Latest,
    /// 2025-11-25: the request names no version of 2026-07-28 or later.
    Previous,
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
            in_session: false,
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
            Some(id @ (Value::String(_) | Value::Number(_))) => Some(id),
            Some(_) => {
                return Some(error(
                    None,
                    INVALID_REQUEST,
                    "Invalid Request: an id is a string or a number",
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
        let era = match era(&params) {
            Ok(era) => era,
            Err(rejected) => return rejected.answer(id),
        };
        let reply = Reply { id, era };
        match (era, method) {
            (Era::Latest, "server/discover") => reply.result(Discovered {
                supported_versions: [LATEST],
                capabilities: Capabilities { tools: Empty {} },
                instructions: INSTRUCTIONS,
                ttl_ms: TTL_MS,
                cache_scope: "public",
            }),
            (Era::Previous, "initialize") => self.initialize(reply, &params),
            (Era::Previous, "ping") => reply.result(Empty {}),
            (Era::Previous, "server/discover") => error(
                Some(id),
                INVALID_PARAMS,
                format!(
                    "Invalid params: server/discover is {LATEST}'s, whose requests name that \
                     version in params._meta ({VERSION_KEY})"
                ),
            ),
            (Era::Previous, "tools/list" | "tools/call") if !self.in_session => error(
                Some(id),
                INVALID_PARAMS,
                format!(
                    "Invalid params: {method} names no protocol version in params._meta \
                     ({VERSION_KEY}), and no initialize has opened a session"
                ),
            ),
            (_, "tools/list") => self.list_tools(reply, &params),
            (_, "tools/call") => self.call_tool(reply, params),
            _ => error(
                Some(id),
                METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            ),
        }
    }

    /// `initialize`: opens the 2025-11-25 session, whatever version the client asked for.
    fn initialize(&mut self, reply: Reply<'_>, params: &Map<String, Value>) -> String {
        if !params.get("protocolVersion").is_some_and(Value::is_string) {
            return error(
                Some(reply.id),
                INVALID_PARAMS,
                "Invalid params: initialize names the protocolVersion the client speaks",
            );
        }
        self.in_session = true;
        reply.result(Initialized {
            protocol_version: PREVIOUS,
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
        };
        match spec.tool {
            Tool::RepoInfo => call.read(arguments, |ops, req| ops.repo_info(req)),
            Tool::ReadFile => call.read(arguments, |ops, req| ops.read_file(req)),
            Tool::ReadTree => call.read(arguments, |ops, req| ops.read_tree(req)),
            Tool::Show => call.read(arguments, |ops, req| ops.show(req)),
            Tool::Log => call.read(arguments, |ops, req| ops.log(req)),
            Tool::RefList => call.read(arguments, |ops, req| ops.ref_list(req)),
            Tool::RefSet => call.mutate(arguments, |ops, who, req| ops.ref_set(who, req)),
            Tool::RefDelete => call.mutate(arguments, |ops, who, req| ops.ref_delete(who, req)),
            Tool::Commit => call.mutate(arguments, |ops, who, req| ops.commit(who, req)),
            Tool::Verify => call.read(arguments, |ops, req: VerifyReq| {
                let deep = req.deep;
                ops.verify(req)
                    .and_then(|report| commands::clean(report, deep))
            }),
        }
    }
}

/// The era of a request with `params`: 2026-07-28's when its `_meta` names that version or a
/// later one, else 2025-11-25's. A version of 2026-07-28 or later that is not 2026-07-28, and a
/// 2026-07-28 `_meta` without the client's capabilities, are refused.
fn era(params: &Map<String, Value>) -> Result<Era, Rejected> {
    let meta = match params.get("_meta") {
        None => return Ok(Era::Previous),
        Some(Value::Object(meta)) => meta,
        Some(_) => return Err(Rejected::params("params._meta is an object")),
    };
    let version = match meta.get(VERSION_KEY) {
        None => return Ok(Era::Previous),
        Some(Value::String(version)) => version,
        Some(_) => return Err(Rejected::params(&format!("{VERSION_KEY} is a string"))),
    };
    // Revisions are dates, so they sort as strings.
    if version.as_str() < LATEST {
        return Ok(Era::Previous);
    }
    if version != LATEST {
        return Err(Rejected::Version(version.clone()));
    }
    if !meta.get(CAPABILITIES_KEY).is_some_and(Value::is_object) {
        return Err(Rejected::params(&format!(
            "a {LATEST} request carries {CAPABILITIES_KEY} in params._meta"
        )));
    }
    Ok(Era::Latest)
}

/// A request refused before its method is looked at.
enum Rejected {
    /// Its params break a rule: the message says which.
    Params(String),
    /// It names a version from 2026-07-28 on that is not served.
    Version(String),
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
                    "Unsupported protocol version: {requested}; this server serves {LATEST}, \
                     and {PREVIOUS} through initialize"
                ),
                Some(json!({"supported": [LATEST], "requested": requested})),
            ),
        }
    }
}

/// One `tools/call` of the tool `spec`, answered as `reply`.
#[derive(Clone, Copy)]
struct Call<'s, 'a> {
    server: &'s Server<'a>,
    spec: &'static Spec,
    reply: Reply<'s>,
}

impl Call<'_, '_> {
    /// Runs a read: `arguments` as its request `R`, then `call` on the store.
    fn read<R: DeserializeOwned, T: Serialize>(
        self,
        arguments: Value,
        call: impl FnOnce(&dyn StoreOps, R) -> Result<T, OpError>,
    ) -> String {
        let spec = self.spec;
        self.run(|| request_of(spec, arguments), call)
    }

    /// Runs a mutation: `arguments` as its request `R`, then the server's identity, then `call`
    /// on the store as that identity.
    fn mutate<R: DeserializeOwned, T: Serialize>(
        self,
        arguments: Value,
        call: impl FnOnce(&dyn StoreOps, &Identity, R) -> Result<T, OpError>,
    ) -> String {
        let spec = self.spec;
        let (agent, env) = (self.server.global.agent.as_deref(), self.server.env);
        self.run(
            || {
                let req = request_of(spec, arguments)?;
                Ok((context::mutator(agent, env)?, req))
            },
            |ops, (who, req)| call(ops, &who, req),
        )
    }

    /// Runs the tool as a command runs ([`Ctx::envelope_on_store`]): `prepare` builds its
    /// input, then `call` runs on the store, opened; a panic in either is `E_INTERNAL`. The tool
    /// result that holds the envelope.
    fn run<P, T: Serialize>(
        self,
        prepare: impl FnOnce() -> Result<P, OpError>,
        call: impl FnOnce(&dyn StoreOps, P) -> Result<T, OpError>,
    ) -> String {
        let ctx = Ctx {
            global: self.server.global,
            env: self.server.env,
            format: Format::Json { pretty: false },
            started: Instant::now(),
            used: Used::default(),
        };
        let op = self.spec.name;
        match panic::catch_unwind(AssertUnwindSafe(|| {
            ctx.envelope_on_store(op, prepare, call)
        })) {
            Ok(envelope) => self.reply.tool_result(&envelope),
            Err(payload) => {
                let bug = commands::panicked(&*payload);
                let envelope = commands::failed_envelope(op, bug, &ctx.used, ctx.started);
                self.reply.tool_result(&envelope)
            }
        }
    }
}

/// The request `arguments` stand for, of tool `spec`'s type: `E_INVALID_ARG` when they do not
/// fit it. Fields the type does not know are ignored, as the API's types always ignore them.
fn request_of<R: DeserializeOwned>(spec: &Spec, arguments: Value) -> Result<R, OpError> {
    serde_json::from_value(arguments).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            output::printable(&format!("the arguments are not a {}: {e}", spec.request)),
        )
        .with_hint(format!(
            "the {} tool's inputSchema gives its arguments, as swarmops schema {} does",
            spec.name, spec.request
        ))
    })
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
/// could not be read), as one line.
fn error(id: Option<&Value>, code: i64, message: impl AsRef<str>) -> String {
    error_with(id, code, message.as_ref(), None)
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
    /// with the same arguments, it finds the ref as it asked and changes nothing more.
    Moves,
}

impl Effect {
    /// The tool annotations of a tool with this effect.
    fn annotations(self) -> Value {
        let (read_only, destructive, idempotent) = match self {
            Effect::Reads => (true, false, true),
            Effect::Adds => (false, false, false),
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
    supported_versions: [&'static str; 1],
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
    use std::ffi::OsString;

    use camino::Utf8Path;
    use swarmops_api::{ChangeInput, ChangeOp, CommitReq, ContentInput, RefSetReq, ShowReq};
    use swarmops_ops::LocalOps;

    use super::*;

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
        let call = Call {
            server: &server,
            spec: TOOLS.iter().find(|spec| spec.name == "show").unwrap(),
            reply: Reply {
                id: &id,
                era: Era::Previous,
            },
        };
        let answer = call.run(
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

    #[test]
    fn every_tool_names_a_request_type_and_a_line_of_its_own() {
        let all = schema_for_all();
        let mut names = Vec::new();
        for spec in &TOOLS {
            assert!(all.get(spec.request).is_some(), "{}", spec.request);
            assert!(!spec.description.contains('\n'), "{}", spec.name);
            assert!(!spec.description.contains("  "), "{}", spec.name);
            names.push(spec.name);
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TOOLS.len(), "two tools share a name");
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
    fn a_request_names_its_era() {
        let meta = |meta: Value| {
            let mut params = Map::new();
            params.insert("_meta".to_owned(), meta);
            params
        };
        let capabilities = json!({});
        assert_eq!(era(&Map::new()).ok(), Some(Era::Previous));
        assert_eq!(era(&meta(json!({"other": 1}))).ok(), Some(Era::Previous));
        assert_eq!(
            era(&meta(json!({VERSION_KEY: PREVIOUS}))).ok(),
            Some(Era::Previous),
            "an earlier version is the session's"
        );
        assert_eq!(
            era(&meta(
                json!({VERSION_KEY: LATEST, CAPABILITIES_KEY: capabilities})
            ))
            .ok(),
            Some(Era::Latest)
        );
        assert!(matches!(
            era(&meta(json!({VERSION_KEY: "2027-01-01", CAPABILITIES_KEY: {}}))),
            Err(Rejected::Version(version)) if version == "2027-01-01"
        ));
        for refused in [
            json!({VERSION_KEY: LATEST}),
            json!({VERSION_KEY: LATEST, CAPABILITIES_KEY: []}),
            json!({VERSION_KEY: 20_260_728}),
            json!("not an object"),
        ] {
            assert!(
                matches!(era(&meta(refused.clone())), Err(Rejected::Params(_))),
                "{refused}"
            );
        }
    }
}
