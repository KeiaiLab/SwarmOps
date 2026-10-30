//! `swarmops mcp`: the store's operations as an MCP server on stdin and stdout, for the agent
//! harnesses that speak the Model Context Protocol.
//!
//! # The protocol
//!
//! MCP over stdio: JSON-RPC 2.0, one message per line. The server reads a message from stdin,
//! answers it on stdout if it is a request, and reads the next: one at a time, in order. stdout
//! carries nothing but answers. stderr gets a line for a message that has no answer although it
//! is neither a request nor a notification (a response from the client), a line for the I/O
//! error that stops the server, the panic hook's report of a bug, and the reason one of the
//! store's four documented aborts (storage spec §2; README) writes before
//! `std::process::abort` ends the process; nothing else. The server exits 0 when stdin ends, 1
//! if reading stdin or writing stdout fails, and by SIGABRT on those aborts.
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
//! A line is checked to be JSON whole first, by the rules a parse into a JSON value holds it to
//! ([`Json`]), but nothing is built of it: then it is read only as far as the server serves it.
//! Of a message, that is its `id`, `jsonrpc`, `method` and `params`, and of those `params` the
//! `_meta`, `protocolVersion`, `cursor`, `name` and `arguments` ([`members`]), each read from its
//! own JSON text when it is needed; a tool's arguments are read as its request type and nothing
//! else. What else a line holds (a batch, a member the server does not read, a field that a
//! request type does not know) is passed over and kept nowhere, so that a line costs the server
//! no memory beyond its own bytes but what the request it holds takes as its type.
//!
//! | Code | Name | For |
//! |---|---|---|
//! | `-32700` | Parse error | a line that is not JSON, or not UTF-8 |
//! | `-32600` | Invalid Request | a batch, or anything else that is not one object; an object whose `jsonrpc` is not `"2.0"`, whose `id` is not a string or an integer (null, a fraction, an object, an array, a boolean), or that has neither a string `method` nor a response's `result` or `error`; a line longer than `MAX_REQUEST_BYTES` (64 MiB), read no further than one byte past it and the rest passed over to the next line break, always without an `id` |
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
//! `resolve_rev`, `read_file`, `read_tree`, `show`, `log`, `path_history`, `diff`, `op_log`,
//! `ref_list`, `ref_set`, `ref_delete`, `undo`, `commit`, `land`, `blob_put`, `blob_has`,
//! `verify` and the maintenance's `gc`, `repair` and `upgrade` (M6), in `tools/list`'s order; then the git remotes' client-side operations (M13), which
//! no [`StoreOps`] has: `remote_list`, `fetch`, `push`, `sync` and `sync_status`, run on the
//! store's own operations ([`operate_local`]) and taking a remote by its name only, never a URL
//! or a path. `clone` and `remote add|remove` have no tool, so that an agent cannot point the
//! store at a URL of its choosing. A tool's `inputSchema` is its request type's JSON Schema, as
//! `swarmops schema <type>` prints it, with each `$ref` replaced by the definition it names so that
//! the schema stands alone: a keyword beside the reference wins over the definition's, but for
//! `description`, where the reference's comes first and the definition's after it. It is kept a
//! plain object ([`tool_input_schema`]): nothing at its top but `$schema`, `title`,
//! `description`, `type`, `properties` and `required`, since the Claude API refuses a request
//! whose tools hold an input schema with a combinator there, so that one such tool would stop
//! every request of the session; a rule across fields that the request type's schema states
//! with one (`RefSetReq`'s exactly one of `expected_old` and `create`) is the description's and
//! the server's own check. The `expected_old` of `ref_set` and `ref_delete` is a string of the
//! full-id pattern ([`full_id_pattern`]), not any revision. A tool's annotations say whether it
//! changes the store and how; none reaches beyond the store (`openWorldHint: false`) but
//! `fetch`, `push` and `sync`, which reach a git remote.
//!
//! `derive` and `serve` have no tool: `swarmops derive` runs beside the store as maintenance,
//! deriving values ahead of readers as the journal grows, and `swarmops serve` serves stock git,
//! neither of which is an agent's work.
//!
//! A call runs as one command of the CLI does. Its arguments are the request: a field the type
//! does not know is ignored, and arguments that do not fit the type, a field of it named twice
//! among them included (as the CLI refuses one in a change list), are `E_INVALID_ARG`, as is
//! a request that breaks one of this server's own rules ([`Refuse`]): a `ref_set` or a
//! `ref_delete` must name what it expects the ref to hold by its full changeset id, since a ref
//! name or a relative revision resolves when the call runs and so defeats the compare-and-swap,
//! and an id prefix stops resolving once another changeset shares it (the CLI takes any
//! revision for `--expect`, and its help warns why not to); a `land` must name what its
//! `resolutions` were decided `against` by its full changeset id, for the same reasons; and a
//! `commit`, a `blob_put` or a `land` whose `{"file": <path>}` content names a relative path,
//! this process's own stdin or stdout, or anything but a regular file, is refused, since the
//! server would read the file from a working directory the agent cannot see, block on the pipe
//! that carries the protocol, or block on a FIFO or a device. A mutation then
//! needs the server's identity, and last the store is opened and the operation runs; a
//! `verify` that finds problems is `E_STORE_CORRUPT`, as `swarmops verify` is, and a panic in
//! the operation is `E_INTERNAL`. The tool result holds the envelope the CLI prints for the
//! same request, as its `structuredContent` and as the text of its one content block, and
//! `isError` is true exactly when the envelope's `ok` is false.
//!
//! Clients limit long tool results. Claude Code saves a successful one over 25,000 tokens by
//! default, or over 50,000 characters whatever its token count, to a file and gives the model
//! its path instead; it cuts only the text of an error, past about 11,000 characters. A result
//! holds its envelope twice, so the descriptions ask the model for small reads (`max_bytes` of
//! 32 KiB or less, `limit` and `max_changes` of 100 or less, paging with the `cursor`; an
//! `op_log` `limit` of 20 or less, paging with `before`), and [`INSTRUCTIONS`] points it at
//! them; the API's defaults stay as the operations spec gives them. `read_file`, the
//! one tool whose result is file content the model sizes, raises that threshold for itself: its
//! entry in `tools/list` carries `_meta: {"anthropic/maxResultSizeChars": 200000}`
//! ([`READ_FILE_RESULT_CHARS`]), and Claude Code keeps a `read_file` result of up to that many
//! characters in the conversation (500,000 at most) instead of saving it to a file. The other
//! tools carry no `_meta`.
//!
//! # The store and the identity
//!
//! Both are the server's, never a call's. The store is `--repo`, else `SWARMOPS_REPO`, opened
//! anew for each call, so that a call sees what other processes committed; a store that is not
//! there is each call's `E_NOT_A_REPO`, and the server serves on. The identity is `--as`, else
//! `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID`, as for the CLI; a mutation without one is
//! `E_IDENTITY_REQUIRED`. The client starts the server as its own user, in its own trust
//! domain, so `{"file": <path>}` content is read here, as the CLI reads it; a relative path,
//! one that is this process's own stdin or stdout, and one that is not a regular file are
//! refused ([`Refuse`]). The output flags
//! (`--json`, `--human`, `--pretty`) do not
//! apply. A command line that parses as `swarmops mcp` but is refused (a usage error in what
//! follows the word `mcp`, a `SWARMOPS_FORMAT` that is neither `json` nor `human`), and a panic
//! before the server serves, start no server: their envelope goes to stderr, not to stdout,
//! which belongs to the protocol, and the process exits with its code. Of the flags, only those
//! after `mcp` are refused that way: a flag refused before the subcommand (`swarmops --frob
//! mcp`) is a plain usage error of the CLI, whose envelope goes to stdout with exit code 2, and
//! no server starts either.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::str;
use std::time::Instant;

use camino::Utf8Path;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess,
    Visitor,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Number, Value, json};
use swarmops_api::LandReq;
use swarmops_api::RebaseReq;
use swarmops_api::ResolveRevReq;
use swarmops_api::{BlobHasReq, BlobPutReq};
use swarmops_api::{
    CommitReq, ContentInput, DiffReq, Envelope, ErrorCode, Identity, InfoReq, LogReq, OpError,
    OpLogReq, PathHistoryReq, ReadFileReq, ReadTreeReq, RefDeleteReq, RefListReq, RefSetReq, Rev,
    ShowReq, StoreOps, UndoReq, VerifyReq,
};
use swarmops_api::{
    FetchReq, FetchResp, PushReq, PushResp, RemoteList, RemoteListReq, SyncReq, SyncResp,
    SyncStatus, SyncStatusReq,
};
use swarmops_ops::Recording;
use swarmops_ops::content::MAX_REQUEST_BYTES;

use crate::cli::Global;
use crate::commands::{self, Ctx, Used};
use crate::context::{self, Env};
use crate::output::{self, Format, Layout, Printed};

use self::mcp_ws::{WS_TOOLS, operate_ws};
use swarmops_ws::hook::Bootstrap;

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

/// The most bytes the line buffer keeps between messages: a line can be up to
/// [`MAX_REQUEST_BYTES`], which a long-lived server should not hold on to.
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
retryable says whether the operation can succeed when it is run again, as it is or after \
re-reading what the hint names, and a hint is often a swarmops command, \
whose operation is the tool of the same name (ref list is ref_list, cat read_file, ls \
read_tree, info repo_info). commit records a changeset; with advance it also moves that ref \
to it in the same transaction, if the ref still names parent: check result.advanced. ref_set \
and ref_delete change a ref by compare-and-swap, so their expected_old, like commit's parent \
with advance, is the full 64-hex changeset id the ref held when you read it (from ref_list, \
show, log or a commit's result). A \
ref name or a relative revision is read when the call runs instead of pinning the id you read: \
the ref's own name always matches, so the compare-and-swap protects nothing, and main~1 on ref \
main never matches; an id prefix stops resolving once another changeset shares it; the server \
refuses all three. To publish a changeset, land it: land merges it three-way onto main and \
moves main in one step, and on E_CONFLICT details.report says what to resolve. A read \
may show a commit that a failed sync then takes back; a mutation that names it fails under the \
lock instead of building on it. op_log lists the store's transactions, newest first; undo \
restores the refs one moved while they still hold what it set. Clients limit long tool results: \
ask for little, as each tool's description says. A {\"file\": path} content is read by this \
server, not by your client: give an absolute path (a relative one, or the server's own stdin or \
stdout, is refused). Every change is recorded for the agent the server runs as.";

/// The most characters of a `read_file` result that Claude Code keeps in the conversation
/// instead of saving it to a file, which `read_file`'s entry in `tools/list` declares under
/// [`RESULT_CHARS_KEY`]: four times Claude Code's default of 50,000, and under its ceiling of
/// 500,000 even for a client that counts both copies of the envelope a result holds.
const READ_FILE_RESULT_CHARS: u32 = 200_000;

/// The `_meta` key of a tool's entry in `tools/list` under which Claude Code reads the most
/// characters of the tool's text results to keep in the conversation.
const RESULT_CHARS_KEY: &str = "anthropic/maxResultSizeChars";

