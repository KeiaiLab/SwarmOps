# Operating SwarmOps

This guide is for whoever runs SwarmOps for a swarm of coding agents on one host. It installs the
binary, creates a store, gives agents the store through MCP, runs the daily loop between the store
and the git remote that CI and deployment watch (`so fetch`, `so push` and `so sync`, through the
host's git), with a person's review before the trunk moves,
checks, backs up and trims the store, and says what an upgrade means, where the limits are and what
to watch. [pilot.md](pilot.md) runs the same loop once on this repository, as the first real use.
Each task has a page of its own under [operate/](operate/).

## The pages

| Task | Page |
| --- | --- |
| Install the binary, and upgrade it and the stores | [Install and upgrade](operate/install-and-upgrade.md) |
| Choose where a store lives, set its permissions, create it, give writers identities | [Stores](operate/stores.md) |
| Register `swarmops mcp` with Claude Code, and keep a copy of every envelope | [Give agents the store](operate/agents.md) |
| Run the loop between the store and its git remote, with a person's review | [The daily loop](operate/daily-loop.md) |
| Clone, fetch, push and sync with GitHub; tokens and commit authors | [GitHub as a remote](guides/github-remote.md) |
| Build and test an agent's change in a workspace, or in a git worktree | [Build and test a change](operate/build-and-test.md) |
| Keep one writer per trunk through a bare clone, and recover when both sides moved | [One writer per trunk](operate/one-writer-per-trunk.md) |
| Check a store, read a failed check, repair | [Verify and repair](operate/verify-and-repair.md) |
| Back up a store in use, and restore it | [Backup and restore](operate/backup-and-restore.md) |
| Remove what may go, with every writer stopped | [Making room](operate/making-room.md) |
| Watch errors, recoveries, latency, disk and the git side | [Monitoring](operate/monitoring.md) |
| Know where the limits are | [Limits](operate/limits.md) |

## How to read these pages

Every command on these pages was run with the binary built from this repository, and every output
is copied from those runs, cut down where it is long: `…` marks a cut, and the store's path in
`meta.repo`, which echoes the path the caller gave, is left out. `swarmops <command> --help` is
the authority on flags, and the README's [Commands](../README.md#commands) table lists them all.
The examples run the long name, `swarmops`; `so` is the same program.

## Before you start

- **Linux** and **git** on the host, 2.39 or later for a git remote ([Install and upgrade](operate/install-and-upgrade.md#before-you-start)).
- **One host**, a **native Linux filesystem** for the store, and **one Unix user** who owns it
  ([Stores](operate/stores.md#where-a-store-may-live), [permissions](operate/stores.md#set-the-permissions)).

## Sections of the single-page guide

This guide was one page until its sections moved to the task pages above. Each former section
keeps its heading here, so that links to it still arrive, and points to where its text now is.

## Install

[Install and upgrade](operate/install-and-upgrade.md): the installer, a clone, or a release archive.

### With the installer

[Install with the installer](operate/install-and-upgrade.md#install-with-the-installer).

### From a clone

[Install from a clone](operate/install-and-upgrade.md#install-from-a-clone).

### From a release

[Install from a release archive](operate/install-and-upgrade.md#install-from-a-release-archive).

## Create a store

[Create a store](operate/stores.md#create-a-store); where it may live and its size:
[Where a store may live](operate/stores.md#where-a-store-may-live).

## Identities

[Identities](operate/stores.md#identities).

## Give agents the store

[Give agents the store](operate/agents.md).

### The tools agents get

[The tools agents get](operate/agents.md#the-tools-agents-get).

### Where the envelopes are

[Keep a copy of every envelope](operate/agents.md#keep-a-copy-of-every-envelope).

## The daily loop

[The daily loop](operate/daily-loop.md).

### Once: bring the repository in

[Once: bring the repository in](operate/daily-loop.md#once-bring-the-repository-in).

### 1. Take in what git gained

[1. Take in what git gained](operate/daily-loop.md#1-take-in-what-git-gained).

### 2. Agents commit and land

[2. Agents commit and land](operate/daily-loop.md#2-agents-commit-and-land).

### 3. Look at what landed

[3. Look at what landed](operate/daily-loop.md#3-look-at-what-landed).

### 4. Send it to git for review

[4. Send it to git for review](operate/daily-loop.md#4-send-it-to-git-for-review).

### 5. Review, then move the trunk

[5. Review, then move the trunk](operate/daily-loop.md#5-review-then-move-the-trunk).

### Building and testing a change

[Build and test a change](operate/build-and-test.md).

### One writer per trunk

[One writer per trunk](operate/one-writer-per-trunk.md).

## Checking a store

[Verify and repair](operate/verify-and-repair.md).

## Backup and restore

[Backup and restore](operate/backup-and-restore.md).

## Making room

[Making room](operate/making-room.md).

## Upgrades

[Upgrade](operate/install-and-upgrade.md#upgrade).

## Limits

[Limits](operate/limits.md).

## What to monitor

[Monitoring](operate/monitoring.md).
