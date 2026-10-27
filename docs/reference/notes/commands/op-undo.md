`op undo` takes back the ref moves of one operation: every ref that operation `OP` moved goes
back to what it named before `OP`, in one transaction of its own, all of them or none. It moves
refs only. Changesets are never removed, so whatever an undo takes off a ref stays in the store,
readable by id. Use it to take back a mistaken `ref set`, a publish or a land that should not
have reached `main`, a deletion or a creation of a ref. An undo of `main`'s creation would
delete `main`, which is never deleted: it is refused whole with `E_REF_PROTECTED` (exit 1), and
nothing is written.

### What it restores

For each ref, `OP`'s net move counts: from the first `old` the operation recorded for the ref to
the last `new`. A ref the operation moved and moved back is left alone. The undo then moves each
ref, in the order of `OP`'s moves, from `OP`'s `new` back to its `old`:

| What `OP` did to the ref | What the undo does |
| --- | --- |
| Moved it from `A` to `B` (`ref set`, a publish, a land) | Moves it from `B` back to `A` |
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

Before the comparison:

- **`E_OP_NOT_FOUND`** (exit 6): the store has no operation `OP`. `details.last` is the newest
  operation's number, `null` for a store without one.
- **`E_NOTHING_TO_UNDO`** (exit 1): `OP` moved no ref (a `commit` without `--advance`, a
  compaction), or moved each ref back to where it found it.
- **`E_IDENTITY_REQUIRED`** (exit 5): an undo is a write, so it needs `--as` or
  `SWARMOPS_AGENT`.

### After a refusal

`E_CAS_MISMATCH` here carries `details.name` (the ref), `details.actual` (what it names now,
`null` when it does not exist) and `details.op`; the hint is `swarmops op log`, which only reads.
A later operation moved the ref, so the message gives two ways on:

- **Undo the later operations first, newest first, then `OP` again.** Each undo is checked the
  same way, so this works only while nothing else moved the ref meanwhile.
- **Keep the later moves, and land a change that reverses `OP`'s.** No command builds the
  reverse change for you
  ([Known limitations](../../../known-limitations.md#commits-refs-and-history)).

The retry flag says that the undo can succeed once the ref is back where `OP` left it. Running
the same undo again without changing anything fails the same way.

### The undo is an operation

A successful undo records one operation of kind `ref_move`, its number in the result's `op`. Its
moves name the undoer as `actor` and carry the undo's time. Another `op undo` of that number
takes the undo back, which moves the refs forward again.

### Patterns

Take back the newest move of `main`:

```sh
OP=$(swarmops op log | jq -r '[.result.ops[] | select(any(.ref_moves[]; .name == "main"))][0].op')
swarmops op log --before $((OP + 1)) -n 1 | jq -c '.result.ops[0]'
swarmops op undo "$OP"
```

The second line lists the operation once more, to check that its moves are the ones you mean
before you undo it (below).

Read the result: `already_applied: true` means nothing was written because every ref was back
already; otherwise `ref_moves` lists each move made and `op` is the undo's own number.

### Caveats

- **Check the number first.** `op log` lists an operation before its sync has finished. If the
  store takes that frame back (a failed sync, a power loss), the next write's frame gets the same
  number, and an undo of the number you read moves back that other operation's refs, with no
  error, as long as they hold what it set. List the operation again before you undo it, above
  all after an error ([Known limitations](../../../known-limitations.md#commits-refs-and-history)).
- **It is not a revert.** An undo moves refs; it does not record a change that reverses the
  files. After it, the changesets `OP` added are on no ref, yet still in the store.
- **Land keys.** A keyed land looks for an earlier land of its key on the target's
  first-parent chain. A land that an undo took off `main` is no longer there, so the same
  `land --key` run again lands once more (`status: landed`, not `replayed`).
- **Durability and the lock** work as for [`ref set`](../../commands/ref-set.md#after-the-move):
  the undo commits when its frame is durable, `warnings` reports a later step that failed, and a
  lock wait past 10 s is `E_BUSY`.

### Through MCP

The MCP tool is `undo`, with `op`. Its description says the same: a ref moved since stops it,
undo the later operations first, and a second run is a replay.

### Guides

- [Reading and repairing history: Undo an operation](../../../guides/history-and-undo.md#undo-an-operation)
- [Reading and repairing history: Recover from a refusal](../../../guides/history-and-undo.md#recover-from-a-refusal)
- [Concepts: Op log and undo](../../../concepts.md#op-log-and-undo)
