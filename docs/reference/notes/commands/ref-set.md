`ref set` moves one ref to a changeset, or creates it, by compare-and-swap: you say what the ref
holds now (`--expect`) or that it does not exist yet (`--create`), and the store compares that
with the ref under its write lock. It is the low-level move. To publish a change on `main`, agents
`land` it, which merges onto whatever `main` holds by then; `ref set` is for operators and
scripts that point a ref at a changeset they have already chosen, such as a release ref.

### What happens, in order

1. **Arguments.** Exactly one of `--expect` and `--create` is required; both, or neither, is
   `E_INVALID_ARG` (exit 2).
2. **Identity.** Without `--as` or `SWARMOPS_AGENT` the command fails with
   `E_IDENTITY_REQUIRED` (exit 5) before it opens the store.
3. **Revisions.** `REV` and the `--expect` revision are resolved in one snapshot of the store.
   A revision that names nothing is `E_REV_NOT_FOUND` (exit 6), a malformed one `E_INVALID_ARG`
   (exit 2), a prefix more than one changeset shares `E_AMBIGUOUS_ID` (exit 1). Both appear in
   `meta.resolved` with the full ids they named.
4. **The name.** `NAME` must be `main` or `release/<x>`, where `x` is one or more of letters,
   digits, `.`, `_` and `-`, does not start with `.`, and the whole name is at most 255 bytes;
   anything else is `E_REF_NAME_INVALID` (exit 1).
5. **The compare-and-swap,** under the store's write lock, after the write has checked the
   journal's tail, so against the ref as committed, never as a reader's snapshot showed it:

| The ref holds | With `--expect E` | With `--create` |
| --- | --- | --- |
| `E`, which is not `REV` | Moved to `REV`; `old` is `E` | `E_CAS_MISMATCH`, `actual` is `E` |
| `REV` already | Replay: `already_applied: true`, nothing written | Replay: `already_applied: true`, nothing written |
| Another changeset `X` | `E_CAS_MISMATCH`, `actual` is `X` | `E_CAS_MISMATCH`, `actual` is `X` |
| Nothing (no such ref) | `E_CAS_MISMATCH`, `actual` is `null` | Created at `REV`, version 1 |

A replay writes no journal frame: `op log` does not list it and the ref's `version` stays. So the
same `ref set` run twice is safe, whichever run gets there first.

`E_CAS_MISMATCH` is exit 4 and `retryable: true`, but do not rerun the same command: it would
fail again. Read `details.actual` (the hint, `swarmops show <actual>`, only reads) and decide
with that. The `--expect` help above says what to do for each case, and
[When a compare-and-swap fails](../../../guides/commit-and-land.md#when-a-compare-and-swap-fails)
walks through it.

### What `ref set` does not check

- **No ancestry.** `REV` can be any changeset in the store: a descendant of the ref's target, an
  ancestor (moving the ref back), or one on another line of history. Moving `main` drops from
  its history every changeset the new target is not built on. Those changesets stay in the
  store, readable by id, and `op undo` can move the ref back.
- **No merge.** Nothing of the ref's old target is carried over. `land` is the command that
  merges.

### After the move

- **Durability.** The move commits when its journal frame is durable. If a step after that fails
  (such as listing the frame in `journal.off`), the move still stands, `warnings` says what
  failed, and the next write to the store finishes it. `meta.recovered` reports what this write repaired of an
  earlier writer that died mid-write ([Monitoring](../../../operate/monitoring.md#metarecovered)).
- **The record.** The journal records the move with the agent's identity and the time, in
  seconds; `op log` shows it as an operation of kind `ref_move`, which `op undo` can take back.
- **The lock.** A write waits up to 10 s for the store's lock. If another writer holds it the
  whole time, the command fails with `E_BUSY` (exit 4, retryable) and nothing is written: run
  the same command again.

### Patterns

Move a release ref to `main`'s head, expecting the target you read:

```sh
OLD=$(swarmops resolve release/1.0 | jq -r '.result.id')
NEW=$(swarmops resolve main | jq -r '.result.id')
swarmops ref set release/1.0 "$NEW" --expect "$OLD"
```

Create a ref, safe to retry: a second run with the same `REV` is a replay, and one with another
`REV` fails with `E_CAS_MISMATCH`, so no ref is ever silently re-pointed:

```sh
swarmops ref set release/2.0 "$NEW" --create
```

Branch on the outcome in a script:

```sh
out=$(swarmops ref set release/1.0 "$NEW" --expect "$OLD")
case $? in
  0) echo "$out" | jq -r '.result.already_applied' ;;
  4) echo "$out" | jq -r '.error.code, .error.details.actual' ;;
  *) echo "$out" | jq -r '.error.code, .error.message' ;;
esac
```

### Caveats

- **Expect a full id.** A ref name or a relative revision as `--expect` is read when the command
  runs, so it does not pin what you saw: `--expect release/1.0` on `release/1.0` always matches,
  and the compare-and-swap protects nothing. An id prefix pins the id only while no other
  changeset shares it. The CLI accepts all three; the `--expect` help warns.
- **Changing the trunk.** If `main` is exported to a git trunk, moving it by hand is a change of
  the trunk: keep to [One writer per trunk](../../../operate/one-writer-per-trunk.md).

### Through MCP

The MCP tool is `ref_set`, with `name`, `new`, and either `expected_old` or `create: true`. The
server refuses an `expected_old` that is not a full 64-hex changeset id with `E_INVALID_ARG`
(hint `swarmops ref list`), so a ref name, a relative revision or a prefix never reaches the
store.

### Performance

A ref move is one durable write transaction. The performance ledger counts its syncs: two for the
journal (the frame and its entry in `journal.off`) and two for each ref it moves (the ref's new
file and its directory)
([Sync calls per commit](../../../perf.md#sync-calls-per-commit-durability-checked)).

### Guides

- [Commit and land: Move a ref yourself](../../../guides/commit-and-land.md#move-a-ref-yourself)
- [Reading and repairing history: See every write](../../../guides/history-and-undo.md#see-every-write)
- [Scripting the CLI: Retry rules](../../../develop/cli-scripting.md#retry-rules)
- [Concepts: Compare-and-swap](../../../concepts.md#compare-and-swap)
