`op undo` takes back the ref moves of one operation: every ref that operation `OP` moved goes
back to what it named before `OP`, in one transaction of its own, all of them or none. It names
`OP` by its id as `op log` prints it, `<op>:<crc>` (`4:9f3a01c2`): the operation's number and the
CRC-32 of its journal frame, so an id names one operation and no other. It moves
refs only. Changesets are never removed, so whatever an undo takes off a ref stays in the store,
readable by id. Use it to take back a mistaken move, deletion or creation of a `release/<name>`
ref. `main` is never deleted and moves only forward (the M15 design §4): an undo of `main`'s
creation, or one that would move `main` off its first-parent history, such as the undo of a
publish, a land or a `ref set` on `main`, is refused whole with `E_REF_PROTECTED` (exit 1), and
nothing is written. To take `main` back, land a change that reverts it.

### What it restores

For each ref, `OP`'s net move counts: from the first `old` the operation recorded for the ref to
the last `new`. A ref the operation moved and moved back is left alone. The undo then moves each
ref, in the order of `OP`'s moves, from `OP`'s `new` back to its `old`:

| What `OP` did to the ref | What the undo does |
| --- | --- |
| Moved it from `A` to `B` (`ref set`, a publish, a land) | Moves it from `B` back to `A` (`main`: only when `B` is a first-parent ancestor of `A`; otherwise the whole undo is refused, `E_REF_PROTECTED`) |
| Created it at `B` | Deletes it (`main`: the whole undo is refused, `E_REF_PROTECTED`) |
| Deleted it, from `A` | Creates it again at `A`, with `version` 1 |

### When it refuses, and when it is a replay

The undo reads operation `OP` under the store's write lock, after the write has checked the
journal's tail, then compares each ref:

| The ref names now | Result |
| --- | --- |
| What `OP` set it to (`B`; no ref, if `OP` deleted it) | Moved back |
| What it named before `OP` (`A`; no ref, if `OP` created it) | Skipped: an earlier undo, or a later move, put it there already |
| Anything else | The whole undo is refused with `E_CAS_MISMATCH` (exit 4) and nothing is written, the other refs' moves included |

With every ref skipped, nothing is written either: the result says `already_applied: true`, has
no `op`, and lists no moves. So running the same undo again is safe.

A move of `main` that the comparison keeps is judged last, under the same lock: one that would
take `main` off its first-parent history refuses the whole undo with `E_REF_PROTECTED` (exit 1),
and nothing is written. Its details are `name`, `changeset` (the head the undo would drop),
`target`, `op` and `id`; an undo that would delete `main` gives `name` only. The hint is the way
back: land a change that reverts it.

Before the comparison:

- **`E_INVALID_ARG`** (exit 2): the argument is no id: a bare number, upper-case hex, or any
  other form. Nothing is read or written.
- **`E_OP_NOT_FOUND`** (exit 6): the store has no operation with that id. `details.reason` says
  why: `past_end`, its number is past the last operation; `checksum`, the operation at its
  number has another crc, so the one named was taken back and its number went to another write,
  or the id is mistyped. `details.op` and `details.id` give what was asked, and `details.last`
  the newest operation's number, `null` for a store without one. The error never gives the id
  of the operation at that number: undoing it would undo another operation.
- **`E_NOTHING_TO_UNDO`** (exit 1): `OP` moved no ref (a `commit` without `--advance`, a
  compaction), or moved each ref back to where it found it.
- **`E_IDENTITY_REQUIRED`** (exit 5): an undo is a write, so it needs `--as` or
  `SWARMOPS_AGENT`.

### After a refusal

`E_CAS_MISMATCH` here carries `details.name` (the ref), `details.actual` (what it names now,
`null` when it does not exist) and `details.op`; the hint is `swarmops op log`, which only reads.
A later operation moved the ref, so the message gives two ways on (for `main`, only the second,
since `main` moves only forward):

- **Undo the later operations first, newest first, then `OP` again.** Each undo is checked the
  same way, so this works only while nothing else moved the ref meanwhile.
- **Keep the later moves, and land a change that reverses `OP`'s.** No command builds the
  reverse change for you
  ([Known limitations](../../../known-limitations.md#commits-refs-and-history)).

The retry flag says that the undo can succeed once the ref is back where `OP` left it. Running
the same undo again without changing anything fails the same way.

### The undo is an operation

A successful undo records one operation of kind `ref_move`, its number in the result's `op` and
its id in `id`. Its moves name the undoer as `actor` and carry the undo's time. Another
`op undo` of that id takes the undo back, which moves the refs forward again.

### Patterns

Take back the newest move of `release/1.0`:

```sh
ID=$(swarmops op log | jq -r '[.result.ops[] | select(any(.ref_moves[]; .name == "release/1.0"))][0].id')
swarmops op log --before $((${ID%%:*} + 1)) -n 1 | jq -c '.result.ops[0]'
swarmops op undo "$ID"
```

The second line lists the operation once more, to check that its moves are the ones you mean
before you undo it (below).

Read the result: `already_applied: true` means nothing was written because every ref was back
already; otherwise `ref_moves` lists each move made, and `op` and `id` are the undo's own.

### Caveats

- **Check the operation first.** An id read before a failed write took its operation back
  never undoes the operation that took its number next: that one has another crc, and the undo
  is `E_OP_NOT_FOUND` (`checksum`). Two operations share a number and a crc only by a CRC-32
  collision, one chance in 2^32 for each number taken back
  ([Known limitations](../../../known-limitations.md#durability)). An undo moves back the refs
  the operation moved, with no error while they hold what it set: list the operation again
  before you undo it, as the pattern above does.
- **It is not a revert.** An undo moves refs; it does not record a change that reverses the
  files. After it, the changesets `OP` added are on no ref, yet still in the store.
- **Land keys.** A keyed land looks for an earlier land of its key on the target's
  first-parent chain. An undo never takes a land off `main`; one it took off a `release/<name>`
  ref is no longer on that chain, so the same `land --key` run again does not replay it.
- **Durability and the lock** work as for [`ref set`](../../commands/ref-set.md#after-the-move):
  the undo commits when its frame is durable, `warnings` reports a later step that failed, and a
  lock wait past 10 s is `E_BUSY`.

### Through MCP

The MCP tool is `undo`, with `id`. Its description says the same: a ref moved since stops it,
undo the later operations first (on `main`, land a revert), an undo that would move `main` back
is `E_REF_PROTECTED`, and a second run is a replay.

### Guides

- [Reading and repairing history: Undo an operation](../../../guides/history-and-undo.md#undo-an-operation)
- [Reading and repairing history: Recover from a refusal](../../../guides/history-and-undo.md#recover-from-a-refusal)
- [Concepts: Op log and undo](../../../concepts.md#op-log-and-undo)
