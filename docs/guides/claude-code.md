# Use SwarmOps from Claude Code over MCP

SwarmOps is a git replacement for coding-agent swarms. `swarmops mcp` serves a SwarmOps store to
Claude Code as MCP tools. An agent reads files, history and diffs from the store, commits to it,
lands its change with a three-way merge, and undoes an operation.

This guide registers the server, gives each agent process an identity of its own, sets out the
two rules an agent needs (full ids for compare-and-swap, and small results), and gives
`claude --worktree` sessions and `isolation: worktree` subagents a SwarmOps workspace through
Claude Code's worktree hooks.

**Pre-1.0** · Linux today; macOS and Windows next · MIT licence

## Before you start

You need SwarmOps, a store and Claude Code, on one host.

- **SwarmOps.** The [Quick start](https://swarmops.cc/#start) installs `swarmops` and `so`, the same
  program under a shorter name.
- **A store**: one directory, on a native Linux filesystem. A path under `/mnt/`, where WSL mounts
  Windows drives, is refused with `E_UNSUPPORTED_FS`.
- **Claude Code on the same host.** Every operation runs against the host's own store, and the
  client starts the server as its own user; only the git remote tools (`fetch`, `push`, `sync`)
  reach another host, through the user's git.

**Make a store**

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme   # or --repo <store> on each command
so init "$SWARMOPS_REPO" --name acme
```

A new store has no `main` yet. The [Quick start](https://swarmops.cc/#start) makes the first commit
with the CLI, which creates it: through MCP, a `commit` with `advance` needs the full id of a
parent.

## Register the server with Claude Code

For one agent, register the server with `claude mcp add`, naming the store and the agent. Use the
store's absolute path.

**Claude Code**

```sh
claude mcp add swarmops -- so mcp --repo <store> --as <agent>
```

For a project that several agents open, give the project a `.mcp.json` instead:

**.mcp.json**

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

A project's `.mcp.json` is shared by every agent that opens the project, so it takes each agent's
identity from that agent's own `SWARMOPS_AGENT`, which Claude Code expands into the server's
environment, rather than giving all of them one `--as`.

It runs `swarmops`, not `so`: on some machine the short name may be missing (`--bin swarmops`
installs the long one alone) or belong to another program.

The empty default (`:-`) is deliberate. Without it, Claude Code passes an unset `${SWARMOPS_AGENT}`
on as that very text, which the server refuses as an invalid agent name (`E_INVALID_ARG`), and
`claude mcp list` warns too. With it, an unset variable is an empty value, which is no identity, so
an agent without one can read, and gets `E_IDENTITY_REQUIRED` when it writes.

Run `claude mcp list` to see whether Claude Code connected, then ask Claude to call `repo_info`. In
an interactive session, Claude Code asks you to approve the servers a project's `.mcp.json` names
before it uses them; [its MCP documentation](https://code.claude.com/docs/en/mcp) covers scopes and
approval.

## One identity per agent

1. **A Claude Code process**

   Started with its own `SWARMOPS_AGENT`, such as `agent/worker-1`.

2. **One `swarmops mcp` server**

   Claude Code starts one server for a session. By default the session's subagents share it.

3. **One store**

   Each call opens the store, so an agent sees what the others commit meanwhile.

A tool that changes the store needs an identity, `<family>/<instance>`, from `--as` or else
`SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID` naming the agent's run if it has one. Without one it fails
with `E_IDENTITY_REQUIRED` (exit 5). Reads need none.

The store and the identity are the server's, never a call's. No call can name another identity:
every commit and ref move is recorded for the agent the server runs as. That includes the session's
subagents, which share the server by default.

**An identity, for one shell**

```sh
export SWARMOPS_AGENT=agent/worker-1                 # or --as <family>/<instance>
```

An agent that needs an identity of its own runs as its own Claude Code process, with its own
`SWARMOPS_AGENT`. A subagent's definition can also declare a server inline, which Claude Code
connects when the subagent starts and disconnects when it finishes, while a reference to a server by
name shares the session's connection;
[Claude Code's subagent documentation](https://code.claude.com/docs/en/sub-agents) covers both.

SwarmOps records an identity and does not verify it: every envelope's `meta.auth` says `"trust"`. It
treats every agent that runs against a store as a trusted local user acting for the same owner, and
the store directory's permissions are the access boundary.

## The 38 MCP tools

A changeset is what a commit records, and a ref, such as `main`, is a name that points at one.
`swarmops mcp` lists one tool for each store operation but `derive`, which `swarmops derive` runs
beside the store as maintenance, and `blob_get`, which the workspaces use; then eleven workspace
tools.

A tool's arguments are the operation's request. Its result is the envelope the CLI prints for that
request, as `structuredContent` and as text, and a failed operation sets `isError`. A tool's
annotations say whether it changes the store and how.

### Fourteen tools read

- **`repo_info`**

  What the store is and holds: its id and name, the changeset `main` points at, and how many
  changesets and refs it has.

- **`resolve_rev`**

  The full id of the changeset a revision names, and whether the revision is a ref or a changeset.

- **`read_file`**

  One file at a revision, without a checkout, as UTF-8 text or base64, from an offset or a range of
  lines.

- **`read_tree`**

  One directory at a revision, a page of entries at a time, each file with its size.

- **`show`**

  One changeset in full: its parents, author, message, changes and signatures.

- **`log`**

  History along first parents, newest first, from `main` unless `rev` says otherwise.

- **`path_history`**

  The changesets that changed a file, or anything below a directory, newest first.

- **`diff`**

  The files that differ between two revisions, in path order. With `format` names, each one's path
  and status; with `stat`, also its line counts; with `unified`, also its hunks.

- **`op_log`**

  The store's operations, newest first: each write transaction, with the changesets it recorded and
  the refs it moved.

- **`ref_list`**

  Every ref, the changeset it points at, and how many moves it has had.

- **`blob_has`**

  Of the blob ids given, the ones the store holds blobs under.

- **`verify`**

  Checks the whole store's consistency; deep re-hashes every object too.

- **`remote_list`**

  The git remotes the store records: each one's name, URL and the branch `main` follows.

- **`sync_status`**

  How `main` and a remote's branch stand, as the last fetch saw it: the changes the remote lacks
  and the commits `main` lacks. Offline.

### Ten tools write

- **`commit`**

  Records a changeset from an explicit change list. With `advance`, it also moves that ref to the
  changeset in the same transaction, only if the ref still names `parent`.

- **`land`**

  Merges a change three-way onto `main`, or another `target`, and publishes the result as one
  changeset in one transaction. On `E_CONFLICT`, `details.report` lists each conflict, and
  `resolutions` settle them.

- **`rebase`**

  Merges a change three-way onto a ref's head, as `land` does, and records the result there as one
  changeset without moving the ref: build and test it, then `land` it with `expect_target`, the
  head it was rebased onto, so that the landed tree is the tested one.

- **`blob_put`**

  Stores content as a blob, outside any commit, and gives its id: the way to commit a large file.

- **`ref_set`**

  Moves a ref, or creates one, by compare-and-swap.

- **`ref_delete`**

  Removes a ref by compare-and-swap.

- **`undo`**

  Moves every ref an operation of `op_log` moved back to what it named before, all of them or none.
  `main` moves only forward (`E_REF_PROTECTED`). Changesets are never removed.

- **`fetch`**

  Fetches a git remote, by its name, and imports its followed branch's new commits; moves no ref.

- **`push`**

  Sends a store ref to a remote branch by compare-and-swap against what the last fetch saw, never by
  force.

- **`sync`**

  Fetches, lands `main`'s unpublished changes onto the remote's head, moves `main` there and
  pushes; with `mode` `local`, it does not push.

### Three maintenance tools

- **`gc`**, **`repair`** and **`upgrade`**

  `gc` removes the store's leftovers and never an object or a changeset; `repair` rebuilds what
  the journal can rebuild, sets damaged objects aside and verifies again; `upgrade` moves a
  FORMAT 1 store to FORMAT 2, which takes free-form branches and tags, and cannot be undone. While
  one runs, another fails at once with `E_MAINTENANCE_BUSY` (exit 4): retry
  ([Verify and repair](../operate/verify-and-repair.md), [Making room](../operate/making-room.md)).

### Eleven workspace tools

`ws_create`, `ws_status`, `ws_diff`, `ws_commit`, `ws_rebase`, `ws_resolve`, `ws_land`,
`ws_materialize`, `ws_discard`, `ws_list` and `ws_info` work on a workspace, a directory on the
server's host, named by its name in the store's registry. Each mutation's result carries
`status_after`, the workspace's status after it. [Workspaces](#workspaces-for-worktree-sessions-and-subagents)
below has the flow.

`init`, `whoami` and `schema` live in the CLI, and so do `blob get`, `ws delete`, `ws hook`,
`git import`, `git export`, `clone` and `remote add|remove`: an agent runs them as commands, not as
tools. The server speaks MCP 2026-07-28, and 2025-11-25 and 2025-06-18
to clients that open with `initialize`.

## Publish a change

The server's instructions tell the model to publish a changeset by landing it. Three calls do that:

1. `ref_list` gives the full id that `main` holds.
2. `commit` records the change, with that id as `parent` and without `advance`. It moves no ref.
3. `land` merges the changeset three-way onto the head of `main`, whatever `main` gained since, and
   moves `main` to the result in one transaction, as one new changeset whose parent is that head.
   Give it an `idempotency_key`, such as a new uuid v7.

A land that loses a race to another publish merges again and retries by itself, and the same land
run again with its key replays it. A change that conflicts with what `main` gained fails with
`E_CONFLICT` and the conflict report in `details.report`, and records nothing. While `main` has not
moved, a `commit` with `advance` and `parent` publishes in one step.

## Workspaces for worktree sessions and subagents

`claude --worktree` and a subagent with `isolation: worktree` run in a directory of their own.
Registered as Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks, `so ws hook` makes that
directory a SwarmOps workspace instead of a git worktree, so the repository need not be a git
repository. `so ws hook config` prints the settings to merge into `.claude/settings.json`:

```json
{
  "hooks": {
    "WorktreeCreate": [{"hooks": [{"type": "command", "command": "swarmops ws hook worktree-create"}]}],
    "WorktreeRemove": [{"hooks": [{"type": "command", "command": "swarmops ws hook worktree-remove"}]}]
  }
}
```

- **Create.** The hook finds the store from the session's directory: the workspace it stands in,
  else `--repo` or `SWARMOPS_REPO` (both naming different stores is exit 2, neither is exit 6). It
  makes the workspace `<root>/<store>/<name>`, where `<root>` is `SWARMOPS_WS_ROOT`, else
  `~/.local/state/swarmops/workspaces`, and `<store>` the store's name; its creator is
  `claude-code/<name>`. It stands on the head and prefixes of the workspace the session stands in,
  else on `main` with the prefixes of `.swarmops/workspaces.json` (the whole tree without them);
  another session's workspace of the same name gets a suffix. A new workspace gets the files `.worktreeinclude` lists, copied from the session's
  directory, and runs the repository's bootstrap command (`bootstrap.argv` of
  `.swarmops/workspaces.json`); `SWARMOPS_WS_BOOTSTRAP=skip` skips both. The hook prints the
  workspace's path, and a failure is an envelope on stderr. In a workspace with prefixes, which
  holds the `.gitignore` and `.swarmignore` files of the directories above them, a copy those
  files name is ignored at the root or in a directory above a prefix. Any other copy outside the
  prefixes is an edit outside them: `ws commit`, `ws land` and the remove hook refuse it
  (`E_WS_OUTSIDE_PREFIXES`, the directory and its edits kept). A copy inside another directory is
  refused as that directory (`config` for `config/master.key`): `ws materialize config` clears
  it, and an ignore file naming the copy keeps it out of the commit. For a copy no ignore file
  names, following the hint's `ws materialize <path>` makes the next commit store the copy, which
  may be a secret, in the store for good; delete the copy or name it in an ignore file first
  ([known limitations](../known-limitations.md#workspaces)).
- **Remove.** Uncommitted edits are kept as a rescue changeset on the workspace's head (no ref
  moves), its id printed on stderr; then the directory and its registry entry go. A directory that
  is no workspace is left alone, with exit 0.
- **Build, test and land.** In the workspace, the agent edits with its own tools and lands through
  MCP, in one of two ways. `ws_land` with `check` rebases, runs the command in the workspace and
  lands only the tree it ran on, but holds the server for as long as the command runs. With its
  own shell, the agent splits it: `ws_rebase`, then the build and test, then `ws_land` with `mode`
  `exact`, which publishes only if the target has not moved since that rebase (else
  `E_CAS_MISMATCH`: rebase and test again). [Build and test a change](../operate/build-and-test.md)
  shows the same steps at the command line.

Claude Code does not run `WorktreeRemove` when a subagent or a workflow agent finishes, so their
workspaces stay until `so ws delete <name>`; `so ws list --check` lists them. Subagents that share
their session's MCP server share its identity: the workspace's `created_by` names the subagent,
and its commits name the server's agent when the server has one.

## Two rules for agents

Two rules matter more to an agent than to a person at the CLI.

### Give compare-and-swap a full changeset id

`ref_set` and `ref_delete` take `expected_old` as the full changeset id, 64 hex digits, that the ref
held when the agent read it, from `ref_list`, `show`, `log` or a commit's result. A `commit` with
`advance` takes its `parent` by the same rule, and so does a `land` for the changeset its
`resolutions` were decided `against`.

A ref name or a relative revision such as `main~1` is read from the store when the call runs instead
of pinning what the agent saw, so the compare-and-swap protects nothing: the ref's own name always
matches, and `main~1` on `main` never does. An id prefix stops resolving (`E_AMBIGUOUS_ID`) once
another changeset shares it. The server refuses all three with `E_INVALID_ARG`.

### Ask for little

Clients limit long tool results. Claude Code saves a successful one over 25,000 tokens by default,
or over 50,000 characters whatever its token count, to a file and gives the agent its path instead;
it cuts only the text of an error. Each result holds its envelope twice, so an agent should ask for
little:

| Tool | Ask for at most | Read on with |
|---|---|---|
| `read_file` | `max_bytes` of 32768; less for a binary file | `next_offset`, as `offset` |
| `read_tree`, `log`, `path_history` | `limit` of 100 | `cursor` |
| `diff` | `limit` of 100, and `max_bytes` of 32768 for `unified` | `cursor` |
| `show` | `max_changes` of 100 | none |
| `op_log` | `limit` of 20 | `before` |

The tools' descriptions say so, and the operations' own defaults stay as they are. `read_file` alone
raises Claude Code's limit for itself: its entry in `tools/list` carries
`_meta: {"anthropic/maxResultSizeChars": 200000}`, so Claude Code keeps a `read_file` result of up
to 200,000 characters in the conversation instead of saving it to a file.

<a id="errors"></a>
## When a call fails

A failed call returns the envelope with a stable `E_*` code, whether running it again can succeed
(`retryable`), machine-readable `details` and a `hint`, often the next command to run. A hint is
often a `swarmops` command, and the tool of the same name runs it: `ref list` is `ref_list`, `cat`
is `read_file`, `ls` is `read_tree` and `info` is `repo_info`.

**A write without an identity**

```json
{
  "ok": false,
  "op": "commit",
  "api": 1,
  "error": {
    "code": "E_IDENTITY_REQUIRED",
    "message": "this operation changes the store, so it needs the caller's identity",
    "retryable": false,
    "exit": 5,
    "details": {},
    "hint": "pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"
  },
  "meta": {"repo": "…", "elapsed_ms": 0, "resolved": {}, "auth": "trust"}
}
```

- **`E_IDENTITY_REQUIRED`**

  The server runs without an identity, and a write reached it. Start the Claude Code process with
  `SWARMOPS_AGENT` set, or register the server with `--as`.

- **`E_INVALID_ARG` on `${SWARMOPS_AGENT}`**

  Claude Code passed an unset variable on as text. Keep the `:-` default, and set `SWARMOPS_AGENT`
  before you start Claude Code.

- **`E_CAS_MISMATCH` (exit 4)**

  Another agent moved the ref first, and the ref's target is in `details.actual`. A `commit` with
  `advance` that fails this way records nothing. Commit on the head you read, and `land` it: `land`
  merges three-way onto the ref's head however far the ref moved. Re-sending your whole-file puts on
  top of `details.actual` would revert the winner's edits to shared files.

- **`E_CONFLICT` (exit 3)**

  `land` could not merge the change and recorded nothing. `details.report` gives each conflicting
  file's kind, its hunks and the changeset and agent that last changed it on the target. Settle each
  path with `resolutions`, against the report's `theirs` by its full id, and land again; a path the
  target changed since comes back stale.

- **`E_BUSY`, `E_IO_TRANSIENT` (exit 4)**

  Retryable: the call can succeed if it runs again.

- **`E_INVALID_ARG` on `{"file": …}` content**

  The server reads the file itself, as the user it runs as, so it refuses a relative path, its own
  stdin or stdout, and anything that is not a regular file. Give an absolute path.

- **A result saved to a file**

  Claude Code limited a long result. Ask for little, as the second rule says.

- **`E_NOT_A_REPO` (exit 6)**

  There is no store at `--repo` or `SWARMOPS_REPO`. The server opens the store anew for each call,
  and serves on.

- **The server exits**

  The store runs in the server's process, so the server ends with it on any of the store's four
  documented aborts. The client sees the server exit, and has to start it again. A `commit` or a
  `land` that carries an `idempotency_key` is safe to run again: the same call replays instead of
  recording a second changeset. A commit's key is a uuid v7, new for each commit.

- **The server never starts**

  A command line that parses as `swarmops mcp` but is refused starts no server, and its envelope
  goes to stderr, not to stdout, which belongs to the protocol.

## What to know first

- **Two ways to work.** Through the tools alone, an agent reads a file at a revision and writes
  whole-file changes in a commit. In a workspace, it edits files on disk, builds and tests them,
  and lands the tested tree.
- **Git remotes by name only.** The tools `remote_list`, `fetch`, `push`, `sync` and `sync_status`
  work with the remotes the store records; adding a remote and `clone` stay on the command line
  ([GitHub as a remote](github-remote.md#through-mcp)). `git import` and `git export` are not tools.
- **One host.** SwarmOps is built and tested on x86_64 Linux today. Between hosts, work moves
  through a git remote.
- **Landing is not yet measured under a swarm's load.** `land` is built, and it merges again and
  retries by itself when the ref moves.
- **Pre-1.0.** A minor version, 0.2 to 0.3 say, may break a public surface other than the hash contract.
  The MCP tools' names, input schemas and results are such a surface.
- **What a client may rely on.** A new tool, optional argument or error code is not a breaking
  change, so a client should allow for them. The wording of `error.message` and of a `hint` is not
  part of the contract: the `code`, `exit`, `retryable` and the keys of `details` are.

## Go further

- The README's [MCP section](../../README.md#mcp) has the full text.
- [Known limitations](../known-limitations.md) lists what is still open, and which milestone takes
  each item.
- [Parallel coding agents on one repository](parallel-agents.md) sets SwarmOps beside git worktrees.
- The [home page](https://swarmops.cc/#mcp) lists the tools and the two ways to register the server.
