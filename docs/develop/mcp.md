# MCP integration

`swarmops mcp` serves a store's operations to an agent as an MCP server on stdin and stdout. An
agent harness that speaks the Model Context Protocol, such as Claude Code, then calls each
operation as a tool, and gets back the same JSON envelope the CLI prints. This page covers
registering the server, the protocol exchange a client has with it, how identity works, every
tool with its matching command, result sizes and what goes wrong.

Every exchange below is from a real run of `swarmops` 0.1.0 against a scratch store at
`/tmp/dev-guide/acme`. Where a line is long, the command pipes it through `jq` to keep the part
that matters, and the filter is shown; `…` marks text cut by hand.

- [Register the server with Claude Code](#register-the-server-with-claude-code)
- [Any stdio MCP client](#any-stdio-mcp-client)
- [Protocol versions](#protocol-versions)
- [Identity: one per agent process](#identity-one-per-agent-process)
- [The tools](#the-tools)
- [Landing through MCP](#landing-through-mcp)
- [Result size limits](#result-size-limits)
- [Failure modes](#failure-modes)

## Register the server with Claude Code

### One agent: `claude mcp add`

For a single agent, register the server with its identity in the server's environment:

```sh
claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- swarmops mcp --repo /tmp/dev-guide/acme
claude mcp list | grep swarmops
```

```text
Added stdio MCP server swarmops with command: swarmops mcp --repo /tmp/dev-guide/acme to local config
File modified: …/.claude.json [project: /tmp/dev-guide/local]
swarmops: swarmops mcp --repo /tmp/dev-guide/acme - ✔ Connected
```

That is Claude Code's `local` scope: private to you, in the directory you ran it in
(`claude mcp remove swarmops -s local` takes it out again).

### Every agent of a project: `.mcp.json`

For a project that several agent sessions open, register the server once at the project's root.
`claude mcp add --scope project` writes the `.mcp.json` for you. Single quotes keep your shell from
expanding `${SWARMOPS_AGENT:-}`, so that Claude Code expands it later, in each session:

```sh
claude mcp add --scope project swarmops -e 'SWARMOPS_AGENT=${SWARMOPS_AGENT:-}' -- swarmops mcp --repo /tmp/dev-guide/acme
cat .mcp.json
SWARMOPS_AGENT=agent/worker-1 claude mcp list | grep swarmops
```

```text
Added stdio MCP server swarmops with command: swarmops mcp --repo /tmp/dev-guide/acme to project config
File modified: /tmp/dev-guide/proj/.mcp.json
{
  "mcpServers": {
    "swarmops": {
      "type": "stdio",
      "command": "swarmops",
      "args": [
        "mcp",
        "--repo",
        "/tmp/dev-guide/acme"
      ],
      "env": {
        "SWARMOPS_AGENT": "${SWARMOPS_AGENT:-}"
      }
    }
  }
}swarmops: swarmops mcp --repo /tmp/dev-guide/acme - ⏸ Pending approval (run `claude` to approve)
```

Claude Code asks once, in each project, to approve the servers a `.mcp.json` names; until then
`claude mcp list` shows `⏸ Pending approval`. Three details of the entry matter:

- **`${SWARMOPS_AGENT:-}`, not one `--as`.** The file is shared by every session that opens the
  project, so each session's identity comes from its own `SWARMOPS_AGENT`. The empty default is
  deliberate: Claude Code passes an unset `${SWARMOPS_AGENT}` on as that very text, which the server
  refuses as an invalid agent name (`E_INVALID_ARG`), while an empty value is no identity, so a
  write gets `E_IDENTITY_REQUIRED` and reads still work.
- **`swarmops`, not `so`.** On some machine the short name may be missing (`cargo install --bin
  swarmops` installs the long one alone) or belong to another program.
- **An absolute store path.** The server runs in a working directory the agent cannot see.

## Any stdio MCP client

The transport is JSON-RPC 2.0 over stdio, one message per line. The server reads a message,
answers it on stdout if it is a request, and reads the next: one at a time, in order. stdout
carries nothing but answers. It exits 0 when stdin ends.

### A session, start to finish

A client that opens with `initialize` (the 2025-11-25 handshake) lists the tools, reads a file and
commits a change. The messages, one per line:

```sh
cat session.jsonl
```

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example","version":"0.0.1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"rev":"main","path":"src/lib.rs","max_bytes":32768}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"commit","arguments":{"message":"shout","changes":[{"path":"src/lib.rs","op":"put","content":{"inline":"pub fn greet() -> &'static str {\n    \"HELLO\"\n}\n"}}]}}}
```

```sh
SWARMOPS_AGENT=agent/worker-1 swarmops mcp --repo /tmp/dev-guide/acme < session.jsonl > answers.jsonl
echo "rc=$?"
```

```text
rc=0
```

Four answers, one per request; the notification gets none.

**`initialize`.** The server answers at the version asked for, with its identity and its
`instructions` for the model (cut here; the full text tells the model how to use the tools: full
ids for compare-and-swap, `land` to publish, small pages):

```text
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"swarmops","title":"SwarmOps","version":"0.2.0"},"instructions":"SwarmOps is source control for coding-agent swarms: many agents commit to one store at once. …"}}
```

**`tools/list`.** One page of 35 tools. Each entry holds `name`, `title`, `description`,
`inputSchema` and `annotations`; `read_file` alone also carries `_meta`:

```sh
sed -n 2p answers.jsonl | jq -c '.result.tools[] | {name, title, annotations} + (if ._meta then {_meta} else {} end)'
```

```text
{"name":"repo_info","title":"Describe the store","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"resolve_rev","title":"Resolve a revision","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"read_file","title":"Read a file","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true},"_meta":{"anthropic/maxResultSizeChars":200000}}
{"name":"read_tree","title":"List a directory","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"show","title":"Show a changeset","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"log","title":"List history","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"path_history","title":"List a path's history","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"diff","title":"Diff two revisions","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"op_log","title":"List operations","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"ref_list","title":"List refs","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"ref_set","title":"Move a ref","annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false}}
{"name":"ref_delete","title":"Delete a ref","annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false}}
{"name":"undo","title":"Undo an operation","annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false}}
{"name":"commit","title":"Commit a changeset","annotations":{"destructiveHint":false,"idempotentHint":false,"openWorldHint":false,"readOnlyHint":false}}
{"name":"land","title":"Land a change","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false}}
{"name":"blob_put","title":"Store a blob","annotations":{"destructiveHint":false,"idempotentHint":false,"openWorldHint":false,"readOnlyHint":false}}
{"name":"blob_has","title":"Find stored blobs","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"verify","title":"Verify the store","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"remote_list","title":"List git remotes","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
{"name":"fetch","title":"Fetch a git remote","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":true,"readOnlyHint":false}}
{"name":"push","title":"Push to a git remote","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":true,"readOnlyHint":false}}
{"name":"sync","title":"Sync with a git remote","annotations":{"destructiveHint":false,"idempotentHint":false,"openWorldHint":true,"readOnlyHint":false}}
{"name":"sync_status","title":"Show sync status","annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true}}
```

A tool's `inputSchema` is its request type's JSON Schema, as `swarmops schema <Type>` prints it,
with every definition it refers to written in place. It is kept a plain object (only `$schema`,
`title`, `description`, `type`, `properties` and `required` at its top), since the Claude API
refuses a tool whose schema has a combinator there. A rule across fields that the request type
states with a top-level `oneOf` (exactly one of `ref_set`'s `expected_old` and `create`) is left
to the description and to the server's own check. `read_file`'s, cut to its keys:

```sh
sed -n 2p answers.jsonl | jq -c '.result.tools[] | select(.name == "read_file") | .inputSchema | {title, required, properties: (.properties | keys)}'
```

```text
{"title":"ReadFileReq","required":["rev","path"],"properties":["lines","max_bytes","offset","path","rev"]}
```

**`tools/call`.** The result holds the envelope twice: as `structuredContent`, and as the text of
the one content block. `isError` is true exactly when the envelope's `ok` is false:

```sh
sed -n 3p answers.jsonl
```

```text
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"ok\":true,\"op\":\"read_file\",\"api\":1,\"result\":{\"blob\":\"2cda0801d989cfda6046cb35010ee0d591257324bf4d95d5543b1f7ac41d0463\",\"size\":47,\"kind\":\"file\",\"encoding\":\"utf8\",\"content\":\"pub fn greet() -> &'static str {\\n    \\\"hello\\\"\\n}\\n\",\"truncated\":false},\"meta\":{\"repo\":\"/tmp/dev-guide/acme\",\"elapsed_ms\":0,\"resolved\":{\"main\":\"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118\"},\"auth\":\"trust\"}}"}],"structuredContent":{"ok":true,"op":"read_file","api":1,"result":{"blob":"2cda0801d989cfda6046cb35010ee0d591257324bf4d95d5543b1f7ac41d0463","size":47,"kind":"file","encoding":"utf8","content":"pub fn greet() -> &'static str {\n    \"hello\"\n}\n","truncated":false},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{"main":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118"},"auth":"trust"}},"isError":false}}
```

The commit, recorded for the server's agent, `agent/worker-1`. It names no `advance`, so it moves
no ref; the next step is a land ([Landing through MCP](#landing-through-mcp)):

```sh
sed -n 4p answers.jsonl | jq -c '.result.structuredContent.result'
```

```text
{"changeset":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","parent":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118","root":"00842161e3ab9a26a8a0d593193afaf10915f9fcf570b04c7a088c1b3cd7209d","files_changed":1,"blobs_written":1,"replayed":false}
```

### Methods

| Method | Served |
| --- | --- |
| `initialize` | Opens a session at 2025-11-25 or 2025-06-18; a request that names 2026-07-28 gets `-32601`. |
| `notifications/initialized`, other notifications | No answer. A `notifications/cancelled` finds nothing to cancel: each request is answered before the next is read. |
| `ping` | `{}`, also before `initialize`; not in 2026-07-28 (`-32601`). |
| `server/discover` | 2026-07-28 only: the versions served, capabilities and instructions. |
| `tools/list` | One page; a `cursor` is `-32602`. |
| `tools/call` | `name` and `arguments`, the operation's request. |

## Protocol versions

The server speaks three revisions of MCP:

| Version | How a client opens it |
| --- | --- |
| 2026-07-28 | No handshake: each request names the version in `params._meta`, with the client's capabilities. |
| 2025-11-25 | `initialize`; the version given to a client that asks for one the handshake does not serve. |
| 2025-06-18 | `initialize`; the first revision with `structuredContent` and `outputSchema`. |

Both handshake revisions get the same tools and results. 2025-03-26 and older are not served.

A 2026-07-28 client sends each request on its own. Results carry `resultType: "complete"` and the
server's identity in `_meta`; `server/discover` and `tools/list` say a client may keep them an
hour (`ttlMs`), since they change only with the binary. A version the server does not serve gets
`-32022` with the list it does, and a request that names no version before any `initialize` gets
`-32602` (the `content` block, the envelope's second copy, is cut by the `jq` filter):

```sh
cat s3.jsonl
SWARMOPS_AGENT=agent/worker-1 swarmops mcp --repo /tmp/dev-guide/acme < s3.jsonl | jq -c 'if .result.structuredContent then (.result.content = "…") else . end | if .result.instructions then (.result.instructions = "…") else . end'
```

```text
{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"resolve_rev","arguments":{"rev":"main"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2027-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}
{"jsonrpc":"2.0","id":4,"method":"tools/list"}
{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28","2025-11-25","2025-06-18"],"capabilities":{"tools":{}},"instructions":"…","ttlMs":3600000,"cacheScope":"public","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"swarmops","title":"SwarmOps","version":"0.2.0"}}}}
{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":"…","structuredContent":{"ok":true,"op":"resolve_rev","api":1,"result":{"id":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118","kind":"ref"},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{"main":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118"},"auth":"trust"}},"isError":false,"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"swarmops","title":"SwarmOps","version":"0.1.0"}}}}
{"jsonrpc":"2.0","id":3,"error":{"code":-32022,"message":"Unsupported protocol version: 2027-01-01; this server serves 2026-07-28 in params._meta, and 2025-11-25 and 2025-06-18 through initialize","data":{"requested":"2027-01-01","supported":["2026-07-28","2025-11-25","2025-06-18"]}}}
{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"Invalid params: tools/list names no protocol version in params._meta (io.modelcontextprotocol/protocolVersion), and no initialize has opened a session"}}
```

## Identity: one per agent process

The store and the identity belong to the server, never to a call:

- the store is `--repo`, else `SWARMOPS_REPO`, opened anew for each call, so a call sees what
  other agents committed meanwhile;
- the identity is `--as`, else `SWARMOPS_AGENT` (`<family>/<instance>`), with `SWARMOPS_RUN_ID`
  naming the agent's run if it has one.

No tool argument names another identity: every commit, land and ref move is recorded for the
server's agent. Reads need no identity; without one, every write fails with
`E_IDENTITY_REQUIRED` (exit 5 in the envelope).

Claude Code starts one server for a session, and the session's subagents share it, so every write
they make is recorded for the session's agent. An agent that needs an identity of its own runs as
its own Claude Code process, started with its own `SWARMOPS_AGENT` in its environment: with the
`.mcp.json` above, two sessions of the same project started as `agent/worker-1` and
`agent/worker-2` get one server each, each recording its own agent.

Give every agent process its own instance name, and yourself one of your own, such as
`operator/ada`, so that `log`, `show` and `op log` say who did what.

## The tools

One tool per operation, named as the envelope's `op` is, in `tools/list`'s order. Its arguments are
the CLI command's request; `swarmops schema <Request>` prints the type.

| Tool | CLI command | Request type | Changes the store |
| --- | --- | --- | --- |
| `repo_info` | `swarmops info` | `InfoReq` | no |
| `resolve_rev` | `swarmops resolve REV` | `ResolveRevReq` | no |
| `read_file` | `swarmops cat REV:PATH` | `ReadFileReq` | no |
| `read_tree` | `swarmops ls REV[:PATH]` | `ReadTreeReq` | no |
| `show` | `swarmops show REV` | `ShowReq` | no |
| `log` | `swarmops log [REV]` | `LogReq` | no |
| `path_history` | `swarmops history REV:PATH` | `PathHistoryReq` | no |
| `diff` | `swarmops diff FROM TO` | `DiffReq` | no |
| `op_log` | `swarmops op log` | `OpLogReq` | no |
| `ref_list` | `swarmops ref list` | `RefListReq` | no |
| `ref_set` | `swarmops ref set NAME REV` | `RefSetReq` | moves a ref |
| `ref_delete` | `swarmops ref delete NAME` | `RefDeleteReq` | moves a ref |
| `undo` | `swarmops op undo OP` | `UndoReq` | moves refs |
| `commit` | `swarmops commit` | `CommitReq` | adds a changeset; with `advance`, moves a ref |
| `land` | `swarmops land HEAD` | `LandReq` | adds a changeset and moves the target |
| `rebase` | `swarmops rebase HEAD` | `RebaseReq` | adds a changeset |
| `blob_put` | `swarmops blob put` | `BlobPutReq` | adds a blob |
| `blob_has` | `swarmops blob has` | `BlobHasReq` | no |
| `verify` | `swarmops verify` | `VerifyReq` | no |
| `remote_list` | `swarmops remote list` | `RemoteListReq` | no |
| `fetch` | `swarmops fetch REMOTE` | `FetchReq` | adds what the remote holds |
| `push` | `swarmops push REMOTE` | `PushReq` | no; moves the remote's branch |
| `sync` | `swarmops sync REMOTE` | `SyncReq` | adds changesets and moves `main` |
| `sync_status` | `swarmops sync status REMOTE` | `SyncStatusReq` | no |
| `ws_create` | `swarmops ws create NAME` | `WsCreateReq` | makes a workspace |
| `ws_status` | `swarmops ws status` | `WsStatusReq` | no |
| `ws_diff` | `swarmops ws diff` | `WsDiffReq` | no |
| `ws_commit` | `swarmops ws commit` | `WsCommitReq` | adds a changeset |
| `ws_rebase` | `swarmops ws rebase` | `WsRebaseReq` | adds a changeset |
| `ws_resolve` | `swarmops ws resolve` | `WsResolveReq` | the workspace only |
| `ws_land` | `swarmops ws land` | `WsLandReq` | adds changesets and moves the target |
| `ws_materialize` | `swarmops ws materialize PATH` | `WsMaterializeReq` | the workspace only |
| `ws_discard` | `swarmops ws discard` | `WsDiscardReq` | the workspace only |
| `ws_list` | `swarmops ws list` | `WsListReq` | no |
| `ws_info` | `swarmops ws info` | `WsInfoReq` | no |

`init`, `whoami`, `schema`, `blob get`, `ws delete`, `ws hook`, `git import`, `git export`,
`clone`, `remote add|remove`, `derive` and `mcp` have no tool: an operator or a script runs them as
commands. A workspace tool names its workspace by its name in the store's registry, and works on
that directory on the server's host. A `verify` that finds problems is `E_STORE_CORRUPT`,
as `swarmops verify` is.

Rules the server adds to the request types, each refused with `E_INVALID_ARG`:

- **Full ids for compare-and-swap.** `ref_set`'s and `ref_delete`'s `expected_old`, a `commit`'s
  `parent` when it has `advance`, and `land`'s `resolutions.against` must be the full 64-hex
  changeset id you read. A ref name or a relative revision such as `main~1` is read when the call
  runs, so the compare-and-swap would protect nothing; an id prefix stops resolving once another
  changeset shares it. The CLI's `--expect` still takes any revision.
- **Absolute, regular files for `{"file": <path>}`.** In `commit`, `blob_put` and `land`'s
  resolutions the server reads the file itself, as the user it runs as, so a relative path, the
  server's own stdin or stdout, and anything that is not a regular file (a FIFO, a socket, a device,
  a directory) are refused. An agent can store any file that user can read, also one its own
  sandbox keeps from it ([SECURITY.md](../../SECURITY.md)).

## Landing through MCP

To publish, an agent commits on the head it read, then lands. Here `agent/worker-2` moved `main`
after `agent/worker-1` read it, changing the same line as worker-1's commit `0e5171a0…` from the
session above:

```sh
swarmops --as agent/worker-2 commit -m "greet the swarm" --changes rival.json --parent bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118 --advance main | jq -c '.result.advanced'
```

```text
{"name":"main","old":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118","new":"3acdfd7888b83ed5af6f578f84928953e979de8fa0438e19e565fc6469f48c82","already_applied":false}
```

Worker-1 lands its commit with an `idempotency_key`. The land meets the conflict, records nothing,
and returns `E_CONFLICT` with the report in `details.report`:

```sh
cat land.jsonl
SWARMOPS_AGENT=agent/worker-1 swarmops mcp --repo /tmp/dev-guide/acme < land.jsonl | sed 1d > land1.jsonl
jq -c '{isError: .result.isError, error: (.result.structuredContent.error | {code, retryable, exit})}' land1.jsonl
jq '.result.structuredContent.error.details.report | {theirs, files: [.files[] | {path, theirs_agent, hunks}]}' land1.jsonl
```

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example","version":"0.0.1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"land","arguments":{"head":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","idempotency_key":"land-shout-1"}}}
{"isError":true,"error":{"code":"E_CONFLICT","retryable":false,"exit":3}}
{
  "theirs": "3acdfd7888b83ed5af6f578f84928953e979de8fa0438e19e565fc6469f48c82",
  "files": [
    {
      "path": "src/lib.rs",
      "theirs_agent": "agent/worker-2",
      "hunks": [
        {
          "base": {
            "first": 2,
            "last": 2,
            "text": "    \"hello\"\n"
          },
          "id": 0,
          "ours": {
            "first": 2,
            "last": 2,
            "text": "    \"HELLO\"\n"
          },
          "theirs": {
            "first": 2,
            "last": 2,
            "text": "    \"hello, swarm\"\n"
          }
        }
      ]
    }
  ]
}
```

It resolves the file against the report's `theirs`, by its full id, and lands again with a new key.
The same call sent a second time replays the first (`status: replayed`) and writes nothing:

```sh
cat resolve.jsonl
SWARMOPS_AGENT=agent/worker-1 swarmops mcp --repo /tmp/dev-guide/acme < resolve.jsonl | sed 1d | jq -c '{id, isError: .result.isError, result: (.result.structuredContent.result | {status, landed, rebased_onto, attempts})}'
```

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example","version":"0.0.1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"land","arguments":{"head":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","idempotency_key":"land-shout-2","resolutions":{"against":"3acdfd7888b83ed5af6f578f84928953e979de8fa0438e19e565fc6469f48c82","files":{"src/lib.rs":{"content":{"inline":"pub fn greet() -> &'static str {\n    \"HELLO, SWARM\"\n}\n"}}}}}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"land","arguments":{"head":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","idempotency_key":"land-shout-2","resolutions":{"against":"3acdfd7888b83ed5af6f578f84928953e979de8fa0438e19e565fc6469f48c82","files":{"src/lib.rs":{"content":{"inline":"pub fn greet() -> &'static str {\n    \"HELLO, SWARM\"\n}\n"}}}}}}}
{"id":2,"isError":false,"result":{"status":"landed","landed":"e7046994727e92138a6f40edbb9f67b65c7ab8c38d4a0b4076ac9273b834c844","rebased_onto":"3acdfd7888b83ed5af6f578f84928953e979de8fa0438e19e565fc6469f48c82","attempts":1}}
{"id":3,"isError":false,"result":{"status":"replayed","landed":"e7046994727e92138a6f40edbb9f67b65c7ab8c38d4a0b4076ac9273b834c844","rebased_onto":"3acdfd7888b83ed5af6f578f84928953e979de8fa0438e19e565fc6469f48c82","attempts":1}}
```

A resolution is `{"take": "ours"}`, `{"take": "theirs"}`, `{"content": CONTENT}` (with an optional
`mode`) or `{"delete": true}`; it decides its path and every path below it. A path the target
changed again since `against` comes back in a new report's `stale`: read the new `theirs`, and
resolve against it.

An agent that must build or test its change before it lands it works in a workspace: `ws_create`,
edits on disk, then `ws_land` with `check`, or `ws_rebase`, its own build and test, and `ws_land`
with `mode` `exact`, which lands only the tree it tested
([Build and test a change](../operate/build-and-test.md)).

## Result size limits

Clients limit long tool results. Claude Code saves a successful one over 25,000 tokens by default,
or over 50,000 characters whatever its token count, to a file and gives the agent its path instead;
it cuts only the text of an error. Each result holds its envelope twice, so ask for little:

| Tool | Ask for | Page on with |
| --- | --- | --- |
| `read_file` | `max_bytes` of 32768 or less (less for a binary file, sent as base64) | `offset` = the result's `next_offset` |
| `read_tree` | `limit` of 100 or less | `cursor` |
| `log`, `path_history` | `limit` of 100 or less | `cursor` |
| `diff` | `limit` of 100 or less; `max_bytes` of 32768 or less for `unified` | `cursor` |
| `show` | `max_changes` of 100 or less | |
| `op_log` | `limit` of 20 or less | `before` |

The tools' descriptions say the same, so the model sees it; the operations' own defaults (such as
`read_file`'s `max_bytes` of 1 MiB) stay as they are. `read_file` alone raises Claude Code's limit
for itself: its `tools/list` entry carries `_meta: {"anthropic/maxResultSizeChars": 200000}`, so
Claude Code keeps a `read_file` result of up to 200,000 characters in the conversation.

A file made of control characters reads back far larger than its size, since JSON spells most of
them in six bytes each; [known limitations](../known-limitations.md#reads-and-response-sizes) has
the numbers.

## Failure modes

Three layers fail differently: the server can refuse to start, a message can be a protocol error,
and an operation can fail inside a successful tool call.

### The server does not start

A command line that parses as `swarmops mcp` but is refused starts no server. Its envelope goes to
stderr, since stdout belongs to the protocol, and the process exits with the envelope's code:

```sh
swarmops mcp --frob </dev/null >out.txt 2>err.txt; echo "exit=$?"
wc -c < out.txt
cat err.txt
```

```text
exit=2
0
{"ok":false,"op":"mcp","api":1,"error":{"code":"E_INVALID_ARG","message":"unexpected argument '--frob' found","retryable":false,"exit":2,"details":{},"hint":"Usage: swarmops mcp [OPTIONS]\nFor more information, try '--help'."},"meta":{"elapsed_ms":1,"resolved":{},"auth":"trust"}}
```

A usage error before the word `mcp` (`swarmops --frob mcp`) is an ordinary command error, printed
on stdout with exit code 2, and starts no server either.

### Protocol errors

A message the server cannot serve gets a JSON-RPC error, and the server reads on:

| Code | For |
| --- | --- |
| `-32700` | A line that is not JSON, or not UTF-8. |
| `-32600` | A batch or anything else that is not one object; a bad `jsonrpc` or `id`; a line longer than 64 MiB. |
| `-32601` | A method the session's revision does not have. |
| `-32602` | Bad `params`; no version before `initialize`; an unknown tool name; `arguments` that are not an object; a `tools/list` cursor. |
| `-32603` | A panic outside an operation (a bug). |
| `-32022` | A protocol version the server does not serve; `data.supported` lists the ones it does. |

```sh
printf '%s\n' 'not json' | swarmops mcp; echo "exit=$?"
```

```text
{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error: expected ident at line 1 column 2"}}
exit=0
```

### Failed operations

An operation that fails is a successful tool call whose result has `isError: true` and the
envelope's `error`: a stable `E_*` code, `retryable`, the CLI's `exit`, machine-readable `details`
and a `hint`, often the command (and so the tool) to run next. In this session the server has no
identity:

```sh
cat s2.jsonl
env -u SWARMOPS_AGENT swarmops mcp --repo /tmp/dev-guide/acme < s2.jsonl | sed 1d | jq -c 'if .result.structuredContent then {id, structuredContent: .result.structuredContent, isError: .result.isError} else . end'
```

```text
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"example","version":"0.0.1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ref_set","arguments":{"name":"main","new":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","expected_old":"main"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ref_set","arguments":{"name":"main","new":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","expected_old":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118"}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"git_import","arguments":{}}}
{"jsonrpc":"2.0","id":5,"method":"ping"}
{"id":2,"structuredContent":{"ok":false,"op":"ref_set","api":1,"error":{"code":"E_INVALID_ARG","message":"expected_old \"main\" is not a full changeset id, which a compare-and-swap needs: a ref name or a relative revision is read when the call runs instead of pinning the id you read (main itself always matches, so the compare-and-swap would protect nothing, and main~1 never matches), and an id prefix stops resolving once another changeset shares it; pass the 64-hex id the ref held when you read it, from ref_list, show, log or a commit's result","retryable":false,"exit":2,"details":{"expected_old":"main","name":"main"},"hint":"swarmops ref list"},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{},"auth":"trust"}},"isError":true}
{"id":3,"structuredContent":{"ok":false,"op":"ref_set","api":1,"error":{"code":"E_IDENTITY_REQUIRED","message":"this operation changes the store, so it needs the caller's identity","retryable":false,"exit":5,"details":{},"hint":"pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{},"auth":"trust"}},"isError":true}
{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"Unknown tool: git_import"}}
{"jsonrpc":"2.0","id":5,"result":{}}
```

Arguments that do not fit the request type are `E_INVALID_ARG` too, with a hint that names the
schema. A store that is not there fails each call with `E_NOT_A_REPO`, and the server serves on.
A relative `{"file": …}` path is refused before anything is read:

```sh
swarmops mcp < nostore.jsonl | sed 1d | jq -c '.result.structuredContent.error | {code, exit, hint}'
SWARMOPS_AGENT=agent/worker-1 swarmops mcp --repo /tmp/dev-guide/acme < blob.jsonl | sed 1d | jq -c '.result.structuredContent.error | {code, message}'
```

```text
{"code":"E_NOT_A_REPO","exit":6,"hint":"pass --repo <store> or set SWARMOPS_REPO; swarmops init <dir> creates a store"}
{"code":"E_INVALID_ARG","message":"the client file \"notes.json\" is a relative path, which the server would read from its own working directory, not yours: name a regular file by its absolute path, or send the bytes as inline or base64 content"}
```

The first server has neither `--repo` nor `SWARMOPS_REPO` and is asked for `repo_info`; the
second is asked for a `blob_put` of `{"file": "notes.json"}`. Each file holds an `initialize`
line, then that one `tools/call`.

What to do with each code is the same as at the CLI: [CLI scripting](cli-scripting.md#retry-rules)
gives the retry rules, and its [Exit codes](cli-scripting.md#exit-codes) the exit codes.

### The server exits

The store runs in the server's process, so the server ends with it on any of the store's four
documented aborts: the process aborts with SIGABRT, saying why on stderr, and the client sees the
server exit and has to start it again. The server also exits 1 if reading stdin or writing stdout
fails. The four aborts, the first three of which any commit or ref move can meet:

- a failed journal write or sync whose cut-back fails too;
- a cut-back that fails while the repair before a write writes unlisted frames afresh;
- frames that repair writes afresh which land anywhere but where they were and cannot be cut back;
- an `init` that cannot take its `FORMAT` back, which no tool runs.

### Seeing what agents did

The server keeps none of its answers. `swarmops op log` lists the writes that succeeded; for the
calls themselves, read the client's transcript, or copy the server's answers to a file with the
wrapper in [Operations: where the envelopes are](../operations.md#where-the-envelopes-are).
