---
title: "Product: what SwarmOps is for"
description: "The problem SwarmOps solves for agent swarms on git, its goal, who it is for, and what it is not."
---

# SwarmOps

SwarmOps is source control for a swarm of coding agents on one host: 10 to 50 Claude Code or
LangGraph processes working in one repository at once. It is written in Rust and fully embedded:
one program, no external database, no daemon. Its main users are LLM agents, so every operation is
a library function and the command line is a thin shell that prints JSON.

## The problem

A swarm of agents is not a team of people, and it needs other things from source control.

- **Many agents write at once.** Tens of agent processes commit into one repository on one host.
  With git, each agent works in its own worktree and branch, and publishing is left to the agent:
  git's push refuses a moved branch and leaves the fetch, the merge and the retry to the pusher.
- **Locks outlive their holders.** A git process killed with `SIGKILL` while it holds `index.lock`
  or a ref's `.lock` leaves the file behind, to be removed by hand.
- **Agents read answers, not prose.** Git reports errors as free text on stderr. An agent needs a
  stable code, whether retrying can help, and what to do next.
- **Agents get killed mid-write.** Git's defaults (2.43) sync packs, pack metadata and the commit
  graph, not loose objects, refs or the index.

## The goal

One embedded store that many agents commit into at once, safely, with a machine-readable contract,
and a way back to git so that existing CI and deployments keep running.

- **One store, many writers.** Object writes take no lock, metadata writes queue in the kernel for
  one lock, and refs move only by compare-and-swap. Every commit and ref move is a frame of one
  journal, so all of them have a single global order.
- **Publishing that merges by itself.** `land` merges an agent's change three-way onto `main` and
  publishes it in one transaction, merging again and retrying when `main` moves; a conflict comes
  back as a JSON report, and nothing is recorded.
- **One JSON envelope per command.** A stable `E_*` code, a fixed exit code, `retryable`,
  machine-readable `details` and a `hint`; `swarmops schema` describes every type, and
  `swarmops mcp` serves the operations to MCP clients such as Claude Code.
- **Full fsync, always.** A commit returns only once its journal frame is on disk, a crash leaves
  no stale lock (the lock is an `flock`, which the kernel releases when its holder dies), and the
  next writer repairs whatever a crash cut short.
- **Back to git.** `git import` brings a git history in, and `git export` writes the store's history
  back into a git branch, a commit imported from git as itself, byte for byte.

## Who it is for

- **Teams that run many coding agents on one host** against one repository, through Claude Code,
  LangGraph or any MCP client, and want each agent to commit and publish on its own.
- **People who build agent harnesses** and want a source-control contract their code can parse:
  JSON in, one JSON envelope out, an exit code per class of failure.
- **Operators who keep git as the system of record**: the store imports from git and exports to it,
  so review, CI and deployment stay where they are.

## What it is not

- **Not a general-purpose git replacement.** Git parity is not a v1 goal: no tags, free-form
  branch names, symbolic refs, user hooks, signing, submodules or LFS. Refs are `main` and
  `release/<name>`.
- **Not multi-host yet.** Every agent and command runs on the host that holds the store. Between
  hosts, git carries the history, one writing host at a time; offline-first sync is M11.
- **Not a review tool yet.** SwarmOps does not hold a landing for a person's approval; review stays
  in git until M7.
- **No workspaces yet.** Agents read and write through the commands and tools, a whole file per
  change; building and testing a change on disk before it lands comes with M5, and the
  [operations guide](/docs/operations/#building-and-testing-a-change) gives the stopgap.
- **Linux only.** macOS and Windows are M12.
- **Not faster than git at raw throughput.** Writers take turns: each commit holds one store-wide
  lock across two journal syncs, about 21 ms for an uncontended commit through the binary on the
  maintainers' host.

## Lineage

SwarmOps' design has prior art in git, Mononoke's Bonsai changesets and pushrebase, and jj's
operation log. It adapts several principles that Anthropic described for its in-house system Scion,
in the engineering post
["Redesigning source control at Anthropic"](https://www.anthropic.com/engineering/redesigning-source-control),
to a single host. SwarmOps is an independent project, not affiliated with or endorsed by Anthropic,
and contains no Scion code, which is not public.

## Read on

- [Features](/product/features/): every v0.1.0 command, with a real example.
- [How it works](/product/how-it-works/): the crates, the store on disk, writes, reads, durability.
- [SwarmOps and git](/product/vs-git/): where it is ahead, at parity and behind.
- [Roadmap](/roadmap/): what is delivered, what comes next, and when it is usable for what.
