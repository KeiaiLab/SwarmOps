---
title: "SwarmOps and git"
description: "Where SwarmOps stands against git, dimension by dimension, judged strictly: ahead, at parity, behind, or not yet comparable."
---

# SwarmOps and git

SwarmOps is not meant as a general-purpose git replacement: it imports from git (M3) and exports
to it (M8). Where the two compare, this is where SwarmOps stands, judged strictly:

**Ahead**: built, with evidence · **Ahead by design**, **behind by design**: judged from the
design and the code, never measured against git · **Parity** · **Behind** · **Not yet
comparable**: not built yet. In the middle column, ✅ marks what is delivered and 📋 what is
planned, each with its milestone.

| Dimension | SwarmOps | Against git |
| --- | --- | --- |
| Hash | ✅ M0: BLAKE3-256 ids, with the object's kind and layout version hashed in. ✅ M1: every read re-hashes. | **Ahead** of git's default SHA-1: 128-bit collision resistance against at most 80 bits, by cryptanalysis, not by benchmark. **Parity** with git's opt-in SHA-256. |
| Order of writes | ✅ M1: every commit and ref move is a frame of one journal, appended under one lock, so all of them have a single global order. | **Ahead by design.** |
| Stale locks | ✅ M1: the writer lock is an `flock`, which the kernel releases when its holder dies. | **Ahead by design.** A git process killed with `SIGKILL` while it holds `index.lock` or a ref's `.lock` leaves the file behind, to be removed by hand (git removes its lock files on the signals it can catch). |
| Durability | ✅ M1: full fsync is the only mode; an object is synced before a journal frame names it, and a commit returns after its frame's `fdatasync`. ✅ M1.5: the policy for a failed fsync. | **Ahead by design.** Git's defaults (2.43) sync packs, pack metadata and the commit graph, not loose objects, refs or the index. |
| Machine-readable contract | ✅ M1: one JSON envelope per command, with a stable `E_*` code, a fixed exit code, `retryable`, `details` and a `hint`; a JSON Schema for every request and response. ✅ M1.6: an I/O failure names its operation and store file, and no host path but in two `E_IO`s (a symlink loop met while working out where the store's path leads, and tempfile's giving up after 65,536 taken temp-file names); a write reports what its recovery repaired (`meta.recovered`); `verify` lists typed findings. | **Ahead by design** for errors, which git reports as free text on stderr. Git's plumbing and `--porcelain` formats narrow the gap for output. Known gap: `E_REF_NOT_FOUND` and `E_WS_STALE` are not raised yet. |
| Commits from many writers | ✅ M1: concurrent commits all succeed, short of a lock wait past 10 s or, since M1.6, a snapshot that meets a new commit in each of its 32 rounds (the retryable `E_BUSY`). | **Parity** with agents in separate git worktrees, on lost commits and lock failures. |
| One ref by compare-and-swap | ✅ M1: `ref set --expect` or `--create`, `ref delete --expect`; setting a ref to the target it holds is a replay. ✅ M1.6: `commit --advance` moves a ref to the new changeset in the commit's own transaction, or records nothing. | **Parity** with `git update-ref <ref> <new> <old>`, plus the replay. |
| Reading a path at a revision | ✅ M1: `cat REV:PATH` and `ls REV[:PATH]`, without a checkout. | **Parity** with `git show REV:PATH` and `git ls-tree`. |
| Offline, on one host | ✅ M1: no network code; every operation runs against the host's own store. | **Parity.** |
| Throughput of many writers | ✅ M1: writers take turns, each commit holding one store-wide lock across two journal `fdatasync`s, and two `fsync`s more when it moves a ref (`--advance`). | **Behind by design.** Agents in git worktrees serialize nothing, and git's defaults sync none of a commit's files. |
| Refs beyond one compare-and-swap | ✅ M1: refs are `main` and `release/<name>` only, moved one per call. | **Behind**: no multi-ref transactions, tags, free-form branch names or symbolic refs. |
| Breadth of reads | Today one file or one directory per call. ✅ M2: a file from a byte offset or by a range of lines, and a directory with the directories below it to a depth, each file with its size. 📋 M5: sparse workspaces. | **Behind**: git has batch reads, `git grep <tree>` and sparse checkouts. |
| Storage on disk | ✅ M1: one file per object, zstd-compressed when that makes it smaller; no packs, no delta compression. 📋 M6: gc, which removes leftovers but packs nothing. | **Behind by design**, in bytes and in files, against git's packs. |
| Conflicts | Since M3, `E_GIT_IMPORT_DIVERGED` signals a conflict when a git import's target and the git history have diverged. ✅ M4: `land` merges three-way without a checkout and reports a conflict as `E_CONFLICT` with a JSON report: each conflicting file's kind, its hunks (each side's lines, numbered as `cat --lines` takes them, and their text) and the target's changeset and agent that last changed it. ✅ M4: `land --resolve` resolves them without a checkout, path by path (take a side, new content, or delete), against the report's `theirs`; a path the target changed since comes back stale. | **Parity** with `git merge-tree --write-tree`, which merges without a checkout and reports typed conflicts. Resolving without a checkout is one request here, where git takes plumbing (`git hash-object`, a temporary index, `git commit-tree`). |
| Review | Nothing today. 📋 M7: change requests, their classification, approvals and a landing gate. | **Behind** today: git ships patch-review tools, and Gerrit and GitHub add review on top. M7 is **not yet comparable**. |
| Sync between hosts | ✅ M8: through a git remote, one writing host at a time: the writer exports, then pushes with git, and the others fetch with git, then import; two hosts that both write stop on `E_GIT_EXPORT_DIVERGED` or `E_GIT_IMPORT_DIVERGED` rather than merge. 📋 M11: offline-first sync, a full store on every host, `fetch` and `push` against one upstream. | **Behind**: git clones, fetches and pushes over any transport, and bundles without one. |
| Ecosystem | ✅ M1: the CLI and `swarmops schema`. ✅ M1.5: an MCP server. ✅ M3: git import. ✅ M8: git export. No user hooks, signing, submodules or LFS. | **Behind**, after v1 too: git parity is not a v1 goal, and git has MCP servers as well. |
| Maturity | Pre-release: v0.1.0 is the first tag, and the first spec dates from 2026-10-15. | **Behind**: git has hosted itself since 2005. |
| Landing | ✅ M4: `land` merges an agent's change onto `main` and publishes it as one changeset in one transaction, merging again and retrying by itself when `main` moves, and a retry with the same key replays it. 📋 M4, after the pilot: the landing model chosen by measurement. | **Ahead by design** of git alone, whose push refuses a moved branch and leaves the fetch, the merge and the retry to the pusher; not yet measured under a swarm's load. |

No git baseline has been measured: these verdicts come from the design, the code, and git's own
documentation and source, not from a benchmark, and a side-by-side benchmark against git
worktrees is not on the roadmap. SwarmOps' own measurements are in
[perf](/docs/perf/).

## What the verdicts mean for you

- **Use SwarmOps** where many agents on one host commit into one repository at once and need
  machine-readable answers, a single order of writes, no stale locks and a land that merges and
  retries by itself.
- **Keep git** for everything around it: review, CI, deployment, hosting, tags, hooks, signing,
  submodules and LFS. The store imports from git and exports to it, so git stays the system of
  record ([the daily loop](/docs/operations/#the-daily-loop)).
- **Expect to pay in throughput and disk**: writers take turns under one lock with full fsync, and
  objects are stored one file each, with no packs.

[Features](/product/features/) · [How it works](/product/how-it-works/) · [Roadmap](/roadmap/)
