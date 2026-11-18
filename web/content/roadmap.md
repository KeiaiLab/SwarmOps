---
title: "Roadmap: SwarmOps and GitHub, in three topologies"
description: "Version control for swarms of coding agents that works together with GitHub, in three topologies, and where the detailed roadmap keeps each step."
---

# Roadmap

SwarmOps is a version control system for swarms of coding agents that works together with GitHub.
"Works together" means three setups, each natural for the people and tools in it.

## The three topologies

- **Local git, remote SwarmOps.** People and tools that keep stock git (`git clone`, `fetch`,
  `pull`, `push`) against a host where agents land into a store.
- **Local SwarmOps, remote GitHub.** A swarm whose repository, CI, pull requests and deployments
  stay on GitHub; the store clones, fetches and pushes it directly.
- **Local SwarmOps, remote SwarmOps.** Stores on several hosts that sync natively and keep working
  offline.

A topology counts as done when its acceptance criteria pass in CI.

## Checks and delivery

Planned, and not built: SwarmOps takes on a role beside the three topologies. It describes what
must be checked for a tree, decides what a change needs, runs those checks on the caller's host or
on runners that ask for work, records each result against the tree it is for, admits a land only
on recorded results, and delivers from a store. Its first two milestones, M30 and M31, start now;
nothing of them is on `stable` yet. Checks run by GitHub on the pushed branch keep working
meanwhile, as they do today.

The eleven milestones:

- **M30, check definitions and plans.** What a tree says should run, and what a change would start.
- **M31, checks run where the caller is.** A workspace or a directory runs a plan's checks on its
  own host.
- **M32, results on a tree.** A result recorded against its tree, reused under the same key, and
  exchanged with GitHub.
- **M33, triggers and admission.** What makes a run wanted, decided from the store's records
  before any runner starts.
- **M34, runners that ask for work.** A runner is a client: it claims a run, runs it and records
  the result.
- **M35, trust, isolation and secrets.** Who may make what run where, and what a run may see.
- **M36, logs, reports and artifacts.** What a run leaves behind, and for how long.
- **M37, the gate's rules.** Rule sets, an evaluate-only mode, bypass records, intake rules and
  thresholds.
- **M38, delivery.** Environments, deployment records, approvals, and releases with assets.
- **M39, caches and set-up time.** Kept directories and workspaces on a runner, and keyed caches.
- **M40, figures, cost and events.** What checks cost per landed change, budgets and event
  delivery.

The first steps happen in this repository's own checks:

1. A check file names the gate's commands, and the gate fails when the file does not parse.
2. The CI job starts the gate through `check run`; GitHub stays the trigger and the reporter.
3. A red gate answers with the failed test cases as findings.
4. Every gate run leaves a line of diagnostics.
5. `check plan` answers which optional jobs a change needs.
6. The gate becomes several checks with declared inputs.
7. The gate runs in shadow mode and records what a plan would have skipped.
8. The weekly heavy jobs take their host lock as an exclusive group.
9. A store on the maintainers' host follows `stable`; nothing lands in it.
10. GitHub's verdicts are recorded by tree. Waits on an open question: whether the check record of
    M7 ships ahead of the rest of M7.
11. A tree that passed is not checked again. Waits on the same question.
12. The trunk moves into the store and its queue. Waits on three open questions: whether this
    repository's trunk moves into a store, whether the first slices of M7 ship early, and
    the facts of the queue this repository uses today.

Steps 1 to 8 use no store and write to none. No figure for this role is measured.

## Where to read on

- [Where we are](/docs/roadmap#where-we-are): what each topology does today.
- [To usable](/docs/roadmap#to-usable): what SwarmOps needs before a swarm can rely on it.
- [Milestones](/docs/roadmap#milestones): each step, its deliverables and its target.
- [Checks and delivery](/docs/roadmap#checks-and-delivery): the role, its 169 units in one table
  per milestone, and the first steps in full.
- [Goals](/docs/goals): each goal's metric, target and what has been measured so far.
- [Changelog](/docs/changelog): what has changed, release by release.
