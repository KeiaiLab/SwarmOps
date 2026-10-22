---
title: "Features: everything SwarmOps v0.1.0 does"
description: "Every shipped v0.1.0 capability, grouped: store, write and land, read and history, operations and undo, integrity, MCP, git round trip."
---

# Features

Every capability SwarmOps v0.1.0 ships, grouped by what it is for, each with its command, a real
example and the document that explains it.

The examples ran in one session against scratch stores with `swarmops` 0.1.0, and their output is
shown as printed. `SWARMOPS_REPO=acme` (the store) and `SWARMOPS_AGENT=agent/worker-1` (the
identity) were set; `--as` gives a command another agent's identity. A revision such as
`e71a197799a7` is a unique prefix of a changeset id, 12 or more hex digits; ids and times differ
on every run. `--human` prints a short text rendering instead of the JSON envelope.

| Group | Commands |
| --- | --- |
| [Store](#store) | `init`, `info`, `whoami`, `schema`, `derive` |
| [Write and land](#write-and-land) | `commit`, `land`, `blob put`, `blob has`, `ref set`, `ref delete` |
| [Read and history](#read-and-history) | `resolve`, `cat`, `ls`, `show`, `log`, `history`, `diff`, `ref list` |
| [Operations and undo](#operations-and-undo) | `op log`, `op undo` |
| [Integrity](#integrity) | `verify`, full fsync, crash recovery |
| [MCP](#mcp) | `mcp` |
| [Git round trip](#git-round-trip) | `git import`, `git export` |

## Store

A store is one directory: content-addressed object files and an append-only journal that is the
source of truth. It must be on a local filesystem the store supports (ext4 with a journal, XFS or
btrfs). Explained in [The store on disk](/docs/architecture/#the-store-on-disk) and
[Create a store](/docs/operations/#create-a-store).

### `init`: create a store

Creates a store in a directory (made if missing) and describes it.

```console
$ so init demo --name demo --human
repo_id: 53cecd8b0c756a4ae7c95e0000a0e79b
name: demo
store_version: 1
changesets: 0
refs: 0
```

### `info`: describe the store

Its id, name, `main`, and how many changesets and refs it holds.

```console
$ so info --human
repo_id: 427ffaf696c8d43354cb0310ddaeed94
name: acme
main: e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
store_version: 1
changesets: 1
refs: 1
```

### `whoami`: the identity changes carry

Reads need no identity. A commit, a land, a ref move or a git import needs one,
`<family>/<instance>`, from `--as` or `SWARMOPS_AGENT`; without one it fails with
`E_IDENTITY_REQUIRED` (exit 5). Explained in [Identities](/docs/operations/#identities).

```console
$ so --human whoami
agent: agent/worker-1
auth: trust
```

### `schema`: the contract as JSON Schema

The JSON Schema of every request and response type, or of one. v0.1.0 describes 91 types.

```console
$ so schema | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["result"]))'
91
$ so schema LandStatus
{"ok":true,"op":"schema","api":1,"result":{"LandStatus":{"$schema":"https://json-schema.org/draft/2020-12/schema","description":"How a land ended.","oneOf":[{"const":"landed","description":"This request landed the change.","type":"string"},{"const":"replayed","description":"A land with the same `idempotency_key` had landed it already; nothing was written.","type":"string"}],"title":"LandStatus"}},"meta":{"elapsed_ms":1,"resolved":{},"auth":"trust"}}
```

### `derive`: derived values ahead of readers

Derives the eager derived values (the first-parent skip list) of the changesets the journal
records, ahead of any reader: once, up to 1,000 journal frames, or with `--follow` as they are
committed. Explained in [Reading](/docs/architecture/#reading).

```console
$ so derive --human
derived: 7
cached: 0
next: 10
```

### Output and exit codes

Every command but `mcp` prints exactly one JSON envelope on stdout (`derive --follow` prints one a
line). A failure's `error` carries a stable `E_*` code, `retryable`, machine-readable `details`
and a `hint`, and the exit code is `error.exit`: 0 success, 1 failed, 2 malformed request,
3 conflict, 4 retryable, 5 identity or policy, 6 not found. Explained in
[The contract](/docs/architecture/#the-contract) and the README's
[Output and exit codes](https://github.com/KeiaiLab/SwarmOps#output-and-exit-codes).

```console
$ so cat main:nope.txt --human
error: E_PATH_NOT_FOUND: nothing is at "nope.txt" in main
details.path: nope.txt
details.rev: main
hint: swarmops ls main
```

That run exited with code 6.

## Write and land

A change list is a JSON array of puts and deletes; content is inline text, base64, a file read on
the client, or a blob stored before. Refs move only by compare-and-swap. Explained in the README's
[Revisions and change lists](https://github.com/KeiaiLab/SwarmOps#revisions-and-change-lists),
[Writing](/docs/architecture/#writing) and
[Agents commit and land](/docs/operations/#2-agents-commit-and-land).

### `commit`: record a changeset

Records a changeset from a change list, on `--parent` or else on `main`'s head. With
`--advance REF` it also moves REF to the new changeset in the same transaction, only if REF still
names the parent (for a root commit, only if REF does not exist yet); otherwise it records nothing
and fails with `E_CAS_MISMATCH`. With `--key`, a uuid v7, a retry replays the first run.

```console
$ cat first.json
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
 {"path": "README.md", "op": "put", "content": {"inline": "acme\n"}}]
$ so commit -m first --changes first.json --advance main
{"ok":true,"op":"commit","api":1,"result":{"changeset":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","root":"221a668585eaf52d59a7f72432961e142617e6ff6eb03719ace8512e11273cb4","files_changed":2,"blobs_written":2,"advanced":{"name":"main","new":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","already_applied":false},"replayed":false},"meta":{"repo":"acme","elapsed_ms":1393,"resolved":{},"auth":"trust"}}
```

Two more agents commit on the same parent, without moving any ref:

```console
$ cat g.json
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\npub fn g() {}\n"}}]
$ so --as agent/worker-2 commit -m 'add g' --changes g.json --parent e71a197799a7
{"ok":true,"op":"commit","api":1,"result":{"changeset":"2ad559c4a3e1d1cba5d6d12da2cea3cf2276b8212eaa4eb6b7c9450bf154ae33","parent":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","root":"c6952621b35e8f0a00f377c47c1c7f971b81bd8fa4cade1fa2c9d8aee447296e","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"acme","elapsed_ms":394,"resolved":{"e71a197799a7":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce"},"auth":"trust"}}
$ so --as agent/worker-3 commit -m 'readme' --changes h.json --parent e71a197799a7
{"ok":true,"op":"commit","api":1,"result":{"changeset":"e556fc0d571f5d76e67a28cb288ce114df5f6ccb1436676f93df5d0dce56b8e8","parent":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","root":"44f570ced0923bbb0276704ae5a7e40f3278010f3e0a0521a19deb5d4a6db199","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"acme","elapsed_ms":324,"resolved":{"e71a197799a7":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce"},"auth":"trust"}}
```

### `land`: merge onto `main` and publish

Merges a change three-way onto the target ref's head (`main` by default), whatever it gained
since, and publishes it as one new changeset in one transaction. A land that loses a race to
another publish merges again and retries by itself, up to `--max-attempts` (8 by default). With
`--key`, the same land run again replays it (`status: replayed`).

```console
$ so --as agent/worker-2 land 2ad559c4a3e1 --key 019a0b6e-7c3d-7000-8000-000000000001 --human
landed 09a8de96fe4c00c99d958d62e6d0009439463de886b4dbfe6c10d2a57a994454 on main (onto e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce, attempt 1)
$ so --as agent/worker-3 land e556fc0d571f --key 019a0b6e-7c3d-7000-8000-000000000002 --human
landed 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093 on main (onto 09a8de96fe4c00c99d958d62e6d0009439463de886b4dbfe6c10d2a57a994454, attempt 1)
$ so --as agent/worker-3 land e556fc0d571f --key 019a0b6e-7c3d-7000-8000-000000000002 --human
replayed 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
```

#### Conflicts, resolved without a checkout

A change that conflicts with what the target gained fails with `E_CONFLICT` (exit 3) and a report
in `details.report`: each conflicting file's kind, its hunks (each side's lines and text) and the
target's changeset and agent that last changed it. Nothing is recorded. `land --resolve` resolves
the conflicts path by path (take a side, new content, or delete) against the report's `theirs`.

```console
$ cat c3.json
[{"path": "README.md", "op": "put", "content": {"inline": "acme by worker-4\n"}}]
$ so --as agent/worker-4 commit -m 'readme again' --changes c3.json --parent e71a197799a7
{"ok":true,"op":"commit","api":1,"result":{"changeset":"9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d","parent":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","root":"cb912cc0b9b0f5857eb6e4596c76bcd73bb554ab868bc53a29db79e3111ad59b","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"acme","elapsed_ms":391,"resolved":{"e71a197799a7":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce"},"auth":"trust"}}
$ so --as agent/worker-4 land 9c0f8c84ea28 --human
error: E_CONFLICT: 1 path(s) of the change conflict with the target's head 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093: nothing was recorded
conflict: content README.md
hint: swarmops cat 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093:README.md, then swarmops land 9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d --resolve <file> with each path's resolution and against = 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
```

The same failure as JSON, with the whole report:

```console
$ so --as agent/worker-4 land 9c0f8c84ea28
{"ok":false,"op":"land","api":1,"error":{"code":"E_CONFLICT","message":"1 path(s) of the change conflict with the target's head 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093: nothing was recorded","retryable":false,"exit":3,"details":{"report":{"base":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","files":[{"base_blob":"1b5bf06eef966fbe8bbe4c8e6c184f9ea92be7e02d265924585dc0a9595025ed","hunks":[{"base":{"first":1,"last":1,"text":"acme\n"},"id":0,"ours":{"first":1,"last":1,"text":"acme by worker-4\n"},"theirs":{"first":1,"last":1,"text":"acme: f and g\n"}}],"kind":"content","ours_blob":"7c08d1a969da18c02024a9ee45b7ef442ae59c7334292d211a4675c31b0336b1","path":"README.md","theirs_agent":"agent/worker-3","theirs_blob":"990ef51d432d27f5b53ce872dfdf144f5a67a08cad718a8ab3b30244b975a25e","theirs_changeset":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093"}],"kinds":["content"],"ours":"9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d","theirs":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093","truncated":false}},"hint":"swarmops cat 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093:README.md, then swarmops land 9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d --resolve <file> with each path's resolution and against = 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093"},"meta":{"repo":"acme","elapsed_ms":67,"resolved":{"9c0f8c84ea28":"9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d","main":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093"},"auth":"trust"}}
```

```console
$ cat resolve.json
{"against": "997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093", "files": {"README.md": {"content": {"inline": "acme: f and g, by worker-3 and worker-4\n"}}}}
$ so --as agent/worker-4 land 9c0f8c84ea28 --resolve resolve.json --human
landed 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313 on main (onto 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093, attempt 1)
```

### `blob put` and `blob has`: large content outside a commit

`blob put` stores a file's content as a blob, outside any commit, and prints its id; a change then
names it as `{"blob": "<id>"}`, which counts toward neither the change list's 64 MiB nor a commit's
1 GiB. `blob has` lists which of the given ids the store holds.

```console
$ so blob put big.bin --human
blob: ff04111dfaf7dda065b4f48b10c93900e0c22eb44f2edaa9fbc96a4b4fdb5ad0
size: 101
written: true
$ so blob has ff04111dfaf7dda065b4f48b10c93900e0c22eb44f2edaa9fbc96a4b4fdb5ad0 0000000000000000000000000000000000000000000000000000000000000000 --human
present[0]: ff04111dfaf7dda065b4f48b10c93900e0c22eb44f2edaa9fbc96a4b4fdb5ad0
```

### `ref set` and `ref delete`: compare-and-swap

Refs are `main` and `release/<name>`. `ref set NAME REV` takes `--expect OLD` or `--create`;
`ref delete NAME` takes `--expect OLD`. Setting a ref to the target it holds is a replay
(`already_applied: true`). When another agent moved the ref first, the move fails with
`E_CAS_MISMATCH` (exit 4) and the ref's target in `details.actual`.

```console
$ so ref set release/v1 e71a197799a7 --create --human
name: release/v1
new: e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
already_applied: false
$ so ref set release/v1 main --expect e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce --human
name: release/v1
old: e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
new: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
already_applied: false
$ so ref set release/v1 main --expect e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
{"ok":true,"op":"ref_set","api":1,"result":{"name":"release/v1","old":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","new":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093","already_applied":true},"meta":{"repo":"acme","elapsed_ms":32,"resolved":{"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","main":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093"},"auth":"trust"}}
```

A move that expects what the ref no longer holds:

```console
$ so ref set main e71a197799a7 --expect e71a197799a7 --human
error: E_CAS_MISMATCH: ref main moved to 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313: commit your change on top of it (--parent 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313), then retry with --expect 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313; re-apply your edits to the content at 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313 rather than re-sending your old files
details.actual: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
details.name: main
retryable: true
hint: swarmops show 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
```

```console
$ so ref delete release/v1 --expect 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093 --human
name: release/v1
old: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
already_applied: false
```

## Read and history

Reads take no lock and need no checkout or identity; a read by changeset id is immutable. Every
revision a request names is resolved in one snapshot, and `meta.resolved` says what each named.
Responses are bounded for an agent's context, with cursors to page on. Explained in
[Reading](/docs/architecture/#reading) and [Look at what landed](/docs/operations/#3-look-at-what-landed).

### `resolve`: what a revision names

A revision is `main`, `release/<name>`, a changeset id or a unique prefix of 12 or more hex
digits, then any number of `~N` (the N-th first parent).

```console
$ so resolve main --human
id: e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
kind: ref
```

### `cat`: one file at a revision

The whole file, or part of it: from byte N (`--offset`) or by a range of lines (`--lines`). UTF-8
text, or base64.

```console
$ so cat main:src/lib.rs
{"ok":true,"op":"read_file","api":1,"result":{"blob":"25253a37517b6d3658b2754e03736043ccbab828e8452d5c91b2f27a7e66a0eb","size":28,"kind":"file","encoding":"utf8","content":"pub fn f() {}\npub fn g() {}\n","truncated":false},"meta":{"repo":"acme","elapsed_ms":99,"resolved":{"main":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093"},"auth":"trust"}}
$ so cat main:src/lib.rs --lines 2-2 --human
blob: 25253a37517b6d3658b2754e03736043ccbab828e8452d5c91b2f27a7e66a0eb
size: 28
kind: file
encoding: utf8
content: |
  pub fn g() {}
truncated: false
offset: 14
lines.first: 2
lines.last: 2
```

### `ls`: one directory at a revision

A page at a time, each file with its size; with `--depth`, the directories below it too.

```console
$ so ls main:src --human
entries[0].name: lib.rs
entries[0].kind: file
entries[0].size: 28
entries[0].id: 25253a37517b6d3658b2754e03736043ccbab828e8452d5c91b2f27a7e66a0eb
truncated: false
$ so ls main --depth 2 --human
entries[0].name: README.md
entries[0].path: README.md
entries[0].kind: file
entries[0].size: 14
entries[0].id: 990ef51d432d27f5b53ce872dfdf144f5a67a08cad718a8ab3b30244b975a25e
entries[1].name: src
entries[1].path: src
entries[1].kind: dir
entries[1].id: 2d4ccf909d1ab9fdff77a459c3f2851f924a03c463bab10fc17915c8e467f093
entries[2].name: lib.rs
entries[2].path: src/lib.rs
entries[2].kind: file
entries[2].size: 28
entries[2].id: 25253a37517b6d3658b2754e03736043ccbab828e8452d5c91b2f27a7e66a0eb
truncated: false
```

### `show`: one changeset

Parents, author, message, changes and signatures; a landed changeset also names its lander, its
key and the changesets it was squashed from.

```console
$ so show main --max-changes 1
{"ok":true,"op":"show","api":1,"result":{"id":"63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313","seq":6,"parents":["997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093"],"root":"f635f92df3fa94208e6764d4ec1516072e4ec46e247a617b95480f28a6b363bd","author":{"agent":"agent/worker-4"},"lander":{"agent":"agent/worker-4"},"created_at":1792626919000,"message":"readme again","squashed_from":["9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d"],"changes":[{"path":"README.md","op":"put","kind":"file","blob":"5913476130ad8acd3507596652ec8b245afb38f25c7787d609ccff1e0b5895b5"}],"files_changed":1,"changes_truncated":false,"metadata":{"agent":"agent/worker-4","lander":"agent/worker-4","squashed_from":"9c0f8c84ea28694a16055907268b0ce5080e6f77e343d2df0262f84b3c87806d"},"signatures":{"author":{"name":"agent/worker-4","email":"worker-4@agent.swarmops.local","secs":1792626917,"offset_secs":0},"committer":{"name":"agent/worker-4","email":"worker-4@agent.swarmops.local","secs":1792626919,"offset_secs":0}}},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313"},"auth":"trust"}}
```

### `log`: history along first parents

Newest first, from `main` by default; with `--path`, only the changesets that change those paths.

```console
$ so log -n 2 --human
entries[0].id: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
entries[0].parents[0]: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
entries[0].author_agent: agent/worker-4
entries[0].created_at: 1792626919000
entries[0].title: readme again
entries[1].id: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
entries[1].parents[0]: 09a8de96fe4c00c99d958d62e6d0009439463de886b4dbfe6c10d2a57a994454
entries[1].author_agent: agent/worker-3
entries[1].created_at: 1792626916000
entries[1].title: readme
truncated: true
cursor: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
```

### `history`: the changesets that changed a path

Newest first, each with what the path holds after it.

```console
$ so history main:README.md --human
path: README.md
entries[0].changeset: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
entries[0].op: put
entries[0].kind: file
entries[0].id: 990ef51d432d27f5b53ce872dfdf144f5a67a08cad718a8ab3b30244b975a25e
entries[0].author_agent: agent/worker-3
entries[0].created_at: 1792626916000
entries[0].title: readme
entries[1].changeset: e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
entries[1].op: put
entries[1].kind: file
entries[1].id: 1b5bf06eef966fbe8bbe4c8e6c184f9ea92be7e02d265924585dc0a9595025ed
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626912000
entries[1].title: first
truncated: false
```

### `diff`: what differs between two revisions

The files that differ, in path order, a page at a time: `names` (the default), `stat` (binary,
inserted and deleted lines) or `unified` (hunks).

```console
$ so diff main~2 main --human
from: 09a8de96fe4c00c99d958d62e6d0009439463de886b4dbfe6c10d2a57a994454
to: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
files[0].path: README.md
files[0].status: modified
truncated: false
$ so diff e71a197799a7 main --format stat --human
from: e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce
to: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
files[0].path: README.md
files[0].status: modified
files[0].binary: false
files[0].insertions: 1
files[0].deletions: 1
files[1].path: src/lib.rs
files[1].status: modified
files[1].binary: false
files[1].insertions: 1
files[1].deletions: 0
truncated: false
```

### `ref list`: every ref

```console
$ so ref list
{"ok":true,"op":"ref_list","api":1,"result":{"refs":[{"name":"main","target":"63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313","version":6},{"name":"release/v1","target":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093","version":2}]},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

## Operations and undo

The journal is shown as the store's operations, newest first: every commit, land and ref move is
one write transaction, numbered in commit order. `op undo` moves every ref an operation moved back
to what it named before, by compare-and-swap in one transaction, all or none: if another operation
moved one of them since, it fails with `E_CAS_MISMATCH` and writes nothing. Changesets are never
removed, and an undo is an operation of its own, which another undo takes back. Explained in the
README's [First commands](https://github.com/KeiaiLab/SwarmOps#first-commands) and
[Concurrency](/docs/architecture/#concurrency).

### `op log`: the store's operations

```console
$ so op log -n 1 --human
ops[0].op: 8
ops[0].kind: land
ops[0].changesets[0].id: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
ops[0].changesets[0].agent: agent/worker-4
ops[0].changesets[0].created_at: 1792626919000
ops[0].changesets[0].title: readme again
ops[0].changesets_total: 1
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
ops[0].ref_moves[0].new: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
ops[0].ref_moves[0].actor: agent/worker-4
ops[0].ref_moves[0].at: 1792626919
truncated: true
```

### `op undo`: take an operation back

```console
$ so op undo 8 --human
undone: 8
op: 9
ref_moves[0].name: main
ref_moves[0].old: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
ref_moves[0].new: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626920
already_applied: false
$ so resolve main --human
id: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
kind: ref
$ so op undo 9 --human
undone: 9
op: 10
ref_moves[0].name: main
ref_moves[0].old: 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
ref_moves[0].new: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626921
already_applied: false
$ so resolve main --human
id: 63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313
kind: ref
```

## Integrity

- **Self-verifying objects.** Ids are BLAKE3-256 over the object's kind, layout version and bytes,
  and every read re-hashes.
- **Full fsync, the only mode.** An object is synced before a journal frame names it, and a commit
  returns after its frame's `fdatasync`; there is no fast mode.
- **Crash recovery.** The writer lock is an `flock`, released by the kernel when its holder dies,
  and the next writer repairs whatever a crash cut short; `meta.recovered` says what it repaired.
- **Tested through the real binary**: crash sweeps at every filesystem call, injected I/O errors,
  write-ordering checks and simulated power losses.

Explained in [Durability](/docs/architecture/#durability) and
[Checking a store](/docs/operations/#checking-a-store).

### `verify`: check the store

Reports damage as typed findings; `--deep` also re-hashes every object and checks the derived
values against the objects. A failed verify is `E_STORE_CORRUPT` with the report in `details`.

```console
$ so verify --deep --human
objects_checked: 21
changesets_checked: 7
problems: []
findings: []
```

## MCP

`swarmops mcp` serves the store's operations to an agent as an MCP server on stdin and stdout, one
tool per operation: `repo_info`, `resolve_rev`, `read_file`, `read_tree`, `show`, `log`,
`path_history`, `diff`, `op_log`, `ref_list`, `ref_set`, `ref_delete`, `undo`, `commit`, `land`,
`blob_put`, `blob_has` and `verify`. A tool's input schema is the request type's schema, and its
result is the envelope the CLI prints, as `structuredContent` and as text; a failed operation sets
`isError`. The server speaks MCP 2026-07-28, and 2025-11-25 and 2025-06-18 to clients that open
with `initialize`. Every change is recorded for the identity the server runs as. Explained in the
README's [MCP](https://github.com/KeiaiLab/SwarmOps#mcp) and
[Give agents the store](/docs/operations/#give-agents-the-store).

The README's [Register the server](https://github.com/KeiaiLab/SwarmOps#register-the-server) gives
the Claude Code command and a project's `.mcp.json`. The tools a client gets:

```console
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | so mcp | tail -n 1 | python3 -c 'import json,sys; print(" ".join(t["name"] for t in json.load(sys.stdin)["result"]["tools"]))'
repo_info resolve_rev read_file read_tree show log path_history diff op_log ref_list ref_set ref_delete undo commit land blob_put blob_has verify
```

A session: `initialize`, then a `read_file` call. The answers are the server's initialize result,
with its instructions to the agent, and the tool's result.

```console
$ cat mcp.in
{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "demo", "version": "0"}}}
{"jsonrpc": "2.0", "method": "notifications/initialized"}
{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "read_file", "arguments": {"rev": "main", "path": "src/lib.rs"}}}
$ so mcp --as agent/worker-5 < mcp.in
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"swarmops","title":"SwarmOps","version":"0.1.0"},"instructions":"SwarmOps is source control for coding-agent swarms: many agents commit to one store at once. Each tool is one store operation, and its result is the SwarmOps envelope {ok, op, api, result | error, meta}, as structuredContent and again as JSON text. A failed operation sets isError and gives error {code, message, retryable, exit, details, hint}: retryable says whether the operation can succeed when it is run again, as it is or after re-reading what the hint names, and a hint is often a swarmops command, whose operation is the tool of the same name (ref list is ref_list, cat read_file, ls read_tree, info repo_info). commit records a changeset; with advance it also moves that ref to it in the same transaction, if the ref still names parent: check result.advanced. ref_set and ref_delete change a ref by compare-and-swap, so their expected_old, like commit's parent with advance, is the full 64-hex changeset id the ref held when you read it (from ref_list, show, log or a commit's result). A ref name or a relative revision is read when the call runs instead of pinning the id you read: the ref's own name always matches, so the compare-and-swap protects nothing, and main~1 on ref main never matches; an id prefix stops resolving once another changeset shares it; the server refuses all three. To publish a changeset, land it: land merges it three-way onto main and moves main in one step, and on E_CONFLICT details.report says what to resolve. A read may show a commit that a failed sync then takes back; a mutation that names it fails under the lock instead of building on it. op_log lists the store's transactions, newest first; undo restores the refs one moved while they still hold what it set. Clients limit long tool results: ask for little, as each tool's description says. A {\"file\": path} content is read by this server, not by your client: give an absolute path (a relative one, or the server's own stdin or stdout, is refused). Every change is recorded for the agent the server runs as."}}
{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"ok\":true,\"op\":\"read_file\",\"api\":1,\"result\":{\"blob\":\"25253a37517b6d3658b2754e03736043ccbab828e8452d5c91b2f27a7e66a0eb\",\"size\":28,\"kind\":\"file\",\"encoding\":\"utf8\",\"content\":\"pub fn f() {}\\npub fn g() {}\\n\",\"truncated\":false},\"meta\":{\"repo\":\"acme\",\"elapsed_ms\":0,\"resolved\":{\"main\":\"63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313\"},\"auth\":\"trust\"}}"}],"structuredContent":{"ok":true,"op":"read_file","api":1,"result":{"blob":"25253a37517b6d3658b2754e03736043ccbab828e8452d5c91b2f27a7e66a0eb","size":28,"kind":"file","encoding":"utf8","content":"pub fn f() {}\npub fn g() {}\n","truncated":false},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313"},"auth":"trust"}},"isError":false}}
```

## Git round trip

`git import` and `git export` read and write a git repository on this host; neither is an MCP
tool, so an agent runs them as commands. Between hosts, git carries the history, one writing host
at a time. Explained in the README's
[Bring a git repository in](https://github.com/KeiaiLab/SwarmOps#bring-a-git-repository-in) and
[Send the work back to git](https://github.com/KeiaiLab/SwarmOps#send-the-work-back-to-git),
[Git import](/docs/architecture/#git-import), [Git export](/docs/architecture/#git-export) and
[The daily loop](/docs/operations/#the-daily-loop).

### `git import`: bring a git history in

Every commit `--ref` reaches that the store does not hold becomes a changeset, parents first, and
`--into` (`main`) moves to it. Run again, it takes only what is new. Content a changeset cannot
hold (a submodule, a name that is not UTF-8, an odd mode) is refused with `E_GIT_IMPORT_LOSSY`
unless `--lossy` allows its class. Here `src-git` is a git work tree with one commit on `stable`,
and `shop` an empty store:

```console
$ SWARMOPS_REPO=shop so git import src-git --ref stable --human
imported: 1
skipped: 0
head_changeset: d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66
git_head_sha: 68c302640bce10c2e42a847edcc028cc2853778c
target_ref: main
ref_moved: true
lossy: []
lossy_total: 0
$ SWARMOPS_REPO=shop so git import src-git --ref stable --human
imported: 0
skipped: 1
head_changeset: d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66
git_head_sha: 68c302640bce10c2e42a847edcc028cc2853778c
target_ref: main
ref_moved: false
old_head: d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66
lossy: []
lossy_total: 0
```

An imported changeset is its git commit: its author and committer, its id in `git.oid`, and no
agent (`unknown/unknown`).

```console
$ SWARMOPS_REPO=shop so show main~1 --human
id: d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66
seq: 0
parents: []
root: df3040af7381e7e412c6a0c5e9ab8b490ab80d92dedad94530d98340ced54a6e
author.agent: unknown/unknown
created_at: 1790856000000
message: |
  hello
changes[0].path: hello.txt
changes[0].op: put
changes[0].kind: file
changes[0].blob: 464418f5a53907c921906843e6f1f4f984f144067b3bf32d69c1ff4c13627464
files_changed: 1
changes_truncated: false
metadata.git.oid: 68c302640bce10c2e42a847edcc028cc2853778c
signatures.author.name: Dev
signatures.author.email: dev@example.com
signatures.author.secs: 1790856000
signatures.author.offset_secs: 0
signatures.committer.name: Dev
signatures.committer.email: dev@example.com
signatures.committer.secs: 1790856000
signatures.committer.offset_secs: 0
```

### `git export`: send the work back to git

Writes the history behind a store ref into a git branch: a changeset imported from git as the
commit it came from, byte for byte, and one made in the store as a commit with its own author and a
`SwarmOps-Changeset:` trailer. The branch moves by compare-and-swap; a branch that moved in git
fails with `E_GIT_EXPORT_DIVERGED` (exit 3), and nothing is written. After a commit in the store:

```console
$ SWARMOPS_REPO=shop so commit -m 'add world' --changes w.json --parent d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66 --advance main --human
changeset: 69e0d93bdcd3bde4045a43befdcbb3266f939ce0a35c496ef577d3ba30d426f0
parent: d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66
root: 2ea14bc946ae80ad434601d493ada10ff417edab444e33516a498c867fe19fa3
files_changed: 1
blobs_written: 1
advanced.name: main
advanced.old: d3edcc44672ff86cbc134eab17d66df8e67a9abad388f3ae8eddc9ec3068fd66
advanced.new: 69e0d93bdcd3bde4045a43befdcbb3266f939ce0a35c496ef577d3ba30d426f0
advanced.already_applied: false
replayed: false
$ SWARMOPS_REPO=shop so git export out.git --branch stable --human
exported 2 commits to stable (none → d24bfc07e8608349c43c01bcb83c176dc50cd70b)
exported: 2
objects_written: 6
head_changeset: 69e0d93bdcd3bde4045a43befdcbb3266f939ce0a35c496ef577d3ba30d426f0
head_sha: d24bfc07e8608349c43c01bcb83c176dc50cd70b
branch: stable
branch_moved: true
incremental: false
rewritten: 0
$ SWARMOPS_REPO=shop so git export out.git --branch stable --human
stable is up to date at d24bfc07e8608349c43c01bcb83c176dc50cd70b
exported: 0
objects_written: 0
head_changeset: 69e0d93bdcd3bde4045a43befdcbb3266f939ce0a35c496ef577d3ba30d426f0
head_sha: d24bfc07e8608349c43c01bcb83c176dc50cd70b
branch: stable
old_sha: d24bfc07e8608349c43c01bcb83c176dc50cd70b
branch_moved: false
incremental: true
rewritten: 0
$ git -C out.git log --format='%h %an %s' stable
d24bfc0 agent/worker-1 add world
68c3026 Dev hello
$ git -C out.git log -1 --format=%B stable
add world

SwarmOps-Changeset: 69e0d93bdcd3bde4045a43befdcbb3266f939ce0a35c496ef577d3ba30d426f0

```

## What v0.1.0 does not do

No workspaces (M5), review (M7) or sync between hosts but through git (M11); Linux only (M12). The
[roadmap](/roadmap/) lists what comes next, and the
[known limitations](/docs/known-limitations/) list every open item with the milestone that takes it.
