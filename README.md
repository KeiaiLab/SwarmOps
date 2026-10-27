<p align="center">
  <img src="docs/assets/logo.svg" alt="The SwarmOps logo: four agents' branches merging into one main line" width="112" height="112">
</p>

<h1 align="center">SwarmOps</h1>

<p align="center">
  <strong>A git replacement for coding-agent swarms: many writers, one store, one JSON envelope per command.</strong>
</p>

<p align="center">
  <a href="https://github.com/KeiaiLab/SwarmOps/actions/workflows/ci.yml?query=branch%3Astable"><img src="https://github.com/KeiaiLab/SwarmOps/actions/workflows/ci.yml/badge.svg?branch=stable" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>
  <a href="docs/policies/msrv-and-platforms.md"><img src="https://img.shields.io/badge/rust-1.99-orange" alt="Rust 1.99"></a>
  <a href="CHANGELOG.md"><img src="https://img.shields.io/badge/status-pre--release-yellow" alt="Status: pre-release"></a>
</p>

<p align="center">
  <a href="https://swarmops.cc">Website</a> ·
  <a href="#core-features">Features</a> ·
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#documentation">Documentation</a> ·
  <a href="#swarmops-and-git">SwarmOps and git</a> ·
  <a href="#roadmap">Roadmap</a> ·
  <a href="#mcp">MCP</a> ·
  <a href="#commands">Commands</a>
</p>

SwarmOps is source control for a swarm of coding agents: 10 to 50 Claude Code or LangGraph
processes on one host commit into one embedded store at once, and every command answers in one
JSON envelope. It is one Rust program, with no external database and no daemon, and it moves the
work to and from git, so that CI and deployments keep running.

