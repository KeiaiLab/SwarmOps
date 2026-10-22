`ref list` reads every ref in one snapshot of the store's metadata and prints each with its
target and its version. It writes nothing, takes no lock and needs no identity, so any number of
agents can run it while others commit and land.

### What it lists

- **Every ref, sorted by name.** A store has two kinds of ref: `main`, and `release/<name>`
  ([Ref](../../../concepts.md#ref)). There are no other branches, tags or symbolic refs, so
  `main` always sorts first.
- **`target`** is the full 64-hex id of the changeset the ref names now. Pass it as it is to
  `ref set --expect` or `ref delete --expect`, or as `--parent` to a commit.
- **`version`** counts the transactions that changed the ref's target since the ref was created,
  starting at 1. A move that finds the ref at its target already (a replay, `already_applied`)
  writes nothing and leaves the version as it is. A ref that is deleted and created again starts
  at 1 again.

A store that `init` just made has no refs: `refs` is `[]`, and `main` exists only once a commit
with `--advance main`, a `ref set main <rev> --create` or a `git import` makes it.

### Patterns

Read one ref's target, empty when the ref does not exist:

```sh
swarmops ref list | jq -r '.result.refs[] | select(.name == "release/1.0") | .target'
```

To read one ref you can also `swarmops resolve <name>`. It fails with `E_REV_NOT_FOUND` (exit 6)
when the ref does not exist, and records the id it read in `meta.resolved`.

Read a ref, then move it only if no one moved it in between: give the `target` you read to
`--expect`. The compare-and-swap then fails with `E_CAS_MISMATCH` instead of overwriting another
agent's move ([`ref set`](../../commands/ref-set.md)).

### Caveats

- **A snapshot, not a lock.** The list is true when it was read. Another writer can move a ref a
  moment later; only a compare-and-swap (`--expect`) tells you whether it did.
- **Reads can show a move that is not durable yet.** Like every local read, `ref list` reads the
  journal as it is, so it can show a move whose sync has not finished. If that sync fails, or a
  power loss comes first, the store takes the move back
  ([Known limitations: Concurrency and the lock](../../../known-limitations.md#concurrency-and-the-lock)).
- **A damaged ref file fails the listing.** A ref file that does not read as one fails the
  command with `E_STORE_CORRUPT` (exit 1), `details.store_file` naming the file and the hint
  `swarmops verify`, which reports it as a `corrupt_ref` finding.

### Through MCP

The MCP tool is `ref_list`, with no arguments. Its result is the same `RefListResp`.

### Performance

Every request reads every ref file once, in its snapshot. The performance ledger measured that
snapshot on
10,000 changesets at 55.8 µs with no refs besides `main`, 145 µs with 16 and 1.55 ms with 256
(p50; [v0.1.0 run](../../../perf.md#v010--just-perf-in-a-quiet-window-2026-10-21)). Refs are
`main` and `release/*` by design, so a store normally holds few.

### Guides

- [Reading and repairing history: List the refs](../../../guides/history-and-undo.md#list-the-refs)
- [Commit and land: Move a ref yourself](../../../guides/commit-and-land.md#move-a-ref-yourself)
- [Scripting the CLI: jq recipes](../../../develop/cli-scripting.md#jq-recipes)