/// The tools, in the order `tools/list` gives them.
static TOOLS: [Spec; 27] = [
    Spec {
        tool: Tool::RepoInfo,
        name: "repo_info",
        request: "InfoReq",
        title: "Describe the store",
        description: "What the store is and holds: its id and name, the changeset main points \
                      at, and how many changesets and refs it has.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::ResolveRev,
        name: "resolve_rev",
        request: "ResolveRevReq",
        title: "Resolve a revision",
        description: "The full id of the changeset a revision names (a ref name, a full id or a \
                      unique prefix of 12 or more hex digits, then any ~N suffixes), resolved as \
                      every tool resolves one, and its kind: ref when the revision is exactly a \
                      ref's name, whose target moves with the ref, changeset otherwise. Its CLI \
                      command is swarmops resolve.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::ReadFile,
        name: "read_file",
        request: "ReadFileReq",
        title: "Read a file",
        description: "Reads one file at a revision, without a checkout, as UTF-8 text or \
                      base64, from offset or the lines a range names; pass max_bytes of 32768 or \
                      less (less for a binary file), since clients limit long tool results and \
                      this one holds the content twice, and read on from next_offset while the \
                      result is truncated, at the full id meta.resolved gave for rev: \
                      next_offset pins no revision, and reads on to the file's end, past a lines \
                      range.",
        effect: Effect::Reads,
        result_chars: Some(READ_FILE_RESULT_CHARS),
    },
    Spec {
        tool: Tool::ReadTree,
        name: "read_tree",
        request: "ReadTreeReq",
        title: "List a directory",
        description: "Lists one directory at a revision, without a checkout, a page of entries \
                      at a time, each file with its size, and with depth the directories below it \
                      too; pass a limit of 100 or less and page on with the cursor at the full id \
                      meta.resolved gave for rev, since clients limit long tool results and a \
                      cursor pins no revision.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::Show,
        name: "show",
        request: "ShowReq",
        title: "Show a changeset",
        description: "One changeset in full: its parents, author, message, changes and \
                      signatures; pass max_changes of 100 or less, since clients limit long tool \
                      results.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::Log,
        name: "log",
        request: "LogReq",
        title: "List history",
        description: "History along first parents, newest first, from main unless rev says \
                      otherwise; pass a limit of 100 or less and page on with the cursor, since \
                      clients limit long tool results.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::PathHistory,
        name: "path_history",
        request: "PathHistoryReq",
        title: "List a path's history",
        description: "The changesets that changed a file, or anything below a directory, newest \
                      first along first parents from main unless rev says otherwise, each with \
                      what the path holds after it (the CLI command is history); pass a limit of \
                      100 or less and page on with the cursor, since clients limit long tool \
                      results.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::Diff,
        name: "diff",
        request: "DiffReq",
        title: "Diff two revisions",
        description: "The files that differ between two revisions, in path order: with format \
                      names (the default) each one's path and status, with stat also its line \
                      counts, with unified also its hunks; pass a limit of 100 or less, keep \
                      max_bytes at 32768 or less for unified, and page on with the cursor and the \
                      first page's from and to (full ids), since clients limit long tool results \
                      and a cursor pins no revision.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::OpLog,
        name: "op_log",
        request: "OpLogReq",
        title: "List operations",
        description: "The store's operations, newest first: each write transaction, with the id \
                      undo takes, its kind, the changesets it recorded and the refs it moved; \
                      pass a limit of 20 or less and page on with before, since clients \
                      limit long tool results.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::RefList,
        name: "ref_list",
        request: "RefListReq",
        title: "List refs",
        description: "Every ref, the changeset it points at, and how many moves it has had.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::RefSet,
        name: "ref_set",
        request: "RefSetReq",
        title: "Move a ref",
        description: "Moves a ref, or creates one, by compare-and-swap: give create, or \
                      expected_old, the full 64-hex changeset id the ref held when you read it; a \
                      ref name or a relative revision is refused, since it is read when the call \
                      runs instead of pinning the id you read, and so is an id prefix, which \
                      stops resolving once another changeset shares it.",
        effect: Effect::Moves,
        result_chars: None,
    },
    Spec {
        tool: Tool::RefDelete,
        name: "ref_delete",
        request: "RefDeleteReq",
        title: "Delete a ref",
        description: "Removes a ref by compare-and-swap, if it still holds expected_old, the \
                      full 64-hex changeset id it held when you read it; a ref name or a relative \
                      revision is refused, since it is read when the call runs instead of pinning \
                      the id you read, and so is an id prefix, which stops resolving once another \
                      changeset shares it.",
        effect: Effect::Moves,
        result_chars: None,
    },
    Spec {
        tool: Tool::Undo,
        name: "undo",
        request: "UndoReq",
        title: "Undo an operation",
        description: "Moves every ref an operation of op_log moved back to what it named before, \
                      by compare-and-swap in one transaction: all of them or none. It takes the \
                      operation's id from op_log (4:9f3a01c2); a bare number is E_INVALID_ARG. \
                      A ref moved since stops it (E_CAS_MISMATCH, nothing written: undo the \
                      later operations first); an undo run again is a replay (already_applied). \
                      Changesets are never removed.",
        effect: Effect::Moves,
        result_chars: None,
    },
    Spec {
        tool: Tool::Commit,
        name: "commit",
        request: "CommitReq",
        title: "Commit a changeset",
        description: "Records a changeset from an explicit change list, on parent or else on \
                      main's head. With advance it also moves that ref to the changeset in the \
                      same transaction, only if the ref still names parent, which must then be \
                      the full 64-hex id you read; otherwise it records nothing (E_CAS_MISMATCH, \
                      details.committed false). Check result.advanced: a server that does not \
                      know advance moves no ref. An idempotency_key, a uuid v7 new for each \
                      commit, makes a retry safe: the same call again replays it \
                      (result.replayed) instead of recording it twice. A change list that \
                      changes nothing is E_EMPTY_COMMIT unless allow_empty is true. A \
                      {\"file\": path} content is read by this server: give an absolute \
                      path.",
        effect: Effect::Adds,
        result_chars: None,
    },
    Spec {
        tool: Tool::Land,
        name: "land",
        request: "LandReq",
        title: "Land a change",
        description: "Merges head (with its first-parent chain since base) three-way onto target, \
                      main by default, and publishes the result as one changeset in one \
                      transaction; on E_CONFLICT, details.report lists each conflict: resolve them \
                      with resolutions, against the report's theirs, its full 64-hex id (a path \
                      the target changed since comes back in the report's stale). Give \
                      idempotency_key to make a retry safe.",
        effect: Effect::Lands,
        result_chars: None,
    },
    Spec {
        tool: Tool::Rebase,
        name: "rebase",
        request: "RebaseReq",
        title: "Rebase a change",
        description: "Merges head (with its first-parent chain since base) three-way onto onto's \
                      head, main by default, as land merges it, and records the result as one \
                      changeset there without moving any ref: build and test that changeset, \
                      then land it with expect_target set to result.onto, so that the landed \
                      tree is the one you tested. Pass head, base and resolutions' against as \
                      full 64-hex ids, and an idempotency_key to make a retry safe.",
        effect: Effect::Adds,
        result_chars: None,
    },
    Spec {
        tool: Tool::BlobPut,
        name: "blob_put",
        request: "BlobPutReq",
        title: "Store a blob",
        description: "Stores content as a blob, outside any commit, and gives its id: a commit's \
                      change then names it as {\"blob\": id} content instead of carrying its \
                      bytes, the way to commit a large file. A {\"file\": path} content is \
                      read by this server: give an absolute path. A hint's swarmops blob put \
                      is this tool.",
        effect: Effect::Adds,
        result_chars: None,
    },
    Spec {
        tool: Tool::BlobHas,
        name: "blob_has",
        request: "BlobHasReq",
        title: "Find stored blobs",
        description: "Of the blob ids given, the ones the store holds blobs under: the \
                      contents a commit can name by id without storing them again.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::Verify,
        name: "verify",
        request: "VerifyReq",
        title: "Verify the store",
        description: "Checks the whole store's consistency; deep re-hashes every object and \
                      checks the derived values against the objects too.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::Gc,
        name: "gc",
        request: "GcReq",
        title: "Remove leftovers",
        description: "Removes the store's leftovers (temp files past a day, a crashed create's \
                      staging, derived values of older versions, what an upgrade left) and never \
                      an object or a changeset; dry_run lists them only. E_MAINTENANCE_BUSY \
                      while another gc, repair or upgrade runs: retry.",
        effect: Effect::Maintains,
        result_chars: None,
    },
    Spec {
        tool: Tool::Repair,
        name: "repair",
        request: "RepairReq",
        title: "Repair the store",
        description: "Rebuilds what the journal can rebuild (its offsets, the index, the ref \
                      files), quarantines damaged objects and drops damaged cache entries, then \
                      verifies; the journal itself is never cut. dry_run lists the actions only.",
        effect: Effect::Maintains,
        result_chars: None,
    },
    Spec {
        tool: Tool::Upgrade,
        name: "upgrade",
        request: "UpgradeReq",
        title: "Upgrade the store format",
        description: "Moves the store to the latest format this build writes, which takes \
                      free-form branches and tags; irreversible, older releases do not open it \
                      afterwards. On a store of that format already it changes nothing.",
        effect: Effect::Maintains,
        result_chars: None,
    },
    Spec {
        tool: Tool::Local(LocalTool::RemoteList),
        name: "remote_list",
        request: "RemoteListReq",
        title: "List git remotes",
        description: "The git remotes the store records: each one's name, URL, followed branch \
                      and the token variable it names, if any. Adding and removing remotes is \
                      the CLI's.",
        effect: Effect::Reads,
        result_chars: None,
    },
    Spec {
        tool: Tool::Local(LocalTool::Fetch),
        name: "fetch",
        request: "FetchReq",
        title: "Fetch a git remote",
        description: "Fetches a git remote, by its name, into the store's mirror, and its \
                      followed branch's commits into the store as changesets; main never moves. \
                      Run again with nothing new, it imports nothing.",
        effect: Effect::Fetches,
        result_chars: None,
    },
    Spec {
        tool: Tool::Local(LocalTool::Push),
        name: "push",
        request: "PushReq",
        title: "Push to a git remote",
        description: "Sends a store ref (main by default) to a git remote's branch by \
                      compare-and-swap, never by force: the remote moves only from what the last \
                      fetch saw. A remote that moved since is E_REMOTE_DIVERGED and nothing is \
                      sent: fetch or sync, then push again. Run again, it sends nothing.",
        effect: Effect::Fetches,
        result_chars: None,
    },
    Spec {
        tool: Tool::Local(LocalTool::Sync),
        name: "sync",
        request: "SyncReq",
        title: "Sync with a git remote",
        description: "Fetches a git remote, lands main's unpublished changes onto its head (those \
                      the remote holds already are dropped), moves main there and pushes it; \
                      with mode local it pushes nothing. A conflict is E_CONFLICT with the \
                      report a land gives, and nothing moves.",
        effect: Effect::Syncs,
        result_chars: None,
    },
    Spec {
        tool: Tool::Local(LocalTool::SyncStatus),
        name: "sync_status",
        request: "SyncStatusReq",
        title: "Show sync status",
        description: "How main and a git remote's followed branch stand, as the last fetch saw \
                      the remote: how many changes are unpublished and how far behind main is, \
                      offline; pass a limit of 100 or less.",
        effect: Effect::Reads,
        result_chars: None,
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
    /// The most bytes a message line may hold: [`MAX_REQUEST_BYTES`], or less in a test.
    max_line: u64,
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
        Server::with_max_line(global, env, MAX_REQUEST_BYTES)
    }

    /// [`Server::new`], whose message lines hold at most `max_line` bytes.
    fn with_max_line(global: &'a Global, env: Env<'a>, max_line: u64) -> Server<'a> {
        let all = swarmops_api::schema_for_all();

        // The store's tools, then the workspaces' (M5).
        let store_tools = TOOLS.iter().map(|spec| ToolDef {
            name: spec.name,
            title: spec.title,
            description: spec.description,
            input_schema: tool_input_schema(spec, &all),
            annotations: spec.effect.annotations(),
            meta: spec
                .result_chars
                .map(|chars| json!({RESULT_CHARS_KEY: chars})),
        });
        let ws_tools = WS_TOOLS.iter().map(|spec| ToolDef {
            name: spec.name,
            title: spec.title,
            description: spec.description,
            input_schema: plain_schema(spec.request, &all[spec.request]),
            annotations: spec.tool.annotations(),
            meta: None,
        });
        let tools = store_tools.chain(ws_tools).collect();
        Server {
            global,
            env,
            tools,
            session: None,
            max_line,
        }
    }

    /// Answers each message `input` holds, a line each, on `output`, until `input` ends. A line
    /// is read no further than one byte past `max_line`: a longer one is an Invalid Request
    /// without an `id`, its rest passed over unread to the next line break.
    fn serve(&mut self, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        let mut line = Vec::new();
        loop {
            line.clear();
            line.shrink_to(LINE_KEEP);
            let cap = self.max_line.saturating_add(1);
            if io::Read::take(&mut input, cap).read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            let over = line.last() != Some(&b'\n')
                && u64::try_from(line.len()).unwrap_or(u64::MAX) > self.max_line;
            let answer = if over {
                input.skip_until(b'\n')?;
                Some(error(
                    None,
                    INVALID_REQUEST,
                    format!(
                        "Invalid Request: a message line holds at most {} bytes; send large \
                         contents as {{\"file\": path}}, or store them with blob_put first and \
                         name them by id: {{\"blob\": id}}",
                        self.max_line
                    ),
                ))
            } else {
                self.answer(&line)
            };
            if let Some(answer) = answer {
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
        // The whole line is checked to be JSON, and then only what the server serves is read of
        // it: what else it holds is kept nowhere (the module documentation, # Errors).
        if let Err(e) = serde_json::from_slice::<Json>(line) {
            return Some(error(None, PARSE_ERROR, format!("Parse error: {e}")));
        }
        // JSON that passed the check is UTF-8: its strings were checked, the rest is ASCII.
        let Ok(line) = str::from_utf8(line) else {
            return Some(error(None, PARSE_ERROR, "Parse error: a line is UTF-8"));
        };
        let Some([id, jsonrpc, method, params, result, failure]) = members(line, MESSAGE) else {
            return Some(error(
                None,
                INVALID_REQUEST,
                "Invalid Request: a message is one JSON object; batches are not served",
            ));
        };
        let id = match id.map(request_id) {
            None => None,
            Some(Some(id)) => Some(id),
            Some(None) => {
                return Some(error(
                    None,
                    INVALID_REQUEST,
                    "Invalid Request: an id is a string or an integer",
                ));
            }
        };
        if jsonrpc.and_then(string).as_deref() != Some("2.0") {
            return Some(error(
                id.as_ref(),
                INVALID_REQUEST,
                "Invalid Request: a message has \"jsonrpc\": \"2.0\"",
            ));
        }
        let method = match method.map(string) {
            Some(Some(method)) => method,
            Some(None) => {
                return Some(error(
                    id.as_ref(),
                    INVALID_REQUEST,
                    "Invalid Request: a method is a string",
                ));
            }
            None if result.is_some() || failure.is_some() => {
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
        let params = match params {
            None => Params::default(),
            Some(params) if is_null(params) => Params::default(),
            Some(params) => match Params::of(params) {
                Some(params) => params,
                None => {
                    return Some(error(
                        Some(&id),
                        INVALID_PARAMS,
                        "Invalid params: params are an object",
                    ));
                }
            },
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
    fn request(&mut self, id: &Value, method: &str, params: Params<'_>) -> String {
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
    fn initialize(&mut self, id: &Value, params: &Params<'_>) -> String {
        let Some(asked) = params.protocol_version.and_then(string) else {
            return error(
                Some(id),
                INVALID_PARAMS,
                "Invalid params: initialize names the protocolVersion the client speaks",
            );
        };
        let version = HANDSHAKE
            .into_iter()
            .find(|version| *version == asked.as_str())
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
    fn list_tools(&self, reply: Reply<'_>, params: &Params<'_>) -> String {
        if params.cursor.is_some_and(|cursor| !is_null(cursor)) {
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
    fn call_tool(&self, reply: Reply<'_>, params: Params<'_>) -> String {
        let Some(name) = params.name.and_then(string) else {
            return error(
                Some(reply.id),
                INVALID_PARAMS,
                "Invalid params: tools/call names the tool to call",
            );
        };
        let store_tool = TOOLS.iter().find(|spec| spec.name == name);
        let ws_tool = WS_TOOLS.iter().find(|spec| spec.name == name);
        let spec: &'static dyn Named = match (store_tool, ws_tool) {
            (Some(spec), _) => spec,
            (None, Some(spec)) => spec,
            (None, None) => {
                return error(
                    Some(reply.id),
                    INVALID_PARAMS,
                    format!("Unknown tool: {name}"),
                );
            }
        };
        let arguments = match params.arguments {
            None => NO_ARGUMENTS,
            Some(arguments) if is_null(arguments) => NO_ARGUMENTS,
            Some(arguments) if is_object(arguments) => arguments.get(),
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
        match (store_tool, ws_tool) {
            (Some(store_tool), _) => operate(store_tool.tool, call),
            (None, Some(ws_tool)) => operate_ws(ws_tool.tool, call),
            (None, None) => unreachable!("an unknown tool is answered above"),
        }
    }
}

/// The members of a message that the server reads ([`members`]): its `id`, `jsonrpc`, `method`
/// and `params`, and a response's `result` and `error`, by which a message without a `method` is
/// known as one.
const MESSAGE: [&str; 6] = ["id", "jsonrpc", "method", "params", "result", "error"];

/// The members of a request's `params` that the server reads ([`Params`]).
const PARAMS: [&str; 5] = ["_meta", "protocolVersion", "cursor", "name", "arguments"];

/// A tool's arguments when a call gives none, or `null`: an empty object.
const NO_ARGUMENTS: &str = "{}";

/// The members of a request's `params` that the server reads, each as its JSON text ([`members`]):
/// none when the request has no `params`, or `null` ones.
#[derive(Debug, Default, Clone, Copy)]
struct Params<'a> {
    /// `_meta`, where a request of 2026-07-28 names its version ([`claim`]).
    meta: Option<&'a RawValue>,
    /// `initialize`'s `protocolVersion`.
    protocol_version: Option<&'a RawValue>,
    /// A `tools/list` cursor, which no request needs: there is one page.
    cursor: Option<&'a RawValue>,
    /// The tool a `tools/call` calls.
    name: Option<&'a RawValue>,
    /// The tool's arguments, read as its request type only once the tool is known.
    arguments: Option<&'a RawValue>,
}

impl<'a> Params<'a> {
    /// The members of `params`; `None` when it is not an object.
    fn of(params: &'a RawValue) -> Option<Params<'a>> {
        let [meta, protocol_version, cursor, name, arguments] = members(params.get(), PARAMS)?;
        Some(Params {
            meta,
            protocol_version,
            cursor,
            name,
            arguments,
        })
    }
}

/// The members named `names` of the JSON object `json`, each as its own JSON text, a slice of
/// `json` left unparsed (of a member named twice, the later, as a JSON value of the object keeps
/// it); every other member is passed over, and nothing is built of it. `None` when `json` is not
/// an object. `json` has passed [`Json`]'s check, so what this passes over is JSON already.
fn members<'j, const N: usize>(
    json: &'j str,
    names: [&str; N],
) -> Option<[Option<&'j RawValue>; N]> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let found = (&mut deserializer).deserialize_map(Members(&names)).ok()?;
    deserializer.end().ok()?;
    Some(found)
}

/// The visitor of an object whose members [`members`] keeps: the ones named in its list.
struct Members<'n, const N: usize>(&'n [&'n str; N]);

impl<'de, const N: usize> Visitor<'de> for Members<'_, N> {
    type Value = [Option<&'de RawValue>; N];

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut found = [None; N];
        while let Some(name) = map.next_key_seed(Name(self.0))? {
            match name {
                Some(at) => found[at] = Some(map.next_value()?),
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(found)
    }
}

/// A member's name, read as its place in the list of names that [`members`] keeps: `None` for a
/// name not in it.
struct Name<'n, const N: usize>(&'n [&'n str; N]);

impl<'de, const N: usize> DeserializeSeed<'de> for Name<'_, N> {
    type Value = Option<usize>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Option<usize>, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<const N: usize> Visitor<'_> for Name<'_, N> {
    type Value = Option<usize>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a member's name")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Option<usize>, E> {
        Ok(self.0.iter().position(|kept| *kept == name))
    }
}

/// A JSON value read through and kept nowhere: [`Server::answer`] checks a line with it before
/// it reads anything of the line, by the rules a parse into a JSON value holds the line to
/// (strings of UTF-8 with valid escapes, numbers in range, at most 127 levels of nesting, nothing
/// after the value), and builds nothing, whatever the line holds.
struct Json;

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(Json)
    }
}

impl<'de> Visitor<'de> for Json {
    type Value = Json;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Json, E> {
        Ok(Json)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Json, E> {
        Ok(Json)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Json, E> {
        Ok(Json)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Json, E> {
        Ok(Json)
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Json, E> {
        Ok(Json)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        while seq.next_element::<Json>()?.is_some() {}
        Ok(Json)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        while map.next_entry::<Json, Json>()?.is_some() {}
        Ok(Json)
    }
}

/// The string that the JSON text `json` holds, unescaped; `None` for any other value.
fn string(json: &RawValue) -> Option<String> {
    let text = json.get();
    if text.starts_with('"') {
        serde_json::from_str(text).ok()
    } else {
        None
    }
}

/// Whether the JSON text `json` holds an object.
fn is_object(json: &RawValue) -> bool {
    json.get().starts_with('{')
}

/// Whether the JSON text `json` is `null`.
fn is_null(json: &RawValue) -> bool {
    json.get() == "null"
}

/// The request id that the JSON text `json` holds: a string or an integer, else `None`.
fn request_id(json: &RawValue) -> Option<Value> {
    let text = json.get();
    match text.as_bytes().first()? {
        b'"' => serde_json::from_str(text).ok().map(Value::String),
        b'-' | b'0'..=b'9' => serde_json::from_str::<Number>(text)
            .ok()
            .filter(|n| n.is_i64() || n.is_u64())
            .map(Value::Number),
        _ => None,
    }
}

/// What the `_meta` of a request with `params` claims: no version, a version with a handshake
/// (before 2026-07-28), or 2026-07-28 with the client's capabilities. A version of 2026-07-28
/// or later that is not 2026-07-28, and a 2026-07-28 `_meta` without the client's
/// capabilities, are refused.
fn claim(params: &Params<'_>) -> Result<Claim, Rejected> {
    let Some(meta) = params.meta else {
        return Ok(Claim::None);
    };
    let Some([version, capabilities]) = members(meta.get(), [VERSION_KEY, CAPABILITIES_KEY]) else {
        return Err(Rejected::params("params._meta is an object"));
    };
    let version = match version.map(string) {
        None => return Ok(Claim::None),
        Some(Some(version)) => version,
        Some(None) => return Err(Rejected::params(&format!("{VERSION_KEY} is a string"))),
    };
    // Revisions are dates, so they sort as strings.
    if version.as_str() < LATEST {
        return Ok(Claim::Handshake(version));
    }
    if version != LATEST {
        return Err(Rejected::Version(version));
    }
    if !capabilities.is_some_and(is_object) {
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

    /// Runs a client-side read: the request `R`, then `call` on the store's own operations.
    fn read_local<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn RemoteOps, R) -> Result<T, OpError>,
    ) -> Self::Done;

    /// Runs a client-side mutation: the request `R`, then `call` on the store's own operations
    /// as the server's identity.
    fn mutate_local<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn RemoteOps, &Identity, R) -> Result<T, OpError>,
    ) -> Self::Done;

    /// Runs a workspace operation (M5): the request `R`, then `call` on the store, with the
    /// root it was opened at, the server's identity if it has one (a workspace mutation without
    /// one acts as the workspace's `created_by`) and whether a new workspace is bootstrapped
    /// (`SWARMOPS_WS_BOOTSTRAP`).
    fn workspace<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(
            &dyn StoreOps,
            &Utf8Path,
            Option<&Identity>,
            Bootstrap,
            R,
        ) -> Result<T, OpError>,
    ) -> Self::Done;
}

/// The client-side operations a tool runs (M13's git remotes): no [`StoreOps`] has them, and
/// they take a remote by its name only. The store's [`Recording`] has them; [`operate_local`]
/// ties each tool to its method through this trait.
trait RemoteOps {
    /// `remote list`.
    fn remote_list(&self, req: RemoteListReq) -> Result<RemoteList, OpError>;
    /// `fetch`.
    fn fetch(&self, req: FetchReq) -> Result<FetchResp, OpError>;
    /// `push`.
    fn push(&self, req: PushReq) -> Result<PushResp, OpError>;
    /// `sync`, as `who`.
    fn sync(&self, who: &Identity, req: SyncReq) -> Result<SyncResp, OpError>;
    /// `sync status`.
    fn sync_status(&self, req: SyncStatusReq) -> Result<SyncStatus, OpError>;
}

impl RemoteOps for Recording<'_> {
    fn remote_list(&self, req: RemoteListReq) -> Result<RemoteList, OpError> {
        Recording::remote_list(self, req)
    }

    fn fetch(&self, req: FetchReq) -> Result<FetchResp, OpError> {
        Recording::fetch(self, req)
    }

    fn push(&self, req: PushReq) -> Result<PushResp, OpError> {
        Recording::push(self, req)
    }

    fn sync(&self, who: &Identity, req: SyncReq) -> Result<SyncResp, OpError> {
        Recording::sync(self, who, req)
    }

    fn sync_status(&self, req: SyncStatusReq) -> Result<SyncStatus, OpError> {
        Recording::sync_status(self, req)
    }
}

/// Runs `tool`'s operation as `how` runs operations: the one place where a tool meets the
/// [`StoreOps`] method it stands for, and so the request type it reads, which the method's
/// signature fixes.
fn operate<O: Operate>(tool: Tool, how: O) -> O::Done {
    match tool {
        Tool::RepoInfo => how.read(|ops, req| ops.repo_info(req)),
        Tool::ResolveRev => how.read(|ops, req| ops.resolve_rev(req)),
        Tool::ReadFile => how.read(|ops, req| ops.read_file(req)),
        Tool::ReadTree => how.read(|ops, req| ops.read_tree(req)),
        Tool::Show => how.read(|ops, req| ops.show(req)),
        Tool::Log => how.read(|ops, req| ops.log(req)),
        Tool::PathHistory => how.read(|ops, req| ops.path_history(req)),
        Tool::Diff => how.read(|ops, req| ops.diff(req)),
        Tool::OpLog => how.read(|ops, req| ops.op_log(req)),
        Tool::RefList => how.read(|ops, req| ops.ref_list(req)),
        Tool::RefSet => how.mutate(|ops, who, req| ops.ref_set(who, req)),
        Tool::RefDelete => how.mutate(|ops, who, req| ops.ref_delete(who, req)),
        Tool::Undo => how.mutate(|ops, who, req| ops.undo(who, req)),
        Tool::Commit => how.mutate(|ops, who, req| ops.commit(who, req)),
        Tool::Land => how.mutate(|ops, who, req| ops.land(who, req)),
        Tool::Rebase => how.mutate(|ops, who, req| ops.rebase(who, req)),
        Tool::BlobPut => how.mutate(|ops, who, req| ops.blob_put(who, req)),
        Tool::BlobHas => how.read(|ops, req| ops.blob_has(req)),
        Tool::Verify => how.read(|ops, req: VerifyReq| {
            let depth = commands::Depth::of(req.deep);
            ops.verify(req)
                .and_then(|report| commands::clean(report, depth))
        }),
        Tool::Gc => how.read(|ops, req| ops.gc(req)),
        Tool::Repair => how.read(|ops, req| ops.repair(req)),
        Tool::Upgrade => how.read(|ops, req| ops.upgrade(req)),
        Tool::Local(tool) => operate_local(tool, how),
    }
}

/// Runs the client-side `tool`'s operation as `how` runs operations: the one place where such a
/// tool meets the [`RemoteOps`] method it stands for, on the store's own operations ([`Recording`],
/// which [`Ctx::envelope_on_local`] hands), so that [`StoreOps`] does not widen.
fn operate_local<O: Operate>(tool: LocalTool, how: O) -> O::Done {
    match tool {
        LocalTool::RemoteList => how.read_local(|ops, req| ops.remote_list(req)),
        LocalTool::Fetch => how.read_local(|ops, req| ops.fetch(req)),
        LocalTool::Push => how.read_local(|ops, req| ops.push(req)),
        LocalTool::Sync => how.mutate_local(|ops, who, req| ops.sync(who, req)),
        LocalTool::SyncStatus => how.read_local(|ops, req| ops.sync_status(req)),
    }
}

/// One `tools/call` of the tool `spec` on `arguments`, the JSON text of an object, answered as
/// `reply`.
struct Call<'s, 'a> {
    server: &'s Server<'a>,
    spec: &'static dyn Named,
    reply: Reply<'s>,
    arguments: &'s str,
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

    /// The arguments as the request, then `call` on the store's own operations.
    fn read_local<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn RemoteOps, R) -> Result<T, OpError>,
    ) -> String {
        let Call {
            server,
            spec,
            reply,
            arguments,
        } = self;
        run_local(
            server,
            spec,
            reply,
            || request_of(spec, arguments),
            |ops, req| call(&ops, req),
        )
    }

    /// The arguments as the request, then the server's identity, then `call` on the store's
    /// own operations as that identity.
    fn mutate_local<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(&dyn RemoteOps, &Identity, R) -> Result<T, OpError>,
    ) -> String {
        let Call {
            server,
            spec,
            reply,
            arguments,
        } = self;
        let (agent, env) = (server.global.agent.as_deref(), server.env);
        run_local(
            server,
            spec,
            reply,
            || {
                let req = request_of(spec, arguments)?;
                Ok((context::mutator(agent, env)?, req))
            },
            |ops, (who, req)| call(&ops, &who, req),
        )
    }

    /// The arguments as the request, then the server's identity if it has one, then `call` on
    /// the store with the root it was opened at.
    fn workspace<R: Refuse, T: Serialize>(
        self,
        call: impl FnOnce(
            &dyn StoreOps,
            &Utf8Path,
            Option<&Identity>,
            Bootstrap,
            R,
        ) -> Result<T, OpError>,
    ) -> String {
        let Call {
            server,
            spec,
            reply,
            arguments,
        } = self;
        let (agent, env) = (server.global.agent.as_deref(), server.env);
        let ctx = Ctx {
            global: server.global,
            env: server.env,
            format: Format::Json {
                layout: Layout::Compact,
            },
            started: Instant::now(),
            used: Used::default(),
        };
        let op = spec.name();

        // The store opened anew for the call, as for every tool; a panic is E_INTERNAL.
        let prepare = || {
            let req = request_of(spec, arguments)?;
            Ok((
                context::identity(agent, env)?,
                context::bootstrap(env)?,
                req,
            ))
        };
        let run = |ops: Recording<'_>,
                   root: &Utf8Path,
                   (who, how, req): (Option<Identity>, Bootstrap, R)| {
            call(&ops, root, who.as_ref(), how, req)
        };
        match panic::catch_unwind(AssertUnwindSafe(|| ctx.envelope_at_root(op, prepare, run))) {
            Ok(envelope) => reply.tool_result(&envelope),
            Err(payload) => {
                let bug = commands::panicked(&*payload);
                let envelope = commands::failed_envelope(op, bug, &ctx.used, ctx.started);
                reply.tool_result(&envelope)
            }
        }
    }
}

/// Runs tool `spec` for `server` as a command runs ([`Ctx::envelope_on_store`]): `prepare`
/// builds its input, then `call` runs on the store, opened; a panic in either is `E_INTERNAL`.
/// The tool result that holds the envelope, answered as `reply`.
fn run<P, T: Serialize>(
    server: &Server<'_>,
    spec: &dyn Named,
    reply: Reply<'_>,
    prepare: impl FnOnce() -> Result<P, OpError>,
    call: impl FnOnce(&dyn StoreOps, P) -> Result<T, OpError>,
) -> String {
    let ctx = Ctx {
        global: server.global,
        env: server.env,
        format: Format::Json {
            layout: Layout::Compact,
        },
        started: Instant::now(),
        used: Used::default(),
    };
    let op = spec.name();
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

/// Runs the client-side tool `spec` for `server` as a client-side command runs
/// ([`Ctx::envelope_on_local`]): `prepare` builds its input, then `call` runs on the store's own
/// operations, opened; a panic in either is `E_INTERNAL`. The tool result that holds the
/// envelope, answered as `reply`.
fn run_local<P, T: Serialize>(
    server: &Server<'_>,
    spec: &dyn Named,
    reply: Reply<'_>,
    prepare: impl FnOnce() -> Result<P, OpError>,
    call: impl FnOnce(Recording<'_>, P) -> Result<T, OpError>,
) -> String {
    let ctx = Ctx {
        global: server.global,
        env: server.env,
        format: Format::Json {
            layout: Layout::Compact,
        },
        started: Instant::now(),
        used: Used::default(),
    };
    let op = spec.name();
    match panic::catch_unwind(AssertUnwindSafe(|| {
        ctx.envelope_on_local(op, prepare, call)
    })) {
        Ok(envelope) => reply.tool_result(&envelope),
        Err(payload) => {
            let bug = commands::panicked(&*payload);
            let envelope = commands::failed_envelope(op, bug, &ctx.used, ctx.started);
            reply.tool_result(&envelope)
        }
    }
}

/// The request `arguments` stand for, of tool `spec`'s type, read from their JSON text as that
/// type and nothing else: `E_INVALID_ARG` when they do not fit it (a field of it named twice
/// included), or break a rule of this server's ([`Refuse`]). Fields the type does not know are
/// ignored, as the API's types always ignore them, and kept nowhere.
fn request_of<R: Refuse>(spec: &dyn Named, arguments: &str) -> Result<R, OpError> {
    let req: R = serde_json::from_str(arguments).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            output::printable(&format!(
                "the arguments are not a {}: {}",
                spec.request(),
                unplaced(&e)
            )),
        )
        .with_hint(format!(
            "the {} tool's inputSchema gives its arguments; swarmops schema {} gives the \
             request type, without the rules this server adds",
            spec.name(),
            spec.request()
        ))
    })?;
    req.refuse()?;
    Ok(req)
}

/// What `e` says, without the line and column it was found at: they count from the start of a
/// tool's arguments, which is no place in the line the client sent.
fn unplaced(e: &serde_json::Error) -> String {
    let said = e.to_string();
    let place = format!(" at line {} column {}", e.line(), e.column());
    match said.strip_suffix(&place) {
        Some(said) => said.to_owned(),
        None => said,
    }
}

/// This server's own rules for a request, which its type cannot say: a request type with rules
/// refuses what breaks them, the others nothing.
trait Refuse: DeserializeOwned {
    /// `E_INVALID_ARG` for a request that breaks the rule.
    fn refuse(&self) -> Result<(), OpError> {
        Ok(())
    }
}

impl Refuse for InfoReq {}
impl Refuse for ResolveRevReq {}
impl Refuse for ReadFileReq {}
impl Refuse for ReadTreeReq {}
impl Refuse for ShowReq {}
impl Refuse for LogReq {}
impl Refuse for PathHistoryReq {}
impl Refuse for DiffReq {}
impl Refuse for OpLogReq {}
impl Refuse for RefListReq {}
impl Refuse for UndoReq {}
impl Refuse for VerifyReq {}
impl Refuse for swarmops_api::GcReq {}
impl Refuse for swarmops_api::RepairReq {}
impl Refuse for swarmops_api::UpgradeReq {}
impl Refuse for BlobHasReq {}
impl Refuse for RemoteListReq {}
impl Refuse for FetchReq {}
impl Refuse for PushReq {}
impl Refuse for SyncReq {}
impl Refuse for SyncStatusReq {}

/// A commit that advances a ref names its parent by its full id ([`full_parent`]). A change's
/// client file is read by this server, so its path must be absolute (the agent
/// cannot see the server's working directory), must not be this process's own stdin or
/// stdout, which carry the protocol, and must name a regular file, a symlink to one included:
/// opening a FIFO blocks until something opens it for writing, and a device can block or never
/// end. A read of any of them would hold up the session, which serves one request at a time.
/// The path is `stat`ed, never opened; one that cannot be `stat`ed passes, and its read reports
/// the error.
impl Refuse for CommitReq {
    fn refuse(&self) -> Result<(), OpError> {
        if let Some(name) = &self.advance {
            full_parent(name, self.parent.as_ref())?;
        }
        for (index, change) in self.changes.iter().enumerate() {
            let Some(ContentInput::File(file)) = &change.content else {
                continue;
            };
            let Some(why) = unreadable(file) else {
                continue;
            };
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                output::printable(&format!(
                    "change {index} ({:?}): the client file {file:?} {why}: name a regular file \
                     by its absolute path, or send the bytes as inline or base64 content",
                    change.path
                )),
            )
            .with_details(Map::from_iter([
                ("index".to_owned(), Value::from(index)),
                ("path".to_owned(), Value::from(change.path.as_str())),
                ("file".to_owned(), Value::from(file.as_str())),
            ])));
        }
        Ok(())
    }
}

/// A blob's client file is read by this server as a commit's is, so the same rules hold
/// ([`unreadable`]); the refusal names the `file`.
impl Refuse for BlobPutReq {
    fn refuse(&self) -> Result<(), OpError> {
        let ContentInput::File(file) = &self.content else {
            return Ok(());
        };
        let Some(why) = unreadable(file) else {
            return Ok(());
        };
        Err(OpError::new(
            ErrorCode::InvalidArg,
            output::printable(&format!(
                "the client file {file:?} {why}: name a regular file by its absolute path, or send \
                 the bytes as inline or base64 content"
            )),
        )
        .with_details(Map::from_iter([(
            "file".to_owned(),
            Value::from(file.as_str()),
        )])))
    }
}

/// Why this server refuses to read client file `file`, which it would read for the caller
/// ([`Refuse`] for `CommitReq` says why each): a relative path, this process's own stdin or
/// stdout, or anything but a regular file. `None` for a file it reads. The path is `stat`ed,
/// never opened.
fn unreadable(file: &str) -> Option<&'static str> {
    if !Path::new(file).is_absolute() {
        Some(
            "is a relative path, which the server would read from its own working directory, \
             not yours",
        )
    } else if commands::is_own_stdio(file) {
        Some("is this process's own stdin or stdout, which carry the protocol")
    } else if fs::metadata(file).is_ok_and(|meta| !meta.is_file()) {
        Some(
            "is not a regular file (a FIFO, a socket, a device or a directory), which a read \
             could block on",
        )
    } else {
        None
    }
}

