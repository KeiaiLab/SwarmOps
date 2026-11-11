---
title: "Features: what SwarmOps does, with real examples"
description: "Many writers in one store, landing with retry and a conflict report, workspaces, one JSON envelope per command, an MCP server, the op log and undo, the git round trip, GitHub as a remote, crash safety and verify."
---

# Features

The examples below ran one after another, with the binary built from the SwarmOps repository, against
the store that the [quick start](/docs/guides/quick-start) makes; the two git examples make stores of their
own, the remote one against a local repository served through a stand-in for ssh. In
the outputs, ids are cut to their first eight hex digits (`4a7bc757…`), a store's path to `…`,
and a `…` line stands for lines left out. `--human` prints a short text rendering of the
envelope instead of JSON.

## Many agents commit into one store at once

Each agent process writes the store directly. Object writes take no lock, metadata writes queue
in the kernel for one lock, and refs move only by compare-and-swap. Here eight agents each commit
a note and land it on `main`, all at once:

```sh
work() {  # one agent: commit a note, then land it on main
  local me=agent/worker-$1 cs
  printf '[{"path": "notes/%s.txt", "op": "put", "content": {"inline": "from worker %s\\n"}}]' $1 $1 > note-$1.json
  cs=$(so --as $me --human commit -m "note $1" --changes note-$1.json | sed -n 's/^changeset: //p')
  so --as $me --human land "$cs"
}
for i in 1 2 3 4 5 6 7 8; do work $i > land-$i.txt & done; wait
cat land-*.txt
so --human ls main:notes | grep name
```

```text
landed 42df54c0… on main (onto 1a2861f1…, attempt 4)
landed 8dff860c… on main (onto c1a3b584…, attempt 1)
landed fb162384… on main (onto 80d99e24…, attempt 4)
landed 0d8001c4… on main (onto 42df54c0…, attempt 5)
landed c1a3b584… on main (onto 4a7bc757…, attempt 1)
landed e0f1f8a6… on main (onto 0d8001c4…, attempt 5)
landed 80d99e24… on main (onto 8dff860c…, attempt 3)
landed 1a2861f1… on main (onto fb162384…, attempt 4)
entries[0].name: 1.txt
entries[1].name: 2.txt
entries[2].name: 3.txt
entries[3].name: 4.txt
entries[4].name: 5.txt
entries[5].name: 6.txt
entries[6].name: 7.txt
entries[7].name: 8.txt
```

All eight landed. `attempt` is the try that succeeded: a land whose `main` moved under it merged
again and retried by itself. [Parallel agents](/docs/guides/parallel-agents) compares this with
agents in git worktrees.

## Land: a three-way merge onto a ref, with retry and a conflict report

`land` merges a change three-way onto a ref's head (`main` by default), whatever the ref gained
since, without a checkout, and moves the ref to the result in one transaction, as one new
changeset. It retries up to `--max-attempts` times (8 by default) when the ref moves meanwhile,
and with `--key` the same land run again replays it. Here two agents change the same line from
the same `main`:

```sh
cat > two.json <<'JSON'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() -> u32 {\n    2\n}\n"}}]
JSON
cat > three.json <<'JSON'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() -> u32 {\n    3\n}\n"}}]
JSON
a=$(so --as agent/worker-1 --human commit -m "f returns 2" --changes two.json | sed -n 's/^changeset: //p')
b=$(so --as agent/worker-2 --human commit -m "f returns 3" --changes three.json | sed -n 's/^changeset: //p')
so --as agent/worker-1 --human land "$a"
so --as agent/worker-2 --human land "$b"; echo "exit $?"
```

```text
landed 31528945… on main (onto e0f1f8a6…, attempt 1)
error: E_CONFLICT: 1 path(s) of the change conflict with the target's head 31528945…: nothing was recorded
conflict: content src/lib.rs
hint: swarmops cat 31528945…:src/lib.rs, then swarmops land 7436166c… --resolve <file> (resolutions over MCP) with each path's resolution and against = 31528945…
exit 3
```

