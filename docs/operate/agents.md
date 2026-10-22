# Give agents the store

`swarmops mcp` serves the store's operations to an agent as an MCP server on stdin and stdout.
This page registers it with Claude Code, lists what agents can call, and keeps a copy of every
answer for [monitoring](monitoring.md). Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- A store ([Stores](stores.md#create-a-store)), and an identity for each agent process
  ([Identities](stores.md#identities)).
- Claude Code (`claude`) on the same host, running as the user who owns the store
  ([permissions](stores.md#set-the-permissions)).

## Register the server for one agent

Register it with Claude Code for one agent, with its identity:

```sh
claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- swarmops mcp --repo ~/.local/state/swarmops/repos/acme
claude mcp list
```

```text
Added stdio MCP server swarmops with command: swarmops mcp --repo … to local config
…
swarmops: swarmops mcp --repo … - ✔ Connected
```

`✔ Connected` is the check. That registration is Claude Code's `local` scope: private to you, in
the directory you ran it in.

## Register the server for a project

For a project that several agent sessions open, one `.mcp.json` at its root registers the server
for every one of them, and each session gives its own identity in its own `SWARMOPS_AGENT`, which
Claude Code expands into the server's environment:

```json
{
  "mcpServers": {
    "swarmops": {
      "command": "swarmops",
      "args": ["mcp", "--repo", "/home/me/.local/state/swarmops/repos/acme"],
      "env": {"SWARMOPS_AGENT": "${SWARMOPS_AGENT:-}"}
    }
  }
}
```

Claude Code asks once, in each project, to approve the servers a `.mcp.json` names; until then
`claude mcp list` shows the server as `⏸ Pending approval`. The README's [MCP](../../README.md#mcp)
section says why the entry runs `swarmops` rather than `so`, and why the empty default (`:-`)
matters. Give the store's path absolute, as above.

## The tools agents get

The server lists 35 tools, one per operation, each taking the operation's request as its
arguments and returning the envelope the CLI prints for it:

| Reads | Writes | Workspaces |
| --- | --- | --- |
| `repo_info`, `resolve_rev`, `read_file`, `read_tree`, `show`, `log`, `path_history`, `diff`, `op_log`, `ref_list`, `blob_has`, `verify`, `remote_list`, `sync_status` | `commit`, `land`, `rebase`, `blob_put`, `ref_set`, `ref_delete`, `undo`, `fetch`, `push`, `sync` | `ws_create`, `ws_status`, `ws_diff`, `ws_commit`, `ws_rebase`, `ws_resolve`, `ws_land`, `ws_materialize`, `ws_discard`, `ws_list`, `ws_info` |

`init`, `whoami`, `schema`, `blob get`, `ws delete`, `ws hook`, `git import`, `git export`,
`derive`, `clone` and `remote add|remove` are commands only: you run them, not the agents. The git remote tools take a remote by its name
only, and run git with the server's environment: its ssh agent and token variables
([GitHub as a remote](../guides/github-remote.md#through-mcp)).

- The store and the identity are the server's: `--repo` (else `SWARMOPS_REPO`) and `--as` (else
  `SWARMOPS_AGENT`). No call can name others, and without an identity every write fails with
  `E_IDENTITY_REQUIRED`.
- Content given as `{"file": <absolute path>}`, in `commit`, `blob_put` or `land`'s
  `resolutions`, makes the server read that path itself, with the permissions of the user it runs
  as: an agent can store any file that user can read, also one that a sandbox of the agent's own
  file access keeps from it ([SECURITY.md](../../SECURITY.md)).
- Claude Code starts one server for a session, and the session's subagents share it and its
  identity. An agent that needs an identity of its own runs as its own Claude Code process.
- The server opens the store for each call, so it sees what other agents commit meanwhile. It
  exits on any of the store's four documented aborts (storage spec §2; the README's
  [MCP](../../README.md#store-identity-and-lifetime) section lists them), such as a failed journal
  write whose cut-back fails too: the client sees the server exit, and has to start it again.
- The server tells the model how to use it, in its MCP `instructions` and its tools'
  descriptions: a full 64-hex changeset id wherever a compare-and-swap needs one, `land` to
  publish (on `E_CONFLICT`, `details.report` says what to resolve), an `idempotency_key` to make a
  retry safe, and small pages (`max_bytes` of 32768 or less, a `limit` of 100 or less), since
  clients limit long tool results. Put the same in the project's `CLAUDE.md` if you want it in
  every prompt.
- Agents work without a checkout: they read files through `read_file` and `read_tree`, and write
  whole files through `commit`. An agent that must build or test its change works in a workspace,
  a directory on the server's host that the workspace tools name by its name
  ([Build and test a change](build-and-test.md)); Claude Code's worktree hooks make one for each
  `--worktree` session and `isolation: worktree` subagent
  ([Claude Code](../guides/claude-code.md#workspaces-for-worktree-sessions-and-subagents)).

## Keep a copy of every envelope

Every tool result is an envelope, and the server keeps none of them: `swarmops mcp` writes its
answers to the client and to nothing else, and `op log` lists the writes that succeeded, not the
calls that failed or how long they took. What [monitoring](monitoring.md) reads is therefore
in the client's transcript of the session (Claude Code keeps one for each, as JSON lines under
`~/.claude/projects/`), or in a file that you have the server's answers copied to. This wrapper does
the second, one file per agent, named by its identity.

1. Write the wrapper:

   ```sh
   cat > ~/.local/bin/swarmops-mcp-log <<'EOF'
   #!/bin/bash
   # `swarmops` with a copy of every answer the server writes, in one file per agent.
   dir=~/.local/state/swarmops/envelopes
   install -d -m 0700 "$dir"
   agent=${SWARMOPS_AGENT:-no-identity}
   exec swarmops "$@" > >(tee -a "$dir/${agent//\//_}.jsonl")
   EOF
   chmod +x ~/.local/bin/swarmops-mcp-log
   ```

2. Register it. It takes the arguments of `swarmops` as they are, so a registration differs from
   the ones above in its command only:

   ```json
   {
     "mcpServers": {
       "swarmops": {
         "command": "/home/me/.local/bin/swarmops-mcp-log",
         "args": ["mcp", "--repo", "/home/me/.local/state/swarmops/repos/acme"],
         "env": {"SWARMOPS_AGENT": "${SWARMOPS_AGENT:-}"}
       }
     }
   }
   ```

   `claude mcp add` takes it in the same place:

   ```sh
   claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- ~/.local/bin/swarmops-mcp-log mcp --repo ~/.local/state/swarmops/repos/acme
   claude mcp list
   ```

   ```text
   Added stdio MCP server swarmops with command: …/swarmops-mcp-log mcp --repo … to local config
   …
   swarmops: …/swarmops-mcp-log mcp --repo … - ✔ Connected
   ```

The wrapper `exec`s the server, so the client talks to the server itself and sees how it ends as it
would without the wrapper, the SIGABRT of one of the store's aborts included. `tee` appends each
answer to the agent's file as the server writes it: `agent/worker-1` writes
`agent_worker-1.jsonl`. A line is one JSON-RPC response. A tool call's holds the call's
envelope twice, as an object in `structuredContent` and as the text of the one content block, with
`isError` true when the envelope's `ok` is false; the answers to `initialize` and `tools/list` are
lines too, and hold no envelope. A failed `land`, cut down:

```text
{"jsonrpc":"2.0","id":6,"result":{"content":[{"type":"text","text":"{\"ok\":false,\"op\":\"land\",\"api\":1,\"error\":{\"code\":\"E_CONFLICT\",…}"}],"structuredContent":{"ok":false,"op":"land","api":1,"error":{"code":"E_CONFLICT",…},…},"isError":true}}
```

The files hold what the agents read and write, and a `read_file` line holds its page twice. They
grow for as long as the agents work and nothing prunes them: the wrapper makes their directory
`0700`, and old files are yours to move away between sessions.

Next: [the daily loop](daily-loop.md), and [what to read in these files](monitoring.md).