/// A land's resolutions apply while the target holds each path as `against` did, so `against`
/// is a full changeset id ([`concrete_against`]); a resolution's client file is read by this
/// server as a commit's is, so the same rules hold ([`unreadable`]); the refusal names the
/// resolution's `path` and the `file`.
impl Refuse for LandReq {
    fn refuse(&self) -> Result<(), OpError> {
        if let Some(expected) = &self.expect_target {
            full_rev("expect_target", expected)?;
        }
        let Some(resolutions) = &self.resolutions else {
            return Ok(());
        };
        concrete_against(&resolutions.against)?;
        for (path, resolution) in &resolutions.files {
            let Some(ContentInput::File(file)) = &resolution.content else {
                continue;
            };
            let Some(why) = unreadable(file) else {
                continue;
            };
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                output::printable(&format!(
                    "the resolution of {path:?}: the client file {file:?} {why}: name a regular \
                     file by its absolute path, or send the bytes as inline or base64 content"
                )),
            )
            .with_details(Map::from_iter([
                ("path".to_owned(), Value::from(path.as_str())),
                ("file".to_owned(), Value::from(file.as_str())),
            ])));
        }
        Ok(())
    }
}

/// `E_INVALID_ARG` unless `against`, what a land's resolutions were decided against, is a full
/// changeset id ([`is_full_id`]): a resolution applies while the target holds its path as
/// `against` did, so `against` must pin the head the caller read the conflict report at. A ref
/// name or a relative revision is read when the call runs instead, so it holds every path as the
/// target does and protects nothing, and an id prefix stops resolving once another changeset
/// shares it. The CLI takes any revision there, as it does for `--expect`; an agent through this
/// server gets the rule enforced.
fn concrete_against(against: &Rev) -> Result<(), OpError> {
    let rev = against.as_str();
    if is_full_id(rev) {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::InvalidArg,
        output::printable(&format!(
            "resolutions.against {rev:?} is not a full changeset id: a resolution applies while \
             the target holds its path as against did, and a ref name or a relative revision is \
             read when the call runs instead of pinning the head you read, so it would protect \
             nothing, while an id prefix stops resolving once another changeset shares it; pass \
             the conflict report's theirs, its 64-hex id"
        )),
    )
    .with_details(Map::from_iter([("against".to_owned(), Value::from(rev))])))
}

