# Parallel coding agents on one repository

Run several coding agents, such as Claude Code sessions, on one codebase and they collide in the
working tree, in git's locks and at merge time. This guide shows each failure with git 2.43, what
git worktrees fix and what they leave open, how SwarmOps, a git replacement for coding-agent swarms,
handles the same three changes, and where worktrees are still the better tool.

**Pre-1.0** · Linux today; macOS and Windows next · MIT licence

> ## In short
>
> 1. **A shared checkout breaks first.** Agents in one working tree share one HEAD, one index and
>    one set of files. One agent's `git add -A` commits another's unfinished edit, a second git
>    command fails while the first holds `index.lock`, and a git process killed with `SIGKILL`
>    leaves that lock behind.
> 2. **A worktree per agent fixes the files, not the integration.** Each agent gets its own files,
>    HEAD and index. The branches still have to be merged into one trunk, one at a time, and a
>    conflict shows up only then.
> 3. **SwarmOps integrates inside the store.** Agents commit into one store without a checkout, refs
>    move only by compare-and-swap, and `land` merges an agent's change three-way onto `main`,
>    retries by itself when `main` moved, and returns a conflict as a JSON report.
> 4. **Worktrees still lead** on write throughput, a directory to build and test in, branches and
>    tags, review tooling, platforms and maturity.
> 5. **Measured:** 32 processes committing 1,000 times each through the binary, with no `E_BUSY`, at
>    72.7 commits a second on one busy host. **Not measured:** any comparison with git, and landing
>    under a swarm's load.

## What breaks when agents share one checkout

A working tree is one set of files with one HEAD and one index. The git manual says a linked
worktree shares everything with its repository “except per-worktree files such as HEAD, index,
etc.”, so agents that work in the same tree share those too. Four things go wrong. The runs below
use git 2.43.0.

### One agent commits another's unfinished work

The index is shared, so `git add -A` stages whatever is in the tree, whoever wrote it, and the next
commit takes it. Here agent A has left `src/lib.rs` half-written while agent B edits the README and
commits.

**git 2.43: one checkout, two agents**

```text
# Agent A leaves a half-written edit in the shared tree.
$ printf 'pub fn f() -> u32 {\n    todo!()\n}\n' > src/lib.rs
# Agent B finishes its README change and commits everything.
$ printf '# acme\n\nA small crate.\n' > README.md
$ git add -A && git commit -q -m "describe the crate"
$ git show --stat --format=%s HEAD
describe the crate

 README.md  | 2 ++
 src/lib.rs | 2 +-
 2 files changed, 3 insertions(+), 1 deletion(-)
```

B's commit holds A's `todo!()`.

### Git commands collide on `index.lock`

git holds `.git/index.lock` while it rewrites the index. A second command that needs the index does
not wait; it fails. The slow pre-commit hook below only widens the window.

**git 2.43: two commands, one index**

```text
# Agent A commits a.txt; its pre-commit hook takes 4 s, so .git/index.lock stays held.
$ git commit -m "A" a.txt &
# A second later, agent B stages its own file.
$ git add b.txt
fatal: Unable to create '…/.git/index.lock': File exists.

Another git process seems to be running in this repository, e.g.
an editor opened by 'git commit'. Please make sure all processes
are terminated then try again. If it still fails, a git process
may have crashed in this repository earlier:
remove the file manually to continue.
```

### A killed agent leaves the lock behind

Kill the first command in that run with `SIGKILL` and the lock file stays. Every later command that
needs the index fails with the same error until someone removes the file by hand. git removes its
lock files on the signals it can catch; `SIGKILL` is not one of them.

### Files and HEAD are shared too

Two agents that write the same file leave the last writer's version, and nothing is merged. A
`git switch` by one agent changes the files under all the others, and their next commit goes to the
new branch.

Claude Code's documentation names the usual workaround: “Agent teams don't isolate teammates in
worktrees, so partition the work so each teammate owns a different set of files.” Partitioning stops
two agents writing the same file. The shared index, lock and HEAD remain, and a partition only holds
while the agents keep to it. Worktrees give each agent its own.

## What git worktrees fix, and what they leave open

`git worktree add` gives each agent a linked worktree: its own directory, HEAD and index, over the
repository's shared objects and refs. Claude Code builds on it. `claude --worktree <name>` creates a
worktree under `.claude/worktrees/<name>/` by default, on a new branch named `worktree-<name>`, and
a subagent runs in a worktree of its own when its definition says `isolation: worktree`.
Claude Code's documentation says worktrees “isolate file edits”. They do, and the four failures
above stop. Four things remain.

