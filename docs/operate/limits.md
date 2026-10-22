# Limits

The ones an operator meets first; [known-limitations.md](../known-limitations.md) lists them all,
each with the milestone that takes it.

- **One host, one writer per trunk.** No sync between hosts until M11; git carries history between
  them, with one host landing and exporting and the others importing.
- **No workspaces** (M5): agents read and write through the tools, a whole file per change. An agent
  that must build first works in a git worktree
  ([Building and testing a change](build-and-test.md)).
- **No review** (M7): SwarmOps does not hold a landing for a person's approval. The loop puts the
  review in git, between the review branch and the trunk ([step 5](daily-loop.md#5-review-then-move-the-trunk)).
- **Nothing is collected** (M6): `objects/tmp/` keeps the leftovers of interrupted writes, and
  `meta/derived/` every derived value. A store only grows, but for what [Making room](making-room.md)
  removes by hand.
- **No repair tool** (M6): `verify` reports damage, and a restore fixes it.
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
