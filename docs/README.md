# SwarmOps documentation

SwarmOps is version control designed for large pools of coding agents: their processes commit into
one [store](concepts.md#store) at once, every command answers in one JSON
[envelope](concepts.md#envelope), and the work goes back to git.

It is measured today at 50 agents landing on one host: 25.0 lands/s, nothing lost
([perf.md, "D2-8 gated runs — 2026-11-09"](perf.md#d2-8-gated-runs--2026-11-09)).

## Start here

- **Install it:** [Install SwarmOps on Linux](guides/install.md): one command, a package, or a
  build from source.
- **Make a first commit:** [Quick start](guides/quick-start.md): a store, a commit, a
  [land](concepts.md#land) and an undo.
- **Run a swarm:** [Parallel agents](guides/parallel-agents.md): many agents on one repository,
  beside git worktrees.
- **Bring a git repository in:** [Git round trip](guides/git-round-trip.md): import, land, and
  export back for CI.
- **Look something up:** [CLI reference](reference/cli.md): every command, its arguments, its
  output and its errors.
- **Keep a store healthy:** [Verify and repair](operate/verify-and-repair.md): check a store, read
  what it finds, repair it.

## How these pages are organised

Start pages ([Install](guides/install.md) first) take you from nothing to a store with work in it.
Guides ([Commit and land](guides/commit-and-land.md) first) walk through one task with runs you
can repeat.
The concept page ([Concepts](concepts.md)) defines each noun once, from the store to the
[workspace](concepts.md#workspace).
Reference pages ([CLI reference](reference/cli.md) first) are generated from the code: commands,
errors, limits, types and tools.
Operate pages ([Operating SwarmOps](operations.md) first) are for whoever runs SwarmOps for a swarm
on one host.
Design pages ([Goals](goals.md) first) say what SwarmOps sets out to do, what is measured and what
is still open.

## What SwarmOps does not do

[Goals, Non-goals](goals.md#non-goals) lists what SwarmOps does not set out to be: among them a
general-purpose replacement of git, a cluster, and writes without a lock.
[Known limitations](known-limitations.md) lists what is still open, and which milestone takes each
item.

## All pages

### Start

| Page | Read it to |
| --- | --- |
| [Install](guides/install.md) | Install `swarmops` and `so` with the installer, a package, a release archive or from source; upgrade or remove them. |
| [Quick start](guides/quick-start.md) | Make a store, commit, land a change, meet a conflict and read the op log. |
| [Concepts](concepts.md) | Learn what a store, a [revision](concepts.md#revision), a [change list](concepts.md#change-list), a [ref](concepts.md#ref), a land, an envelope and a workspace are. |

### Guides

| Page | Read it to |
| --- | --- |
| [Commit and land](guides/commit-and-land.md) | Publish a change: commits, compare-and-swap, landing, retries and conflicts. |
| [Parallel agents](guides/parallel-agents.md) | Run many agents on one repository, and see how that compares with agents in git worktrees. |
| [History and undo](guides/history-and-undo.md) | Read history and diffs, and take an operation back. |
| [Git round trip](guides/git-round-trip.md) | Import a git repository, and export the work back for review and CI. |
| [GitHub as a remote](guides/github-remote.md) | Clone, fetch, push and sync with GitHub; pull requests, tokens in CI, commit authors, a sync that stops. |
| [A store as a remote](guides/store-remote.md) | Clone, fetch, push and sync another store that `swarmops serve` serves; what crosses between stores and what stays. |
| [Claude Code](guides/claude-code.md) | Give Claude Code agents the store through MCP. |

### Develop

| Page | Read it to |
| --- | --- |
| [MCP integration](develop/mcp.md) | Connect any MCP client: registration, the stdio exchange, identity, every tool and its command, limits and failures. |
| [CLI scripting](develop/cli-scripting.md) | Drive SwarmOps from scripts: the envelope, exit codes, jq recipes, retries and idempotency keys. |
| [For agents and programs](agents.md) | Find the pages as Markdown, and know what a program may rely on. |
| [Code style](develop/style.md) | How the Google Rust style guide is kept here: what the gate enforces, what review reads for, the recorded deviations. |

### Operate

| Page | Read it to |
| --- | --- |
| [Operations](operations.md) | Find the task page for running SwarmOps for a swarm. |
| [Stores](operate/stores.md) | Create a store, place it on a supported filesystem, and set identities. |
| [Install and upgrade](operate/install-and-upgrade.md) | Install, upgrade and remove SwarmOps on a host. |
| [Agents](operate/agents.md) | Give agents the store through MCP. |
| [The daily loop](operate/daily-loop.md) | Move work between the store and git every day. |
| [Build and test](operate/build-and-test.md) | Build and test a change on disk before it lands. |
| [One writer per trunk](operate/one-writer-per-trunk.md) | Keep one writer on the git side of each trunk. |
| [Monitoring](operate/monitoring.md) | Know what to watch on a running store. |
| [Limits](operate/limits.md) | Know the limits a store and a request enforce. |
| [Making room](operate/making-room.md) | See what a store keeps and how to free space. |
| [Backup and restore](operate/backup-and-restore.md) | Back a store up and bring it back. |
| [Verify and repair](operate/verify-and-repair.md) | Check a store and act on what the check finds. |
| [Serve over ssh](operate/serve-over-ssh.md) | Let people and CI clone, fetch and push a store with stock git over ssh. |
| [Serve over HTTP](operate/serve-over-http.md) | Serve the same store over HTTPS, through a web server. |
| [Where serve differs from git](operate/serve-differences.md) | See each place where `swarmops serve` answers stock git differently from git's server. |
| [Troubleshooting](troubleshooting.md) | Go from a symptom to its cause and the command that fixes it. |

### Reference

| Page | Read it to |
| --- | --- |
| [CLI reference](reference/cli.md) | Look up a command, its arguments and its output. |
| [Error reference](reference/errors.md) | Look up an `E_*` code, its exit code and what to do. |
| [Limits](reference/limits.md) | Look up a limit, its value, constant, commands and error. |
| [Schema types](reference/schema-types.md) | Look up a request, response or value type and its fields. |
| [MCP tool reference](reference/mcp-tools/README.md) | Look up an MCP tool, its input schema, annotations, an example call and its CLI command. |
| [Coming from git](reference/coming-from-git.md) | Look up a git command, what to type instead and the hint a refusal carries. |

### Project

| Page | Read it to |
| --- | --- |
| [Goals](goals.md) | See what SwarmOps sets out to do: each goal with its metric, target, measured value and milestone. |
| [Roadmap](roadmap.md) | See where SwarmOps is going: the milestones, built and planned, and working with GitHub in three topologies. |
| [Architecture](architecture.md) | Understand the design: crates, data model, the store on disk, the write and read paths, durability, concurrency, the contract. |
| [Known limitations](known-limitations.md) | See what is still open, and which milestone takes each item. |
| [Performance](perf.md) | See every measurement, and the decisions taken on them. |
| [Pilot](pilot.md) | Follow the runbook of the first real use, on this repository. |
| [Changelog](../CHANGELOG.md) | See what changed, release by release. |
| [Versioning and stability](policies/versioning-and-stability.md) | Know what stays compatible, and how a breaking change is made. |
| [MSRV and platforms](policies/msrv-and-platforms.md) | Know the Rust version and the platforms SwarmOps supports. |
| [Clean-room development](policies/clean-room.md) | See how third-party designs reach the code: requirements only, with provenance and checks. |
| [SwarmOps and git](https://swarmops.cc/product/vs-git) | Compare SwarmOps with git, dimension by dimension. |
| [Metadata store comparison](../bench/compare/README.md) | See how the native metadata store is measured against embedded databases. |
| [Support](../SUPPORT.md) | Find where to ask a question or report a problem. |
| [Security](../SECURITY.md) | Report a vulnerability privately, and learn what counts as one. |

The design documents are the authority: the umbrella spec and the storage, operations and review
specs, in Korean, which the maintainers keep privately; [Architecture](architecture.md) is their
public overview. How changes are made is in [CONTRIBUTING.md](../CONTRIBUTING.md), who makes them
in [MAINTAINERS.md](../MAINTAINERS.md), and how a release is cut in [RELEASE.md](../RELEASE.md).
