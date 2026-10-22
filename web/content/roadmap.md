---
title: Roadmap
description: 'The milestones of SwarmOps, in the order they are built: what is delivered and what is planned.'
---

The milestones, in the order they are built. ✅ delivered · 📋 planned.

| Status | Milestone | Scope |
| --- | --- | --- |
| ✅ | M0 | Bootstrap: the workspace and its gates, the canonical codec and its golden vectors, the fsync experiment. |
| ✅ | M1 | The single-agent core: `init`, `commit`, `cat`, `ls`, `log` and ref compare-and-swap, over the native metadata store and object files; crash sweeps. |
| ✅ | M1.5 | Hardening and opening: CI behind one `tests-pass` check, `--locked` everywhere; I/O-error injection, write-ordering checks and the fsync-failure policy; power-loss images; the project policies; the tests' shared support in one crate; benchmarks of the metadata store against embedded databases; the MCP server and `so`. |
| ✅ | M1.6 | The code-review follow-ups, before M2: the error contract an agent sees (typed refusals, I/O failures that name the store file and no host path but in two rare `E_IO`s, a report of what a write's recovery repaired); a bound on the lock-free reader's retries; structured `verify` findings, and MCP tool schemas kept plain objects; object reads without a second copy or an allocation of the size a file claims; the test doubles M1.5 left, in one place; spec hygiene (the write-ordering rules W1–W7, §8 in step with the code); the harness for the measurements M2's sync decision needs, whose quiet-window runs are still to come; and `commit --advance`, the commit primitive M4's landing reuses. |
| ✅ | M2 | Diff, history and the derived-data engine. |
| ✅ | M3 | git import. |
| ✅ | M4 | Merge and landing, and the journal shown as an operation log with undo. Landing is a compare-and-swap loop that merges again, incrementally, when the target moves; its measurement under a swarm's load, and the landing model that measurement chooses, come next. |
| ✅ | M8 | git export, moved ahead so that work landed in SwarmOps goes back to git and today's deployment pipelines keep running. |
| 📋 | M12 | Portability: macOS and Windows builds, tests and releases. |
| 📋 | M5 | Workspaces, with an adapter for Claude Code's worktree hooks, so that an agent builds and tests a change on disk before it lands it. |
| 📋 | M6 | Verify, crash safety and fuzzing: fsck and repair, gc, detection of unsupported filesystems, bounded reads of store files, a capped `verify` report, and the next store `FORMAT`. |
| 📋 | M7 | Blame and review. |
| 📋 | M9 | Simulation and a benchmark gate: the store's own code under a deterministic simulator and in real processes killed at random, and benchmarks that fail on a regression. |
| 📋 | M11 | Remote sync, offline-first: a full store on every host, `fetch`, `push` and `sync status` against one upstream, over ssh. |

The order of the milestones from M5 on is settled once the pilot has run; until then the table is
the plan. [Known limitations](/docs/known-limitations) lists what is still open, and which
milestone takes each item.
