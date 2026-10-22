---
title: "Roadmap: SwarmOps and GitHub, in three topologies"
description: "Local git with a SwarmOps remote, a SwarmOps store with a GitHub remote, and stores syncing with each other: what works today, what is planned, and the milestones that deliver each."
---

# Roadmap

SwarmOps is a version control system for swarms of coding agents that works together with GitHub.
Working together means three topologies, each natural for the people and tools in it. This page
summarises the [detailed roadmap](/docs/roadmap/), which has the commands, the design, the
acceptance criteria and the targets of each. Everything marked planned is planned; command names
of planned work are provisional.

## The three topologies

### 1. Local git, remote SwarmOps

**Status: planned (M14).** People and tools keep stock git against a host where agents land into
a store:

```sh
git clone ssh://so-host/acme
git push origin main                   # a fast-forward, by compare-and-swap
git push origin HEAD:refs/land/main    # merged onto main, as an agent lands
```

`swarmops serve` answers git's fetch and push protocols per connection, under ssh or as a CGI
program behind a web server: no daemon. A push becomes a compare-and-swap on the store's ref, or a
land with a three-way merge; a conflict comes back to the git client as a rejected ref with the
conflicting paths. **Today:** no server; people use a bare git repository that `git export`
writes, and someone imports and lands their pushes.
[Details](/docs/roadmap/#topology-1-local-git-remote-swarmops)

### 2. Local SwarmOps, remote GitHub

**Status: today by hand; native planned (M13).** The store clones, fetches and pushes a GitHub
repository itself, so CI, pull requests and deployments on GitHub keep working:

```sh
so clone https://github.com/acme/acme --into ~/.local/state/swarmops/repos/acme
so push origin main:review             # a branch for a pull request
so sync origin                         # land on top of GitHub's main, then push
```

Commits from GitHub keep their ids through every round trip, and a squash-merged pull request no
longer stops the next import. **Today:** `git import` from a bare clone, `git export` back to it, and
a hand-run `git push` ([the daily loop](/docs/operate/daily-loop/)).
[Details](/docs/roadmap/#topology-2-local-swarmops-remote-github)

### 3. Local SwarmOps, remote SwarmOps

**Status: today through a git remote, one writing host; native planned (M11).** Stores on several
hosts sync over ssh and keep working offline:

```sh
so clone so://hub/acme --into ~/.local/state/swarmops/repos/acme
so sync hub                            # each local land, landed upstream exactly once
```

[Details](/docs/roadmap/#topology-3-local-swarmops-remote-swarmops)

## Milestones

✅ delivered · 📋 planned. M0 to M4 and M8 make v0.1.0, and M12 is next; **the order from M5 on is
settled once the pilot has run, and until then this table is the plan.** M13 and M14 are new.

| Status | Milestone | Topology | What it delivers | Depends on |
| --- | --- | --- | --- | --- |
| ✅ | M0 | | Bootstrap: the workspace and its gates, the canonical codec, the fsync experiment. | |
| ✅ | M1, M1.5, M1.6 | | The single-agent core, its hardening, the MCP server and `so`, and the code-review follow-ups. | |
| ✅ | M2 | | Diff, history and the derived-data engine. | |
| ✅ | M3 | 2, 3 | git import. | |
| ✅ | M4 | | Merge and landing, and the operation log with undo. | |
| ✅ | M8 | 2, 3 | git export, so that CI and deployments keep running. | |
| 📋 | M12 | all | Portability: macOS and Windows builds, tests and releases. | |
| 📋 | M13 | 2 | Git remotes from a store: packs, git's fetch and push as a client, `so clone`, `so remote`, `so fetch`, `so push`, `so sync`. | M3, M4, M8 |
| 📋 | M5 | | Workspaces, with an adapter for Claude Code's worktree hooks. | |
| 📋 | M6 | 1, 3 | Verify, crash safety and fuzzing; the next store `FORMAT`, with branch names, tags and remote-tracking refs. | |
| 📋 | M14 | 1 | Git clients against a store: `swarmops serve` over ssh and HTTP, pushes that land, access control. | M13, M6 |
| 📋 | M7 | | Blame and review. | M6 |
| 📋 | M9 | | Simulation and a benchmark gate. | |
| 📋 | M11 | 3 | Native sync between stores, offline-first, over `so://`. | M6, M7, M9, M13, M14 |

An always-on RPC server is a follow-up after v1, not a milestone.

## When it is usable for what

| From | Usable |
| --- | --- |
| M1 | By one agent. |
| M4 | By a swarm, landing through MCP. |
| M8, in v0.1.0 | On an existing git repository, its landed work exported back so that CI and deployments run as before. |
| M13 | With GitHub as the remote, without a hand-run `git push`. |
| M5 | By agents that build and test their changes in a workspace before they land them. |
| M14 | By people and tools with stock git, against a store. |
| M7 | With review. |
| M9 | As the complete v1. |
| M11 | Across hosts, offline-first. |

## Next: the pilot

The pilot runs SwarmOps' own repository through the round trip: its history imported from git,
changes landed by agents through `swarmops mcp`, and the result exported back to git for its CI
to run ([the pilot](/docs/pilot/)). After it come the measurements that need a quiet host, and the
order of the milestones above is settled.

## Where to follow

[Detailed roadmap](/docs/roadmap/) · [Goals](/docs/goals/) ·
[Known limitations](/docs/known-limitations/) ·
[Changelog](https://github.com/KeiaiLab/SwarmOps/blob/stable/CHANGELOG.md) ·
[GitHub](https://github.com/KeiaiLab/SwarmOps)