/// A rebase (M5) names its change by full ids: `head`, `base` and the resolutions' `against`,
/// since the changeset it records is the one a land with `expect_target` publishes, and a ref
/// name or a relative revision read when the call runs would rebase another change than the
/// one the caller read.
impl Refuse for RebaseReq {
    fn refuse(&self) -> Result<(), OpError> {
        full_rev("head", &self.head)?;
        if let Some(base) = &self.base {
            full_rev("base", base)?;
        }
        match &self.resolutions {
            Some(resolutions) => concrete_against(&resolutions.against),
            None => Ok(()),
        }
    }
}

/// `E_INVALID_ARG` unless `rev`, request field `field`, is a full changeset id
/// ([`is_full_id`]): a ref name or a relative revision is read when the call runs instead of
/// pinning the changeset the caller read, and an id prefix stops resolving once another
/// changeset shares it.
fn full_rev(field: &str, rev: &Rev) -> Result<(), OpError> {
    let rev = rev.as_str();
    if is_full_id(rev) {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::InvalidArg,
        output::printable(&format!(
            "{field} {rev:?} is not a full changeset id: a ref name or a relative revision is \
             read when the call runs instead of pinning the changeset you read, and an id \
             prefix stops resolving once another changeset shares it; pass its 64-hex id"
        )),
    )
    .with_details(Map::from_iter([(field.to_owned(), Value::from(rev))])))
}

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

