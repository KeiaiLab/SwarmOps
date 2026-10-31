# Quick start: a store and a first commit

This walk-through installs SwarmOps, creates a store, publishes a first commit, lands work from
three agents, resolves a conflict, reads the operation log and checks the store, in about ten
minutes.

## What do I need first?

A Linux host, x86_64 or aarch64, with glibc 2.34 or newer, and a place for the store on a native
Linux filesystem (ext4, XFS or btrfs), not under `/mnt/`. SwarmOps is pre-1.0: a minor release may
still break a command or its output
([versioning policy](../policies/versioning-and-stability.md#before-10)).

The blocks below are POSIX sh: run them in bash, one after another, in one shell and an empty
directory. The shown output is from a real run. Your ids differ, because a changeset's id covers
its time; here ids are cut to 8 digits (`…`), and `/home/me` stands for your home directory.

## 1. How do I install it?

One command installs both names from the latest release, checked against its `SHA256SUMS`:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash
```

That puts `swarmops` and `so` in `~/.local/bin` (`/usr/local/bin` as root) and prints the `PATH`
line to add if that directory is not on your `PATH` ([Install SwarmOps on Linux](install.md) has
the options). Then:

```sh
so --version
```

```text
swarmops 0.1.0
```

`so` is the short form of `swarmops`; every command runs the same by either name.

## 2. How do I create a store?

Name the store and your identity, then create the store:

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme   # or --repo <store> on each command
export SWARMOPS_AGENT=agent/worker-1                       # or --as <family>/<instance>
install -d -m 0700 ~/.local/state/swarmops/repos           # a private parent directory
so init "$SWARMOPS_REPO" --name acme
```

```text
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"3ca95e25fe9bbe471edc25c8c77d2073","name":"acme","store_version":2,"changesets":0,"refs":0},"meta":{"repo":"/home/me/.local/state/swarmops/repos/acme","elapsed_ms":34,"resolved":{},"auth":"trust"}}
```

`init` makes the directory if it is missing. A store has no default location: every command finds
it by `--repo` or `SWARMOPS_REPO`. The identity is `<family>/<instance>`; every change you record
carries it. `--human` prints a short text rendering instead of JSON:

```sh
so --human whoami
```

```text
agent: agent/worker-1
auth: trust
```

## 3. How do I make a first commit?

A commit takes a change list, a JSON array of puts and deletes. `--advance main` publishes it on
`main` in the same transaction:

```sh
printf '#!/bin/sh\necho hello\n' > run.sh                 # the client file the second change reads
cat > changes.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
 {"path": "run.sh", "op": "put", "content": {"file": "./run.sh"}, "mode": "exec"}]
EOF
so commit -m "first" --changes changes.json --advance main  # creates main at the new changeset
```

```text
{"ok":true,"op":"commit","api":1,"result":{"changeset":"ecca9a10…","root":"b8a4f79e…","files_changed":2,"blobs_written":2,"advanced":{"name":"main","new":"ecca9a10…","already_applied":false},"replayed":false},"meta":{"repo":"/home/me/.local/state/swarmops/repos/acme","elapsed_ms":21,"resolved":{},"auth":"trust"}}
```

The first change puts `src/lib.rs` from inline text; the second puts `run.sh` from the client file,
with the `exec` mode. Directories follow from the paths. A put's content may also be `base64`, or
`blob`, the id of a blob stored before with `blob put`.

`--advance main` moves `main` only if `main` still names the commit's parent; for a root commit, as
here, only if `main` does not exist yet. Otherwise the commit records nothing and fails with
`E_CAS_MISMATCH`.

## 4. How do I read it back?

`REV:PATH` names a path at a revision; `main` is a revision, and so is a changeset id or a unique
prefix of 12 or more of its digits.

```sh
so --human cat main:src/lib.rs
so --human ls main
so --human log
```

```text
blob: f5f5220aa3d385d93f53321ce67338241ed06963653b5b823667cb2a45121a9d
size: 14
kind: file
encoding: utf8
content: |
  pub fn f() {}
truncated: false
entries[0].name: run.sh
entries[0].kind: exec
entries[0].size: 21
entries[0].id: 7df41635e764cd4b28e9bf65c0246161cc5bf4ba894b11fdc40f5357f6ddde26
entries[1].name: src
entries[1].kind: dir
entries[1].id: b3df0b759177d000a36c5c20217f3e62a8f9c9dbe9a2d119aa3558912838c011
truncated: false
entries[0].id: ecca9a10…
entries[0].parents: []
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626497000
entries[0].title: first
truncated: false
```

`so show main` prints the whole changeset: parents, author, message, changes and git-style
signatures.

## 5. What does a command print?

Without `--human`, each command prints one JSON envelope on stdout: pretty on a terminal, compact
otherwise. The same `cat`, compact:

```sh
so cat main:src/lib.rs
```

```text
{"ok":true,"op":"read_file","api":1,"result":{"blob":"f5f5220aa3d385d93f53321ce67338241ed06963653b5b823667cb2a45121a9d","size":14,"kind":"file","encoding":"utf8","content":"pub fn f() {}\n","truncated":false},"meta":{"repo":"/home/me/.local/state/swarmops/repos/acme","elapsed_ms":1,"resolved":{"main":"ecca9a10…"},"auth":"trust"}}
```

`op` names the operation (`cat` is `read_file`), and `meta.resolved` what each revision named. A
failure has `"ok":false` and an `error` with a stable `E_*` code, its exit code and a `hint`, often
the next command to run; [Output and exit codes](../../README.md#output-and-exit-codes) lists the
exit codes. Scripts read the JSON; this page reads ids out of it with a small helper:

```sh
field() { python3 -I -c 'import functools, json, sys; print(functools.reduce(lambda v, k: v[k], sys.argv[1].split("."), json.load(sys.stdin)))' "$1"; }
```

## 6. How do several agents publish at once?

Each agent commits on the `main` it read, without moving any ref, then lands its changeset. Two
agents, `worker-1` and `worker-2`, start from the same `main`:

```sh
BASE=$(so resolve main | field result.id)
cat > w1.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() -> u32 { 1 }\n"}}]
EOF
cat > w2.json <<'EOF'
[{"path": "README.md", "op": "put", "content": {"inline": "# acme\n"}}]
EOF
C1=$(so --as agent/worker-1 commit -m "f returns 1" --changes w1.json --parent "$BASE" | field result.changeset)
C2=$(so --as agent/worker-2 commit -m "add a readme" --changes w2.json --parent "$BASE" | field result.changeset)
```

`land` merges a changeset three-way onto `main`'s head, whatever `main` gained since, and moves
`main` to the result in one transaction. `--key` makes a retry safe; make a new one for each land:

```sh
so --human --as agent/worker-1 land "$C1" --key "$(uuidgen)"
K=$(uuidgen)
so --human --as agent/worker-2 land "$C2" --key "$K"
so --human --as agent/worker-2 land "$C2" --key "$K"       # the same land again
so --human ls main
```

```text
landed 11eb953b… on main (onto ecca9a10…, attempt 1)
landed 4c54d337… on main (onto 11eb953b…, attempt 1)
replayed 4c54d337…
entries[0].name: README.md
entries[0].kind: file
entries[0].size: 7
entries[0].id: 745ffc98a5d48ca0a2256493c1eb7b93c89a4d67aba17d1b5d554452a604efc1
entries[1].name: run.sh
entries[1].kind: exec
entries[1].size: 21
entries[1].id: 7df41635e764cd4b28e9bf65c0246161cc5bf4ba894b11fdc40f5357f6ddde26
entries[2].name: src
entries[2].kind: dir
entries[2].id: ec620838c888afff8e4e69263ce3ae95c30fbee9babf57cccd20723da13f6964
truncated: false
```

`worker-2`'s change landed onto `worker-1`'s, so `main` holds both. Run again with its key, the land
replays and writes nothing. A land that loses a race to another publish merges again and retries by
itself.

## 7. What happens on a conflict?

A third agent changes the same line of `src/lib.rs`, also from `BASE`:

```sh
cat > w3.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() -> u32 { 2 }\n"}}]
EOF
C3=$(so --as agent/worker-3 commit -m "f returns 2" --changes w3.json --parent "$BASE" | field result.changeset)
so --human --as agent/worker-3 land "$C3" --key "$(uuidgen)"; echo "exit $?"
```

```text
error: E_CONFLICT: 1 path(s) of the change conflict with the target's head 4c54d337…: nothing was recorded
conflict: content src/lib.rs
hint: swarmops cat 4c54d337…:src/lib.rs, then swarmops land 4b4e8668… --resolve <file> (resolutions over MCP) with each path's resolution and against = 4c54d337…
exit 3
```

Nothing was recorded. In JSON, `error.details.report` gives each conflicting path with the base's,
ours and theirs lines. The agent resolves it without a checkout: per path, the content to keep,
against the `main` the report named, then lands again:

```sh
THEIRS=$(so resolve main | field result.id)
cat > resolve.json <<EOF
{"against": "$THEIRS",
 "files": {"src/lib.rs": {"content": {"inline": "pub fn f() -> u32 { 3 }\n"}}}}
EOF
so --human --as agent/worker-3 land "$C3" --resolve resolve.json --key "$(uuidgen)"
so --human cat main:src/lib.rs
```

```text
landed aa1b0e84… on main (onto 4c54d337…, attempt 1)
blob: a922acde6e5a003e306c91b7984b6c9ed5c4dad587912f36f3d6873f5dac17a4
size: 24
kind: file
encoding: utf8
content: |
  pub fn f() -> u32 { 3 }
truncated: false
```

The hint names the program `swarmops`, whichever name you invoked.

## 8. How do I see and undo an operation?

`op log` lists the store's operations, newest first: every commit, land and ref move is one write
transaction, numbered in commit order and named by an id (`<op>:<crc>`). `op undo <id>` moves every
ref an operation moved back to what it named before, all of them or none, by compare-and-swap.
`main` moves only forward, so the undo of a land refuses with `E_REF_PROTECTED` and writes nothing:

```sh
so --human op log -n 1
so --human op undo 6:8e2a4f10; echo "exit $?"
```

```text
ops[0].op: 6
ops[0].id: 6:8e2a4f10
ops[0].kind: land
ops[0].changesets[0].id: aa1b0e84…
ops[0].changesets[0].agent: agent/worker-3
ops[0].changesets[0].created_at: 1792626497000
ops[0].changesets[0].title: f returns 2
ops[0].changesets[0].land_key: 4ccbb36c-71e9-44a0-a6c4-8fd4453d48bc
ops[0].changesets_total: 1
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 4c54d337…
ops[0].ref_moves[0].new: aa1b0e84…
ops[0].ref_moves[0].actor: agent/worker-3
ops[0].ref_moves[0].at: 1792626497
truncated: true
error: E_REF_PROTECTED: main moves only forward: aa1b0e84… is not a first-parent ancestor of 4c54d337… (the undo of operation 6:8e2a4f10), so nothing was written
details.changeset: aa1b0e84…
details.id: 6:8e2a4f10
details.name: main
details.op: 6
details.target: 4c54d337…
hint: main moves only forward: land a change that takes it back
exit 1
```

`main` still holds the resolved land. To take it back, land a change that reverts it. An undo
moves a `release/<name>` ref either way, and is an operation of its own, which another undo takes
back. Changesets are never removed. If another operation has moved one of the refs since, an undo
refuses with `E_CAS_MISMATCH` and writes nothing.

## 9. How do I check the store?

```sh
so --human verify --deep
```

```text
objects_checked: 23
changesets_checked: 7
problems: []
findings: []
```

`verify --deep` re-hashes every object and checks the derived values against the objects. An empty
`problems` list is a clean store.

## What did each command do?

| Command | What it does |
| --- | --- |
| `so init "$SWARMOPS_REPO" --name acme` | Create a store in the directory (made if missing) and describe it. |
| `so whoami` | The identity this caller's changes carry. |
| `so commit -m … --changes … --advance main` | Record a changeset from a change list and, in the same transaction, move `main` to it. |
| `so commit -m … --changes … --parent REV` | Record a changeset on `REV`, moving no ref. |
| `so resolve main` | The full id of the changeset a revision names. |
| `so cat main:src/lib.rs` | One file at a revision. |
| `so ls main` | One directory at a revision, each file with its size. |
| `so show main` | One changeset: parents, author, message, changes, signatures. |
| `so log` | History along first parents, newest first. |
| `so land C --key K` | Merge changeset `C` three-way onto `main` and publish the result; the same key replays. |
| `so land C --resolve FILE --key K` | Land again with a resolution for each conflicting path. |
| `so op log -n 1` | The store's operations, newest first. |
| `so op undo ID` | Move every ref the operation `op log` lists as `ID` moved back, all or none; `main` only forward. |
| `so verify --deep` | Re-hash every object and check the derived values against the objects. |

## What next?

Give the store to your agents, or connect it to a git repository; these pages say how.

- [MCP (Model Context Protocol)](../../README.md#mcp): `swarmops mcp` serves the store to an agent
  harness such as Claude Code.
- The git round trip: [Bring a git repository in](../../README.md#bring-a-git-repository-in), then
  [Send the work back to git](../../README.md#send-the-work-back-to-git).
- [Concepts](../concepts.md): the store, changesets, revisions, refs, landing and the envelope, one
  definition each.
- [Commands](../../README.md#commands): every command, its revisions and change lists, and its
  output.
- [Operating SwarmOps](../operations.md): a store for a swarm of coding agents on one host, from day
  to day.
- [Install SwarmOps on Linux](install.md): pinning, updates, uninstalling and where a store may
  live.

## Sources

The commands and their output come from a run of SwarmOps 0.1.0. The command descriptions come from
the README's [Commands](../../README.md#commands) table, the envelope from
[Output and exit codes](../../README.md#output-and-exit-codes), and the conflict resolution from
[Operating SwarmOps, Agents commit and land](../operations.md#2-agents-commit-and-land).