Take three agents that start from the same `main`. Agent 1 changes `f` in `src/lib.rs` to return 2,
agent 2 adds a line to the README, and agent 3 changes the same line of `f` to return 3. The
SwarmOps section below runs the same three changes.

### One branch each, and a branch lives in one worktree

By default git refuses to check a branch out in two worktrees, so each agent works on a branch of
its own. The work still has to get from three branches onto one trunk.

**git 2.43: a worktree for each agent**

```text
# Each agent gets a worktree and a branch of its own (output omitted).
$ git worktree add ../agent-1 -b agent-1/f-returns-2
$ git worktree add ../agent-2 -b agent-2/describe
$ git worktree add ../agent-3 -b agent-3/f-returns-3
# A fourth worktree on main, which the main checkout already has:
$ git worktree add ../agent-4 main
Preparing worktree (checking out 'main')
fatal: 'main' is already used by worktree at '…/acme'
```

### Merging is a serial step, and conflicts show up late

Merged into `main` one at a time, agent 1's branch fast-forwards and agent 2's merges cleanly. Agent
3's stops with a conflict, after all three agents have finished, and leaves the checkout where
`main` lives half-merged, with conflict markers in the file. Whoever owns that checkout resolves it.

**git 2.43: merging the three branches into main**

```text
# Each agent commits in its own directory. In the main checkout, one branch at a time:
$ git merge --no-edit agent-1/f-returns-2     # fast-forwards
$ git merge --no-edit agent-2/describe        # merges cleanly
$ git merge --no-edit agent-3/f-returns-3
Auto-merging src/lib.rs
CONFLICT (content): Merge conflict in src/lib.rs
Automatic merge failed; fix conflicts and then commit the result.
$ git status --short
UU src/lib.rs
$ head -6 src/lib.rs
pub fn f() -> u32 {
<<<<<<< HEAD
    2
=======
    3
>>>>>>> agent-3/f-returns-3
```

`git merge-tree --write-tree` runs the same merge without reading or writing a working tree or
index, and exits 1 on a conflict, so a script can look for conflicts between branches before anyone
merges. Resolving one without a checkout then takes plumbing: `git hash-object`, a temporary index,
`git commit-tree`.

### A moved trunk sends the pusher back

Agents that push to a shared trunk meet the next problem when the trunk moves: the second push is
refused with `[rejected] main -> main (fetch first)`. The fetch, the merge and the retry are left to
the pusher.

### Every worktree is a fresh checkout

Claude Code's documentation: “A worktree is a fresh checkout, so initialize your development
environment there”. Each agent's worktree needs its own setup.

## What SwarmOps does: one store, no checkout

SwarmOps is a git replacement for coding-agent swarms. It is designed for 10 to 50 Claude Code or
LangGraph processes on one host, committing to one repository at once, and it is written in Rust and
fully embedded: one program, no external database, no daemon. Agents do not check files out of it.
They read a path at a revision, `so cat main:src/lib.rs`, and write a commit from a JSON list of
whole-file changes. (`so` is the short name of `swarmops`: the same program.)

### Many writers, one history

