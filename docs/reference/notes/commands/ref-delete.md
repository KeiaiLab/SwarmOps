`ref delete` removes one ref by compare-and-swap: it deletes the ref only if it still names the
changeset you give as `--expect`. Deleting a ref removes the name only. Every changeset stays in
the store, readable by id, and `op undo` of the deletion creates the ref again.

### What happens, in order

1. **Identity.** Without `--as` or `SWARMOPS_AGENT` the command fails with
   `E_IDENTITY_REQUIRED` (exit 5) before it opens the store.
2. **The expected target.** `--expect` is resolved as a revision first. A revision that names
   nothing is `E_REV_NOT_FOUND` (exit 6): so `--expect release/1.0` on a ref that is gone fails
   here, before the delete is tried.
3. **The name.** `NAME` must be `main` or `release/<x>` (letters, digits, `.`, `_`, `-`, not
   starting with `.`, at most 255 bytes); anything else is `E_REF_NAME_INVALID` (exit 1).
   `main` is never deleted: a delete of it that passes the compare-and-swap below is
   `E_REF_PROTECTED` (exit 1, `details.name`), and nothing is written.
4. **The compare-and-swap,** under the store's write lock, against the ref as committed:

| The ref holds | Result |
| --- | --- |
| The expected changeset | Deleted; `old` is that changeset, `already_applied: false`; for `main`, `E_REF_PROTECTED` |
| Nothing (no such ref) | Replay: `already_applied: true`, nothing written |
| Another changeset | `E_CAS_MISMATCH` (exit 4), `details.actual` is what it holds |

A ref that does not exist counts as deleted already, whoever deleted it, so a retry of a delete
that succeeded is safe. This holds for a name that never existed too: `ref delete release/typo
--expect <id>` succeeds with `already_applied: true` as long as `<id>` is a changeset in the
store. Read `already_applied` when it matters whether this run deleted the ref.

On `E_CAS_MISMATCH`, someone moved the ref after you read it. The hint, `swarmops show
<actual>`, only reads: look at what the ref holds now before you delete it with
`--expect <actual>`.

### Deleting `main`

`main` is never deleted, in every store format: `ref delete main --expect <id>` with the id
`main` names is `E_REF_PROTECTED` (exit 1, not retryable), and `main` keeps its target. There is
no override. To point `main` elsewhere, move it with `swarmops ref set main <id> --expect <old>`.
A stale `--expect` is still `E_CAS_MISMATCH`: the compare comes first.

### After the delete

The journal records the deletion as a ref move with no new target, with the agent's identity
and the time; `op log` lists it as a `ref_move` whose move has `old` and no `new`. A ref created
again under the same name starts at `version` 1. Durability, `warnings`, `meta.recovered` and the
10 s lock wait (`E_BUSY`) work as for [`ref set`](../../commands/ref-set.md#after-the-move).

### Patterns

Delete a ref you just read:

```sh
OLD=$(swarmops resolve release/1.0 | jq -r '.result.id')
swarmops ref delete release/1.0 --expect "$OLD"
```

Take a branch imported from git into its own ref, land it on `main`, then delete the ref:
[One writer per trunk](../../../operate/one-writer-per-trunk.md#change-the-trunk-without-breaking-the-rule)
runs this sequence.

Undo a deletion: find the newest operation that deleted the ref (a move with no `new`) and undo
it. `op log` lists 50 operations unless `-n` says otherwise; page with `--before` for older ones.

```sh
OP=$(swarmops op log | jq -r '[.result.ops[] | select(any(.ref_moves[]; .name == "release/1.0" and .new == null))][0].op')
swarmops op undo "$OP"
```

### Caveats

- **Expect a full id.** A ref name or a relative revision as `--expect` is read when the
  command runs: `--expect release/1.0` always matches the ref it deletes, so the compare-and-swap
  protects nothing. An id prefix pins the id only while no other changeset shares it.
- **No cleanup.** Deleting a ref frees no space: the changesets it named, and their objects,
  stay.

### Through MCP

The MCP tool is `ref_delete`, with `name` and `expected_old`. The server refuses an
`expected_old` that is not a full 64-hex changeset id with `E_INVALID_ARG` (hint
`swarmops ref list`).

### Guides

- [Commit and land: Move a ref yourself](../../../guides/commit-and-land.md#move-a-ref-yourself)
- [Reading and repairing history: Undo a ref's deletion](../../../guides/history-and-undo.md#undo-a-refs-deletion)
- [Concepts: Ref](../../../concepts.md#ref)
