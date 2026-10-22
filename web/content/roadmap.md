---
title: "Roadmap: what SwarmOps delivers, and what comes next"
description: "Delivered milestones M0 to M4 and M8, planned M12, M5, M6, M7, M9 and M11, when SwarmOps is usable for what, and the open items of each."
---

# Roadmap

SwarmOps is built in milestones, in the order below. Eight are delivered and make the first
release, v0.1.0; six are planned. **The order of the milestones from M5 on is settled once the
pilot has run; until then the table is the plan.**

✅ delivered · 📋 planned

| Status | Milestone | What it delivers |
| --- | --- | --- |
| ✅ | M0 | Bootstrap: the workspace and its gates, the canonical codec and its golden vectors, the fsync experiment. |
| ✅ | M1 | The single-agent core: `init`, `commit`, `cat`, `ls`, `log` and ref compare-and-swap, over the native metadata store and object files; crash sweeps. |
| ✅ | M1.5 | Hardening and opening: I/O-error injection, write-ordering checks and the fsync-failure policy; power-loss images; the project policies; benchmarks of the metadata store against embedded databases; the MCP server and `so`. |
| ✅ | M1.6 | The code-review follow-ups: the error contract an agent sees, a bound on the lock-free reader's retries, structured `verify` findings, MCP tool schemas kept plain objects, and `commit --advance`, the commit primitive landing reuses. |
| ✅ | M2 | Diff, history and the derived-data engine. |
| ✅ | M3 | git import. |
| ✅ | M4 | Merge and landing, and the journal shown as an operation log with undo. |
| ✅ | M8 | git export, moved ahead so that work landed in SwarmOps goes back to git and today's deployment pipelines keep running. |
| 📋 | M12 | Portability: macOS and Windows builds, tests and releases. |
| 📋 | M5 | Workspaces, with an adapter for Claude Code's worktree hooks, so that an agent builds and tests a change on disk before it lands it. |
| 📋 | M6 | Verify, crash safety and fuzzing: fsck and repair, gc, detection of unsupported filesystems, bounded reads of store files, a capped `verify` report, and the next store `FORMAT`. |
| 📋 | M7 | Blame and review. |
| 📋 | M9 | Simulation and a benchmark gate: the store's own code under a deterministic simulator and in real processes killed at random, and benchmarks that fail on a regression. |
| 📋 | M11 | Remote sync, offline-first: a full store on every host, `fetch`, `push` and `sync status` against one upstream, over ssh. |

An RPC server is a follow-up after v1, not a milestone.

## When it is usable for what

| From | Usable |
| --- | --- |
| M1 | By one agent. |
| M4 | By a swarm, landing through MCP. |
| M8, in v0.1.0 | On an existing git repository: its history imported, its landed work exported back, so that its CI and deployments run as before. |
| M5 | By agents that build and test their changes in a workspace before they land them. |
| M7 | With review. |
| M9 | As the complete v1. |
| M11 | Across hosts, offline-first. |

## Next: the pilot

The pilot runs SwarmOps' own repository through the round trip: its history imported from git,
changes committed and landed by agents through `swarmops mcp`, and the result exported back to git
for its CI to run. Its runbook, with the success criteria and the rollback, is
[the pilot](/docs/pilot/).

After it come the measurements that need a quiet host:

- landing by 32 and 50 processes through the binary against the design's targets, and the landing
  model those numbers choose;
- whether a transaction can sync the journal once instead of twice;
- the import's and the export's timings on larger repositories.

What is known so far about landing under load: in one closed-loop run through the binary on a
loaded host (load 80 to 87, so the figures are indicative), 8, 16 and 32 processes landing 4, 4 and
3 times each on one `main` landed every change exactly once with `verify --deep` clean, and 22 %,
44 % and 44 % of the lands retried; at 32, four of the 96 took all 8 attempts.

## Open items, by milestone

From the [known limitations](/docs/known-limitations/), which give each item in full, with where it
was found.

### Delivered milestones: work deferred until after the pilot

- **Full fsync is slow on this host**: about 21 ms for an uncontended durable commit through the
  binary, at about 4 ms per fsync. Batching object and journal syncs, and whether any
  `--durability fast` flag is justified, are re-evaluated with new measurements (M2's journal-sync
  decision).