Object writes take no lock. Metadata writes queue in the kernel for one lock, an `flock` that the
kernel releases when its holder dies, so a killed agent leaves no stale lock. Refs move only by
compare-and-swap. A commit returns only once its journal frame is on disk, and the next writer
repairs whatever a crash cut short. [The store as a machine](https://swarmops.cc/explainers/store)
draws it.

### Landing: a three-way merge without a checkout

An agent commits its change on the head it read: `so commit … --parent <the id main held>` records
the change and moves no ref. `so land <changeset> --key <key>` then merges it three-way onto the
head of `main`, whatever `main` gained since, and moves `main` to the result in one transaction, as
one new changeset whose parent is that head. A land that loses a race to another publish merges
again and retries by itself, up to 8 attempts unless told otherwise (`--max-attempts`, at most 32),
and then fails with the retryable `E_CAS_RETRY_EXHAUSTED`. A land run again with its key replays
instead of landing twice.

Here are the three changes from the worktree section. A change list is a JSON array of whole-file
puts and deletes, and `--as` names the agent that makes the change. In the transcripts, ids are cut
to four characters (commands take the full id, or a prefix of 12 or more hex digits), keys keep the
order the binary printed them, and `…` marks what was cut.

**SwarmOps: three agents commit, two land**

```text
# Three agents read main (1348…) and each commits a change on that parent. No ref moves.
$ cat a.json
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() -> u32 {\n    2\n}\n"}}]
$ so commit -m "f returns 2" --changes a.json --parent 1348… --as agent/worker-1
{"ok":true,"op":"commit",…,"result":{"changeset":"f1f3…","parent":"1348…",…},…}
$ so commit -m "describe the crate" --changes b.json --parent 1348… --as agent/worker-2
$ so commit -m "f returns 3" --changes c.json --parent 1348… --as agent/worker-3

# Each agent then lands its changeset on main.
$ so land f1f3… --key f-returns-2 --as agent/worker-1
{"ok":true,"op":"land",…,
  "result":{"status":"landed","landed":"894b…","rebased_onto":"1348…",…,"attempts":1,…},…}
$ so land e7ee… --key describe-the-crate --as agent/worker-2
{"ok":true,"op":"land",…,
  "result":{"status":"landed","landed":"5f6a…","rebased_onto":"894b…",…,"attempts":1,…},…}
```

Agent 2 changed a different file, so its change merged onto the head that agent 1 had just
published: `rebased_onto` is agent 1's landed changeset. `main` stays linear, each land one
changeset whose parent is the previous head.

### Merge conflicts come back as data

Agent 3 changed the same line agent 1 changed. Its land fails with `E_CONFLICT` (exit 3) and records
nothing. The report in `details.report` gives each conflicting file's kind, its hunks with each
side's lines, numbered as `cat --lines` takes them, and the changeset and agent that last changed
the path on `main`. In it, `ours` is the agent's change and `theirs` is what `main` holds.

**SwarmOps: a conflict, then its resolution**

```text
$ so land cd32… --key f-returns-3 --as agent/worker-3
{"ok":false,"op":"land",…,
  "error":{"code":"E_CONFLICT",…,"retryable":false,"exit":3,
    "details":{"report":{"base":"1348…",
      "files":[{…,
        "hunks":[{"base":{…,"text":"    1\n"},"id":0,"ours":{…,"text":"    3\n"},
          "theirs":{…,"text":"    2\n"}}],
        "kind":"content",…,"path":"src/lib.rs","theirs_agent":"agent/worker-1",…}],
      …,"ours":"cd32…","theirs":"5f6a…",…}},
    "hint":"swarmops cat 5f6a…:src/lib.rs, then swarmops land cd32… --resolve <file> …"},
  …}

# Nothing was recorded. The agent reads the other side, decides, and lands again.
$ cat resolve.json
{"against": "5f6a…", "files": {"src/lib.rs": {"take": "ours"}}}
$ so land cd32… --key f-returns-3 --resolve resolve.json --as agent/worker-3
{"ok":true,"op":"land",…,
  "result":{"status":"landed","landed":"dfd9…","rebased_onto":"5f6a…",…,"attempts":1,…},…}
```

`land --resolve` settles paths without a checkout: take a side, give new content, or delete, against
the report's `theirs`. A path that `main` changed since the report comes back as stale. As above,
git can find the same conflict without a checkout; the README rates conflicts as parity with
`git merge-tree --write-tree` and notes that resolving without a checkout is one request here, where
git takes plumbing.

### Compare-and-swap, not overwrite

Publishing without `land` is refused when the ref has moved. The error carries the ref's actual
target, says that nothing was recorded, and is marked retryable.

**SwarmOps: a publish from a stale parent**

```text
$ so commit -m "notes" --changes d.json --parent 1348… --advance main --as agent/worker-4
{"ok":false,"op":"commit",…,
  "error":{"code":"E_CAS_MISMATCH",…,"retryable":true,"exit":4,
    "details":{"actual":"dfd9…","committed":false,"name":"main"},
    "hint":"swarmops show dfd9…"},
  …}
```

Re-sending old whole-file puts on top of the new head would revert the winner's edits to shared
files. The usual way forward is `land`: commit on the head you read and let the merge do the rest.

### Who did what, and undo

Every write names its agent, `<family>/<instance>` such as `agent/worker-1`. The changeset records
it, and the journal records who moved each ref. `so op log` lists the store's operations, newest
first, each with its `id`. `so op undo <id>` moves every ref that operation moved back to what it
named before, all of them or none, by compare-and-swap, and refuses if another operation has moved
one since.

### Claude Code and MCP

`swarmops mcp` serves 35 operations to an agent as MCP tools, among them `read_file`, `commit`,
`land` and the eleven workspace tools. Register it with Claude Code:
`claude mcp add swarmops -- so mcp --repo <store> --as <agent>`. By default Claude Code starts one
server per session and the session's subagents share it, so their commits are recorded for the
session's agent. An agent that needs an identity of its own runs as its own Claude Code process,
with its own `SWARMOPS_AGENT`. [Use SwarmOps from Claude Code over MCP](claude-code.md) covers the
registration, one identity for each agent process and the two rules agents follow; the home page has
[the MCP section](https://swarmops.cc/#mcp).

### Workspaces to build and test in

`so ws create` writes a changeset's tree, or the paths under chosen prefixes, into a directory of
its own, where an agent builds and tests with the project's tools. `so ws land --check` rebases the
workspace onto `main`, runs the check there and publishes only if `main` has not moved since, so
the landed tree is the one the check ran on. Claude Code's `WorktreeCreate` and `WorktreeRemove`
hooks make `claude --worktree` sessions and `isolation: worktree` subagents use a workspace in
place of a git worktree ([Workspaces for worktree sessions and subagents](claude-code.md#workspaces-for-worktree-sessions-and-subagents),
[Build and test a change](../operate/build-and-test.md)).

### Git at both ends

`so git import` brings a git repository's history into a store, and `so git export` writes the
store's history back to a git branch, so work landed in SwarmOps goes back to git and today's
deployment pipelines keep running. Where git still leads is listed next.

## Where git worktrees still lead

These are the dimensions where SwarmOps stands behind git today. Most come from the README's own
comparison with git; the platforms row follows from its platform policy.

| Dimension | Git worktrees | SwarmOps today |
|---|---|---|
| **Write throughput** | Agents serialize nothing, and git's defaults sync none of a commit's files. | Writers take turns: each commit holds one store-wide lock across two journal syncs, and full fsync is the only mode. A commit with no contention takes about 21 ms; 32 processes managed 72.7 commits a second on one busy host. No git number exists to set beside them. |
| **A directory to build and test in** | Every worktree is a checkout of its own. GitButler's documentation recommends worktrees “when agents need incompatible checkout states, separate runtime state, or competing attempts at the same task”. | Since M5, workspaces: a sparse copy of a changeset's tree on disk, with status, commit, rebase and a checked land. Not yet measured: the status of a 50,000-file workspace, and checked lands under a busy `main`, where a check of minutes may have to rebase and run again. |
| **Refs** | Multi-ref transactions, tags, free-form branch names and symbolic refs. | `main` and `release/<name>` only, moved one per call. |
| **Reading** | Batch reads, `git grep <tree>` and sparse checkouts. | One file or one directory per call, with a byte offset or a range of lines for a file and a depth for a directory; `blob get` reads blobs by id in batches, and a workspace puts the files on disk. |
| **Storage on disk** | Packs. | One file per object, zstd-compressed when that makes it smaller; no packs, no delta compression. |
| **Review and the ecosystem** | Patch-review tools, with Gerrit and GitHub adding review on top; hooks, signing, submodules and LFS. | No user hooks, signing, submodules or LFS. Review, with change requests, approvals and a landing gate, is planned for M7. |
| **Between hosts** | Clone, fetch and push over any transport, and bundles without one. | One host. Between hosts, a git remote carries the history, one writing host at a time. Offline-first sync is planned for M11. |
| **Platforms** | Linux, macOS and Windows. | Linux today; macOS and Windows next. A store must live on a native Linux filesystem. |
| **Maturity** | Git has hosted itself since 2005. | Pre-1.0; the first commit is dated 2026-10-15. |

### Where SwarmOps is level or ahead, by design

The README labels each verdict. “Ahead by design” means judged from the design and the code, never
measured against git, and no git baseline has been measured.

- **Lost commits and lock failures: level.** Concurrent commits all succeed, short of a lock wait
  past 10 s or a snapshot that meets a new commit in each of its 32 rounds (the retryable `E_BUSY`).
  The README rates this parity with agents in separate worktrees.
- **Stale locks: ahead by design.** The writer lock is an `flock`, which the kernel releases when
  its holder dies.
- **Conflicts: level.** `land` merges without a checkout and reports typed conflicts, as
  `git merge-tree --write-tree` does.
- **Landing: ahead by design** of git alone, whose push refuses a moved branch and leaves the fetch,
  the merge and the retry to the pusher. Not yet measured under a swarm's load.
- **Errors: ahead by design.** A code, an exit code, a retryable flag, details and a hint, where git
  reports free text on stderr. git's plumbing and `--porcelain` formats narrow the gap for output.

## Which to use

Use git worktrees, or `claude --worktree`, when

- a few agents work on long tasks that a person reviews and merges;
- you need tags, many branches, review tools or hooks, or you work on macOS or Windows today.

With worktrees, a few habits keep the integration step small:

- start each agent on a branch and worktree of its own:
  `git worktree add ../agent-1 -b agent-1/<task>`, or `claude --worktree <name>`;
- set each worktree up, since it is a fresh checkout: install dependencies there, and list
  gitignored files such as `.env` in a `.worktreeinclude` file, which Claude Code copies into each
  worktree it creates;
- look for conflicts before anyone merges: `git merge-tree --write-tree` on two branches exits 1
  when they conflict;
- merge one branch at a time, check the next against the new `main`, and remove a finished worktree
  with `git worktree remove`.

Consider SwarmOps when

- many agent processes, 10 to 50 by design, commit to one trunk on one host
  (Linux today; macOS and Windows next), and the time goes into git locks, overwritten work and
  merge-and-retry loops;
- you want conflicts and errors as data an agent can act on: an error code that does not change, a
  retryable flag, a hint and a conflict report;
- agents must build and test their changes on disk before they land them: a workspace each, and
  `ws land --check` lands the tree the check ran on;
- you accept what the table says: pre-1.0, and a landing that is built but not yet measured under a
  swarm's load.

The two are not exclusive. `git import` and `git export` move history between a store and a git
repository, and [Build and test a change](../operate/build-and-test.md) also has an agent build
and test in a git worktree checked out from an export, then land exactly the tree it tested.

## What has been measured, and what has not

The measurement: 32 processes committing 1,000 times each through the `swarmops` binary made 32,000
commits in 440 s, which is 72.7 commits a second, with no `E_BUSY`, the retryable error for a lock
wait past 10 s. The median commit took 427 ms, an estimated 406 ms of it waiting for the lock, and
the slowest took 918 ms. It ran on one busy development host (WSL2, ext4 on a virtual disk, 32
hardware threads, load average 14.6 to 16.4). The project calls such numbers “goals and
measurements, not guarantees”.

Not measured:

- **A comparison with git.** No git baseline has been measured, and a side-by-side benchmark against
  git worktrees is not on the roadmap. The verdicts on this page come from the design, the code and
  git's own documentation and source.
- **Landing under a swarm's load.** `land` is built. Landing by 32 and 50 processes through the
  binary, and the landing model those numbers choose, come after a pilot on a real repository. A
  first probe through the binary, on a host under load and so only indicative, had 8, 16 and 32
  processes land a few changes each on one `main`: every change landed exactly once, 22 to 44 % of
  the lands retried, and with 32 processes four of the 96 lands took all 8 attempts. An agent that
  meets `E_CAS_RETRY_EXHAUSTED` lands again with the same key, or asks for more attempts.
- **Power loss on a real device.** Crash sweeps abort `init` and `commit` at every one of their
  filesystem calls, as `kill -9` would. Power loss is modelled, not tested on a device.

## Try it

Install SwarmOps, make a store and commit: [the quick start](https://swarmops.cc/#start) has the
commands. Then register the MCP server with Claude Code, one identity for each agent process, as
[the MCP section](https://swarmops.cc/#mcp) shows. The code, the architecture overview and the list
of known limitations are on [GitHub](https://github.com/KeiaiLab/SwarmOps).

## Sources

Facts about SwarmOps come from its repository at milestone M8, October 2026. The runs above used git
2.43.0 and a binary built from that code. Quotes are from the pages listed here.

- git: [git-worktree](https://git-scm.com/docs/git-worktree),
  [git-merge-tree](https://git-scm.com/docs/git-merge-tree) and
  [installing git](https://git-scm.com/install/).
- Claude Code: [Run parallel sessions with worktrees](https://code.claude.com/docs/en/worktrees),
  [Run agents in parallel](https://code.claude.com/docs/en/agents) and
  [Create custom subagents](https://code.claude.com/docs/en/sub-agents).
- GitButler: [Parallel agents](https://docs.gitbutler.com/ai-agents/parallel-agents).
- SwarmOps README: [SwarmOps and git](../../README.md#swarmops-and-git),
  [Quick start](../../README.md#quick-start), [MCP](../../README.md#mcp) and
  [Commands](../../README.md#commands).
- SwarmOps documents: [performance ledger](../perf.md),
  [concurrency](../architecture.md#concurrency), [known limitations](../known-limitations.md),
  [versioning](../policies/versioning-and-stability.md),
  [platforms](../policies/msrv-and-platforms.md) and
  [the operations guide](../operations.md#building-and-testing-a-change).