It is pre-release, at milestone M6 (v0.4.0): usable by one agent, with a store that already takes
commits from many processes at once; merging and landing, which a swarm needs, are built, but
landing is not yet measured under a swarm's load; a store clones, fetches, pushes and syncs with
GitHub itself; and an agent builds and tests its change in a workspace on disk, then lands the
tree it tested ([roadmap](#roadmap)).

## Core features

The examples below ran one after another, with the binary built from this repository, against
the store that the [quick start](#quick-start) makes; the two git examples make stores of their
own, the remote one against a local repository served through a stand-in for ssh. In
the outputs, ids are cut to their first eight hex digits (`4a7bc757…`), a store's path to `…`,
and a `…` line stands for lines left out. `--human` prints a short text rendering of the
envelope instead of JSON.

### Many agents commit into one store at once

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
again and retried by itself. [Parallel agents](docs/guides/parallel-agents.md) compares this with
agents in git worktrees.

### Land: a three-way merge onto a ref, with retry and a conflict report

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
hint: swarmops cat 31528945…:src/lib.rs, then swarmops land 7436166c… --resolve <file> with each path's resolution and against = 31528945…
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

[Commit and land](docs/guides/commit-and-land.md) is the guide.

### Workspaces: build and test on disk, land the tested tree

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
instead of a git worktree, and MCP has eleven workspace tools.
[Build and test a change](docs/operate/build-and-test.md) is the guide.

### One JSON envelope per command

Every command prints one JSON envelope on stdout (the few exceptions are under
[The envelope](docs/develop/cli-scripting.md#the-envelope)): `ok`, `op`, `api`, then `result` or `error`,
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

[Exit codes](docs/develop/cli-scripting.md#exit-codes) has the exit table, and the
[error reference](docs/reference/errors.md) has every code.

### An MCP server for agents

`swarmops mcp` serves the store to an agent as an MCP server on stdin and stdout: 38 tools, one
per operation (27 store operations and 11 workspace operations), each taking the operation's
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

[MCP integration](docs/develop/mcp.md) covers a project's `.mcp.json`, the identity rules and
result sizes; [Claude Code](docs/guides/claude-code.md) is the guide.

<a id="first-commands"></a>

### Op log and undo

`so op log` lists the store's operations, newest first: every commit, publish and ref move is
one write transaction, numbered in commit order (`op`). `so op undo <op>` moves every ref that
operation moved back to what it named before, all of them or none, by compare-and-swap: if
another operation has moved one of them since, it refuses with `E_CAS_MISMATCH` and writes
nothing. Changesets are never removed, and an undo is an operation of its own, which another
undo takes back. Here an operator takes back the resolved land above:

```sh
so --human op log -n 1
so --as operator/ada --human op undo 20
so --human cat main:src/lib.rs --lines 2-2
```

```text
ops[0].op: 20
ops[0].kind: land
…
ops[0].changesets[0].title: f returns 3
…
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 31528945…
ops[0].ref_moves[0].new: e1b27d16…
ops[0].ref_moves[0].actor: agent/worker-2
…
undone: 20
op: 21
ref_moves[0].name: main
ref_moves[0].old: e1b27d16…
ref_moves[0].new: 31528945…
ref_moves[0].actor: operator/ada
ref_moves[0].at: 1792626687
already_applied: false
…
content: |
      2
…
```

[History and undo](docs/guides/history-and-undo.md) is the guide.

### Git import and export, round trip

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

[The git round trip](docs/guides/git-round-trip.md) is the guide.

### GitHub as a remote: clone, push and sync

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

The second push finds nothing to send and runs no git. [GitHub as a remote](docs/guides/github-remote.md)
is the guide, and the operations guide's [daily loop](docs/operations.md#the-daily-loop) runs it
with review and CI.

<a id="the-store-on-disk"></a>

### Crash safety and verify

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
removes the leftovers of crashed writes and deletes no object. A store is one directory; [architecture.md](docs/architecture.md#the-store-on-disk) lists its
files, and the operations guide says how to
[check, back up and restore](docs/operations.md#checking-a-store) one.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/store-dark.svg">
    <img src="docs/assets/store-light.svg" alt="An isometric drawing of the store as a machine: commit cartridges wait on a feed rail behind one gate, the metadata lock, which lets one through at a time in no strict order; a press stamps each one as a frame onto a paper tape, the append-only journal; beside it, object pellets drop into sixteen bins with no gate." width="760">
  </picture>
  <br>
  <sub>The store as a machine: writers wait for one lock, which the kernel hands to one of them at
  a time, in no strict order, and each commit becomes one CRC-framed frame of the journal, while
  objects need no lock.
  <a href="https://swarmops.cc/explainers/store">Interactive version</a>
  (<a href="https://swarmops.cc/explainers/store-light">light</a>), drawn with
  <a href="https://github.com/wheresryan22/anatomy">Anatomy</a> (MIT,
  <a href="web/public/THIRD-PARTY-NOTICES.txt">notice</a>).</sub>
</p>

## Install

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash
```

```text
swarmops-install: Installing SwarmOps 0.1.0 (x86_64-unknown-linux-gnu) into /home/me/.local/bin.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/v0.1.0/swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz
swarmops-install: swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz matches SHA256SUMS.
swarmops 0.1.0
Installed swarmops and so (its short form) in /home/me/.local/bin.
…
```

That installs `swarmops` and `so`, the same program by a shorter name, from the latest
[release](https://github.com/KeiaiLab/SwarmOps/releases), checked against its `SHA256SUMS`, into
`~/.local/bin` (`/usr/local/bin` as root; `/home/me` above stands for the home directory). Run
it again to update. `--version`, `--dir` and `--uninstall` go after `bash -s --`; the
[install guide](docs/guides/install.md) and the operations guide's
[Install](docs/operations.md#install) cover them, release archives and building from a clone.

Linux, x86_64 or aarch64, glibc 2.34 or newer (Ubuntu 22.04, Debian 12, RHEL 9). On glibc 2.34
to 2.38 every start prints one `ld.so` line on stderr, `weak version 'GLIBC_2.39' not found`;
exit codes, stdout and the MCP protocol are unaffected. The aarch64 build is cross-compiled and
smoke-tested under QEMU, not on aarch64 hardware. The installer needs no sudo for its default
directory, sends no telemetry and edits no shell startup file; `bash install.sh --help` lists
every option. From source, with Rust 1.99 (`rust-toolchain.toml` pins it):

```sh
cargo install --git https://github.com/KeiaiLab/SwarmOps --tag v0.1.0 --locked swarmops-cli
```

`cargo install` installs both names (add `--bin swarmops` for the long name alone, if the name
`so` is taken). A store must live on a local filesystem that the store supports (ext4 with a
journal, XFS or btrfs): a path under `/mnt/` (where WSL mounts Windows drives) is refused with
`E_UNSUPPORTED_FS`, and NFS, SMB and FUSE mounts, Docker Desktop's and virtiofs bind mounts among
them, are neither detected nor supported.

## Quick start

A store, a first commit and a read, in a minute. The commands use `so`; `swarmops` runs the same.
In the outputs, ids are cut to eight hex digits and `meta` to `{…}`.

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme   # or --repo <store> on each command
export SWARMOPS_AGENT=agent/worker-1                       # or --as <family>/<instance>
so init "$SWARMOPS_REPO" --name acme
cat > change.json <<'JSON'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() -> u32 {\n    1\n}\n"}}]
JSON
so commit -m "add f" --changes change.json --advance main
so cat main:src/lib.rs
so log
```

```text
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"5357e87f…","name":"acme","store_version":2,"changesets":0,"refs":0},"meta":{…}}
{"ok":true,"op":"commit","api":1,"result":{"changeset":"4a7bc757…","root":"ca354c25…","files_changed":1,"blobs_written":1,"advanced":{"name":"main","new":"4a7bc757…","already_applied":false},"replayed":false},"meta":{…}}
{"ok":true,"op":"read_file","api":1,"result":{"blob":"dfa55099…","size":28,"kind":"file","encoding":"utf8","content":"pub fn f() -> u32 {\n    1\n}\n","truncated":false},"meta":{…}}
{"ok":true,"op":"log","api":1,"result":{"entries":[{"id":"4a7bc757…","parents":[],"author_agent":"agent/worker-1","created_at":1792626681000,"title":"add f"}],"truncated":false},"meta":{…}}
```

`--advance main` made `main` name the new changeset in the commit's own transaction. Reads need
no identity; a commit, a land, a ref move and an undo need one, from `--as` or
`SWARMOPS_AGENT`. The [quick start guide](docs/guides/quick-start.md) goes further, and
[Core features](#core-features) shows landing, conflicts and undo.

## Documentation

The design documents are the authority: the umbrella spec and the storage, operations and review
specs, in Korean. The maintainers keep them privately, and
[docs/architecture.md](docs/architecture.md) is the public overview, in English.
[docs/README.md](docs/README.md) maps the documents by task.

| Document | What it holds |
| --- | --- |
| [Install](docs/guides/install.md) | The installer, release archives, building from source, upgrading and removing. |
| [Quick start](docs/guides/quick-start.md) | The first ten minutes: a store, commits, a land, a conflict, the op log. |
| [Concepts](docs/concepts.md) | What a store, a revision, a change list, a ref, a land and an envelope are. |
| [Commit and land](docs/guides/commit-and-land.md) | Publishing a change: commits, compare-and-swap, landing, retries and conflicts. |
| [History and undo](docs/guides/history-and-undo.md) | Reading history and diffs, and taking an operation back. |
| [Parallel agents](docs/guides/parallel-agents.md) | Many agents on one repository, against agents in git worktrees. |
| [Claude Code](docs/guides/claude-code.md) | Giving Claude Code agents the store through MCP. |
| [Git round trip](docs/guides/git-round-trip.md) | Importing a git repository and exporting the work back for CI. |
| [GitHub as a remote](docs/guides/github-remote.md) | Cloning, fetching, pushing and syncing with GitHub, pull requests, tokens and commit authors. |
| [docs/operations.md](docs/operations.md) | Running SwarmOps: install, a store, agents through MCP, the daily loop between the store and git, checks, backup and restore, upgrades, limits and what to monitor. |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Symptoms, their causes and the commands that fix them. |
| [CLI reference](docs/reference/cli.md) | Every command, its arguments and its output. |
| [Error reference](docs/reference/errors.md) | Every `E_*` code, its exit code and what to do. |
| [Goals](docs/goals.md) | What SwarmOps sets out to do, each goal with its metric, target, measured value and milestone. |
| [docs/architecture.md](docs/architecture.md) | The design in English: crates, data model, the store on disk, the write and read paths, durability, concurrency, the contract. |
| [docs/known-limitations.md](docs/known-limitations.md) | What is still open, and which milestone takes each item. |
| [docs/perf.md](docs/perf.md) | Every measurement, and the decisions taken on them. |
| [docs/pilot.md](docs/pilot.md) | The runbook of the first real use, on this repository: import, one agent session, export, CI, what to measure, the success criteria and the rollback. |
| [bench/compare/README.md](bench/compare/README.md) | The harness that measures the native metadata store against embedded databases. |
| [docs/policies/clean-room.md](docs/policies/clean-room.md) | How third-party designs reach the code: requirements only, with provenance and checks. |
| [docs/policies/versioning-and-stability.md](docs/policies/versioning-and-stability.md) | What stays compatible, and how a breaking change is made. |
| [docs/policies/msrv-and-platforms.md](docs/policies/msrv-and-platforms.md) | The Rust version and the platforms SwarmOps supports. |
| [SUPPORT.md](SUPPORT.md) | Where to ask a question or report a problem. |
| [MAINTAINERS.md](MAINTAINERS.md) | Who maintains SwarmOps, and how decisions are made. |
| [CHANGELOG.md](CHANGELOG.md) | What has changed, release by release. |
| [RELEASE.md](RELEASE.md) | The release checklist. |

## SwarmOps and git

SwarmOps is not meant as a general-purpose git replacement: it imports from git (M3) and exports
to it (M8). Where the two compare, this is where SwarmOps stands, judged strictly:

**Ahead**: built, with evidence · **Ahead by design**, **behind by design**: judged from the
design and the code, never measured against git · **Parity** · **Behind** · **Not yet
comparable**: not built yet. In the middle column, ✅ marks what is delivered and 📋 what is
planned, each with its milestone.

| Dimension | SwarmOps | Against git |
| --- | --- | --- |
| Hash | ✅ M0: BLAKE3-256 ids, with the object's kind and layout version hashed in. ✅ M1: every read re-hashes. | **Ahead** of git's default SHA-1: 128-bit collision resistance against at most 80 bits, by cryptanalysis, not by benchmark. **Parity** with git's opt-in SHA-256. |
| Order of writes | ✅ M1: every commit and ref move is a frame of one journal, appended under one lock, so all of them have a single global order. | **Ahead by design.** |
| Stale locks | ✅ M1: the writer lock is an `flock`, which the kernel releases when its holder dies. | **Ahead by design.** A git process killed with `SIGKILL` while it holds `index.lock` or a ref's `.lock` leaves the file behind, to be removed by hand (git removes its lock files on the signals it can catch). |
| Durability | ✅ M1: full fsync is the only mode; an object is synced before a journal frame names it, and a commit returns after its frame's `fdatasync`. ✅ M1.5: the policy for a failed fsync. | **Ahead by design.** Git's defaults (2.43) sync packs, pack metadata and the commit graph, not loose objects, refs or the index. |
| Machine-readable contract | ✅ M1: one JSON envelope per command, with a stable `E_*` code, a fixed exit code, `retryable`, `details` and a `hint`; a JSON Schema for every request and response. ✅ M1.6: an I/O failure names its operation and store file, and no host path but in two `E_IO`s (a symlink loop met while working out where the store's path leads, and tempfile's giving up after 65,536 taken temp-file names); a write reports what its recovery repaired (`meta.recovered`); `verify` lists typed findings. | **Ahead by design** for errors, which git reports as free text on stderr. Git's plumbing and `--porcelain` formats narrow the gap for output. Known gap: three codes are not raised yet. |
| Commits from many writers | ✅ M1: concurrent commits all succeed, short of a lock wait past 10 s or, since M1.6, a snapshot that meets a new commit in each of its 32 rounds (the retryable `E_BUSY`). | **Parity** with agents in separate git worktrees, on lost commits and lock failures. |
| One ref by compare-and-swap | ✅ M1: `ref set --expect` or `--create`, `ref delete --expect`; setting a ref to the target it holds is a replay. ✅ M1.6: `commit --advance` moves a ref to the new changeset in the commit's own transaction, or records nothing. | **Parity** with `git update-ref <ref> <new> <old>`, plus the replay. |
| Reading a path at a revision | ✅ M1: `cat REV:PATH` and `ls REV[:PATH]`, without a checkout. | **Parity** with `git show REV:PATH` and `git ls-tree`. |
| Offline, on one host | ✅ M1: every operation runs against the host's own store. ✅ M13: only `clone`, `fetch`, `push` and `sync` reach a remote, through the user's git; every other command works without a network. | **Parity.** |
| Throughput of many writers | ✅ M1: writers take turns, each commit holding one store-wide lock across two journal `fdatasync`s, and two `fsync`s more when it moves a ref (`--advance`). | **Behind by design.** Agents in git worktrees serialize nothing, and git's defaults sync none of a commit's files. |
| Refs beyond one compare-and-swap | ✅ M1: refs are `main` and `release/<name>`, moved one per call. ✅ M6: a `FORMAT 2` store also holds free-form branches and tags, which never move. | **Behind**: no multi-ref transactions or symbolic refs. |
| Breadth of reads | Today one file or one directory per call. ✅ M2: a file from a byte offset or by a range of lines, and a directory with the directories below it to a depth, each file with its size. ✅ M5: `blob get` reads blobs by id in batches, and sparse workspaces write a changeset's tree, or the paths under chosen prefixes, to disk. | **Behind**: git has `git grep <tree>`; **parity** with sparse checkouts for building and testing on disk. |
| Storage on disk | ✅ M1: one file per object, zstd-compressed when that makes it smaller; no packs, no delta compression. ✅ M6: `gc`, which removes leftovers but packs nothing. | **Behind by design**, in bytes and in files, against git's packs. |
| Conflicts | Since M3, `E_GIT_IMPORT_DIVERGED` signals a conflict when a git import's target and the git history have diverged. ✅ M4: `land` merges three-way without a checkout and reports a conflict as `E_CONFLICT` with a JSON report: each conflicting file's kind, its hunks (each side's lines, numbered as `cat --lines` takes them, and their text) and the target's changeset and agent that last changed it. ✅ M4: `land --resolve` resolves them without a checkout, path by path (take a side, new content, or delete), against the report's `theirs`; a path the target changed since comes back stale. | **Parity** with `git merge-tree --write-tree`, which merges without a checkout and reports typed conflicts. Resolving without a checkout is one request here, where git takes plumbing (`git hash-object`, a temporary index, `git commit-tree`). |
| Review | Nothing today. 📋 M7: change requests, their classification, approvals and a landing gate. | **Behind** today: git ships patch-review tools, and Gerrit and GitHub add review on top. M7 is **not yet comparable**. |
| Sync between hosts | ✅ M8: through a git remote, one writing host at a time, with `git import` and `git export`. ✅ M13: a store clones, fetches and pushes a git remote over HTTPS or ssh, by running the user's git; a push is a compare-and-swap, never forced, and `sync` lands the store's unpublished changes onto the remote's head, so several stores can write to one remote. 📋 M11: offline-first sync between stores, `so://`. | **Parity** with git's clone, fetch and push to a git remote over HTTPS and ssh, which SwarmOps runs; **behind** on transports, shallow and partial clones, and bundles. |
| Ecosystem | ✅ M1: the CLI and `swarmops schema`. ✅ M1.5: an MCP server. ✅ M3: git import. ✅ M8: git export. ✅ M13: git remotes, and GitHub's pull requests merged by any method. No user hooks, signing, submodules or LFS, and no GitHub API calls. | **Behind**, after v1 too: git parity is not a v1 goal, and git has MCP servers as well. |
| Maturity | Pre-release: v0.1.0 is the first tag, v0.4.0 the latest, and the first spec dates from 2026-10-15. | **Behind**: git has hosted itself since 2005. |
| Landing | ✅ M4: `land` merges an agent's change onto `main` and publishes it as one changeset in one transaction, merging again and retrying by itself when `main` moves, and a retry with the same key replays it. 📋 M4, after the pilot: the landing model chosen by measurement. | **Ahead by design** of git alone, whose push refuses a moved branch and leaves the fetch, the merge and the retry to the pusher; not yet measured under a swarm's load. |

No git baseline has been measured: these verdicts come from the design, the code, and git's own
documentation and source, not from a benchmark, and a side-by-side benchmark against git
worktrees is not on the roadmap (umbrella spec §17). SwarmOps' own measurements are in
[docs/perf.md](docs/perf.md).

SwarmOps' design has prior art in git, Mononoke's Bonsai changesets and pushrebase, and jj's
operation log. It adapts several principles that Anthropic described for its in-house system
Scion, in the engineering post
["Redesigning source control at Anthropic"](https://www.anthropic.com/engineering/redesigning-source-control),
to a single host. SwarmOps is an independent project, not affiliated with or endorsed by
Anthropic, and contains no Scion code, which is not public.

## Roadmap

SwarmOps' goal is version control for agent swarms that works together with GitHub, in three
topologies. [docs/roadmap.md](docs/roadmap.md) is the detailed roadmap: each topology's commands,
design, acceptance criteria and targets. Command names of planned work are provisional.

| Topology | Today | Planned |
| --- | --- | --- |
| 1. Local git, remote SwarmOps | Not served: a bare git repository that `git export` writes, whose pushes someone imports and lands | M14: `swarmops serve` over ssh and HTTP; stock `git clone`, `fetch`, `pull` and `push`; a push to `refs/land/main` lands |
| 2. Local SwarmOps, remote GitHub | ✅ M13: `so clone https://github.com/…`, `so fetch`, `so push`, `so sync`, with no hand-run git ([GitHub as a remote](docs/guides/github-remote.md)) | Signed commits; opening pull requests from SwarmOps |
| 3. Local SwarmOps, remote SwarmOps | Through a git remote, one writing host at a time (M8) | M11: `so clone so://host/…` and `so sync`, offline-first |

The milestones of the umbrella spec (§13), in the order they are built. ✅ delivered · 📋 planned.

| Status | Milestone | Scope |
| --- | --- | --- |
| ✅ | M0 | Bootstrap: the workspace and its gates, the canonical codec and its golden vectors, the fsync experiment. |
| ✅ | M1 | The single-agent core: `init`, `commit`, `cat`, `ls`, `log` and ref compare-and-swap, over the native metadata store and object files; crash sweeps. |
| ✅ | M1.5 | Hardening and opening: CI behind one `tests-pass` check, `--locked` everywhere; I/O-error injection, write-ordering checks and the fsync-failure policy; power-loss images; the project policies; the tests' shared support in one crate; benchmarks of the metadata store against embedded databases; the MCP server and `so`. |
| ✅ | M1.6 | The code-review follow-ups, before M2: the error contract an agent sees (typed refusals, I/O failures that name the store file and no host path but in two rare `E_IO`s, a report of what a write's recovery repaired); a bound on the lock-free reader's retries; structured `verify` findings, and MCP tool schemas kept plain objects; object reads without a second copy or an allocation of the size a file claims; the test doubles M1.5 left, in one place; spec hygiene (the write-ordering rules W1–W7, §8 in step with the code); the harness for the measurements M2's sync decision needs, whose quiet-window runs are still to come; and `commit --advance`, the commit primitive M4's landing reuses. |
| ✅ | M2 | Diff, history and the derived-data engine. |
| ✅ | M3 | git import. |
| ✅ | M4 | Merge and landing, and the journal shown as an operation log with undo. Landing is a compare-and-swap loop that merges again, incrementally, when the target moves; its measurement under a swarm's load, and the landing model that measurement chooses, come next. |
| ✅ | M8 | git export, moved ahead so that work landed in SwarmOps goes back to git and today's deployment pipelines keep running. |
| 📋 | M12 | Portability: macOS and Windows builds, tests and releases. |
| ✅ | M13 | Git remotes from a store (topology 2): the store's git mirror, the user's git run as the transport, `so clone`, `so remote`, `so fetch`, `so push`, `so sync`, `so sync status` and their MCP tools, commit addresses per agent family (`authors.toml`). |
| ✅ | M5 | Workspaces, with an adapter for Claude Code's worktree hooks, so that an agent builds and tests a change on disk before it lands it: `so ws`, `rebase`, `land --expect-target`, `blob get` and eleven MCP workspace tools. |
| ✅ | M6 | Verify, crash safety and fuzzing: `repair`, `gc`, detection of unsupported filesystems by `statfs`, bounded reads of store files, a capped `verify` report, a literal `SIGKILL` test, four more fuzz targets, and store `FORMAT 2`, with branch names, tags and remote-tracking refs, and `upgrade` to it. |
| 📋 | M14 | Git clients against a store (topology 1): `swarmops serve` over ssh and HTTP, run per connection; pushes by compare-and-swap or that land; access control. |
| 📋 | M7 | Blame and review. |
| 📋 | M9 | Simulation and a benchmark gate: the store's own code under a deterministic simulator and in real processes killed at random, and benchmarks that fail on a regression. |
| 📋 | M11 | Remote sync, offline-first (topology 3): a full store on every host, `so clone so://…`, `fetch`, `push`, `sync` and `sync status` against one upstream, over ssh. |

M0, M1, M1.5, M1.6, M2, M3, M4 and M8 make the first release, 0.1.0, together with the security
fixes in its [changelog](CHANGELOG.md), an [operations guide](docs/operations.md) and a
[pilot runbook](docs/pilot.md). The pilot runs SwarmOps' own repository through the round trip:
its history imported from git, changes committed and landed by agents through `swarmops mcp`,
and the result exported back to git for its CI to run. Next come the measurements that need a
quiet host: landing by 32 and 50 processes through the binary against umbrella §15's targets,
and the landing model those numbers choose; whether a transaction can sync the journal once
instead of twice; the import's and the export's timings on larger repositories. M13 came first, in 0.2.0, then
M5, in 0.3.0, both ahead of M12, which is developed separately, and M6 in 0.4.0.

The road to usable ([To usable](docs/roadmap.md#to-usable)): a real repository runs 8 or more
agents for 3 or more days with no lost commit, a clean `verify --deep`, nothing rewritten and an
export that passes `git fsck --strict` and the repository's own gate; G1, G2, G4 and G17 are
measured and met; a benchmark gate in CI fails a regression of more than 20 %; backup and restore,
`repair` and the rollback to git are exercised in that pilot; no open soft finding touches data
safety or the contract; two releases in a row ship no data-safety fix; and, for teams without
GitHub, M14. v0.5.0 runs the pilot and the measurements, v0.6.0 fixes what they miss, v0.7.0 is
M14 and v0.8.0 is operations. M7, M9's simulator, M11 and M12 wait until then.

When it is usable (umbrella spec §1): by one agent from M1; by a swarm, landing through MCP,
from M4; on an existing git repository, its history imported and its landed work exported back
so that its CI and deployments run as before, from M8, in the first release; by agents that
build and test their changes in a workspace before they land them, from M5; with GitHub as the
remote, without a hand-run `git push`, from M13; by people with stock git against a store, from
M14; with review from M7; as the complete v1 at M9; across hosts, offline-first, from M11.
[docs/known-limitations.md](docs/known-limitations.md) lists what is still open, and which
milestone takes each item.

## MCP

`swarmops mcp` serves the store's operations to an agent as an MCP server on stdin and stdout:
38 tools, one per operation (27 store operations and 11 workspace operations), each taking the
operation's request and returning the envelope the CLI prints. [MCP integration](docs/develop/mcp.md) is the reference,
[MCP tools](docs/reference/mcp-tools/README.md) has a page per tool, and
[Claude Code](docs/guides/claude-code.md) is the guide.

- <a id="register-the-server"></a>**Register the server**: `claude mcp add` for one agent, or a
  project's `.mcp.json` for every agent session that opens it
  ([Register the server with Claude Code](docs/develop/mcp.md#register-the-server-with-claude-code)).
- <a id="store-identity-and-lifetime"></a>**Store, identity and lifetime**: the store and the
  identity are the server's, never a call's, and the server ends with the store on any of its four
  documented aborts ([Identity](docs/develop/mcp.md#identity-one-per-agent-process),
  [The server exits](docs/develop/mcp.md#the-server-exits)).
- <a id="rules-for-agents"></a>**Rules for agents**: full changeset ids for every
  compare-and-swap, and small result pages ([The tools](docs/develop/mcp.md#the-tools),
  [Result size limits](docs/develop/mcp.md#result-size-limits)).
- <a id="when-the-server-does-not-start"></a>**When the server does not start**:
  [Failure modes](docs/develop/mcp.md#failure-modes).

## Commands

`swarmops` and `so` run the same commands. The [CLI reference](docs/reference/cli.md) has a page
for each: its syntax, its arguments, an example, what it returns and the errors it fails with.

| Task | Commands |
| --- | --- |
| Make and describe a store | [`init`](docs/reference/commands/init.md) · [`info`](docs/reference/commands/info.md) · [`whoami`](docs/reference/commands/whoami.md) |
| Publish a change | [`commit`](docs/reference/commands/commit.md) · [`land`](docs/reference/commands/land.md) · [`rebase`](docs/reference/commands/rebase.md) · [`blob put`](docs/reference/commands/blob-put.md) · [`blob has`](docs/reference/commands/blob-has.md) · [`blob get`](docs/reference/commands/blob-get.md) |
| Read | [`resolve`](docs/reference/commands/resolve.md) · [`cat`](docs/reference/commands/cat.md) · [`ls`](docs/reference/commands/ls.md) · [`show`](docs/reference/commands/show.md) · [`log`](docs/reference/commands/log.md) · [`history`](docs/reference/commands/history.md) · [`diff`](docs/reference/commands/diff.md) |
| Move refs | [`ref list`](docs/reference/commands/ref-list.md) · [`ref set`](docs/reference/commands/ref-set.md) · [`ref delete`](docs/reference/commands/ref-delete.md) |
| Take an operation back | [`op log`](docs/reference/commands/op-log.md) · [`op undo`](docs/reference/commands/op-undo.md) |
| Check, repair and derive | [`verify`](docs/reference/commands/verify.md) · [`repair`](docs/reference/commands/repair.md) · [`gc`](docs/reference/commands/gc.md) · [`upgrade`](docs/reference/commands/upgrade.md) · [`derive`](docs/reference/commands/derive.md) · [`schema`](docs/reference/commands/schema.md) |
| Move work to and from git | [`git import`](docs/reference/commands/git-import.md) · [`git export`](docs/reference/commands/git-export.md) |
| Work with a git remote | [`clone`](docs/reference/commands/clone.md) · [`remote add`](docs/reference/commands/remote-add.md) · [`remote remove`](docs/reference/commands/remote-remove.md) · [`remote list`](docs/reference/commands/remote-list.md) · [`fetch`](docs/reference/commands/fetch.md) · [`push`](docs/reference/commands/push.md) · [`sync`](docs/reference/commands/sync.md) · [`sync status`](docs/reference/commands/sync-status.md) |
| Build and test in a workspace | [`ws create`](docs/reference/commands/ws-create.md) · [`ws status`](docs/reference/commands/ws-status.md) · [`ws diff`](docs/reference/commands/ws-diff.md) · [`ws commit`](docs/reference/commands/ws-commit.md) · [`ws rebase`](docs/reference/commands/ws-rebase.md) · [`ws resolve`](docs/reference/commands/ws-resolve.md) · [`ws land`](docs/reference/commands/ws-land.md) · [`ws materialize`](docs/reference/commands/ws-materialize.md) · [`ws discard`](docs/reference/commands/ws-discard.md) · [`ws list`](docs/reference/commands/ws-list.md) · [`ws info`](docs/reference/commands/ws-info.md) · [`ws delete`](docs/reference/commands/ws-delete.md) · [`ws hook`](docs/reference/commands/ws-hook.md) |
| Serve agents | [`mcp`](docs/reference/commands/mcp.md) |

- <a id="publishing-a-change"></a>**Publishing a change**: commit on the head you read, then
  land it ([Commit and land](docs/guides/commit-and-land.md)).
- <a id="revisions-and-change-lists"></a>**Revisions and change lists**:
  [Revision](docs/concepts.md#revision), [Change list](docs/concepts.md#change-list) and
  [Write a change list](docs/guides/commit-and-land.md#write-a-change-list).
- <a id="bring-a-git-repository-in"></a>**Bring a git repository in**:
  [`git import`](docs/reference/commands/git-import.md) and [The git round trip](docs/guides/git-round-trip.md).
- <a id="send-the-work-back-to-git"></a>**Send the work back to git**:
  [`git export`](docs/reference/commands/git-export.md) and [The git round trip](docs/guides/git-round-trip.md).
- <a id="output-and-exit-codes"></a>**Output and exit codes**:
  [The envelope](docs/develop/cli-scripting.md#the-envelope),
  [Output flags](docs/develop/cli-scripting.md#output-flags),
  [Exit codes](docs/develop/cli-scripting.md#exit-codes) and the
  [error reference](docs/reference/errors.md).

## Development

[CONTRIBUTING.md](CONTRIBUTING.md) has the workflow and every `just` recipe. `just check` is the
definition of done: fmt, clippy `-D warnings`, the tests, the docs and cargo-deny;
`scripts/check.sh` runs its steps on a machine without `just`. Tests never need the network;
those that read real repositories on this host run only with `SWARMOPS_HOST_REPOS=1`.

### The `test-hooks` feature

The crash and fault tests drive six test-only environment variables:
`SWARMOPS_TEST_CRASH_AT=<k>` aborts the process at its k-th store filesystem call,
`SWARMOPS_TEST_FS_CALLS=<file>` makes it write how many calls it made,
`SWARMOPS_TEST_FAIL_AT=<k>[:once|persistent[:eio|enospc]]` fails its k-th call with an injected
I/O error (and with `persistent` every mutating call after it), `SWARMOPS_TEST_ORDER_CHECK=1`
checks every write against the store's write-ordering rules and panics at one that breaks a rule,
`SWARMOPS_TEST_NOW=<unix seconds>` fixes the time it signs changesets at, and
`SWARMOPS_TEST_GITHUB_HOSTS=<host[:port],…>` makes a push apply GitHub's limits to those hosts.
They act only in a build with the cargo feature `test-hooks` (on `swarmops-store`,
`swarmops-repo`, `swarmops-git`, `swarmops-ops`, `swarmops-cli` and `swarmops-testkit`, the tests'
support; `swarmops-cli/test-hooks` turns on all of them). The gates build the
tests with `--all-features`; a
plain `cargo test` skips the tests that
need the hooks. Every other build, `cargo build --release` included, ignores the variables, and
`just check` runs a release build to show it.

## Contributing, security and license

- [CONTRIBUTING.md](CONTRIBUTING.md): how changes are made, the rules every change follows, and
  the tests' rules.
- [SECURITY.md](SECURITY.md): how to report a vulnerability privately, and what counts as one.
- SwarmOps is released under the [MIT License](LICENSE). Each release archive also carries
  [THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES): the third-party crates compiled into `swarmops`
  and `so`, each with its license and the license texts.