- **Landing retries under a burst**: M4's landing measurements and the landing model they choose.
- **Part of each lock pass through the binary is unexplained**: M1.6's quiet-window runs, then M2's
  journal-sync decision.
- **Commit-path costs** (index tail scans, the reflog walk): M2's measured decision on them.
- **Compaction rewrites both sorted views under the writer lock**: M1.6's compaction bench; M3's
  merge outside the lock if the bench shows a stall of 100 ms or more at 100,000 records, else M6.
- **No test reads a real repository yet**: M3's test of the host's repositories and M8's round trip
  of them.

### M12: portability

The known limitations name no item for M12. Today SwarmOps is Linux only, and the release binaries
print an `ld.so` line on glibc 2.34 to 2.38 (unscheduled).

### M5: workspaces

- Concurrent `init` on one directory is unsupported: a no-clobber `REPO` link, with the workspace
  registry.
- A store is found by `--repo` or `SWARMOPS_REPO` alone: the upward search for a `.swarmws`
  directory comes with workspaces.

### M6: verify, crash safety and fuzzing

Durability and the lock:

- A test with a literal `SIGKILL` on a real device (the crash model is a process crash and a
  modelled power loss).
- A double failure that leaves an index risk open, and the double and triple faults left in the
  journal's failure policy: the next `FORMAT`'s frame header and fsck.
- A failed directory fsync in the object store is retried, not taken back: detection of
  unsupported filesystems by `statfs`.
- The lock is bounded, not first-in, first-out: a ticket or FIFO hand-off on top of `flock`.
- Recovery after a crash walks all of `journal.off` under the lock: fsck, and the lock timeout
  against journal size.
- Compaction rewrites both sorted views under the writer lock: sorted views as immutable runs
  under a manifest, in the next `FORMAT`.

Bounded reads:

- `cat` reads the whole object file; `ls` of a wide directory decodes its whole tree.
- A land merges a file both sides changed in memory, whole; `diff` in stat and unified form reads
  both blobs whole.
- A huge `journal.log`, `journal.off`, index column or sorted view aborts a read.
- The index reads its vouching journal around the store's filesystem seam.

Repair and collection:

- No repair tool: `verify` reports damage, and restoring the store is the only remedy until fsck
  (the index can be rebuilt from the journal in principle).
- A zero-filled index tail over a damaged journal wedges the store, or is counted when no record
  before it has parents.
- A `journal.off` that ends in more than 512 zeroed entries is refused, not repaired.
- A damaged object file, or a damaged or wrong external id entry, stays until it is removed by hand.
- A crafted index's generations steer history queries.
- Temp files of crashed writes stay, and derived values are never collected: `gc`.
- `verify` does not scale to very large stores, and its report has no cap: streaming verify.
- Crafted `journal.off` entries, and a zeroed entry deep in the chain: a checksum on each entry in
  the next `FORMAT`.

Platforms:

- The filesystem check is by path name only, and directory fsync failures are assumed fail-stop:
  detection by `statfs`.

### M7: blame and review

- No hooks or annotations after a land: annotations need a journal record the current `FORMAT`
  lacks, so they come on M6's next `FORMAT`.

### M9: simulation and a benchmark gate

- A retried commit that omits its parent can record a second changeset, and a keyed land searches
  the target's first parents: an index of commit keys, if measurements ask for one.
- No benchmark gate: the benches run by hand; failing on a 20 % regression comes with M9.

### M11: remote sync

- `{"file": "<path>"}` content is read by whoever serves the request: a server that takes requests
  from another host must refuse it.
- One temp-file error names a host path: such a server must not pass it on.

### Unscheduled

Items no milestone has taken yet, among them: `undo` does not take back a land that later lands
built on; history follows first parents only; a page cursor does not pin the revisions it pages;
a read can give up under sustained commits. The
[known limitations](/docs/known-limitations/) list them all.

## Where to follow

[Changelog](https://github.com/KeiaiLab/SwarmOps/blob/stable/CHANGELOG.md) ·
[Known limitations](/docs/known-limitations/) · [Features](/product/features/) ·
[GitHub](https://github.com/KeiaiLab/SwarmOps)
