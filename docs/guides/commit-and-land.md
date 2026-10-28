# Commit and land

This guide shows how to publish work into a SwarmOps store: write a change list, commit it, land
it on `main`, and handle what happens when other agents publish at the same time. It covers
conflicts, retries, refs and identities, each with a run you can repeat.

Agents do the same through the MCP tools `commit`, `land`, `ref_set` and `ref_delete`, which take
the fields the flags below set (`parent`, `advance`, `idempotency_key`, `allow_empty`,
`resolutions` and so on; `swarmops schema` gives each one). The README's
[MCP](../../README.md#mcp) section registers the server.

Every command below was run with `swarmops` 0.1.0, built from this repository, against a fresh
store. Every output is copied from that run. In JSON envelopes, `"repo":"…"` and the `…` after it
mark a cut: the store's path and the rest of `meta` are left out. Changeset ids differ in every
run, since a changeset records its time.

## How publishing works

Three operations move work into a store:

- **`commit`** records a changeset from a change list, on top of a parent. It moves no ref,
  unless you ask it to with `--advance`.
- **`land`** merges a change three-way onto a ref (`main` by default) and moves the ref to the
  result, in one transaction. If the ref moved meanwhile, it merges again and retries.
- **`ref set`** and **`ref delete`** move a ref by compare-and-swap: only if it still names what
  you say it names.

```text
main:  A ─── B ─── L      L: C's change merged onto B, published as one new changeset
        \
         C                an agent's commit on A, which no ref names
```

Nothing needs a checkout. An agent reads files with `cat` and `ls`, sends whole files in its
change list, and lands.

## Before you start

You need the `swarmops` binary ([Install](../../README.md#install)) and a store. A store is one
directory; every command finds it by `--repo` or `SWARMOPS_REPO`.

```sh
swarmops --version
export SWARMOPS_REPO="$PWD/acme"
swarmops init "$SWARMOPS_REPO" --name acme --human
```

```text
swarmops 0.1.0
repo_id: e0cc93ee1ae570f2d8194ec4ebbbc32c
name: acme
store_version: 1
changesets: 0
refs: 0
```

Every command prints one JSON envelope on stdout. `--human` prints a short text rendering
instead; this guide uses it where a script would not read the output. Two shell helpers read the
JSON and make keys:

```sh
# field: print one field of an envelope, as text, or as JSON for an object or a list.
field() { python3 -I -c 'import functools, json, sys; v = functools.reduce(lambda v, k: v[k], sys.argv[1].split("."), json.load(sys.stdin)); print(v if isinstance(v, str) else json.dumps(v, indent=2))' "$1"; }

# uuid7: print a new uuid v7 (RFC 9562), the key a commit takes.
uuid7() { python3 -I -c 'import os, time; b = bytearray((time.time_ns() // 1000000).to_bytes(6, "big") + os.urandom(10)); b[6] = b[6] & 15 | 0x70; b[8] = b[8] & 63 | 0x80; h = b.hex(); print(h[:8], h[8:12], h[12:16], h[16:20], h[20:], sep="-")'; }
```

```sh
uuid7
```

```text
01a160e4-19f4-7a73-9743-9f144eba1210
```

Use any tool that makes a uuid v7 instead, if you have one.

## Say who you are

Reads need no identity. Everything that writes does: `commit`, `land`, `blob put`, `ref set`,
`ref delete`, `op undo` and `git import`. The identity is `--as <family>/<instance>`, or else
`SWARMOPS_AGENT`. `SWARMOPS_RUN_ID`, when set, names the run the agent acts in. `whoami` shows
what a command would act as:

```sh
swarmops whoami --human
swarmops --as operator/ada whoami --human
export SWARMOPS_AGENT=agent/worker-1 SWARMOPS_RUN_ID=run-0042
swarmops whoami --human
swarmops --as agent/worker-2 whoami --human
```

```text
auth: anonymous
agent: operator/ada
auth: trust
agent: agent/worker-1
run_id: run-0042
auth: trust
agent: agent/worker-2
run_id: run-0042
auth: trust
```

The rules:

- `--as` wins over `SWARMOPS_AGENT`. `SWARMOPS_RUN_ID` applies whichever of the two named the
  agent, as the last run shows.
- A variable set to the empty string counts as unset.
- The family is `[a-z0-9-]`; the instance is `[A-Za-z0-9._-]` and does not start with `.`. The
  whole name is at most 255 bytes.
- `auth: trust` means the store takes the name as claimed. It verifies nothing.

A name that breaks the rules, and a write without any name, are refused:

```sh
swarmops --as Agent/1 whoami --human
echo "[]" | env -u SWARMOPS_AGENT swarmops commit -m "nothing" --changes - --human
```

```text
error: E_INVALID_ARG: --as: invalid agent name "Agent/1": expected <family>/<instance> of at most 255 bytes
details.source: --as
hint: agent names look like agent/worker-17: family [a-z0-9-]+, instance [A-Za-z0-9._-]+ not starting with '.'
error: E_IDENTITY_REQUIRED: this operation changes the store, so it needs the caller's identity
hint: pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1
```

The first exits 2, the second 5.

Give every agent process its own instance, such as `agent/worker-1`, and yourself one of your own,
such as `operator/ada`. The store records the identity:

- on a changeset, as its author (`agent` and `run_id` in its metadata);
- on a landed changeset, also as its lander (`lander` and `lander_run_id`);
- on every ref move, as its actor, which `swarmops op log` shows.

`git export` writes the name as the commit's author or committer, with the email
`<instance>@<family>.swarmops.local`. An MCP server takes its identity the same way, from `--as`
or the environment it starts with.

The rest of this guide runs as `agent/worker-1` in run `run-0042`, unless a command says
`--as`.

## Start a store's history

A new store has no `main`. The first commit creates it with `--advance main`:

```sh
cat start.json
swarmops commit -m "start acme" --changes start.json --advance main --human
```

```text
[{"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nA small service.\n"}},
 {"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hello\"\n}\n"}}]
changeset: 999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3
root: 3d39d1861c98525dac73e96f41d09332f77b762dde37823dcbdeb3129438d212
files_changed: 2
blobs_written: 2
advanced.name: main
advanced.new: 999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3
advanced.already_applied: false
replayed: false
```

To bring an existing git history in instead, use `swarmops git import`
([operations.md](../operations.md#once-bring-the-repository-in)).

## Publish a change

Publishing takes three steps: read the head, commit on it, land.

### 1. Read the head

```sh
BASE=$(swarmops resolve main | field result.id)
```

`resolve` gives the full id of the changeset `main` names now. Keep it: it is what your edits
are based on.

### 2. Commit on it

```sh
cat change.json
swarmops commit -m "describe acme" --changes change.json --parent "$BASE" > commit.json
cat commit.json
C=$(field result.changeset < commit.json)
```

```text
[{"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nA small service that greets.\n"}}]
{"ok":true,"op":"commit","api":1,"result":{"changeset":"dd71fe85abe461c94dab72e126f580827a0796582a2075f6e00d3817cd6ce450","parent":"999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3","root":"9a26124c3c1b89b3507c349418cdbcc8be39b5748e5249b9c7097c4e11f4168b","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"…",…}}
```

The result names the new changeset, its parent and its root tree. `files_changed` counts the
paths it changes; a put of what a path holds already does not count. `blobs_written` counts the
blobs this commit placed in the store; content the store held already counts 0.

The commit moved no ref. `main` still names `$BASE`. Without `--parent`, a commit builds on
`main`'s head as it is when the command runs; give `--parent` so that the commit is on what you
read.

### 3. Land it

```sh
swarmops land "$C" --key "$(uuid7)" > land.json
cat land.json
swarmops log -n 2 --human
```

```text
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"26cee040899b510d7fd08f93a2f0f4662921e09f85e5fa4a6242610bcc2d5d9d","rebased_onto":"999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3","target":"main","base":"999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3","attempts":1,"relocated":false,"files_changed":1,"squashed_from":["dd71fe85abe461c94dab72e126f580827a0796582a2075f6e00d3817cd6ce450"]},"meta":{"repo":"…",…}}
entries[0].id: 26cee040899b510d7fd08f93a2f0f4662921e09f85e5fa4a6242610bcc2d5d9d
entries[0].parents[0]: 999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626925000
entries[0].title: describe acme
entries[1].id: 999a7002bcfb45005a5f61863c96995426908136f0c8333b09061a2d93c041d3
entries[1].parents: []
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626924000
entries[1].title: start acme
truncated: false
```

`land` published a new changeset, `landed`, whose parent is the head it merged onto,
`rebased_onto`. It is not the changeset you committed: a land squashes the change into one
changeset on the target, so `main` stays linear, and `squashed_from` lists what it was taken
from. Its author is the change's author and its committer is the caller who landed it.

The fields of the result:

| Field | Meaning |
| --- | --- |
| `status` | `landed`, or `replayed` when a land with the same `--key` had landed it already. |
| `landed` | The new changeset on the target. |
| `rebased_onto` | The target's head it landed on: `landed`'s parent. |
| `target` | The ref it landed on. |
| `base` | Where the change starts: what the head's chain added after it is what landed. |
| `attempts` | How many times the land tried to publish. |
| `relocated` | The target moved after the land read it, and the publish put the change on the new head itself, without another attempt. |
| `files_changed` | How many paths the landed changeset changes. |
| `squashed_from` | The changesets the land squashed, oldest first. |
| `auto_merged` | The paths both sides changed that merged cleanly; left out when there are none. |

A land run again without a key finds the change on the target already:

```sh
swarmops land "$C" --human
```

```text
error: E_NOTHING_TO_LAND: nothing to land: the target has the change already
details.reason: no_changes
```

It exits 1. [Make a retry safe](#make-a-retry-safe) shows how a key turns a rerun into a replay
instead.

### Land options

| Option | Default | What it does |
| --- | --- | --- |
| `--target REF` | `main` | The ref to land on: `main` or `release/<name>`, which must exist. |
| `--base REV` | the merge base of HEAD and the target's head, along first parents | Where the change starts: a first-parent ancestor of HEAD, whose own changes are not landed. |
| `-m MSG` | HEAD's message | The landed changeset's message. |
| `--key KEY` | none | Makes a retry safe: 1 to 64 letters, digits and `-`. |
| `--max-attempts N` | 8 | How many times to try to publish before `E_CAS_RETRY_EXHAUSTED`: 1 to 32. |
| `--max-conflict-bytes N` | 32768 | The most bytes of hunk text a conflict report carries. |
| `--resolve FILE` | none | Resolutions for a conflict report, as JSON, or `-` for stdin. |

## Write a change list

A change list is a JSON array. Each change names a `path`, an `op` (`put` or `delete`), and for a
put its `content` and, optionally, its `mode`. Paths are `/`-separated and relative to the
repository's root; directories follow from the paths.

### Content: inline, base64, file or blob

A put's `content` is an object with one key:

| Form | Example | Use it for |
| --- | --- | --- |
| `inline` | `{"inline": "fn main() {}\n"}` | UTF-8 text, taken verbatim. |
| `base64` | `{"base64": "AAAA"}` | Bytes, in standard, padded base64. |
| `file` | `{"file": "build.sh"}` | A file on your machine, read by the CLI (a relative path is from the current directory). |
| `blob` | `{"blob": "6db1e8e7…"}` (id shortened) | A blob the store holds already, by its 64-hex id. The commit neither sends nor reads its bytes. |

`swarmops blob put FILE`, or `-` for stdin, stores a blob and prints its id. `cat` gives the blob
id of a file at a revision, so a change can reuse it.

### Modes

`mode` is `file` (the default, git's 100644), `exec` (100755) or `symlink`, whose content is
the link's target. A delete takes no content and no mode.

### Rename or move a file

There is no rename operation. Delete the old path and put the new one in the same change list;
name the old content by its blob id so that nothing is sent again.

This change list uses every form. It moves `README.md` to `docs/README.md`, leaves a symlink at
`README`, and adds an executable script, a binary file stored before, and a small binary file
given in base64:

```sh
printf "#!/bin/sh\ncargo build --release\n" > build.sh
LOGO=$(printf "\211PNG\r\n" | swarmops blob put - | field result.blob)
README=$(swarmops cat main:README.md | field result.blob)
cat > assets.json <<EOF
[{"path": "scripts/build.sh", "op": "put", "content": {"file": "build.sh"}, "mode": "exec"},
 {"path": "assets/logo.png", "op": "put", "content": {"blob": "$LOGO"}},
 {"path": "assets/zero.bin", "op": "put", "content": {"base64": "AAAA"}},
 {"path": "README", "op": "put", "content": {"inline": "docs/README.md"}, "mode": "symlink"},
 {"path": "docs/README.md", "op": "put", "content": {"blob": "$README"}},
 {"path": "README.md", "op": "delete"}]
EOF
BASE=$(swarmops resolve main | field result.id)
swarmops commit -m "move the readme, add assets" --changes assets.json --parent "$BASE" > commit.json
field result < commit.json
```

```text
{
  "changeset": "bfad6b92eead9b66e94f89f9dfb31b3c1e5bb2864c4e42ec6a85b20ce8846678",
  "parent": "26cee040899b510d7fd08f93a2f0f4662921e09f85e5fa4a6242610bcc2d5d9d",
  "root": "d0c3b6d581cde3495a792c626ea02d30db022d771431604b0b1883ca43a65da2",
  "files_changed": 6,
  "blobs_written": 3,
  "replayed": false
}
```

Three of the five contents were new to the store; the logo and the readme were not. `show` lists
what the changeset records:

```sh
C=$(field result.changeset < commit.json)
swarmops show "$C" --human | grep "^changes"
swarmops land "$C" --key "$(uuid7)" --human
```

```text
changes[0].path: README
changes[0].op: put
changes[0].kind: symlink
changes[0].blob: 45e2f3b755de260c661fdc8600796b297742591f10fc343c1a4d66cc4f6d6214
changes[1].path: README.md
changes[1].op: delete
changes[2].path: assets/logo.png
changes[2].op: put
changes[2].kind: file
changes[2].blob: 6db1e8e714b41c40a22e522fbc59957a8a66d0438e9055ab7ee62b3dfd40d293
changes[3].path: assets/zero.bin
changes[3].op: put
changes[3].kind: file
changes[3].blob: 2035d184f855deaded9620a08aa243465ea45da2b4fb0457d6c787557d7a88ec
changes[4].path: docs/README.md
changes[4].op: put
changes[4].kind: file
changes[4].blob: ef617763b6864a8e0068455041849ec40e512e68f856e350e95a7f4ed967dc6d
changes[5].path: scripts/build.sh
changes[5].op: put
changes[5].kind: exec
changes[5].blob: 4b6f78d9caa894ad8ac93fbe50b84def3e944da4adecdd2f06b1623391a802df
changes_truncated: false
landed 68cc12a030d5d5cc2b295716fca66de5ecc01965c781d03dc328be70c7a1ca01 on main (onto 26cee040899b510d7fd08f93a2f0f4662921e09f85e5fa4a6242610bcc2d5d9d, attempt 1)
```

### Limits

| Limit | Value | Past it |
| --- | --- | --- |
| One put's content | 512 MiB | `E_TOO_LARGE`, `scope` `put` |
| A commit's contents together | 1 GiB (a `blob` named by id counts nothing) | `E_TOO_LARGE`, `scope` `commit` |
| The change list's text (`--changes` file or stdin) | 64 MiB | `E_TOO_LARGE`, `scope` `request` |

The sizes are checked before any content is read, and a refused list writes nothing. A large file
goes as `file` content, which does not count against the 64 MiB of the list's text, or through
`blob put` first, which keeps it out of the commit's 1 GiB. A single file over 512 MiB cannot be
stored at all:

```sh
truncate -s 600M big.bin
echo "[{\"path\": \"big.bin\", \"op\": \"put\", \"content\": {\"file\": \"big.bin\"}}]" | swarmops commit -m "x" --changes - --human
swarmops blob put big.bin --human
```

```text
error: E_TOO_LARGE: change 0 ("big.bin"): the content is 629145600 bytes, more than the 536870912 a put may hold
details.file: big.bin
details.index: 0
details.limit: 536870912
details.path: big.bin
details.scope: put
details.size: 629145600
error: E_TOO_LARGE: the content is 629145600 bytes, more than the 536870912 a put may hold
details.file: big.bin
details.limit: 536870912
details.scope: put
details.size: 629145600
```

Both exit 1.

### What a change list refuses

Each path is named by one change only, and has no empty, `.` or `..` part. A delete needs a file
at its path; it does not delete a directory. Delete each file below it instead: a directory that
a delete leaves empty goes with it.

```sh
echo "[{\"path\": \"nope.txt\", \"op\": \"delete\"}]" | swarmops commit -m "x" --changes - --human
echo "[{\"path\": \"docs\", \"op\": \"delete\"}]" | swarmops commit -m "x" --changes - --human
echo "[{\"path\": \"../x\", \"op\": \"put\", \"content\": {\"inline\": \"\"}}]" | swarmops commit -m "x" --changes - --human
```

```text
error: E_PATH_NOT_FOUND: nope.txt: there is no file to delete there
details.path: nope.txt
error: E_TYPE_CONFLICT: docs: a file and a directory conflict there
details.path: docs
error: E_INVALID_ARG: change 0 ("../x"): not a repository path: path has a `.` or `..` component
details.index: 0
details.path: ../x
```

They exit 6, 1 and 2. An error about one change names it by its `index` in the list, from 0, and
its `path`.

## Record a changeset that changes nothing

A change list that changes nothing is refused, unless you pass `--allow-empty`:

```sh
echo "[]" | swarmops commit -m "checkpoint" --changes - --human
echo "[]" | swarmops commit -m "checkpoint" --changes - --allow-empty --human
```

```text
error: E_EMPTY_COMMIT: the change list changes nothing
hint: pass --allow-empty (allow_empty over MCP) to record a changeset that changes nothing
changeset: 483992a7615836d3898ade34af8efa1cb8076de326a3858f2893b805a6e6ab6e
parent: 68cc12a030d5d5cc2b295716fca66de5ecc01965c781d03dc328be70c7a1ca01
root: d0c3b6d581cde3495a792c626ea02d30db022d771431604b0b1883ca43a65da2
files_changed: 0
blobs_written: 0
replayed: false
```

The first exits 1. The changeset the second records has its parent's root tree. Like every
commit without `--advance`, it moves no ref.

## Publish without a land

`commit --advance REF` records the changeset and moves REF to it in the same transaction: both,
or neither. The move happens only if REF still names `--parent`, so it is always a one-step
fast-forward with no merge:

```sh
BASE=$(swarmops resolve main | field result.id)
cat doc.json
swarmops commit -m "document greet" --changes doc.json --parent "$BASE" --advance main --human
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hello\"\n}\n"}}]
changeset: 4b1d176bffa0e62f9a55a2476c7b5fa0480e5cf354237160304c1f591abf7dc8
parent: 68cc12a030d5d5cc2b295716fca66de5ecc01965c781d03dc328be70c7a1ca01
root: acdd6550e5eaeb433e826694f45e80d174047efd99f7a3279626dadbe285a1b9
files_changed: 1
blobs_written: 1
advanced.name: main
advanced.old: 68cc12a030d5d5cc2b295716fca66de5ecc01965c781d03dc328be70c7a1ca01
advanced.new: 4b1d176bffa0e62f9a55a2476c7b5fa0480e5cf354237160304c1f591abf7dc8
advanced.already_applied: false
replayed: false
```

Give `--parent` as the full id you read. A ref name resolves when the command runs, and without
`--parent` the commit builds on REF's head as it is then: the move then catches only a race from
that moment on. The MCP server refuses `advance` without a full 64-hex `parent`.

Use `--advance` when you know nobody else writes the ref, as when you start a store. When other
agents publish too, land: `--advance` fails as soon as anyone else moved the ref, while `land`
merges your change onto wherever the ref went.

### Two agents publish at once

Two agents read the same head and publish with `--advance` at the same time:

```sh
echo '[{"path": "src/time.rs", "op": "put", "content": {"inline": "// time\n"}}]' > time.json
echo '[{"path": "src/name.rs", "op": "put", "content": {"inline": "// name\n"}}]' > name.json
BASE=$(swarmops resolve main | field result.id)
swarmops --as agent/worker-1 commit -m "add time" --changes time.json --parent "$BASE" --advance main > one.json &
swarmops --as agent/worker-2 commit -m "add name" --changes name.json --parent "$BASE" --advance main > two.json &
wait
cat one.json two.json
```

```text
{"ok":true,"op":"commit","api":1,"result":{"changeset":"94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4","parent":"4b1d176bffa0e62f9a55a2476c7b5fa0480e5cf354237160304c1f591abf7dc8","root":"b3af61a7d63de4d76ddccd632fb583d13b7fb4dd1c0eb14c37329ffea3cc5342","files_changed":1,"blobs_written":1,"advanced":{"name":"main","old":"4b1d176bffa0e62f9a55a2476c7b5fa0480e5cf354237160304c1f591abf7dc8","new":"94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4","already_applied":false},"replayed":false},"meta":{"repo":"…",…}}
{"ok":false,"op":"commit","api":1,"error":{"code":"E_CAS_MISMATCH","message":"ref main moved to 94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4, so this commit recorded nothing: commit your change on top of it (--parent 94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4 --advance main), re-applying your edits to the content at 94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4 rather than re-sending your old files","retryable":true,"exit":4,"details":{"actual":"94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4","committed":false,"name":"main"},"hint":"swarmops show 94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4"},"meta":{"repo":"…",…}}
```

One wins; which one is up to timing. The other gets `E_CAS_MISMATCH` (exit 4), and its commit
recorded nothing (`details.committed: false`). `details.actual` is what `main` names now. The
loser lands instead, which merges its change onto the winner's:

```sh
C=$(swarmops --as agent/worker-2 commit -m "add name" --changes name.json --parent "$BASE" | field result.changeset)
swarmops --as agent/worker-2 land "$C" --key "$(uuid7)" --human
```

```text
landed 420f0120b59af77899efc1840000f66dc491504e2b67b09d4750941cffdedc38 on main (onto 94ec8bff30f7aa4da79356b7636c7f4e1f2fe8bc1e5dc01c0eac7cc3f2ed7ab4, attempt 1)
```

## Make a retry safe

A command can die, or its answer can be lost, after the store recorded its work. A key makes
running it again safe: the rerun finds what the first run recorded and says so, instead of
recording it twice.

### Commit keys

`commit --key` takes a uuid v7. The commit signs at the key's time, to the second, and records
the key as its `commit_key` metadata, so the same request on the same parent makes the same
changeset. The store holds it already, so the rerun writes nothing and says `replayed: true`:

```sh
echo '[{"path": "CHANGELOG.md", "op": "put", "content": {"inline": "# Changelog\n"}}]' > log.json
BASE=$(swarmops resolve main | field result.id)
KEY=$(uuid7)
swarmops commit -m "add a changelog" --changes log.json --parent "$BASE" --key "$KEY" --human
swarmops commit -m "add a changelog" --changes log.json --parent "$BASE" --key "$KEY" --human
```

```text
changeset: 5c3acbb85210a1b9a49b610baea1ec5142efacfc2a1d2fd7f1d408ffc99c814d
parent: 420f0120b59af77899efc1840000f66dc491504e2b67b09d4750941cffdedc38
root: a82fd41f003f9b8336ebeee0d2a61266e59d2a121493cb30d2fbb9f935be62aa
files_changed: 1
blobs_written: 1
replayed: false
changeset: 5c3acbb85210a1b9a49b610baea1ec5142efacfc2a1d2fd7f1d408ffc99c814d
parent: 420f0120b59af77899efc1840000f66dc491504e2b67b09d4750941cffdedc38
root: a82fd41f003f9b8336ebeee0d2a61266e59d2a121493cb30d2fbb9f935be62aa
files_changed: 1
blobs_written: 0
replayed: true
```

Rules for commit keys:

- Make a new key for each commit, and give it again only to retry that commit.
- Give `--parent` as a full id. Without it, a retry after `main` moved commits again, on the new
  head.
- With `--advance`, a ref whose head carries the key is a replay too.
- Anything but a uuid v7 is refused:

```sh
swarmops commit -m "add a changelog" --changes log.json --parent "$BASE" --key 4f1c2a50-2b7e-4e43-9a51-3c1d2e0f9b11 --human
```

```text
error: E_INVALID_ARG: idempotency_key "4f1c2a50-2b7e-4e43-9a51-3c1d2e0f9b11" is not a uuid v7: its version digit is 4, not 7
details.idempotency_key: 4f1c2a50-2b7e-4e43-9a51-3c1d2e0f9b11
hint: make a new uuid v7 for each commit, and give it again only to retry it
```

It exits 2.

### Land keys

`land --key` takes 1 to 64 letters, digits and `-`, such as a uuid v7. A land with a key first
looks down the target's first parents, as far as the change's base, for a changeset landed with
the same key. One squashed from the same head is the earlier land: the rerun replays it and
writes nothing.

```sh
C=$(swarmops commit -m "add a changelog" --changes log.json --parent "$BASE" --key "$KEY" | field result.changeset)
LAND_KEY=$(uuid7)
swarmops land "$C" --key "$LAND_KEY" --human
swarmops land "$C" --key "$LAND_KEY" | field result.status
```

```text
landed ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095 on main (onto 420f0120b59af77899efc1840000f66dc491504e2b67b09d4750941cffdedc38, attempt 1)
replayed
```

The first line replays the commit from the section above (same key, same parent), so `C` is
the changeset made there. A key that landed another change is refused:

```sh
echo '[{"path": "LICENSE", "op": "put", "content": {"inline": "MIT\n"}}]' > licence.json
D=$(swarmops commit -m "add a licence" --changes licence.json --parent "$BASE" | field result.changeset)
swarmops land "$D" --key "$LAND_KEY" --human
```

```text
error: E_INVALID_ARG: land key "01a160e4-4c48-7bfa-8d7c-c65e2fe68a29" landed another change already, as ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095
details.key: 01a160e4-4c48-7bfa-8d7c-c65e2fe68a29
details.landed: ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095
hint: swarmops show ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095
```

It exits 2. A land that failed recorded nothing, so its key stays free: keep the same key when
you land the same change again after a conflict, as [Resolve a conflict](#resolve-a-conflict)
does. `show` gives a landed changeset's key as `land_key`, and `git export` writes it as a
`SwarmOps-Land-Key:` trailer.

## Land a chain of commits

An agent can commit several times before it lands, each commit on the one before. `land` takes
the last one and squashes the chain since the base into one changeset:

```sh
for f in a b c d; do echo "[{\"path\": \"src/$f.rs\", \"op\": \"put\", \"content\": {\"inline\": \"// $f\\n\"}}]" > $f.json; done
BASE=$(swarmops resolve main | field result.id)
C1=$(swarmops commit -m "add a" --changes a.json --parent "$BASE" | field result.changeset)
C2=$(swarmops commit -m "add b" --changes b.json --parent "$C1" | field result.changeset)
C3=$(swarmops commit -m "add c" --changes c.json --parent "$C2" | field result.changeset)
swarmops land "$C3" -m "add a, b and c" --key "$(uuid7)" | field result
```

```text
{
  "status": "landed",
  "landed": "e1fbe42920f001ce52e9cc1e36c12a2733f82d97dc711af3f52a20c9bc5f605e",
  "rebased_onto": "ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095",
  "target": "main",
  "base": "ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095",
  "attempts": 1,
  "relocated": false,
  "files_changed": 3,
  "squashed_from": [
    "8dcfbfb4873069528846ead48d14b882801178b9f8795c22de7162e0899026ef",
    "bcc5485cb1d2a35567122488ae82fb3a7fe084b918a0d3381bdd279a6e21ab62",
    "bfd3c0ad9bc635cdcd5303b4ee38fa5620b1fd3e21e1ad886276d3d73c299826"
  ]
}
```

To go on working after a land, start from `main`'s new head. If you keep committing on your old
chain instead, land with `--base` set to the last commit you landed. Without it, the base is
still where the chain left `main` (`main` holds a squashed copy, not your commits), so the land
takes the whole chain again:

```sh
C4=$(swarmops commit -m "add d" --changes d.json --parent "$C3" | field result.changeset)
swarmops land "$C4" --base "$C3" --key "$(uuid7)" | field result
```

```text
{
  "status": "landed",
  "landed": "bda64dbd665e510c9f28b70ed7d27bcfda20ed8e0b806a732f35038a039f4723",
  "rebased_onto": "e1fbe42920f001ce52e9cc1e36c12a2733f82d97dc711af3f52a20c9bc5f605e",
  "target": "main",
  "base": "bfd3c0ad9bc635cdcd5303b4ee38fa5620b1fd3e21e1ad886276d3d73c299826",
  "attempts": 1,
  "relocated": false,
  "files_changed": 1,
  "squashed_from": [
    "13813eb1b39e07a7d79e89315d2ab727dbcb3ba5924a7b70b376238bf346e299"
  ]
}
```

## Land on a release ref

A ref is `main` or `release/<name>`. `--target` lands on a release ref, which must exist:

```sh
cat > fix.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hello!\"\n}\n"}}]
EOF
swarmops ref set release/1.0 "$BASE" --create --human
FIX=$(swarmops commit -m "fix greet" --changes fix.json --parent "$BASE" | field result.changeset)
swarmops land "$FIX" --target release/1.0 --key "$(uuid7)" --human
swarmops land "$FIX" --target release/2.0 --human
```

```text
name: release/1.0
new: ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095
already_applied: false
landed d7bb03413416624314fd8a0e15009104bdc59e48c9354e5ac5cc763351934adc on release/1.0 (onto ceb0401d18e3433059e9241785f92551c761f482f895c9a57aa8c941d2471095, attempt 1)
error: E_REV_NOT_FOUND: release/2.0: not found
details.rev: release/2.0
hint: swarmops ref list
```

The last exits 6. [Move a ref yourself](#move-a-ref-yourself) covers `ref set`.

## Two agents landing at once

Two lands at once both succeed when their changes merge. Here one agent adds a file and the
other edits `src/lib.rs`, on the same base:

```sh
echo '[{"path": "src/clock.rs", "op": "put", "content": {"inline": "// clock\n"}}]' > clock.json
cat > wave.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hello\"\n}\n\npub fn wave() {}\n"}}]
EOF
BASE=$(swarmops resolve main | field result.id)
A=$(swarmops --as agent/worker-1 commit -m "add clock" --changes clock.json --parent "$BASE" | field result.changeset)
B=$(swarmops --as agent/worker-2 commit -m "add wave" --changes wave.json --parent "$BASE" | field result.changeset)
swarmops --as agent/worker-1 land "$A" --key "$(uuid7)" > a.json &
swarmops --as agent/worker-2 land "$B" --key "$(uuid7)" > b.json &
wait
field result < a.json; field result < b.json
swarmops log -n 3 --human | grep -E "title|author"
```

```text
{
  "status": "landed",
  "landed": "d0d7f1726ce275a45137730726e19f76d1f6aa04541d70b5d4e36eddc095df34",
  "rebased_onto": "bda64dbd665e510c9f28b70ed7d27bcfda20ed8e0b806a732f35038a039f4723",
  "target": "main",
  "base": "bda64dbd665e510c9f28b70ed7d27bcfda20ed8e0b806a732f35038a039f4723",
  "attempts": 1,
  "relocated": false,
  "files_changed": 1,
  "squashed_from": [
    "f1746d37584785e592ab4c6754bc13feff4fca80aff4fe918e4cca7091170f1f"
  ]
}
{
  "status": "landed",
  "landed": "3994c02dbe4e228e0db558707b23c54b0feda4270c9e4aad6d5512fa34eb4615",
  "rebased_onto": "d0d7f1726ce275a45137730726e19f76d1f6aa04541d70b5d4e36eddc095df34",
  "target": "main",
  "base": "bda64dbd665e510c9f28b70ed7d27bcfda20ed8e0b806a732f35038a039f4723",
  "attempts": 2,
  "relocated": false,
  "files_changed": 1,
  "squashed_from": [
    "e2f0875e6356f9e8b2c0d4d81022701fa133a7f6a10086113b0f552a3cec3f68"
  ]
}
entries[0].author_agent: agent/worker-2
entries[0].title: add wave
entries[1].author_agent: agent/worker-1
entries[1].title: add clock
entries[2].author_agent: agent/worker-1
entries[2].title: add d
```

The second land found `main` moved by the first, merged again onto the new head, and published
on its second attempt: its `rebased_onto` is the first land's `landed`. Neither agent did
anything for it. A land tries `--max-attempts` times (8 by default, at most 32); if the target
moves before every one of them can publish, it fails with `E_CAS_RETRY_EXHAUSTED` (exit 4),
records nothing, and can be run again.

When both changes touch the same lines, the second land meets a conflict instead.

## Resolve a conflict

Two agents change the same line of `src/lib.rs` on the same base; the second also edits
`src/clock.rs`, which the first does not touch:

```sh
BASE=$(swarmops resolve main | field result.id)
cat hello-swarm.json hi.json
W1=$(swarmops --as agent/worker-1 commit -m "greet the swarm" --changes hello-swarm.json --parent "$BASE" | field result.changeset)
W2=$(swarmops --as agent/worker-2 commit -m "a shorter greeting" --changes hi.json --parent "$BASE" | field result.changeset)
swarmops --as agent/worker-1 land "$W1" --key "$(uuid7)" --human
KEY2=$(uuid7)
swarmops --as agent/worker-2 land "$W2" --key "$KEY2" --human
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hello, swarm\"\n}\n\npub fn wave() {}\n"}}]
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hi\"\n}\n\npub fn wave() {}\n"}},
 {"path": "src/clock.rs", "op": "put", "content": {"inline": "// clock, in UTC\n"}}]
landed 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40 on main (onto 3994c02dbe4e228e0db558707b23c54b0feda4270c9e4aad6d5512fa34eb4615, attempt 1)
error: E_CONFLICT: 1 path(s) of the change conflict with the target's head 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40: nothing was recorded
conflict: content src/lib.rs
hint: swarmops cat 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40:src/lib.rs, then swarmops land 298552d6226ffb544df36fdd210ebf2ca1450539f5680a3c3b450d3fa1e7e393 --resolve <file> (resolutions over MCP) with each path's resolution and against = 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40
```

The land exits 3 and records nothing. `main` is as the first agent left it. Only
`src/lib.rs` conflicts; `src/clock.rs` would merge as it is.

### Read the report

The JSON envelope carries the whole report in `error.details.report`:

```sh
swarmops --as agent/worker-2 land "$W2" --key "$KEY2" --pretty > conflict.json
cat conflict.json
```

```json
{
  "ok": false,
  "op": "land",
  "api": 1,
  "error": {
    "code": "E_CONFLICT",
    "message": "1 path(s) of the change conflict with the target's head 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40: nothing was recorded",
    "retryable": false,
    "exit": 3,
    "details": {
      "report": {
        "base": "3994c02dbe4e228e0db558707b23c54b0feda4270c9e4aad6d5512fa34eb4615",
        "files": [
          {
            "base_blob": "523a5aea768768d075e72372e889b37374e5eabce1de653ebd975ddd18bd31eb",
            "hunks": [
              {
                "base": {
                  "first": 3,
                  "last": 3,
                  "text": "    \"hello\"\n"
                },
                "id": 0,
                "ours": {
                  "first": 3,
                  "last": 3,
                  "text": "    \"hi\"\n"
                },
                "theirs": {
                  "first": 3,
                  "last": 3,
                  "text": "    \"hello, swarm\"\n"
                }
              }
            ],
            "kind": "content",
            "ours_blob": "062ff646b2756fd8424178d50638b773180f0ad3a02dd284a9c096e97601bbd3",
            "path": "src/lib.rs",
            "theirs_agent": "agent/worker-1",
            "theirs_blob": "d152d554939fd2ccb9a3b428a984b5236e0053d17ca47f6aea9195da5d12b8d8",
            "theirs_changeset": "10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40"
          }
        ],
        "kinds": [
          "content"
        ],
        "ours": "298552d6226ffb544df36fdd210ebf2ca1450539f5680a3c3b450d3fa1e7e393",
        "theirs": "10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40",
        "truncated": false
      }
    },
    "hint": "swarmops cat 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40:src/lib.rs, then swarmops land 298552d6226ffb544df36fdd210ebf2ca1450539f5680a3c3b450d3fa1e7e393 --resolve <file> (resolutions over MCP) with each path's resolution and against = 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40"
  },
  "meta": {
    "repo": "…",
    …
  }
}
```

How to read it:

- **`ours`** is your change's head; **`theirs`** is the target's head your change met;
  **`base`** is where your change starts.
- **`files`** has one entry for each conflicting file. Its `kind` says what kind of conflict it
  is (table below). `base_blob`, `ours_blob` and `theirs_blob` are the file's content on each
  side; a side without a file there has none.
- **`hunks`** are the places where both sides edited a text file's lines. Each side's lines are
  numbered from 1, both ends included, as `cat --lines` takes them. A side with no lines there
  has `last` one less than `first`.
- **`theirs_changeset`** and **`theirs_agent`** name the newest changeset on the target that
  changed the file, and who wrote it: what to read, and whom to ask.
- **`auto_merged`** lists the paths both sides changed that merged cleanly; it is left out when
  there are none.
- **`truncated`** is true when hunk texts were left out to stay within `--max-conflict-bytes`
  (32768 by default). Line numbers and blob ids are always there; read the texts with `cat`.

| `kind` | What happened |
| --- | --- |
| `content` | Both sides edited the same lines of a text file, or lines next to each other. |
| `binary` | Both sides changed a file that is not text, or a symlink, differently. |
| `delete_modify` | Your change deleted the file; the target changed it. |
| `modify_delete` | Your change changed the file; the target deleted it. |
| `add_add` | Both sides added the file, with contents that do not merge. |
| `mode` | Both sides changed the file's kind (file, executable or symlink), differently. |
| `type_change` | One side has a file where the other has a directory. |

Read the other side's lines at `theirs`:

```sh
THEIRS=$(field error.details.report.theirs < conflict.json)
swarmops cat "$THEIRS:src/lib.rs" --lines 3-3 --human
```

```text
blob: d152d554939fd2ccb9a3b428a984b5236e0053d17ca47f6aea9195da5d12b8d8
size: 88
kind: file
encoding: utf8
content: |
      "hello, swarm"
truncated: false
offset: 49
lines.first: 3
lines.last: 3
```

### Write the resolutions

Resolutions are a JSON object: `against`, the report's `theirs` by its full id, and `files`,
what to make of each path. Each path takes exactly one of these:

| Resolution | Result |
| --- | --- |
| `{"take": "ours"}` | The file as your change has it. |
| `{"take": "theirs"}` | The file as the target has it. |
| `{"content": CONTENT}` | New content, in any form a change list takes (`inline`, `base64`, `file`, `blob`), within a commit's limits. An optional `"mode"` sits beside it; without one, the kind ours has there, else theirs', else a regular file. |
| `{"delete": true}` | No file at the path, nor below it. |

A resolution decides its path and every path below it. The rest of the merge stands as it
merged. Here the agent keeps both intents in new content, and lands again with the same key:

```sh
cat > resolve.json <<EOF
{"against": "$THEIRS",
 "files": {"src/lib.rs": {"content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n\npub fn wave() {}\n"}}}}
EOF
swarmops --as agent/worker-2 land "$W2" --resolve resolve.json --key "$KEY2" --human
swarmops cat main:src/lib.rs --lines 3-3 | field result.content
swarmops show main --human | grep -E "^(author|lander|message|changes\[[0-9]\]\.path)"
```

```text
landed 93251c23120dee2e0ec58d32a64bd5ad6d310841a9a7451ced551c6bd66ad8bf on main (onto 10c18b5463b47eadcf50505cf1d80845551f87f6ffa65bd8fceeb02a264aad40, attempt 1)
    "hi, swarm"

author.agent: agent/worker-2
author.run_id: run-0042
lander.agent: agent/worker-2
lander.run_id: run-0042
message: a shorter greeting
changes[0].path: src/clock.rs
changes[1].path: src/lib.rs
```

The landed changeset holds both files: the resolved `src/lib.rs` and the clean `src/clock.rs`.
No checkout was made at any point. Through MCP, the `land` tool takes the same object as its
`resolutions` argument, and refuses an `against` that is not a full 64-hex id.

`take` takes the whole file from one side. `{"take": "ours"}` drops every edit the target made
to that file, also on lines that did not conflict. Write `content` when both sides' edits
should stay.

### When a resolution is stale

A resolution applies only while the target holds its path as `against` did. If another agent
changes the path in between, the resolution is stale, and the land fails again with a fresh
report. Here three agents edit `src/lib.rs`: worker-1 and worker-2 the doc comment on line 1,
worker-3 a line further down. Worker-2 gets a report, then worker-3 lands before worker-2's
resolution:

```sh
cat > s1.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Greets.\npub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n\npub fn wave() {}\n"}}]
EOF
cat > s2.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hi.\npub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n\npub fn wave() {}\n"}}]
EOF
cat > s3.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "/// Says hello.\npub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n\n/// Waves.\npub fn wave() {}\n"}}]
EOF
BASE=$(swarmops resolve main | field result.id)
X1=$(swarmops --as agent/worker-1 commit -m "reword the doc" --changes s1.json --parent "$BASE" | field result.changeset)
X2=$(swarmops --as agent/worker-2 commit -m "say hi" --changes s2.json --parent "$BASE" | field result.changeset)
X3=$(swarmops --as agent/worker-3 commit -m "document wave" --changes s3.json --parent "$BASE" | field result.changeset)
swarmops --as agent/worker-1 land "$X1" --key "$(uuid7)" --human
KEY3=$(uuid7)
THEIRS=$(swarmops --as agent/worker-2 land "$X2" --key "$KEY3" | field error.details.report.theirs)
swarmops --as agent/worker-3 land "$X3" --key "$(uuid7)" --human
echo "{\"against\": \"$THEIRS\", \"files\": {\"src/lib.rs\": {\"take\": \"ours\"}}}" > take-ours.json
swarmops --as agent/worker-2 land "$X2" --resolve take-ours.json --key "$KEY3" > stale.json
field error.message < stale.json; field error.details.report.stale < stale.json
```

```text
landed 50dd89fcf610c05d78d6f3667b299bb0334f8ebc88490d3c2b0426083cfd8d7c on main (onto 93251c23120dee2e0ec58d32a64bd5ad6d310841a9a7451ced551c6bd66ad8bf, attempt 1)
landed 954b553d7a25705b80dac63b499d7f2c2df374ea11f376d4880e99c6f9af52c8 on main (onto 50dd89fcf610c05d78d6f3667b299bb0334f8ebc88490d3c2b0426083cfd8d7c, attempt 1)
1 path(s) of the change conflict with the target's head 954b553d7a25705b80dac63b499d7f2c2df374ea11f376d4880e99c6f9af52c8, and 1 resolution(s) are stale, as it holds their path(s) otherwise than their against: nothing was recorded
[
  "src/lib.rs"
]
```

The land exits 3 and records nothing. The report's `stale` names the path, and its `theirs` is
the target's new head. Read the path there, decide again, and resolve against the new `theirs`:

```sh
THEIRS=$(field error.details.report.theirs < stale.json)
echo "{\"against\": \"$THEIRS\", \"files\": {\"src/lib.rs\": {\"take\": \"ours\"}}}" > take-ours.json
swarmops --as agent/worker-2 land "$X2" --resolve take-ours.json --key "$KEY3" --human
swarmops cat main:src/lib.rs | field result.content
```

```text
landed 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9 on main (onto 954b553d7a25705b80dac63b499d7f2c2df374ea11f376d4880e99c6f9af52c8, attempt 1)
/// Says hi.
pub fn greet() -> &'static str {
    "hi, swarm"
}

pub fn wave() {}

```

This run also shows the cost of `take`. Worker-3's `/// Waves.` line merged cleanly, yet it is
gone: `{"take": "ours"}` took worker-2's whole file. New `content` that keeps worker-3's line
would have kept it.

A conflict left without a resolution fails the land too. A resolution for a path that does not
conflict applies all the same, while the target holds that path as `against` did.

## Move a ref yourself

`land` and `commit --advance` move refs for you. `ref set` and `ref delete` move them directly,
by compare-and-swap. Use them to create a release ref, point it at a changeset, or remove it.
Here an operator does it:

```sh
export SWARMOPS_AGENT=operator/ada
swarmops ref list --human
OLD=$(swarmops resolve release/1.0 | field result.id)
NEW=$(swarmops resolve main | field result.id)
swarmops ref set release/1.0 "$NEW" --expect "$OLD" --human
swarmops ref set release/1.0 "$NEW" --expect "$OLD" --human
```

```text
refs[0].name: main
refs[0].target: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
refs[0].version: 16
refs[1].name: release/1.0
refs[1].target: d7bb03413416624314fd8a0e15009104bdc59e48c9354e5ac5cc763351934adc
refs[1].version: 2
name: release/1.0
old: d7bb03413416624314fd8a0e15009104bdc59e48c9354e5ac5cc763351934adc
new: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
already_applied: false
name: release/1.0
old: d7bb03413416624314fd8a0e15009104bdc59e48c9354e5ac5cc763351934adc
new: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
already_applied: true
```

`version` counts the committed moves of a ref. The same move made again finds the ref in the
requested state already, writes nothing and says `already_applied: true`, so it is safe to
retry.

To move a ref past several changesets at once, commit each with `--parent` on the one before,
then move the ref once: `swarmops ref set <ref> <last> --expect <old>` (or `--create`).

`ref set` takes exactly one of the two:

- **`--expect OLD`** moves an existing ref, only if it names `OLD` now.
- **`--create`** creates a ref. If the ref holds the same target already, that is a replay; at
  another target it is `E_CAS_MISMATCH`.

```sh
swarmops ref set release/2.0 "$NEW" --create --human
swarmops ref set release/2.0 "$OLD" --create --human
swarmops ref set feature "$NEW" --create --human
```

```text
name: release/2.0
new: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
already_applied: false
error: E_CAS_MISMATCH: ref release/2.0 exists already and points at 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9: commit your change on top of it (--parent 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9), then retry with --expect 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9; re-apply your edits to the content at 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9 rather than re-sending your old files
details.actual: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
details.name: release/2.0
retryable: true
hint: swarmops show 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
error: E_REF_NAME_INVALID: invalid ref name "feature"
details.name: feature
hint: a ref name is main or release/<name>, the name of letters, digits, '.', '_' and '-', not starting with '.'
```

They exit 0, 4 and 1.

`ref delete` always takes `--expect`:

```sh
swarmops ref delete release/2.0 --expect "$OLD" --human
swarmops ref delete release/2.0 --expect "$NEW" --human
swarmops op log -n 1 --human
```

```text
error: E_CAS_MISMATCH: ref release/2.0 now points at 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9; re-check before deleting
details.actual: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
details.name: release/2.0
retryable: true
hint: swarmops show 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
name: release/2.0
old: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
already_applied: false
ops[0].op: 38
ops[0].kind: ref_move
ops[0].changesets: []
ops[0].changesets_total: 0
ops[0].ref_moves[0].name: release/2.0
ops[0].ref_moves[0].old: 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9
ops[0].ref_moves[0].actor: operator/ada
ops[0].ref_moves[0].at: 1792626970
truncated: true
```

`op log` records who moved the ref, and `swarmops op undo <op>` moves it back.

Give `--expect` as the full id you read. A ref name, or a relative revision such as `main~1`, is
read when the command runs, so it does not pin what you saw: the ref's own name always matches,
and the compare-and-swap protects nothing. An id prefix pins it only until another changeset
shares the prefix. The MCP server refuses all three.

## When a compare-and-swap fails

`E_CAS_MISMATCH` (exit 4, `retryable: true`) means a ref did not name what you said it named:
someone moved it first. Nothing was recorded. `details.actual` is what the ref names now, and
`details.name` is the ref:

```sh
swarmops ref set release/1.0 "$OLD" --expect "$OLD"
```

```text
{"ok":false,"op":"ref_set","api":1,"error":{"code":"E_CAS_MISMATCH","message":"ref release/1.0 moved to 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9: commit your change on top of it (--parent 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9), then retry with --expect 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9; re-apply your edits to the content at 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9 rather than re-sending your old files","retryable":true,"exit":4,"details":{"actual":"36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9","name":"release/1.0"},"hint":"swarmops show 36f80fb0ef55b7746efd722256370ac342d33a834b449d5295d8192fd30db3b9"},"meta":{"repo":"…",…}}
```

What to do next:

1. **Look at what won.** `swarmops show <actual>` says who moved the ref and what changed.
2. **To publish your change, land it.** Commit it on the head you read, without `--advance`,
   and `land` it: the land merges it three-way onto `<actual>`, however far the ref moved. This
   is what [Two agents publish at once](#two-agents-publish-at-once) does.
3. **To publish without a land,** read the files you changed at `<actual>`, apply your edits to
   them again, and commit with `--parent <actual> --advance <ref>`, or commit with
   `--parent <actual>` and retry `ref set` with `--expect <actual>`.
4. **To delete,** check `<actual>` first, then delete with `--expect <actual>`.

Do not send your old change list again on top of `<actual>`. A put carries the whole file, so
your old puts would undo the winner's edits to every file you both changed.

## Errors you will meet

| Code | Exit | When | What to do |
| --- | --- | --- | --- |
| `E_IDENTITY_REQUIRED` | 5 | A write without `--as` or `SWARMOPS_AGENT`. | Name the agent. |
| `E_INVALID_ARG` | 2 | A bad agent name, path, key or option value; a land key used for another change. | Fix the request; `details` names the field. |
| `E_EMPTY_COMMIT` | 1 | A change list that changes nothing. | Pass `--allow-empty` if you meant it. |
| `E_TOO_LARGE` | 1 | A content, a commit or a change list past its limit. | Use `file` content or `blob put`; `details.scope` says which limit. |
| `E_PATH_NOT_FOUND` | 6 | A delete of a path with no file. | Check the path at the parent with `ls`. |
| `E_TYPE_CONFLICT` | 1 | A delete of a directory, or a put where a file and a directory meet. | Delete the files below the directory; check the paths. |
| `E_REV_NOT_FOUND` | 6 | A revision or target ref that does not exist. | `swarmops ref list`. |
| `E_REF_NAME_INVALID` | 1 | A ref name that is not `main` or `release/<name>`. | Use one of those. |
| `E_NOTHING_TO_LAND` | 1 | The target has the change already. | Nothing; use `--key` to get a replay instead. |
| `E_CONFLICT` | 3 | The change does not merge onto the target, or a resolution is stale. | [Resolve a conflict](#resolve-a-conflict). |
| `E_CAS_MISMATCH` | 4 | A ref moved before your `--advance`, `ref set` or `ref delete`. | [When a compare-and-swap fails](#when-a-compare-and-swap-fails). |
| `E_CAS_RETRY_EXHAUSTED` | 4 | The target moved before every one of a land's attempts. | Run the land again, or raise `--max-attempts`. |
| `E_BUSY` | 4 | Another writer held the store's lock for the whole wait. | Run it again. |

`swarmops schema` gives every request and response type, and the README's
[Output and exit codes](../../README.md#output-and-exit-codes) explains the envelope.
