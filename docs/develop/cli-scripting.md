# Scripting the CLI

Scripts, CI jobs and agents that do not speak MCP drive SwarmOps through the `swarmops` command
(or `so`, the same program by a shorter name). Every command prints one JSON envelope on stdout
and exits with a code that says what kind of failure it was, so a script needs no text parsing:
read the exit code, then the envelope's fields.

This page covers the envelope, the output flags, the exit codes, `jq` recipes for common tasks,
which errors to retry and how, idempotency keys, and a complete script that commits and lands in a
loop. Every command and output below is from a real run of `swarmops` 0.1.0 against scratch stores
under `/tmp/dev-guide/`, with `jq` 1.7.1. `…` marks text cut by hand.

- [The envelope](#the-envelope)
- [Output flags](#output-flags)
- [Exit codes](#exit-codes)
- [jq recipes](#jq-recipes)
- [Retry rules](#retry-rules)
- [Idempotency keys](#idempotency-keys)
- [A complete example: commit and land in a loop](#a-complete-example-commit-and-land-in-a-loop)

## Setting the store and the identity

Every command takes the store from `--repo <store>`, else `SWARMOPS_REPO`. A command that writes
(a commit, a land, a ref move, an undo, a git import) also needs an identity: `--as
<family>/<instance>`, else `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID` naming the agent's run if it
has one. A script usually exports both once:

```sh
export SWARMOPS_REPO=/tmp/dev-guide/demo
export SWARMOPS_AGENT=agent/worker-1
```

A variable set to the empty string counts as unset. An explicit empty `--repo ""` names no store and
does not fall back to `SWARMOPS_REPO`, so an unset shell variable in `--repo "$X"` cannot pick
another store.

## The envelope

```sh
swarmops info
```

```json
{"ok":true,"op":"repo_info","api":1,"result":{"repo_id":"94b6b112787a77d96c23be442f233f0d","name":"acme","main":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118","store_version":2,"changesets":2,"refs":1},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

| Field | What it holds |
| --- | --- |
| `ok` | `true` on success, `false` on failure. |
| `op` | The operation's name, which is also its MCP tool's name: `cat` is `read_file`, `ls` is `read_tree`, `info` is `repo_info`. |
| `api` | The API version, `1`. |
| `result` | The operation's response; present exactly when `ok` is true. `swarmops schema <Type>` gives its fields. |
| `error` | The failure; present exactly when `ok` is false (never `null`). |
| `meta.repo` | The store as you named it; omitted when the command used no store. |
| `meta.elapsed_ms` | How long the command took. |
| `meta.resolved` | Every revision the command resolved: the string you gave and the full changeset id it named. |
| `meta.auth` | The store's authentication mode (`trust`). |
| `meta.recovered` | Present only after a `commit`, `ref set` or `ref delete` whose write first repaired what an earlier, crashed writer left: what it repaired, on success and on `E_CAS_MISMATCH` alike. It is no warning. |

A failure's `error`:

```sh
swarmops --as agent/worker-1 ref set main 0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0 --expect 0e5171a021aa
```

```json
{"ok":false,"op":"ref_set","api":1,"error":{"code":"E_CAS_MISMATCH","message":"ref main moved to bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118: commit your change on top of it (--parent bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118), then retry with --expect bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118; re-apply your edits to the content at bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118 rather than re-sending your old files","retryable":true,"exit":4,"details":{"actual":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118","name":"main"},"hint":"swarmops show bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118"},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{"0e5171a021aa":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0","0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0":"0e5171a021aabc37bf6b92efa555514c389d582c32542f16ae88cd43873f60c0"},"auth":"trust"}}
```

| Field | What it holds |
| --- | --- |
| `code` | A stable `E_*` code. Match on this, never on `message`. |
| `message` | What happened, for a person. |
| `retryable` | Whether running it again can succeed, as it is or after re-reading what the error names. |
| `exit` | The process exit code. |
| `details` | Machine-readable context: `actual` for `E_CAS_MISMATCH`, `report` for `E_CONFLICT`, `fs_op` and `store_file` for an I/O failure (a file of the store, never a host path); `{}` when there is none. |
| `hint` | Often the next command to run; `null` when there is none. |

`swarmops schema` prints the JSON Schema of every request and response type and of the error
codes; `swarmops schema <Type>` prints one.

Hints and messages name the program `swarmops`, whichever name ran it, with two exceptions that
name it as you invoked it: a usage error's hint, the usage line (`Usage: so …`), and the message
for a missing subcommand (`'so' requires a subcommand but one was not provided`).

Two runs print no envelope. `derive --follow` prints one envelope a line for each run that derived
something, and `mcp` speaks MCP on stdout. And a run that meets one of the store's four documented
aborts prints no envelope at all: the process aborts, saying why on stderr. A script should treat
stdout that is not one JSON object as a failure of its own.

## Output flags

| Flag | Effect |
| --- | --- |
| `--json` | The JSON envelope: the default, unless `SWARMOPS_FORMAT` is `human`. |
| `--human` | A short text rendering instead, for a person. |
| `--pretty[=BOOL]` | Pretty-print the JSON. The default is pretty on a terminal and compact otherwise, so a pipe gets one line. |
| `SWARMOPS_FORMAT=json\|human` | The format when neither `--json` nor `--human` is given; any other value is `E_INVALID_ARG`. |

`--json` and `--human` may both appear; the last one wins. Scripts should pass `--json`
explicitly, or leave `SWARMOPS_FORMAT` unset, so that a user's `SWARMOPS_FORMAT=human` cannot
change what the script parses. The exit code is the same whatever the format.

```sh
swarmops info --human
swarmops info --pretty
SWARMOPS_FORMAT=human swarmops log -n 2
swarmops cat main:nope.rs --human; echo "exit=$?"
```

```text
repo_id: 94b6b112787a77d96c23be442f233f0d
name: acme
main: bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118
store_version: 1
changesets: 2
refs: 1
{
  "ok": true,
  "op": "repo_info",
  "api": 1,
  "result": {
    "repo_id": "94b6b112787a77d96c23be442f233f0d",
    "name": "acme",
    "main": "bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118",
    "store_version": 1,
    "changesets": 2,
    "refs": 1
  },
  "meta": {
    "repo": "/tmp/dev-guide/acme",
    "elapsed_ms": 0,
    "resolved": {},
    "auth": "trust"
  }
}
entries[0].id: bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118
entries[0].parents: []
entries[0].author_agent: operator/ada
entries[0].created_at: 1792626751000
entries[0].title: first
truncated: false
error: E_PATH_NOT_FOUND: nothing is at "nope.rs" in main
details.path: nope.rs
details.rev: main
hint: swarmops ls main
exit=6
```

## Exit codes

The exit code is `error.exit`, and 0 on success:

| Exit | Meaning | Codes |
| --- | --- | --- |
| 0 | Success. | |
| 1 | The operation failed. | `E_EXISTS`, `E_AMBIGUOUS_ID`, `E_IS_DIR`, `E_TOO_LARGE`, `E_EMPTY_COMMIT`, `E_TYPE_CONFLICT`, `E_REF_NAME_INVALID`, `E_STORE_CORRUPT`, `E_UNSUPPORTED_FS`, `E_IO`, `E_INTERNAL`, `E_NOTHING_TO_LAND`, `E_NOTHING_TO_UNDO`, `E_GIT_IMPORT_LOSSY`, `E_GIT_UNSUPPORTED`, `E_GIT_READ`, `E_GIT_WRITE`, `E_WS_EXISTS`, `E_WS_DIRTY`, `E_PATH_NOT_EMPTY`, `E_UNRESOLVED_CONFLICTS`, `E_MARKERS_PRESENT`, `E_WS_UNPORTABLE`, `E_WS_OUTSIDE_PREFIXES`, `E_WS_STALE`, `E_CHECK_FAILED` |
| 2 | A malformed request or command line. | `E_INVALID_ARG` |
| 3 | A conflict with what the change lands on: resolve it, then run it again. | `E_CONFLICT`, `E_GIT_IMPORT_DIVERGED`, `E_GIT_EXPORT_DIVERGED`, `E_REMOTE_DIVERGED` |
| 4 | Retryable: it can succeed if run again. | `E_CAS_MISMATCH`, `E_CAS_RETRY_EXHAUSTED`, `E_BUSY`, `E_IO_TRANSIENT`, `E_MAINTENANCE_BUSY`, `E_REMOTE_UNREACHABLE`, `E_WS_LOCKED` |
| 5 | An identity or policy requirement refused it. | `E_IDENTITY_REQUIRED`, `E_AUTH`, `E_POLICY_DENIED`, `E_REMOTE_REJECTED` |
| 6 | Something it names does not exist. | `E_NOT_A_REPO`, `E_REV_NOT_FOUND`, `E_PATH_NOT_FOUND`, `E_UNKNOWN_PARENT`, `E_REF_NOT_FOUND`, `E_OP_NOT_FOUND`, `E_GIT_NOT_A_REPO`, `E_GIT_REF_NOT_FOUND`, `E_REMOTE_NOT_FOUND`, `E_WS_NOT_FOUND` |

`E_REF_NOT_FOUND`, `E_AUTH`, `E_POLICY_DENIED` and `E_MAINTENANCE_BUSY` are not raised yet; they are
in the schema so that a script can match them before any operation raises them
([known limitations](../known-limitations.md#serving-requests)). `ErrorCode` in
[crates/swarmops-api/src/error.rs](../../crates/swarmops-api/src/error.rs) is the one table of
each code's exit and `retryable`.

```sh
swarmops cat main:nope.rs; echo "exit=$?"
swarmops commit -m x --changes base.json; echo "exit=$?"
swarmops --frob info; echo "exit=$?"
```

```text
{"ok":false,"op":"read_file","api":1,"error":{"code":"E_PATH_NOT_FOUND","message":"nothing is at \"nope.rs\" in main","retryable":false,"exit":6,"details":{"path":"nope.rs","rev":"main"},"hint":"swarmops ls main"},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{"main":"bae3bcec1c5a0b70695b8baae3d38c34de91b270fa788268c696ed233a342118"},"auth":"trust"}}
exit=6
{"ok":false,"op":"commit","api":1,"error":{"code":"E_IDENTITY_REQUIRED","message":"this operation changes the store, so it needs the caller's identity","retryable":false,"exit":5,"details":{},"hint":"pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"},"meta":{"repo":"/tmp/dev-guide/acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
exit=5
{"ok":false,"op":"swarmops","api":1,"error":{"code":"E_INVALID_ARG","message":"unexpected argument '--frob' found","retryable":false,"exit":2,"details":{},"hint":"Usage: swarmops [OPTIONS] <COMMAND>\nFor more information, try '--help'."},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
exit=2
```

A usage error is an envelope too, on stdout, with `op` naming the command as far as it parsed.

## jq recipes

Each recipe runs against the store the [example below](#a-complete-example-commit-and-land-in-a-loop)
builds. The JSON is compact when piped, one envelope a line, which `jq` reads as it is.

```text
$ swarmops resolve main | jq -r '.result.id'
d51a93cdecb749beafb0e6073b9197bb37c015b22fe1f9c032a8e9c38e8bea3d
$ swarmops ref list | jq -r '.result.refs[] | "\(.name) \(.target)"'
main d51a93cdecb749beafb0e6073b9197bb37c015b22fe1f9c032a8e9c38e8bea3d
$ swarmops ls main --depth 3 | jq -r '.result.entries[] | select(.kind == "file") | .path'
TODO.md
docs/notes.md
src/lib.rs
$ swarmops show main | jq -c '.result.changes[] | {path, op}'
{"path":"TODO.md","op":"put"}
$ swarmops diff f9fac3d60106 main --format stat | jq -r '.result.files[] | "\(.status) \(.path) +\(.insertions) -\(.deletions)"'
added TODO.md +1 -0
added docs/notes.md +3 -0
modified src/lib.rs +1 -1
$ swarmops op log -n 3 | jq -c '.result.ops[] | {op, kind}'
{"op":7,"kind":"publish"}
{"op":6,"kind":"land"}
{"op":5,"kind":"commit"}
$ swarmops cat main:nope.rs | jq -r 'if .ok then .result.content else "\(.error.code) retryable=\(.error.retryable) exit=\(.error.exit)" end'
E_PATH_NOT_FOUND retryable=false exit=6
```

| Task | Recipe |
| --- | --- |
| The full id a ref or prefix names | `swarmops resolve REV \| jq -r '.result.id'` |
| A file's text | `swarmops cat REV:PATH \| jq -r '.result.content'` (when `.result.encoding` is `utf8`; `base64` content decodes with `jq -r '.result.content' \| base64 -d`) |
| Every file below a directory | `swarmops ls REV:DIR --depth N \| jq -r '.result.entries[] \| select(.kind == "file") \| .path'` |
| What a changeset changed | `swarmops show REV \| jq -c '.result.changes[]'` |
| Changed files between two revisions | `swarmops diff FROM TO \| jq -r '.result.files[] \| "\(.status) \(.path)"'` |
| The new changeset of a commit | `swarmops commit … \| jq -r '.result.changeset'` |
| Where a land put the target | `swarmops land … \| jq -r '.result.landed'` |
| Branch on the failure | `jq -r '.error.code // empty'` on the envelope, after a non-zero exit |

A read cut short says so: `cat` gives `truncated` and `next_offset` (read on with `--offset`), and
`log`, `ls`, `history`, `diff` give a `cursor` while there is more. This loop pages through the
whole history three entries at a time:

```sh
cat page.sh
bash page.sh
```

```text
cursor=
while :; do
  page=$(swarmops log -n 3 ${cursor:+--cursor "$cursor"})
  echo "$page" | jq -r '.result.entries[] | "\(.id[:12]) \(.author_agent) \(.title)"'
  cursor=$(echo "$page" | jq -r '.result.cursor // empty')
  [ -n "$cursor" ] || break
done
d51a93cdecb7 agent/worker-3 apply todo.json
71074a159787 agent/worker-1 apply greet.json
aacf0e0212fb agent/worker-1 apply notes.json
5cc2a136cfe0 agent/worker-2 greet the swarm
f9fac3d60106 operator/ada first
```

## Retry rules

`error.retryable` (exit 4) says that running the operation again can succeed. What "again" means
depends on the code:

| Code | What happened | What to do |
| --- | --- | --- |
| `E_BUSY` | Another writer held the store's lock for the whole wait, or commits landed during every one of a read's attempts to take a snapshot. Nothing was committed. | Run the same command again, after a short wait. |
| `E_IO_TRANSIENT` | A journal append failed and was cut back (`details.committed: false`). Nothing was committed. | Run the same command again. |
| `E_CAS_RETRY_EXHAUSTED` | The land's target moved before every one of its `--max-attempts` (8 by default, at most 32) could publish. Nothing was recorded. | Run the same land again, with the same `--key`. |
| `E_CAS_MISMATCH` | A ref no longer holds what you expected; `details.actual` holds what it does (`null` when the ref does not exist). A `commit --advance` that fails so records nothing (`details.committed: false`). | Do **not** run it again as it is. Land your change instead (`land` merges it onto the ref's head), or re-apply your edits to the files at `details.actual` and commit on that. Re-sending your old whole-file puts on top of `actual` would revert the winner's edits to shared files. |

Everything else is not retryable as it is. In particular:

- `E_CONFLICT` (exit 3): the land recorded nothing. Read `details.report`, resolve each path
  with `--resolve` against the report's `theirs`, and land again.
- `E_IO` (exit 1): the filesystem refused an operation; `details.fs_op` and `details.store_file`
  say which. It fails the same way until the cause (permissions, a full disk) is fixed.
- `E_IDENTITY_REQUIRED` (exit 5) and `E_INVALID_ARG` (exit 2): fix the command.

A retry is safe only when running the command twice cannot record your change twice. A land or a
commit with a key replays instead; one without a key, retried after a failure that may have
committed, may not. Give every commit and land a key.

## Idempotency keys

### `land --key`

A key is 1 to 64 letters, digits and `-`, such as a uuid. Make a new one for each change you land,
and give it again only to retry that land. A land with a key first looks down the ref's first
parents, to the change's base, for a changeset landed with the same key: one squashed from the same
head is the land made before, which the land replays (`status: replayed`) without writing anything;
one from another head is `E_INVALID_ARG`. Without a key, the same land run again finds its change
on the ref already: `E_NOTHING_TO_LAND`.

```text
$ BASE=$(swarmops resolve main | jq -r .result.id)
$ C=$(swarmops commit -m "extend the todo" --changes todo2.json --parent "$BASE" | jq -r .result.changeset)
$ LAND=$(uuidgen); echo "$LAND"
51ea69e6-f3d9-4246-af47-11c9e6ea53b6
$ swarmops land "$C" --key "$LAND" | jq -c ".result | {status, landed, attempts}"
{"status":"landed","landed":"fc263ee351d1e964a4c56254ac1de084418cda83459a9af6bb5cca1a5558a969","attempts":1}
$ swarmops land "$C" --key "$LAND" | jq -c ".result | {status, landed, attempts}"
{"status":"replayed","landed":"fc263ee351d1e964a4c56254ac1de084418cda83459a9af6bb5cca1a5558a969","attempts":1}
$ swarmops land "$C" | jq -c ".error | {code, details}"
{"code":"E_NOTHING_TO_LAND","details":{"reason":"no_changes"}}
```

Through MCP the same key is the `land` tool's `idempotency_key`.

### `commit --key`

A commit's key must be a uuid v7 (RFC 9562). The commit signs at the key's time, to the second, and
records the key, so the same command run again on the same parent makes the same changeset, which
the store holds already: the result says `replayed: true`, and nothing is written. Give `--parent`
as the full id you read; without it a retry after `main` moved commits again, on the new head.

`uuidgen` from util-linux 2.39.3, the one this page ran with, makes no v7, so the example uses the
`uuid7` function of the [script below](#a-complete-example-commit-and-land-in-a-loop):

```text
$ BASE=$(swarmops resolve main | jq -r .result.id); ID=$(uuid7); echo "$ID"
01a160e9-3ce0-70f7-a583-35646a0e57e2
$ swarmops commit -m "ship it" --changes todo3.json --parent "$BASE" --advance main --key "$ID" | jq -c ".result | {changeset, replayed}"
{"changeset":"3ab2b4ed1e248252ebff91589762ecef07f9bb02f5ea623cadcc3d57ea7efdb9","replayed":false}
$ swarmops commit -m "ship it" --changes todo3.json --parent "$BASE" --advance main --key "$ID" | jq -c ".result | {changeset, replayed}"
{"changeset":"3ab2b4ed1e248252ebff91589762ecef07f9bb02f5ea623cadcc3d57ea7efdb9","replayed":true}
```

## A complete example: commit and land in a loop

`publish.sh` publishes change lists that an agent prepared on one base onto `main`. For each list it:

1. commits on the base and moves `main` in the same transaction (`--advance`), the fast path while
   no one else has published;
2. on `E_CAS_MISMATCH`, which recorded nothing, commits the same list on the base without moving a
   ref, and lands it: the land merges it three-way onto `main`'s head, whatever `main` gained;
3. on `E_CONFLICT`, stops with exit 3, or, with `RESOLVE=ours` or `RESOLVE=theirs`, takes that side
   for every conflicting path and lands again;
4. retries any other exit 4 as it is, with backoff, which every call's key makes safe.

```bash
#!/usr/bin/env bash
# publish.sh BASE CHANGES.json...: publish change lists an agent prepared on BASE onto main.
# Needs SWARMOPS_REPO and SWARMOPS_AGENT, jq and python3. RESOLVE=ours|theirs settles a
# conflict by taking that side for every conflicting path; unset, a conflict stops the script.
set -u

BASE=$1
shift
MAX_TRIES=5

# A uuid v7, which commit --key takes (land takes any 1-64 letters, digits and '-').
uuid7() {
  python3 -I -c '
import os, time
ms = time.time_ns() // 1_000_000
b = bytearray(ms.to_bytes(6, "big") + os.urandom(10))
b[6] = (b[6] & 0x0F) | 0x70
b[8] = (b[8] & 0x3F) | 0x80
h = b.hex()
print(f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}")'
}

# Runs swarmops; the envelope goes to $ENV_FILE and the exit code to $RC. Exit 4 is retried
# as it is, with backoff, except E_CAS_MISMATCH, which the caller handles: every call here
# carries a key, so a retry replays rather than repeats.
run() {
  local try
  for try in $(seq 1 "$MAX_TRIES"); do
    swarmops "$@" > "$ENV_FILE"
    RC=$?

    # 1. Done, or a failure that running it again cannot fix.
    if [ "$RC" -ne 4 ]; then
      return
    fi
    if [ "$(jq -r '.error.code' "$ENV_FILE")" = E_CAS_MISMATCH ]; then
      return
    fi

    # 2. Retryable: wait, then run the same command again.
    echo "retry $try: $(jq -r '.error.code' "$ENV_FILE")" >&2
    sleep "$try"
  done
}

# Lands CHANGESET onto main; on E_CONFLICT resolves it by $RESOLVE once and lands again.
land() {
  local changeset=$1

  # 1. Land with a key of its own: a retry of this land replays it.
  run land "$changeset" --key "$(uuid7)"
  if [ "$RC" -ne 3 ] || [ -z "${RESOLVE:-}" ]; then
    return
  fi

  # 2. Conflict: take one side for every path the report names, against its theirs.
  jq --arg side "$RESOLVE" \
    '.error.details.report
     | {against: .theirs,
        files: (.files
                | map({key: .path, value: {take: $side}})
                | from_entries)}' \
    "$ENV_FILE" > "$RES_FILE"
  echo "conflict in $(jq -r '[.error.details.report.files[].path] | join(", ")' "$ENV_FILE"): taking $RESOLVE" >&2

  # 3. Land again with the resolutions, as a new land with a new key.
  run land "$changeset" --resolve "$RES_FILE" --key "$(uuid7)"
}

ENV_FILE=$(mktemp)
RES_FILE=$(mktemp)
trap 'rm -f "$ENV_FILE" "$RES_FILE"' EXIT

for changes in "$@"; do
  # 1. Fast path: commit on BASE and move main in the same transaction, if main still names BASE.
  run commit -m "apply $changes" --changes "$changes" --parent "$BASE" --advance main --key "$(uuid7)"

  # 2. main moved since BASE: nothing was recorded. Commit without moving a ref, then land.
  if [ "$RC" -eq 4 ] && [ "$(jq -r '.error.code' "$ENV_FILE")" = E_CAS_MISMATCH ]; then
    echo "main moved to $(jq -r '.error.details.actual' "$ENV_FILE"): landing instead" >&2
    run commit -m "apply $changes" --changes "$changes" --parent "$BASE" --key "$(uuid7)"
    if [ "$RC" -eq 0 ]; then
      land "$(jq -r '.result.changeset' "$ENV_FILE")"
    fi
  fi

  # 3. Report the outcome, and stop at the first change that did not publish.
  if [ "$RC" -ne 0 ]; then
    jq -c '.error | {code, message, hint}' "$ENV_FILE" >&2
    exit "$RC"
  fi
  echo "$changes: $(jq -c '.result | {op: (.status // "advanced"), main: (.landed // .advanced.new)}' "$ENV_FILE")"
done
```

The inputs: the first commit's file, a rival agent's edit to the same line, and the two change
lists `agent/worker-1` prepared on the first commit:

```sh
cat base.json rival.json notes.json greet.json
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hello\"\n}\n"}}]
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hello, swarm\"\n}\n"}}]
[{"path": "docs/notes.md", "op": "put", "content": {"inline": "# Notes\n\ngreet() says hello.\n"}}]
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hi\"\n}\n"}}]
```

The run. `agent/worker-2` publishes first, so worker-1's fast path fails with `E_CAS_MISMATCH` for
both lists: `notes.json` lands cleanly, `greet.json` conflicts on the line worker-2 changed and the
script stops. Run again with `RESOLVE=ours`, it lands worker-1's line. Last, `agent/worker-3`
publishes on the current head, and the fast path succeeds. The script's messages go to stderr,
merged here:

```text
$ swarmops init "$SWARMOPS_REPO" --name demo >/dev/null
$ swarmops --as operator/ada commit -m "first" --changes base.json --advance main | jq -c ".result.advanced"
{"name":"main","new":"f9fac3d601067622febebde455958b93c359164812a9defb957fa3cd62f26d41","already_applied":false}
$ BASE=$(swarmops resolve main | jq -r ".result.id")
$ swarmops --as agent/worker-2 commit -m "greet the swarm" --changes rival.json --parent "$BASE" --advance main | jq -c ".result.advanced"
{"name":"main","old":"f9fac3d601067622febebde455958b93c359164812a9defb957fa3cd62f26d41","new":"5cc2a136cfe06ab61e00b3d322a20a8c80bc80913cee1e92e9dfe2f9848692a7","already_applied":false}
$ SWARMOPS_AGENT=agent/worker-1 ./publish.sh "$BASE" notes.json greet.json; echo "exit=$?"
main moved to 5cc2a136cfe06ab61e00b3d322a20a8c80bc80913cee1e92e9dfe2f9848692a7: landing instead
notes.json: {"op":"landed","main":"aacf0e0212fb5a34121a8dd9a80296fc62d9a0e207e51aaf6c79ebcdb7450009"}
main moved to aacf0e0212fb5a34121a8dd9a80296fc62d9a0e207e51aaf6c79ebcdb7450009: landing instead
{"code":"E_CONFLICT","message":"1 path(s) of the change conflict with the target's head aacf0e0212fb5a34121a8dd9a80296fc62d9a0e207e51aaf6c79ebcdb7450009: nothing was recorded","hint":"swarmops cat aacf0e0212fb5a34121a8dd9a80296fc62d9a0e207e51aaf6c79ebcdb7450009:src/lib.rs, then swarmops land 84ebff1a072e50457bd50ac606a289a51b61d4d4fa5249fe4455599d80a4356e --resolve <file> (resolutions over MCP) with each path's resolution and against = aacf0e0212fb5a34121a8dd9a80296fc62d9a0e207e51aaf6c79ebcdb7450009"}
exit=3
$ SWARMOPS_AGENT=agent/worker-1 RESOLVE=ours ./publish.sh "$BASE" greet.json; echo "exit=$?"
main moved to aacf0e0212fb5a34121a8dd9a80296fc62d9a0e207e51aaf6c79ebcdb7450009: landing instead
conflict in src/lib.rs: taking ours
greet.json: {"op":"landed","main":"71074a1597873f72fd51143a29cdfdab9a10b60cc91ec8df6f00bc203a9a68c9"}
exit=0
$ SWARMOPS_AGENT=agent/worker-3 ./publish.sh "$(swarmops resolve main | jq -r ".result.id")" todo.json; echo "exit=$?"
todo.json: {"op":"advanced","main":"d51a93cdecb749beafb0e6073b9197bb37c015b22fe1f9c032a8e9c38e8bea3d"}
exit=0
$ swarmops log | jq -r ".result.entries[] | \"\(.id[:12]) \(.author_agent) \(.title)\""
d51a93cdecb7 agent/worker-3 apply todo.json
71074a159787 agent/worker-1 apply greet.json
aacf0e0212fb agent/worker-1 apply notes.json
5cc2a136cfe0 agent/worker-2 greet the swarm
f9fac3d60106 operator/ada first
$ swarmops cat main:src/lib.rs | jq -r ".result.content"
pub fn greet() -> &'static str {
    "hi"
}
```

What to know before adapting it:

- **The retry branch did not run here.** No `E_BUSY`, `E_IO_TRANSIENT` or
  `E_CAS_RETRY_EXHAUSTED` happened in this run; the branch follows the [retry rules](#retry-rules).
- **Taking one side is a policy, not a merge.** `RESOLVE=ours` drops the rival's edit to every
  conflicting path, as `src/lib.rs` shows. An agent that can do better reads the report's hunks
  (`.error.details.report.files[].hunks`, with `base`, `ours` and `theirs` lines) and resolves each
  path with `{"content": …}`, as [MCP integration](mcp.md#landing-through-mcp) does.
- **The first conflicting commit stays.** The run that stopped had committed `greet.json` as
  `84ebff1a…` before the land failed; the second run committed it again with a new key. Changesets
  are never removed, and one that no ref reaches is harmless.
- **A key is per attempt at one change.** Each call of `uuid7` makes a new key; `run` retries with
  the key it was given, so only a retry of the same call reuses one.
- **No envelope is a failure too.** On one of the store's four aborts the process prints nothing on
  stdout; the script's `jq` then reads an empty file, and its exit code is the abort's, not 0.