/// How many hex digits a full changeset id has.
const FULL_ID_DIGITS: usize = 64;

/// Whether `rev` is a full changeset id: [`FULL_ID_DIGITS`] hex digits, of either case, as the
/// resolver takes them. The one rule behind [`concrete_expectation`] and [`full_id_pattern`].
fn is_full_id(rev: &str) -> bool {
    rev.len() == FULL_ID_DIGITS && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The JSON Schema pattern of what [`is_full_id`] accepts, for the tool schemas.
fn full_id_pattern() -> String {
    format!("^[0-9a-fA-F]{{{FULL_ID_DIGITS}}}$")
}

/// `E_INVALID_ARG` unless `expected`, the target a move or a delete of ref `name` expects, is a
/// full changeset id ([`is_full_id`]), as the resolver takes them. A ref name or a relative
/// revision is read from the store when the call runs instead of pinning the id the caller read.
/// The ref's own name always matches, so the compare-and-swap protects nothing: a move computed
/// on a stale read would silently drop what another agent moved the ref to, and a move to
/// `main~1` made twice would move the ref twice. `main~1` on `main` names main's parent and never
/// matches. An id's prefix names one changeset, but stops resolving (`E_AMBIGUOUS_ID`) once
/// another changeset shares it. The CLI takes any revision for `--expect`, whose help warns of
/// this; an agent through this server gets the rule enforced.
fn concrete_expectation(name: &str, expected: &Rev) -> Result<(), OpError> {
    let rev = expected.as_str();
    if is_full_id(rev) {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::InvalidArg,
        output::printable(&format!(
            "expected_old {rev:?} is not a full changeset id, which a compare-and-swap needs: a \
             ref name or a relative revision is read when the call runs instead of pinning the \
             id you read ({name} itself always matches, so the compare-and-swap would protect \
             nothing, and {name}~1 never matches), and an id prefix stops resolving once another \
             changeset shares it; pass the 64-hex id the ref held when you read it, from \
             ref_list, show, log or a commit's result"
        )),
    )
    .with_details(Map::from_iter([
        ("name".to_owned(), Value::from(name)),
        ("expected_old".to_owned(), Value::from(rev)),
    ]))
    .with_hint("swarmops ref list"))
}

/// `E_INVALID_ARG` unless `parent`, the parent of a commit that advances ref `name`, is a full
/// changeset id ([`is_full_id`]). The move expects the ref to name the parent, so the parent is
/// a compare-and-swap's expectation, which must pin the id the caller read
/// ([`concrete_expectation`]): an omitted parent is the ref's head when the call runs, and a ref
/// name or a relative revision is read when the call runs too, so the compare-and-swap would
/// protect nothing; an id prefix stops resolving once another changeset shares it. The CLI takes
/// any parent with `--advance`, as a one-step fast-forward that detects a race from the moment
/// it runs; an agent through this server gets the rule enforced.
fn full_parent(name: &str, parent: Option<&Rev>) -> Result<(), OpError> {
    let rev = parent.map(Rev::as_str);
    if rev.is_some_and(is_full_id) {
        return Ok(());
    }
    let mut details = Map::from_iter([("advance".to_owned(), Value::from(name))]);
    let given = match rev {
        Some(rev) => {
            details.insert("parent".to_owned(), Value::from(rev));
            format!("not {rev:?}")
        }
        None => "and none is given".to_owned(),
    };
    Err(OpError::new(
        ErrorCode::InvalidArg,
        output::printable(&format!(
            "advance {name:?} needs a parent given as a full changeset id, {given}: its \
             compare-and-swap expects the ref to name the parent, and an omitted parent, a ref \
             name or a relative revision is read when the call runs instead of pinning the id \
             you read, so the compare-and-swap would protect nothing, while an id prefix stops \
             resolving once another changeset shares it; pass the id the ref held when you read \
             it, from ref_list, show, log or a commit's result"
        )),
    )
    .with_details(details)
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

/// The keywords at the top of a tool's `inputSchema`: a plain object. The Claude API refuses,
/// with a 400 error, a request whose tools hold an input schema with a combinator at its top
/// (`oneOf`, `anyOf` or `allOf`), and Claude Code sends every tool with every request, so one
/// such tool would stop each request of the session. A rule across fields that a request type's
/// schema states that way stays in its description and in the server's own check ([`Refuse`]).
const PLAIN_OBJECT: [&str; 6] = [
    "$schema",
    "title",
    "description",
    "type",
    "properties",
    "required",
];

/// The `inputSchema` of tool `spec`, from `all`, the schema of every type: its request type's
/// schema standing alone ([`standalone`]) and kept a plain object ([`PLAIN_OBJECT`]). The
/// `expected_old` of `ref_set` and `ref_delete`, which the request types give as any revision,
/// asks for a full changeset id instead ([`full_id`]), the one the server takes there
/// ([`concrete_expectation`]).
fn tool_input_schema(spec: &Spec, all: &Value) -> Value {
    let mut request = all[spec.request].clone();
    let pinned = match spec.tool {
        Tool::RefSet | Tool::RefDelete => Some("/properties/expected_old"),
        Tool::Land => Some("/properties/expect_target"),
        _ => None,
    };
    if let Some(expected) = pinned.and_then(|at| request.pointer_mut(at)) {
        full_id(expected);
    }
    plain_schema(spec.request, &request)
}

/// `request`, the schema of request type `name`, standing alone ([`standalone`]) and kept a
/// plain object ([`PLAIN_OBJECT`]): a tool's `inputSchema`.
fn plain_schema(name: &str, request: &Value) -> Value {
    let Value::Object(schema) = standalone(request) else {
        panic!("the schema of {name} is not an object")
    };
    Value::Object(
        schema
            .into_iter()
            .filter(|(key, _)| PLAIN_OBJECT.contains(&key.as_str()))
            .collect(),
    )
}

/// `schema` with each reference to the revision type in it replaced by a string of a full
/// changeset id's pattern ([`full_id_pattern`]). The revision's description goes with the
/// reference, since it offers a ref name; the field's own stays.
fn full_id(schema: &mut Value) {
    match schema {
        Value::Object(members) => {
            if members.get("$ref").and_then(Value::as_str) == Some("#/$defs/Rev") {
                members.remove("$ref");
                members.insert("type".to_owned(), Value::from("string"));
                members.insert("pattern".to_owned(), Value::from(full_id_pattern()));
            }
            members.values_mut().for_each(full_id);
        }
        Value::Array(items) => items.iter_mut().for_each(full_id),
        _ => {}
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
    /// The most characters of a result the tool asks Claude Code to keep in the conversation
    /// instead of saving to a file, if it asks ([`RESULT_CHARS_KEY`]).
    result_chars: Option<u32>,
}

/// What a call of a tool needs of its table's entry, whichever table it is in ([`TOOLS`],
/// [`WS_TOOLS`]): the tool's name and its request type's.
trait Named: Sync {
    /// The tool's name: the operation's, as the envelope's `op` gives it.
    fn name(&self) -> &'static str;
    /// The operation's request type, as `schema_for_all` names it.
    fn request(&self) -> &'static str;
}

impl Named for Spec {
    fn name(&self) -> &'static str {
        self.name
    }

    fn request(&self) -> &'static str {
        self.request
    }
}

/// The operations, one tool each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    RepoInfo,
    ResolveRev,
    ReadFile,
    ReadTree,
    Show,
    Log,
    PathHistory,
    Diff,
    OpLog,
    RefList,
    RefSet,
    RefDelete,
    Undo,
    Commit,
    Land,
    Rebase,
    BlobPut,
    BlobHas,
    Verify,
    Gc,
    Repair,
    Upgrade,
    /// A client-side operation, on the store's own operations ([`operate_local`]).
    Local(LocalTool),
}

