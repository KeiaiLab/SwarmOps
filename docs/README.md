# SwarmOps documentation

The documents, grouped by what you want to do. The [README](../README.md) shows the core
features, each with an example.

## Get started

| Document | Read it to |
| --- | --- |
| [Install](guides/install.md) | Install `swarmops` and `so` with the installer, from a release archive or from source; upgrade or remove them. |
| [Quick start](guides/quick-start.md) | Make a store, commit, land a change, meet a conflict and read the op log. |
| [Concepts](concepts.md) | Learn what a store, a revision, a change list, a ref, a land and an envelope are. |

## Use

| Document | Read it to |
| --- | --- |
| [Commit and land](guides/commit-and-land.md) | Publish a change: commits, compare-and-swap, landing, retries and conflicts. |
| [History and undo](guides/history-and-undo.md) | Read history and diffs, and take an operation back. |
| [Parallel agents](guides/parallel-agents.md) | Run many agents on one repository, and see how that compares with agents in git worktrees. |
| [Claude Code](guides/claude-code.md) | Give Claude Code agents the store through MCP. |
| [Git round trip](guides/git-round-trip.md) | Import a git repository, and export the work back for review and CI. |
| [MCP integration](develop/mcp.md) | Connect any MCP client: registration, the stdio exchange, identity, every tool and its command, limits and failures. |
| [CLI scripting](develop/cli-scripting.md) | Drive SwarmOps from scripts: the envelope, exit codes, jq recipes, retries and idempotency keys. |

## Operate

| Document | Read it to |
| --- | --- |
| [Operations](operations.md) | Find the task page for running SwarmOps for a swarm. |
| [Install and upgrade](operate/install-and-upgrade.md) | Install, upgrade and remove SwarmOps on a host. |
| [Stores](operate/stores.md) | Create a store, place it on a supported filesystem, and set identities. |
| [Agents](operate/agents.md) | Give agents the store through MCP. |
| [The daily loop](operate/daily-loop.md) | Move work between the store and git every day. |
| [Build and test](operate/build-and-test.md) | Build and test a change on disk before it lands. |
| [One writer per trunk](operate/one-writer-per-trunk.md) | Keep one writer on the git side of each trunk. |
| [Verify and repair](operate/verify-and-repair.md) | Check a store and act on what the check finds. |
| [Backup and restore](operate/backup-and-restore.md) | Back a store up and bring it back. |
| [Making room](operate/making-room.md) | See what a store keeps and how to free space. |
| [Monitoring](operate/monitoring.md) | Know what to watch on a running store. |
| [Limits](operate/limits.md) | Know the limits a store and a request enforce. |
| [Troubleshooting](troubleshooting.md) | Go from a symptom to its cause and the command that fixes it. |
| [Pilot](pilot.md) | Follow the runbook of the first real use, on this repository. |
| [Support](../SUPPORT.md) | Find where to ask a question or report a problem. |
| [Security](../SECURITY.md) | Report a vulnerability privately, and learn what counts as one. |

## Reference

| Document | Read it to |
| --- | --- |
| [CLI reference](reference/cli.md) | Look up a command, its arguments and its output. |
| [Error reference](reference/errors.md) | Look up an `E_*` code, its exit code and what to do. |
| [MCP tool reference](reference/mcp-tools/README.md) | Look up an MCP tool, its input schema, annotations, an example call and its CLI command. |
| [Versioning and stability](policies/versioning-and-stability.md) | Know what stays compatible, and how a breaking change is made. |
| [MSRV and platforms](policies/msrv-and-platforms.md) | Know the Rust version and the platforms SwarmOps supports. |
| [Changelog](../CHANGELOG.md) | See what changed, release by release. |

## Design

| Document | Read it to |
| --- | --- |
| [Roadmap](roadmap.md) | See where SwarmOps is going: working with GitHub in three topologies, each with its commands, design, acceptance criteria and milestone. |
| [Goals](goals.md) | See what SwarmOps sets out to do: each goal with its metric, target, measured value and milestone. |
| [Clean-room development](policies/clean-room.md) | See how third-party designs reach the code: requirements only, with provenance and checks. |
| [Architecture](architecture.md) | Understand the design: crates, data model, the store on disk, the write and read paths, durability, concurrency, the contract. |
| [Known limitations](known-limitations.md) | See what is still open, and which milestone takes each item. |
| [Performance](perf.md) | See every measurement, and the decisions taken on them. |
| [Metadata store comparison](../bench/compare/README.md) | See how the native metadata store is measured against embedded databases. |
| [SwarmOps and git](../README.md#swarmops-and-git) | Compare SwarmOps with git, dimension by dimension. |
| [Roadmap](../README.md#roadmap) | See the milestones, built and planned. |

The design documents are the authority: the umbrella spec and the storage, operations and review
specs, in Korean, which the maintainers keep privately; [Architecture](architecture.md) is their
public overview. How changes are made is in [CONTRIBUTING.md](../CONTRIBUTING.md), who makes them
in [MAINTAINERS.md](../MAINTAINERS.md), and how a release is cut in [RELEASE.md](../RELEASE.md).