The second land recorded nothing. In JSON, its `details.report` holds each conflicting file's
kind, its hunks (each side's lines and text) and the agent that last changed it on the target.
`--resolve` settles the conflict without a checkout, path by path: take a side, give new content,
or delete.

```sh
theirs=$(so --human resolve main | sed -n 's/^id: //p')
echo "{\"against\": \"$theirs\", \"files\": {\"src/lib.rs\": {\"take\": \"ours\"}}}" > resolve.json
so --as agent/worker-2 --human land "$b" --resolve resolve.json
```

```text
landed e1b27d16… on main (onto 31528945…, attempt 1)
```

[Commit and land](/docs/guides/commit-and-land) is the guide.

## Workspaces: build and test on disk, land the tested tree

`so ws create` writes a changeset's tree, or the paths under `--prefix`, into a directory of its
own; the agent edits, builds and tests there. `ws land --check` commits the edits, rebases them
onto the target's head, runs the check in the workspace and publishes only if the target has not
moved since, so the landed tree is the one the check ran on; a moved target means rebase and
check again. A command run inside a workspace finds its store. In a store of its own:

```sh
so --as agent/worker-1 --human ws create worker-1 --all
cd worker-1 && printf 'pub fn f() -> u32 {\n    4\n}\n' > src/lib.rs
so --human ws status
so --human ws land -m "f returns 4" --check -- ./check.sh
```

```text
name: worker-1
path: …/worker-1
base: df17fa1a…
head: df17fa1a…
all: true
created: true
…
changes[0].path: src/lib.rs
changes[0].kind: modified
changes[0].size: 28
truncated: false
land.status: landed
land.landed: 0bb07898…
land.rebased_onto: df17fa1a…
…
attempts: 1
…
```

Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks (`so ws hook config` prints the
settings) give `claude --worktree` sessions and `isolation: worktree` subagents a workspace
instead of a git worktree, and MCP has twelve workspace tools.
[Build and test a change](/docs/operate/build-and-test) is the guide.

## One JSON envelope per command

Every command prints one JSON envelope on stdout (the few exceptions are under
[The envelope](/docs/develop/cli-scripting#the-envelope)): `ok`, `op`, `api`, then `result` or `error`,
then `meta`. A failure carries a stable `E_*` code, the exit code, whether running it
again can help (`retryable`), machine-readable `details` and a `hint`, often the next command to
run. `swarmops schema` prints the JSON Schema of every request and response type.

```sh
so cat main:src/lib.rs --lines 2-2
SWARMOPS_AGENT= so commit -m "who am I" --changes two.json; echo "exit $?"
```

```text
{"ok":true,"op":"read_file","api":1,"result":{"blob":"54fad640…","size":28,"kind":"file","encoding":"utf8","content":"    3\n","truncated":false,"offset":20,"lines":{"first":2,"last":2}},"meta":{"repo":"…","elapsed_ms":0,"resolved":{"main":"e1b27d16…"},"auth":"trust"}}
{"ok":false,"op":"commit","api":1,"error":{"code":"E_IDENTITY_REQUIRED","message":"this operation changes the store, so it needs the caller's identity","retryable":false,"exit":5,"details":{},"hint":"pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"},"meta":{"repo":"…","elapsed_ms":0,"resolved":{},"auth":"trust"}}
exit 5
```

[Exit codes](/docs/develop/cli-scripting#exit-codes) has the exit table, and the
[error reference](/docs/reference/errors) has every code.

## An MCP server for agents

`swarmops mcp` serves the store to an agent as an MCP server on stdin and stdout: 46 tools, one
per operation (34 store operations and 12 workspace operations), each taking the operation's
request and returning the envelope the CLI prints.
Register it with Claude Code, one identity per agent process:

```sh
claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- swarmops mcp --repo ~/.local/state/swarmops/repos/acme
claude mcp list
```

```text
Added stdio MCP server swarmops with command: swarmops mcp --repo … to local config
…
swarmops: swarmops mcp --repo … - ✔ Connected
```

[MCP integration](/docs/develop/mcp) covers a project's `.mcp.json`, the identity rules and
result sizes; [Claude Code](/docs/guides/claude-code) is the guide.

## Op log and undo

`so op log` lists the store's operations, newest first: every commit, publish and ref move is
one write transaction, numbered in commit order (`op`) and named by an `id` (`<op>:<crc>`).
`so op undo <id>` moves every ref that operation moved back to what it named before, all of them
or none, by compare-and-swap: if
another operation has moved one of them since, it refuses with `E_CAS_MISMATCH` and writes
nothing. Changesets are never removed, and an undo is an operation of its own, which another
undo takes back. `main` moves only forward: an undo that would take it off its first-parent
history, the undo of a land for one, refuses with `E_REF_PROTECTED` and writes nothing; to take
`main` back, land a change that reverts it. A `release/<name>` ref moves either way. Here an
operator tries to take back the resolved land above:

```sh
so --human op log -n 1
so --as operator/ada --human op undo 20:c41e9a07
```

```text
ops[0].op: 20
ops[0].id: 20:c41e9a07
ops[0].kind: land
…
ops[0].changesets[0].title: f returns 3
…
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 31528945…
ops[0].ref_moves[0].new: e1b27d16…
ops[0].ref_moves[0].actor: agent/worker-2
…
error: E_REF_PROTECTED: main moves only forward: e1b27d16… is not a first-parent ancestor of 31528945… (the undo of operation 20:c41e9a07), so nothing was written
details.changeset: e1b27d16…
details.id: 20:c41e9a07
details.name: main
details.op: 20
details.target: 31528945…
hint: main moves only forward: land a change that takes it back
```

[History and undo](/docs/guides/history-and-undo) is the guide.

## Git import and export, round trip

`git import` brings a git repository's history into a store, and `git export` writes the store's
history into a git branch, so that people, CI and deployment keep working in git. A commit
imported from git exports back as itself, byte for byte; a changeset made in the store exports as
a commit with its agent as author and a `SwarmOps-Changeset:` trailer. Run again, each takes only
what is new. Here `~/src/shop` is a git work tree whose branch `stable` holds one commit:

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/shop
so init "$SWARMOPS_REPO" --name shop > /dev/null
so --human git import ~/src/shop --ref stable
echo '[{"path": "NOTES.md", "op": "put", "content": {"inline": "Landed by an agent.\n"}}]' > notes.json
so --human commit -m "Add notes" --changes notes.json --advance main
so --human git export ~/src/shop --branch review
git -C ~/src/shop log --format='%h %an%n    %s%n    %(trailers:only)' review
so --human git export ~/src/shop --branch review
```

```text
imported: 1
skipped: 0
head_changeset: 71d51aca…
git_head_sha: 52ab5e6e…
target_ref: main
ref_moved: true
lossy: []
lossy_total: 0
changeset: b5097858…
parent: 71d51aca…
…
exported 1 commit to review (none → 416ef535…)
…
416ef53 agent/worker-1
    Add notes
    SwarmOps-Changeset: b5097858…

52ab5e6 Ada
    Start the shop

review is up to date at 416ef535…
…
```

[The git round trip](/docs/guides/git-round-trip) is the guide.

## GitHub as a remote: clone, push and sync

A store can reach a git remote itself: `clone` makes a store from a repository, `push` sends a
store ref to a branch by compare-and-swap, never by force, and `sync` lands the store's unpublished
changes on top of the remote's head and pushes them, recognising the changes a pull request merged
by squash or rebase already brought. SwarmOps runs your own git (2.39 or later) for the transfer,
with your credentials, and keeps the remote's commits in a git mirror inside the store:

```sh
so --human clone git@github.com:acme/shop.git --into "$SWARMOPS_REPO"
so --human commit -m "Add notes" --changes notes.json --advance main
so --human push origin main:review
so --human push origin main:review
```

```text
…
remote.url: git@github.com:acme/shop.git
remote.branch: main
fetch.head.sha: aa3481f6…
imported: 1
main: a7558e3b…
changeset: 48e0853b…
…
branch: review
status: created
new: ba4ab051…
objects_written: 3
rewritten: 0
branch: review
status: up_to_date
…
```

The second push finds nothing to send and runs no git. [GitHub as a remote](/docs/guides/github-remote)
is the guide, and the operations guide's [daily loop](/docs/operations#the-daily-loop) runs it
with review and CI.

## Crash safety and verify

Agents get killed mid-write. Durability is full fsync, with no fast mode: an object is durable
before any metadata names it, and a commit returns only once its journal frame is on disk. The
writer lock is an `flock`, which the kernel releases when its holder dies, so a crash leaves no
stale lock, and the next writer repairs whatever a crash cut short (a commit's or a ref move's
`meta.recovered` says what it repaired). Crash sweeps, injected I/O errors, simulated power
losses and a literal `SIGKILL` test this through the real binary. `verify --deep` re-hashes every
object and checks the derived values against the objects:

```sh
so --human verify --deep
```

```text
objects_checked: 113
changesets_checked: 21
problems: []
findings: []
truncated: false
found: 0
```

`repair` rebuilds what the journal can rebuild (`journal.off`, the index, the ref files) and sets
damaged objects aside for a backup or `blob put` to restore; it never cuts the journal. `gc`
removes the leftovers of crashed writes and deletes no object. A store is one directory; [the architecture overview](/docs/architecture#the-store-on-disk) lists its
files, and the operations guide says how to
[check, back up and restore](/docs/operations#checking-a-store) one.

The store as a machine: writers wait for one lock, which the kernel hands to one of them at a
time, in no strict order, and each commit becomes one CRC-framed frame of the journal, while
objects need no lock ([interactive version](/explainers/store)).

## Every command

The [CLI reference](/docs/reference/cli) lists every command, its arguments and its output, and
the [error reference](/docs/reference/errors) every `E_*` code, its exit code and what to do.

## What it does not do

The [non-goals](/docs/goals#non-goals) say what SwarmOps does not set out to do, and the
[known limitations](/docs/known-limitations) list every open item with the milestone that takes it.
