`commit` records one changeset from an explicit list of changes, relative to a parent. There is
no working copy and no staging area: the change list is the whole input. Without
`--advance` it moves no ref: the change is published by landing it
([`land`](../../commands/land.md)) or by moving a ref to it ([`ref set`](../../commands/ref-set.md)).
[Commit and land](../../../guides/commit-and-land.md) walks through the whole flow.

### The parent

The changeset is built on, in this order of precedence:

1. `--parent REV`, when given: any revision, resolved once, in the snapshot the commit reads in.
2. Else, with `--advance REF`, REF's head as it is when the command runs, or nothing when REF
   does not exist (a root commit that creates REF).
3. Else `main`'s head as it is when the command runs, or nothing when `main` does not exist (a
   root commit).

A head taken by default is recorded in `meta.resolved` under the ref's name. Once `main` exists,
a root commit needs `--advance` naming a ref that does not exist yet.

Give `--parent` as the full id you read the content at. A head read at commit time is not
necessarily the one your edits were based on: another agent may have moved it in between.

### The change list

`--changes FILE` reads a JSON array of changes from FILE, or from stdin for `-`, at most 64 MiB
of text. Each change is:

```json
{"path": "src/lib.rs", "op": "put", "content": {"inline": "fn main() {}\n"}, "mode": "file"}
```

- `path`: a repository path, `/`-separated and relative, with no empty, `.` or `..` component
  and no NUL. Each path may appear in one change only.
- `op`: `put` writes the path, creating or replacing the entry; `delete` removes it and takes
  neither content nor mode.
- `content`, for a `put`, one of four forms: `{"inline": text}` (UTF-8, verbatim),
  `{"base64": text}` (standard, padded), `{"file": path}` (a file on this host, read by the
  command, a relative path taken from the current directory) or `{"blob": id}` (a blob the store
  holds already, from [`blob put`](../../commands/blob-put.md), by its 64 hex digits).
- `mode`: `file` (the default), `exec` or `symlink`, whose content is the link's target.

The whole list is checked before any content is decoded or read, and a refused list writes
nothing. [Write a change list](../../../guides/commit-and-land.md#write-a-change-list) shows
renames, moves and the refusals in full; `swarmops schema ChangeInput` gives the schema.

### Limits

| What | Limit | Past it |
| --- | --- | --- |
| The change list's text | 64 MiB | `E_TOO_LARGE`, `details.scope` `request`, before it is parsed |
| One put's content | 512 MiB | `E_TOO_LARGE`, `scope` `put` |
| A commit's contents together | 1 GiB | `E_TOO_LARGE`, `scope` `commit` |

Contents are measured by the sizes they declare (inline text's length, the bytes base64 decodes
to, a file's length) before any is read. A `blob` named by id counts toward none of them, so
large files go in with `blob put` first and are named by id.

### What is recorded

The changeset's author and committer are both your agent name, with the email
`<instance>@<family>.swarmops.local`, at the current time to the second, UTC. Its metadata holds
`agent`, `run_id` when you have one, and `commit_key` with `--key`. `files_changed` counts the
paths the changeset changes: a put of what a path holds already is no change. A list that
changes nothing is `E_EMPTY_COMMIT` (exit 1) unless `--allow-empty` is given.

### `--advance`: commit and publish in one transaction

With `--advance REF`, the changeset and REF's move to it are one write transaction, one journal
frame, where `commit` then `ref set --expect` take two. REF must be `main` or
`release/<name>`; any other name is `E_REF_NAME_INVALID`. The move is a compare-and-swap
against the parent: when REF names anything else, nothing is recorded, neither the changeset
nor the move, and the commit fails with `E_CAS_MISMATCH` (exit 4, retryable), its
`details.actual` naming what REF holds and `details.committed` `false`. The result's
`advanced` reports the move: `name`, `old` (left out when the move created REF), `new` and
`already_applied`.

`--advance` publishes without a merge: it suits a single writer of a ref, or a script that
re-applies its edits on a failure. With several agents writing at once, commit without it and
[`land`](../../commands/land.md), which merges onto whatever the ref holds by then ([Publish
without a land](../../../guides/commit-and-land.md#publish-without-a-land)). Through MCP,
`advance` needs `parent` as a full 64-hex id.

### `--key`: a retry that is safe

`--key` takes a uuid v7, any case; anything else is `E_INVALID_ARG`, before anything is read.
The commit then signs at the key's time instead of the clock's, and records the key, so the
same command run again on the same parent, by the same agent with the same run id, is the same
changeset: the second
run says `replayed: true`, with `blobs_written` 0, and writes nothing. A different agent with
the same key makes a different changeset, since the agent name is part of what is signed and
recorded. Make a
new key for each commit; [Commit keys](../../../guides/commit-and-land.md#commit-keys) and
[Idempotency keys](../../../develop/cli-scripting.md#idempotency-keys) give a generator.

### Patterns

- The two-step publish of an agent: read `main`'s id, commit on it with `--parent`, then
  [`land`](../../commands/land.md) the result with a land key ([Publish a
  change](../../../guides/commit-and-land.md#publish-a-change)).
- A chain of commits, each with `--parent` the one before, landed at once by landing the last:
  the land squashes the chain ([Land a chain of
  commits](../../../guides/commit-and-land.md#land-a-chain-of-commits)).
- Generated content from a pipe: `--changes -` reads the list from stdin. A change's `file`
  content may not then name this process's own stdin or stdout (`E_INVALID_ARG`): send such
  bytes inline or in base64.

### Failures

| Error | Exit | When |
| --- | --- | --- |
| `E_IDENTITY_REQUIRED` | 5 | Neither `--as` nor `SWARMOPS_AGENT` names an agent ([`whoami`](../../commands/whoami.md)). |
| `E_INVALID_ARG` | 2 | A malformed list, a path named twice, a bad key, a `blob` id the store holds no blob under, a `--parent` that is no revision. For a change, `details.index` and `details.path` name it. |
| `E_TOO_LARGE` | 1 | A limit in the table above. |
| `E_EMPTY_COMMIT` | 1 | The list changes nothing, without `--allow-empty`. |
| `E_TYPE_CONFLICT` | 1 | A change puts an entry below a file, or a file onto a directory. |
| `E_REV_NOT_FOUND` | 6 | `--parent` names a ref or id the store does not hold. |
| `E_CAS_MISMATCH` | 4 | With `--advance`, REF no longer names the parent. Nothing was recorded. |
| `E_BUSY` | 4 | Another writer held the store's lock for the whole wait. Nothing was committed: run it again. |

Exit 4 is retryable; [Retry rules](../../../develop/cli-scripting.md#retry-rules) say how. After
`E_CAS_MISMATCH`, do not re-send your old whole-file puts on top of `actual`: they would revert
what the winner changed in the same files.

### Performance

On the reference host of [the v0.1.0
measurement](../../../perf.md#v010--just-perf-in-a-quiet-window-2026-10-21), one commit alone
took 19 ms (`meta.elapsed_ms`, p50); the [M1 runs](../../../perf.md#m1-cli-level-2026-10-15)
found nearly all of an uncontended commit's time in its fsyncs. 32 processes committing at once
through the binary made 71.7 commits/s together, a commit taking 433 ms at the median and
621 ms at p99, mostly its wait for the store's lock, with no `E_BUSY`. What `--advance` saves
under contention is not measured yet ([publishes under 32
writers](../../../perf.md#m16--publishes-under-32-writers-one-transaction-against-two-not-measured-yet)).