/// The client-side operations, one tool each: the git remotes' (M13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalTool {
    RemoteList,
    Fetch,
    Push,
    Sync,
    SyncStatus,
}

/// What a tool does to the store, which its annotations say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    /// It only reads.
    Reads,
    /// It adds to the store and changes nothing there (`commit`, `rebase`, `blob_put`); run
    /// again, a `commit` adds again, another changeset signed at its own time, unless the call
    /// carries an `idempotency_key`, which makes the same call again a replay (a `blob_put` of
    /// the same content finds it stored, and says so in its result). A `commit` without a key
    /// is not idempotent, so the tool does not say it is.
    Adds,
    /// It moves or deletes refs by compare-and-swap, changing what they name (`ref_set`,
    /// `ref_delete`, `undo`); run again with the same arguments, it finds them as it asked
    /// (`already_applied`) or not as it expected (`E_CAS_MISMATCH`), and changes nothing more.
    Moves,
    /// It adds a changeset onto a ref's head and moves the ref to it, keeping all the ref held
    /// (`land`); run again with the same arguments, it finds its change on the ref and adds
    /// nothing: `replayed` with an `idempotency_key`, `E_NOTHING_TO_LAND` without.
    Lands,
    /// It moves bytes between the store and a git remote, and changes no ref of the store
    /// (`fetch`, `push`): a push moves the remote's branch only from what the last fetch saw, to
    /// a descendant of it. Run again, it fetches or sends nothing new. It reaches a remote, an
    /// open world.
    Fetches,
    /// It lands onto a git remote's head, moves `main` and pushes (`sync`): run again after the
    /// remote moved, it lands again. It reaches a remote, an open world.
    Syncs,
    /// It removes or rewrites store files that the journal and the objects stand for, moving no
    /// ref and recording nothing (`gc`, `repair`, `upgrade`): run again, it finds its work done.
    Maintains,
}

impl Effect {
    /// The tool annotations of a tool with this effect.
    fn annotations(self) -> Value {
        let (read_only, destructive, idempotent, open_world) = match self {
            Effect::Reads => (true, false, true, false),
            Effect::Adds => (false, false, false, false),
            // idempotentHint rests on concrete_expectation: the server refuses an expected_old
            // that is not a full id, so the same move again compares against the same changeset
            // and finds its work done (already_applied, nothing written) or the ref elsewhere
            // (E_CAS_MISMATCH). The ref's own name would match anew at every call and move the
            // ref again. An undo compares against what its operation's frame recorded, the same at
            // every call: run again, it finds the refs restored and writes nothing.
            // gc removes files and repair replaces them: destructive, but run again they find
            // their work done, as an upgrade of an upgraded store does.
            Effect::Moves | Effect::Maintains => (false, true, true, false),
            // A land merges onto the ref's head and moves the ref to a changeset whose parent is
            // that head, so it only adds; run again, it finds its change there (a replay with its
            // key, E_NOTHING_TO_LAND without one) and writes nothing.
            Effect::Lands => (false, false, true, false),
            // A fetch or a push run again finds nothing new to move (the M13 plan's decision 30).
            Effect::Fetches => (false, false, true, true),
            // A sync run again after the remote moved lands again.
            Effect::Syncs => (false, false, false, true),
        };
        json!({
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": open_world,
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
    /// What the tool asks of a client beyond the protocol: `read_file`'s result size for
    /// Claude Code ([`READ_FILE_RESULT_CHARS`]); omitted for the other tools.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<Value>,
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
#[path = "reference/mcp_page.rs"]
mod mcp_page;

#[path = "mcp_ws.rs"]
mod mcp_ws;

/// The test doubles of the tool tables' tests: a way to run a tool's operation that records
/// which store operation it calls ([`probe::Probe`]), on a store that records and fails each
/// call ([`probe::Recorder`]). Shared with the workspace tools' tests ([`mcp_ws`]).
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::RefCell;

    use swarmops_api::ResolvedRev;
    use swarmops_api::{
        BlobHasResp, BlobPutResp, ChangesetDetail, CommitResp, DeriveReq, DeriveResp, FileContent,
        LogPage, OpLogPage, PathHistoryPage, RefListResp, RefSetResp, RepoInfo, TreeListing,
        UndoResp, VerifyReport,
    };

    use super::*;

    /// Runs a tool's operation on a [`Recorder`], with a request read from [`sample`].
    pub(crate) struct Probe<'a> {
        called: &'a RefCell<Vec<(&'static str, &'static str)>>,
        /// What each workspace operation's call ended with: its error's message, or `ok`.
        outcome: &'a RefCell<Vec<String>>,
    }

    impl<'a> Probe<'a> {
        /// A probe recording the store's calls in `called`, and the workspace operations' outcomes
        /// in `outcome`.
        pub(crate) fn new(
            called: &'a RefCell<Vec<(&'static str, &'static str)>>,
            outcome: &'a RefCell<Vec<String>>,
        ) -> Probe<'a> {
            Probe { called, outcome }
        }
    }

    /// Arguments that every operation's request type reads: each takes the fields it knows,
    /// these fill the ones it needs, and it ignores the rest.
    pub(crate) fn sample() -> Value {
        json!({
            "rev": "main",
            "head": "main",
            "from": "main",
            "to": "main",
            "path": "a",
            "name": "main",
            "new": "main",
            "expected_old": "0".repeat(64),
            "message": "m",
            "changes": [],
            "content": {"inline": "x"},
            "ids": [],
            "id": "0:00000000",
            "remote": "origin",
            "workspace": "probe",
            "paths": [],
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

        fn read_local<R: Refuse, T: Serialize>(
            self,
            call: impl FnOnce(&dyn RemoteOps, R) -> Result<T, OpError>,
        ) {
            let req = serde_json::from_value(sample()).expect("every request reads the sample");
            let _ = call(&Recorder(self.called), req);
        }

        fn mutate_local<R: Refuse, T: Serialize>(
            self,
            call: impl FnOnce(&dyn RemoteOps, &Identity, R) -> Result<T, OpError>,
        ) {
            let req = serde_json::from_value(sample()).expect("every request reads the sample");
            let who = Identity::parse("test/probe").unwrap();
            let _ = call(&Recorder(self.called), &who, req);
        }

        fn workspace<R: Refuse, T: Serialize>(
            self,
            call: impl FnOnce(
                &dyn StoreOps,
                &Utf8Path,
                Option<&Identity>,
                Bootstrap,
                R,
            ) -> Result<T, OpError>,
        ) {
            let req = serde_json::from_value(sample()).expect("every request reads the sample");
            let root = tempfile::tempdir().expect("a temporary store root");
            let root = Utf8Path::from_path(root.path()).expect("a UTF-8 temporary directory");
            let outcome = match call(&Recorder(self.called), root, None, Bootstrap::Run, req) {
                Ok(_) => "ok".to_owned(),
                Err(e) => e.message,
            };
            self.outcome.borrow_mut().push(outcome);
        }
    }

    /// The client-side operations, recorded as the store's are.
    impl RemoteOps for Recorder<'_> {
        fn remote_list(&self, _: RemoteListReq) -> Result<RemoteList, OpError> {
            self.called("remote_list", "RemoteListReq")
        }

        fn fetch(&self, _: FetchReq) -> Result<FetchResp, OpError> {
            self.called("fetch", "FetchReq")
        }

        fn push(&self, _: PushReq) -> Result<PushResp, OpError> {
            self.called("push", "PushReq")
        }

        fn sync(&self, _: &Identity, _: SyncReq) -> Result<SyncResp, OpError> {
            self.called("sync", "SyncReq")
        }

        fn sync_status(&self, _: SyncStatusReq) -> Result<SyncStatus, OpError> {
            self.called("sync_status", "SyncStatusReq")
        }
    }

    /// A store that records which operation was called, by its name and the type of request
    /// it took (which its signature fixes), and fails it.
    pub(crate) struct Recorder<'a>(pub(crate) &'a RefCell<Vec<(&'static str, &'static str)>>);

    impl Recorder<'_> {
        pub(crate) fn called<T>(
            &self,
            op: &'static str,
            request: &'static str,
        ) -> Result<T, OpError> {
            self.0.borrow_mut().push((op, request));
            Err(OpError::new(ErrorCode::Internal, "a probe"))
        }
    }

    impl StoreOps for Recorder<'_> {
        fn repo_info(&self, _: InfoReq) -> Result<RepoInfo, OpError> {
            self.called("repo_info", "InfoReq")
        }

        fn resolve_rev(&self, _: ResolveRevReq) -> Result<ResolvedRev, OpError> {
            self.called("resolve_rev", "ResolveRevReq")
        }

        fn commit(&self, _: &Identity, _: CommitReq) -> Result<CommitResp, OpError> {
            self.called("commit", "CommitReq")
        }

