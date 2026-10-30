`op log` lists the store's operations, newest first. An operation is one write transaction: one
frame of the store's journal, numbered from 0 in commit order. Each entry says what kind of write
it was, which changesets it recorded and which refs it moved, by whom and when. It is the
store's audit trail and the input of `op undo`, which takes an operation's `id`. It needs no
identity. It waits for the store's lock, as a write does (`E_BUSY` after 10 s, retryable), so
that a number it lists is never given to another operation. It writes nothing but the repair of
the journal's tail that the next write would make, so a read-only store refuses it as it refuses
a write.

### What an operation is

Every write transaction that records something is an operation: the one of a `commit`, a
`commit --advance`, a `land`, a `ref set`, a `ref delete` or an `op undo`, those of a
`git import`, and a compaction of the changeset index, which a writer runs when the index's
unsorted tail has grown. A write that records nothing makes none: a replay (`already_applied`,
`replayed`), a refused compare-and-swap, a failed command. `derive` and `git export` write only
the store's caches and the git repository, so they are not listed either.

`kind` is read from what the operation recorded:

| Kind | The operation |
| --- | --- |
| `commit` | Recorded changesets and moved no ref (`commit` without `--advance`). |
| `publish` | Recorded one changeset and moved a ref to it (`commit --advance`). |
| `land` | Recorded one changeset whose metadata names a lander, and moved a ref to it (`land`). |
| `ref_move` | Moved refs and recorded no changeset (`ref set`, `ref delete`, `op undo`). |
| `compaction` | Recorded a compaction of the changeset index, and nothing else. |
| `mixed` | Any other mix of changesets, ref moves and compactions. |

### Reading an entry

- **`op`** is the operation's number; `--before` pages on from it.
- **`id`** is the id `op undo` takes: `<op>:<crc>`, the number and the CRC-32 of the operation's
  journal frame as 8 lowercase hex digits (`4:9f3a01c2`). An operation that took the number of
  one a failed write took back has another id.
- **`changesets`** lists at most `--max-changesets` of the changesets the operation recorded, in
  its order, each with its `id`, the `agent` that wrote it, `created_at` (the committer's time,
  in Unix **milliseconds**), the message's first line as `title`, and the land's `land_key` when
  it had one. A changeset imported from git has no agent of its own and shows as
  `unknown/unknown`, as `show` does. `changesets_total` counts all of them, also past the cap.
- **`ref_moves`** lists every ref move in its order: `name`, `old` (absent when the move created
  the ref), `new` (absent when it deleted it), the `actor` that moved it, and `at`, in Unix
  **seconds**, as the writer recorded it.

### Options

- **`--limit N`, `-n N`** (default 50): the most operations on a page; at least 1, as 0 is
  `E_INVALID_ARG` (exit 2).
- **`--before OP`**: list only operations numbered below `OP`. Paging is by number, and new
  operations get higher numbers, so they do not shift the pages below them: when `truncated` is
  `true`, pass the last `op` of the page as `--before` for the next one. `--before 0` gives an
  empty page; a number past the newest operation starts from the newest.
- **`--max-changesets N`** (default 20): the most changesets listed per operation. 0 lists none,
  and `changesets_total` still counts them.

### Patterns

Page through every operation:

```sh
before=
while :; do
  page=$(swarmops op log -n 100 ${before:+--before "$before"})
  echo "$page" | jq -c '.result.ops[] | {op, kind}'
  [ "$(echo "$page" | jq -r '.result.truncated')" = true ] || break
  before=$(echo "$page" | jq -r '.result.ops[-1].op')
done
```

Who moved `main`, and when:

```sh
swarmops op log | jq -c '.result.ops[] | .op as $op | .ref_moves[] | select(.name == "main") | {op: $op, actor, old, new, at}'
```

Before an `op undo`, list the operation again and check that its moves are the ones you mean
(below).

### Caveats

- **It waits behind writers.** A page waits for the write that holds the lock, about one
  transaction; under a long burst of writes it can return `E_BUSY` instead. Retry it.
- **A listed number keeps its operation.** `op log` lists only frames that are synced and listed
  in the journal, so neither a failed sync nor a power loss takes one back. Only the double and
  triple faults the journal's failure policy leaves open lose a listed frame
  ([Known limitations](../../../known-limitations.md#durability)).
- **Times are the writer's clock.** `at` and `created_at` are what each writer recorded, in
  different units; they need not grow with `op`.
- **The log is the journal.** Nothing prunes it: every operation since the store was created is
  listed.

### Through MCP

The MCP tool is `op_log`, with `before`, `limit` and `max_changesets`. Its description asks
agents for a `limit` of 20 or less, since clients cap long tool results.

### Performance

A page reads its frames from the journal and the object of each changeset it lists, so
`--limit` and `--max-changesets` bound its work.

### Guides

- [Reading and repairing history: See every write](../../../guides/history-and-undo.md#see-every-write)
- [Monitoring: `op log`](../../../operate/monitoring.md#op-log)
- [Concepts: Op log and undo](../../../concepts.md#op-log-and-undo)
