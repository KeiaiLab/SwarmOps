# Limits

The ones an operator meets first; [known-limitations.md](../known-limitations.md) lists them all,
each with the milestone that takes it.

- **One host, one writer per trunk.** No sync between hosts until M11; git carries history between
  them, with one host landing and exporting and the others importing.
- **Workspaces are not synced** (M5): a power cut can leave a workspace's files torn, which
  `ws status` shows as edits and `ws discard` restores; and a checked land holds the lock twice, so
  under a busy `main` a long check may rebase and run again until `--max-attempts`
  ([Building and testing a change](build-and-test.md)).
- **No review** (M7): SwarmOps does not hold a landing for a person's approval. The loop puts the
  review in git, between the review branch and the trunk ([step 5](daily-loop.md#5-review-then-move-the-trunk)).
- **No object is collected**: `swarmops gc` removes leftovers and counts the objects no changeset
  reaches, but deletes none, and `meta/derived/` keeps every value of the current versions. A store
  only grows, but for what [Making room](making-room.md) removes.
- **`repair` rebuilds, it does not restore**: it rebuilds the index, `journal.off` and the ref files
  from the journal and sets damaged objects aside, but a damaged journal, or an object whose
  content nothing holds, takes a restore ([Verify and repair](verify-and-repair.md)).
- **Writers take turns.** Every write holds one store-wide lock across two journal syncs: about
  21 ms per uncontended commit on the maintainers' host. With 32 writers committing in a loop, a
  commit can wait a few hundred transactions; on a slow enough disk that passes the 10 s wait for
  the lock, and the commit fails with `E_BUSY`.
- **Exports write loose objects**, one file and one fsync per object, until `git gc` packs them:
  in the pilot's dry runs of 2026-10-20, exporting SwarmOps' history into an empty repository
  wrote 7,651 objects in 12 s on a busy host (a load average of 150 to 500) and in 6.1 s at 80 to
  130; measured again later that day, 8,372 objects took 4.0 s at a load of about 20. An export
  that dies while it moves the branch leaves `refs/heads/<branch>.lock`, and an export leaves no
  reflog entry.
- **Content git can hold and a changeset cannot** (submodules, names that are not UTF-8, odd modes
  and tree orders) is refused by the import unless `--lossy` allows it, and a history imported with
  a loss exports as new commits.
