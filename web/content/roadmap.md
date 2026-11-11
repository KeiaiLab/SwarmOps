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

## Where to read on

- [Where we are](/docs/roadmap#where-we-are): what each topology does today.
- [To usable](/docs/roadmap#to-usable): what SwarmOps needs before a swarm can rely on it.
- [Milestones](/docs/roadmap#milestones): each step, its deliverables and its target.
- [Goals](/docs/goals): each goal's metric, target and what has been measured so far.
- [Changelog](/docs/changelog): what has changed, release by release.
