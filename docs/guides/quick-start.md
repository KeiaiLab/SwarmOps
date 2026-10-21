# Quick start: a store and a first commit

This walk-through creates a SwarmOps store, commits two files and publishes them on `main`, then
reads them back and checks the store.

## What do I need first?

`swarmops` and `so` on your `PATH` ([Install SwarmOps on Linux](install.md)), and a place for the
store on a native Linux filesystem (ext4, XFS or btrfs), not under `/mnt/`. SwarmOps is pre-1.0: a
minor release may still break a command or its output
([versioning policy](../policies/versioning-and-stability.md#before-10)).

## How do I create a store and make a first commit?

Name the store and your identity, create the store, write a change list and commit it with
`--advance main`, then read back what you committed. The examples use `so`, the short form; every
command runs the same by either name.

The hints in the envelopes give their commands as `swarmops …`; only a usage error's hint, the
usage line, names the program as you invoked it (`Usage: so …`).

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme   # or --repo <store> on each command
export SWARMOPS_AGENT=agent/worker-1                       # or --as <family>/<instance>

so init "$SWARMOPS_REPO" --name acme
printf '#!/bin/sh\necho hello\n' > run.sh                 # the client file the second change reads
cat > changes.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
 {"path": "run.sh", "op": "put", "content": {"file": "./run.sh"}, "mode": "exec"}]
EOF
so commit -m "first" --changes changes.json --advance main  # creates main at the new changeset
so cat main:src/lib.rs
so ls main:src
so show main
so log -n 20
so op log -n 5                                            # the store's operations, newest first
so verify --deep
```

`changes.json` is a change list: it puts `src/lib.rs` from inline text, and `run.sh` from the client
file the second change reads, with the `exec` mode; directories follow from the paths. What each
command does:

| Command | What it does |
| --- | --- |
| `so init "$SWARMOPS_REPO" --name acme` | Create a store in the directory (made if missing) and describe it. |
| `so commit -m "first" --changes changes.json --advance main` | Record a changeset from the change list and, in the same transaction, move `main` to it. |
| `so cat main:src/lib.rs` | One file at a revision. |
| `so ls main:src` | One directory at a revision, each file with its size. |
| `so show main` | One changeset: parents, author, message, changes, signatures. |
| `so log -n 20` | History along first parents, newest first. |
| `so op log -n 5` | The store's operations, newest first. |
| `so verify --deep` | Check the store: re-hash every object and check the derived values against the objects. |

## What does a command print?

Each command here prints one JSON envelope on stdout: pretty on a terminal, compact otherwise.
`so cat main:src/lib.rs` prints the file in `result`; here it is compact, with the ids cut short
(`…`):

```json
{"ok":true,"op":"read_file","api":1,"result":{"blob":"f5f5…","size":14,"kind":"file","encoding":"utf8","content":"pub fn f() {}\n","truncated":false},"meta":{"repo":"/home/me/.local/state/swarmops/repos/acme","elapsed_ms":0,"resolved":{"main":"014c…"},"auth":"trust"}}
```

`op` names the operation (`cat` is `read_file`), and `meta.resolved` what each revision named. A
failure's `error` carries a stable `E_*` code, its exit code and a `hint`, often the next command to
run; [Output and exit codes](../../README.md#output-and-exit-codes) lists the exit codes.

## What does `--advance main` do?

`--advance main` publishes the commit: the same transaction moves `main` to the new changeset, only
if `main` still names `--parent` (for a root commit, as here, only if `main` does not exist yet),
and otherwise the commit records nothing and fails with `E_CAS_MISMATCH`.

## How do I publish a later change?

A later publish is a commit, then a land. `so commit -m … --changes … --parent <the id main held
when you read it>` records your change without moving any ref.

`so land <that changeset> --key <a new uuid v7>` then merges it three-way onto `main`'s head,
whatever `main` gained since, and moves `main` to the result in one transaction, as one new
changeset whose parent is that head. A land that loses a race to another publish merges again and
retries by itself; a change that conflicts with what `main` gained fails with `E_CONFLICT` (exit 3)
and the conflict report in `details.report`, recording nothing; and the same land run again with its
key replays it (`status: replayed`).

`commit --parent <that id> --advance main` publishes in one step while `main` has not moved. To move
a ref past several changesets at once, commit them with `--parent`, then move it once with
`so ref set main <last> --expect <old>` (or `--create`).

## How do I see and undo an operation?

`so op log` lists the store's operations, newest first: every commit, publish and ref move is one
write transaction, numbered in commit order (`op`). `so op undo <op>` moves every ref that operation
moved back to what it named before, all of them or none, by compare-and-swap.

If another operation has moved one of them since, the undo refuses with `E_CAS_MISMATCH` and writes
nothing. Changesets are never removed, and an undo is an operation of its own, which another undo
takes back.

## What next?

Give the store to your agents, or connect it to a git repository; these pages say how.

- [Install SwarmOps on Linux](install.md): upgrades, uninstalling and where a store may live.
- [Concepts](../concepts.md): the store, changesets, revisions, refs, landing and the envelope, one
  definition each.
- [MCP (Model Context Protocol)](../../README.md#mcp): `swarmops mcp` serves the store to an agent
  harness such as Claude Code.
- The git round trip: [Bring a git repository in](../../README.md#bring-a-git-repository-in), then
  [Send the work back to git](../../README.md#send-the-work-back-to-git).
- [Commands](../../README.md#commands): every command, its revisions and change lists, and its
  output.
- [Operating SwarmOps](../operations.md): a store for a swarm of coding agents on one host, from day
  to day.

## Sources

This walk-through is the README's [First commands](../../README.md#first-commands), with its
commands unchanged. The command descriptions come from its [Commands](../../README.md#commands)
table, and the envelope from [Output and exit codes](../../README.md#output-and-exit-codes).