        fn land(&self, _: &Identity, _: LandReq) -> Result<swarmops_api::LandResp, OpError> {
            self.called("land", "LandReq")
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

        fn path_history(&self, _: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
            self.called("path_history", "PathHistoryReq")
        }

        fn diff(&self, _: DiffReq) -> Result<swarmops_api::Diff, OpError> {
            self.called("diff", "DiffReq")
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

        fn op_log(&self, _: OpLogReq) -> Result<OpLogPage, OpError> {
            self.called("op_log", "OpLogReq")
        }

        fn undo(&self, _: &Identity, _: UndoReq) -> Result<UndoResp, OpError> {
            self.called("undo", "UndoReq")
        }

        fn verify(&self, _: VerifyReq) -> Result<VerifyReport, OpError> {
            self.called("verify", "VerifyReq")
        }

        fn blob_put(&self, _: &Identity, _: BlobPutReq) -> Result<BlobPutResp, OpError> {
            self.called("blob_put", "BlobPutReq")
        }

        fn blob_has(&self, _: BlobHasReq) -> Result<BlobHasResp, OpError> {
            self.called("blob_has", "BlobHasReq")
        }

        fn rebase(&self, _: &Identity, _: RebaseReq) -> Result<swarmops_api::RebaseResp, OpError> {
            self.called("rebase", "RebaseReq")
        }

        fn blob_get(
            &self,
            _: swarmops_api::BlobGetReq,
        ) -> Result<swarmops_api::BlobGetResp, OpError> {
            self.called("blob_get", "BlobGetReq")
        }

        fn derive(&self, _: DeriveReq) -> Result<DeriveResp, OpError> {
            self.called("derive", "DeriveReq")
        }

        fn gc(&self, _: swarmops_api::GcReq) -> Result<swarmops_api::GcResp, OpError> {
            self.called("gc", "GcReq")
        }

        fn repair(&self, _: swarmops_api::RepairReq) -> Result<swarmops_api::RepairResp, OpError> {
            self.called("repair", "RepairReq")
        }

        fn upgrade(
            &self,
            _: swarmops_api::UpgradeReq,
        ) -> Result<swarmops_api::UpgradeResp, OpError> {
            self.called("upgrade", "UpgradeReq")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    use camino::Utf8Path;
    use swarmops_api::{ChangeInput, ChangeOp, ContentInput};
    use swarmops_ops::LocalOps;

    use super::probe::Probe;
    use super::*;

    /// Each operation's request type has exactly one tool, and each tool runs exactly the
    /// `StoreOps` method of its name, or the client-side operation of its name ([`RemoteOps`]),
    /// on the request type it advertises: a new operation, or a tool wired to the wrong one,
    /// fails here.
    #[test]
    fn the_tools_are_the_store_operations() {
        // `init`, `git import` and `git export` run on the client and are no `StoreOps`
        // operations, so they have no tool, and neither have `clone` and `remote add|remove`,
        // which would let an agent point the store at a URL of its choosing; the workspaces'
        // operations (M5) are tools of their own table, `WS_TOOLS`, but `ws_delete`, which
        // removes a directory tree. `derive` is maintenance, which `swarmops derive` runs beside
        // the store, and no agent's work; `blob_get` is a client's primitive, raw bytes that a
        // workspace writes out.
        let client_side = [
            "InitReq",
            "GitImportReq",
            "GitExportReq",
            "CloneReq",
            "RemoteAddReq",
            "RemoteRemoveReq",
        ];
        let no_tool = ["DeriveReq", "BlobGetReq"];
        let all = swarmops_api::schema_for_all();
        let requests: Vec<&str> = all
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|name| {
                name.ends_with("Req")
                    && !name.starts_with("Ws")
                    && !client_side.contains(name)
                    && !no_tool.contains(name)
            })
            .collect();
        let mut advertised: Vec<&str> = TOOLS.iter().map(|spec| spec.request).collect();
        advertised.sort_unstable();
        assert_eq!(
            advertised, requests,
            "one tool per operation's request type"
        );

        for spec in &TOOLS {
            let called = RefCell::new(Vec::new());
            operate(spec.tool, Probe::new(&called, &RefCell::new(Vec::new())));
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
        let set = |expected: Option<&str>| {
            let mut req = RefSetReq::new("main", "main");
            req.expected_old = expected.map(Rev::from);
            req.create = expected.is_none();
            req
        };
        assert!(set(None).refuse().is_ok());
        assert!(set(Some(&id)).refuse().is_ok());
        assert!(set(Some("main")).refuse().is_err());
        let delete = |expected: &str| RefDeleteReq::new("main", expected);
        assert!(delete(&id).refuse().is_ok());
        assert!(delete("main~1").refuse().is_err());
    }

    /// The tools this server lists, as `tools/list` gives them.
    fn listed() -> Vec<ToolDef> {
        let global = Global::default();
        let env = |_: &str| -> Option<OsString> { None };
        Server::new(&global, &env).tools
    }

    /// Every tool's `inputSchema` is a plain object: of type `object`, with nothing at its top
    /// but `$schema`, `title`, `description`, `type`, `properties` and `required`. The Claude
    /// API refuses, with a 400 error, a request whose tools hold an input schema with `oneOf`,
    /// `anyOf` or `allOf` at the top, and Claude Code sends every tool with every request, so
    /// one such tool would stop each request of the session. The API's schema may state a rule
    /// across fields that way (`RefSetReq`'s exactly one of `expected_old` and `create`); a
    /// tool's leaves it to its description and to the server's own check.
    #[test]
    fn tool_input_schemas_are_plain_objects() {
        let plain = BTreeSet::from([
            "$schema",
            "title",
            "description",
            "type",
            "properties",
            "required",
        ]);
        for tool in listed() {
            let schema = tool.input_schema.as_object().expect("a schema object");
            assert_eq!(schema["type"], "object", "{}", tool.name);
            for key in schema.keys() {
                assert!(
                    plain.contains(key.as_str()),
                    "{}: {key} at the top of its inputSchema",
                    tool.name
                );
            }
        }
    }

    /// The five remote tools (M13) are listed, each with a plain-object `inputSchema` that
    /// takes a remote by its name: no URL and no path, so an agent cannot point the store at a
    /// place of its choosing.
    #[test]
    fn the_remote_tools_have_plain_input_schemas() {
        const REMOTE_TOOLS: [&str; 5] = ["remote_list", "fetch", "push", "sync", "sync_status"];
        let tools = listed();
        for name in REMOTE_TOOLS {
            // 1. Listed, as a plain object.
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("no {name} tool"));
            let schema = tool.input_schema.as_object().expect("a schema object");
            assert_eq!(schema["type"], "object", "{name}");
            for key in schema.keys() {
                assert!(PLAIN_OBJECT.contains(&key.as_str()), "{name}: {key}");
            }

            // 2. No field names a place.
            let properties = schema.get("properties").and_then(Value::as_object);
            for field in properties.into_iter().flat_map(Map::keys) {
                let place = ["url", "path", "git_path"].contains(&field.as_str());
                assert!(!place, "{name}: {field}");
            }
        }
    }

    /// What the strip to a plain object ([`tool_input_schema`]) loses of `schema`, a request
    /// type's schema standing alone: a key at its top that is neither plain ([`PLAIN_OBJECT`]) nor
    /// a combinator (`oneOf`, `anyOf`, `allOf`), and, as `<combinator>.<name>`, a property a
    /// combinator's branch names that the top's `properties` lack, or any other key of a branch
    /// (a nested rule). The fields of a flattened enum are what schemars gives that way.
    fn lost_by_the_strip(schema: &Value) -> BTreeSet<String> {
        let top = &schema["properties"];
        let mut lost = BTreeSet::new();
        for (key, value) in schema.as_object().expect("a schema object") {
            if PLAIN_OBJECT.contains(&key.as_str()) {
                continue;
            }
            if !matches!(key.as_str(), "oneOf" | "anyOf" | "allOf") {
                lost.insert(key.clone());
                continue;
            }
            for branch in value.as_array().into_iter().flatten() {
                let branch = branch.as_object().expect("a branch is a schema object");
                for (inner, rule) in branch {
                    let names: Vec<&str> = match inner.as_str() {
                        "properties" => rule
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|p| p.0.as_str())
                            .collect(),
                        "required" => rule
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect(),
                        "type" | "title" | "description" => continue,
                        nested => {
                            lost.insert(format!("{key}.{nested}"));
                            continue;
                        }
                    };
                    let missing = names.into_iter().filter(|name| top.get(name).is_none());
                    lost.extend(missing.map(|name| format!("{key}.{name}")));
                }
            }
        }
        lost
    }

    /// The strip to a plain object drops a rule across a request type's own fields (today only
    /// `RefSetReq`'s `oneOf`, which its description and the server's check carry), never a
    /// field: one that a later request type gives under a top-level combinator, as schemars
    /// writes a flattened enum's, would vanish from its tool's schema, and the other guards,
    /// which compare the top's `properties`, would not see it go.
    #[test]
    fn the_strip_to_a_plain_object_drops_no_field() {
        let all = swarmops_api::schema_for_all();
        let mut dropped = BTreeSet::new();
        for spec in &TOOLS {
            let schema = standalone(&all[spec.request]);
            assert_eq!(lost_by_the_strip(&schema), BTreeSet::new(), "{}", spec.name);
            let keys = schema.as_object().expect("a schema object").keys();
            dropped.extend(
                keys.filter(|key| !PLAIN_OBJECT.contains(&key.as_str()))
                    .map(|key| format!("{}: {key}", spec.name)),
            );
        }
        assert_eq!(dropped, BTreeSet::from(["ref_set: oneOf".to_owned()]));

        // A flattened untagged enum, as schemars 1.2 gives it (an internally tagged one is the
        // same under a `oneOf`), and a keyword that is no combinator.
        let flattened = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
            "anyOf": [
                {"type": "object", "properties": {"alpha": {"type": "string"}}, "required": ["alpha"]},
                {"type": "object", "properties": {"beta": {"type": "integer"}}, "required": ["beta"]},
            ],
        });
        let lost = BTreeSet::from(["anyOf.alpha".to_owned(), "anyOf.beta".to_owned()]);
        assert_eq!(lost_by_the_strip(&flattened), lost);
        let nested = json!({"properties": {"a": {}}, "oneOf": [{"anyOf": [{"required": ["a"]}]}]});
        assert_eq!(
            lost_by_the_strip(&nested),
            BTreeSet::from(["oneOf.anyOf".to_owned()])
        );
        let keyword = json!({"type": "object", "properties": {}, "if": {"required": ["x"]}});
        assert_eq!(
            lost_by_the_strip(&keyword),
            BTreeSet::from(["if".to_owned()])
        );
    }

    /// The `expected_old` of `ref_set` and `ref_delete` asks in the schema for what the server
    /// takes there ([`concrete_expectation`]): a full changeset id, as a pattern. Its
    /// description is the field's own, without the revision's that offers a ref name; `new`
    /// keeps the revision's, since a move may name its target any way.
    #[test]
    fn a_compare_and_swap_asks_for_a_full_id_in_its_schema() {
        let all = swarmops_api::schema_for_all();
        let revision = all["Rev"]["description"].as_str().unwrap();
        let tools = listed();
        let schema = |name: &str| {
            &tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap()
                .input_schema["properties"]
        };
        let own = |request: &str| all[request]["properties"]["expected_old"]["description"].clone();
        assert_eq!(
            schema("ref_delete")["expected_old"],
            json!({
                "type": "string",
                "pattern": "^[0-9a-fA-F]{64}$",
                "description": own("RefDeleteReq"),
            })
        );
        let set = &schema("ref_set")["expected_old"];
        assert_eq!(
            set["anyOf"],
            json!([{"type": "string", "pattern": "^[0-9a-fA-F]{64}$"}, {"type": "null"}])
        );
        assert_eq!(set["description"], own("RefSetReq"));
        for request in ["RefSetReq", "RefDeleteReq"] {
            let own = own(request);
            assert!(
                !own.as_str().unwrap().contains(revision),
                "{request}: {own}"
            );
        }
        let new = schema("ref_set")["new"]["description"].as_str().unwrap();
        assert!(new.ends_with(revision), "{new}");
    }

    /// One rule says what a full id is: the pattern the schema publishes and the refusal the
    /// server applies agree on each sample, and only the two of 64 hex digits, one of each
    /// case, are full ids.
    #[test]
    fn the_full_id_pattern_and_the_refusal_agree() {
        assert_eq!(full_id_pattern(), "^[0-9a-fA-F]{64}$");
        let id = "0123456789abcdef".repeat(4);
        for (sample, full) in [
            ("main".to_owned(), false),
            ("release/1.2".to_owned(), false),
            (id[..63].to_owned(), false),
            (id.clone(), true),
            (id.to_uppercase(), true),
            (format!("{id}0"), false),
            (format!("{id}~1"), false),
            (format!("{}g", &id[..63]), false),
        ] {
            assert_eq!(is_full_id(&sample), full, "{sample}");
            let refused = concrete_expectation("main", &Rev::from(sample.as_str()));
            assert_eq!(refused.is_ok(), full, "{sample}");
        }
    }

    /// A commit of two puts, the second of whose content is the client file `file`.
    fn naming(file: &str) -> CommitReq {
        let mut readme = ChangeInput::new("README.md", ChangeOp::Put);
        readme.content = Some(ContentInput::Inline("# acme\n".to_owned()));
        let mut lib = ChangeInput::new("src/lib.rs", ChangeOp::Put);
        lib.content = Some(ContentInput::File(file.to_owned()));
        CommitReq::new(vec![readme, lib], "x")
    }

    /// How each refusal of a client file ends: what to send instead.
    const SEND_INSTEAD: &str =
        ": name a regular file by its absolute path, or send the bytes as inline or base64 content";

    /// A commit's client file must be an absolute path, since the server would read a relative
    /// one from its own working directory, which the agent cannot see; and not this process's
    /// own stdin or stdout, which carry the protocol. The refusal names the change and the file,
    /// and what to send instead.
    #[test]
    fn a_client_file_must_be_absolute_and_not_this_process_s_stdio() {
        let refused = |file: &str, why: &str| {
            let error = naming(file).refuse().expect_err(file);
            assert_eq!((error.code.as_str(), error.exit), ("E_INVALID_ARG", 2));
            assert_eq!(
                Value::Object(error.details),
                json!({"index": 1, "path": "src/lib.rs", "file": file})
            );
            assert!(error.message.contains(why), "{file}: {}", error.message);
            assert!(error.message.ends_with(SEND_INSTEAD), "{}", error.message);
            assert!(!error.message.contains('\n'), "{}", error.message);
        };
        refused("src/lib.rs", "relative path");
        refused("./src/lib.rs", "relative path");
        // The test's stdin: /dev/null under the test harness, a pipe under most clients. A
        // socket (what some clients give a server) has no path, so it is skipped.
        if std::fs::metadata("/dev/stdin").is_ok() {
            refused("/dev/stdin", "stdin or stdout");
            refused("/proc/self/fd/0", "stdin or stdout");
        }
        if std::fs::metadata("/dev/stdout").is_ok() {
            refused("/dev/stdout", "stdin or stdout");
        }
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("lib.rs");
        std::fs::write(&regular, "pub fn f() {}\n").unwrap();
        assert!(naming(regular.to_str().unwrap()).refuse().is_ok());
        assert!(
            naming("/no/such/file").refuse().is_ok(),
            "read later, reported then"
        );
    }

    /// A commit's client file must be a regular file, a symlink to one included: a FIFO blocks
    /// the open until something opens it for writing, a socket cannot be opened, and a device
    /// can block or never end, so the server refuses each before it reads anything, and says
    /// what to send instead. A missing file is read later, and reported then.
    #[test]
    fn a_client_file_must_be_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.unwrap().success(), "mkfifo");
        let to_fifo = dir.path().join("to-pipe");
        std::os::unix::fs::symlink(&fifo, &to_fifo).unwrap();
        let socket = dir.path().join("sock");
        let _listening = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // Not /dev/null: under nextest it is this test's own stdin, which the stdio rule
        // refuses first.
        for file in [
            fifo.as_path(),
            to_fifo.as_path(),
            socket.as_path(),
            Path::new("/dev/zero"),
            dir.path(),
        ] {
            let file = file.to_str().unwrap();
            let error = naming(file).refuse().expect_err(file);
            assert_eq!((error.code.as_str(), error.exit), ("E_INVALID_ARG", 2));
            assert_eq!(
                Value::Object(error.details),
                json!({"index": 1, "path": "src/lib.rs", "file": file})
            );
            assert!(
                error.message.contains("not a regular file"),
                "{file}: {}",
                error.message
            );
            assert!(error.message.ends_with(SEND_INSTEAD), "{}", error.message);
            assert!(!error.message.contains('\n'), "{}", error.message);
        }
        let regular = dir.path().join("lib.rs");
        std::fs::write(&regular, "pub fn f() {}\n").unwrap();
        let to_regular = dir.path().join("to-lib.rs");
        std::os::unix::fs::symlink(&regular, &to_regular).unwrap();
        for file in [regular, to_regular, dir.path().join("missing")] {
            let named = naming(file.to_str().unwrap());
            assert!(named.refuse().is_ok(), "{}", file.display());
        }
    }

    /// A blob's client file is held to a commit's rules, by this server and before any read:
    /// a relative path and a device are refused, naming the `file` and why, with what to send
    /// instead; inline and base64 content, and an absolute regular file, pass. The operation's
    /// own open refuses a relative path with the same code, so only the message tells this
    /// refusal from that one.
    #[test]
    fn a_blob_s_client_file_is_held_to_a_commit_s_rules() {
        let file = |file: &str| BlobPutReq::new(ContentInput::File(file.to_owned()));
        for (named, why) in [
            ("src/lib.rs", "relative path"),
            ("./src/lib.rs", "relative path"),
            ("/dev/zero", "not a regular file"),
        ] {
            let error = file(named).refuse().expect_err(named);
            assert_eq!((error.code.as_str(), error.exit), ("E_INVALID_ARG", 2));
            assert_eq!(Value::Object(error.details), json!({"file": named}));
            assert!(error.message.contains(why), "{named}: {}", error.message);
            assert!(error.message.ends_with(SEND_INSTEAD), "{}", error.message);
            assert!(!error.message.contains('\n'), "{}", error.message);
        }

        // What the server reads: content it was sent, and an absolute regular file.
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("blob.bin");
        std::fs::write(&regular, b"\x00\x01").unwrap();
        for content in [
            ContentInput::Inline("x\n".to_owned()),
            ContentInput::Base64("AAE=".to_owned()),
            ContentInput::File(regular.to_str().unwrap().to_owned()),
        ] {
            let what = format!("{content:?}");
            assert!(BlobPutReq::new(content).refuse().is_ok(), "{what}");
        }
    }

    /// A panic in a tool's operation is that call's `E_INTERNAL` envelope, with the store the
    /// server names and what the operation resolved before it panicked, as a command's is.
    #[test]
    fn a_panic_in_an_operation_is_e_internal_with_what_it_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = format!("{}/store", dir.path().to_str().unwrap());
        let (ops, _) = LocalOps::init(Utf8Path::new(&store), "acme").unwrap();
        let who = Identity::parse("agent/worker-1").unwrap();
        let mut change = ChangeInput::new("a.txt".to_owned(), ChangeOp::Put);
        change.content = Some(ContentInput::Inline("a".to_owned()));
        let req = CommitReq::new(vec![change], "m".to_owned());
        let c1 = ops.commit(&who, req).unwrap().changeset;
        let mut main = RefSetReq::new("main", c1.as_str());
        main.create = true;
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
                let mut req = ShowReq::new("main");
                req.max_changes = 1;
                ops.show(req)?;
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

    /// A line longer than the server's cap (`MAX_REQUEST_BYTES`, small here) is answered with
    /// `-32600` and no `id`, and the server serves on: the next request on the same input is
    /// answered. A line of exactly the cap is served, with a line break and as the last line
    /// without one, where only its length tells it from a longer line.
    #[test]
    fn an_over_long_line_is_an_invalid_request_and_the_server_serves_on() {
        let global = Global::default();
        let env = |_: &str| -> Option<OsString> { None };
        let cap = 64;
        let mut server = Server::with_max_line(&global, &env, cap);
        let ping = |id: &str| format!(r#"{{"jsonrpc":"2.0","id":"{id}","method":"ping"}}"#);
        let long = ping(&"x".repeat(200));
        // Ids that pad a request to exactly the cap.
        let at_cap_id = "y".repeat(64 - ping("").len());
        let at_cap = ping(&at_cap_id);
        assert_eq!(at_cap.len(), 64);
        let last_id = "z".repeat(at_cap_id.len());
        let last = ping(&last_id);
        assert_eq!(last.len(), 64);
        let input = format!("{long}\n{}\n{at_cap}\n{last}", ping("after"));
        let mut output = Vec::new();
        server.serve(input.as_bytes(), &mut output).unwrap();
        let answers: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(answers.len(), 4, "{answers:?}");
        assert_eq!(answers[0]["error"]["code"], INVALID_REQUEST, "{answers:?}");
        assert!(answers[0].get("id").is_none(), "{answers:?}");
        let message = answers[0]["error"]["message"].as_str().unwrap();
        assert!(message.contains("64 bytes"), "{message}");
        for (answer, id) in answers[1..]
            .iter()
            .zip(["after", at_cap_id.as_str(), last_id.as_str()])
        {
            assert_eq!(answer, &json!({"jsonrpc": "2.0", "id": id, "result": {}}));
        }
        // The server the CLI starts takes lines of up to `MAX_REQUEST_BYTES`.
        assert_eq!(Server::new(&global, &env).max_line, MAX_REQUEST_BYTES);
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

    /// The instructions fit in the 2,048 characters of a server's instructions that Claude
    /// Code keeps, and are one line without a run of spaces that a broken string continuation
    /// leaves: a sentence added to them names the one it trims to stay within the limit.
    #[test]
    fn the_instructions_fit_in_claude_code_s_2048_characters() {
        let length = INSTRUCTIONS.chars().count();
        assert!(length <= 2048, "{length} characters");
        assert!(!INSTRUCTIONS.contains('\n'));
        assert!(!INSTRUCTIONS.contains("  "));
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
        // What `claim` makes of `params`, read from their JSON text as the server reads them.
        let claimed = |params: Value| {
            let text = params.to_string();
            let params: &RawValue = serde_json::from_str(&text).unwrap();
            claim(&Params::of(params).unwrap())
        };
        let meta = |meta: Value| json!({"_meta": meta});
        assert_eq!(claimed(json!({})).ok(), Some(Claim::None));
        assert_eq!(claimed(meta(json!({"other": 1}))).ok(), Some(Claim::None));
        for version in HANDSHAKE.into_iter().chain(["2024-11-05"]) {
            assert_eq!(
                claimed(meta(json!({VERSION_KEY: version}))).ok(),
                Some(Claim::Handshake(version.to_owned())),
                "an earlier version is the session's, or wants one"
            );
        }
        assert_eq!(
            claimed(meta(json!({VERSION_KEY: LATEST, CAPABILITIES_KEY: {}}))).ok(),
            Some(Claim::Latest)
        );
        assert!(matches!(
            claimed(meta(json!({VERSION_KEY: "2027-01-01", CAPABILITIES_KEY: {}}))),
            Err(Rejected::Version(version)) if version == "2027-01-01"
        ));
        for refused in [
            json!({VERSION_KEY: LATEST}),
            json!({VERSION_KEY: LATEST, CAPABILITIES_KEY: []}),
            json!({VERSION_KEY: 20_260_728}),
            json!("not an object"),
        ] {
            assert!(
                matches!(claimed(meta(refused.clone())), Err(Rejected::Params(_))),
                "{refused}"
            );
        }
    }
}
