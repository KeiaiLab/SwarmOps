<p align="center">
  <img src="docs/assets/logo.svg" alt="The SwarmOps logo: four agents' branches merging into one main line" width="112" height="112">
</p>

# SwarmOps

SwarmOps is version control for swarms of coding agents: many agent processes commit into one repository at once.

[![CI](https://github.com/KeiaiLab/SwarmOps/actions/workflows/ci.yml/badge.svg?branch=stable)](https://github.com/KeiaiLab/SwarmOps/actions/workflows/ci.yml?query=branch%3Astable)
[![Latest release](https://img.shields.io/github/v/release/KeiaiLab/SwarmOps)](https://github.com/KeiaiLab/SwarmOps/releases)
[![Licence](https://img.shields.io/github/license/KeiaiLab/SwarmOps)](LICENSE)

[Documentation](https://swarmops.cc/docs) · [Install](#install) · [Quick start](#quick-start) ·
[Roadmap](#roadmap) · [Contributing](CONTRIBUTING.md)

SwarmOps is pre-release, at v0.11.0 (2026-11-09). It is designed for large pools of coding agents
that commit, land and sync with git. It is measured today at 50 agents landing on one host: 25.0
lands/s, nothing lost ([perf.md](docs/perf.md#d2-8-gated-runs--2026-11-09)). The
[roadmap](docs/roadmap.md) says what comes next.

Linux x86_64 (tested) and aarch64 (smoke-tested under emulation), glibc 2.34 or newer; no macOS or Windows build yet.

## Install

The recommended path:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash
```

It installs `swarmops` and `so` into `~/.local/bin` from the latest
[release](https://github.com/KeiaiLab/SwarmOps/releases), checked against its `SHA256SUMS`, and
running it again updates.

- `.deb` and `.rpm` packages: [install guide](docs/guides/install.md#how-do-i-install-the-deb-or-rpm-package).
- Pinning a version: [install guide](docs/guides/install.md#how-do-i-pin-a-version).
- Building from source: [install guide](docs/guides/install.md#how-do-i-build-from-source).

## Quick start

A store, a first commit and a read. The commands use `so`; `swarmops` runs the same.

```sh
so --version
so init ./acme --name acme
printf '%s\n' '[{"path": "hello.txt", "op": "put", "content": {"inline": "hello\n"}}]' > change.json
so --repo ./acme --as agent/worker-1 commit -m "Add hello" --changes change.json --advance main
so --repo ./acme cat main:hello.txt
```

Output of a 0.11.0 build of `stable` at 6e9dac27, on a busy host:

```text
swarmops 0.11.0+6e9dac27f0ad5098b91fd2156a7e826c9bc5676a
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"e6092c987a444fdf107662f9ec67c1be","name":"acme","store_version":2,"changesets":0,"refs":0},"meta":{"repo":"./acme","elapsed_ms":906,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"commit","api":1,"result":{"changeset":"5d6a7b500a60e85d18f6ffd05d7c62836086a34afd57a1f3d706dca00397692e","root":"df3040af7381e7e412c6a0c5e9ab8b490ab80d92dedad94530d98340ced54a6e","files_changed":1,"blobs_written":1,"advanced":{"name":"main","new":"5d6a7b500a60e85d18f6ffd05d7c62836086a34afd57a1f3d706dca00397692e","already_applied":false},"replayed":false},"meta":{"repo":"./acme","elapsed_ms":267,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"read_file","api":1,"result":{"blob":"464418f5a53907c921906843e6f1f4f984f144067b3bf32d69c1ff4c13627464","size":6,"kind":"file","encoding":"utf8","content":"hello\n","truncated":false},"meta":{"repo":"./acme","elapsed_ms":1,"resolved":{"main":"5d6a7b500a60e85d18f6ffd05d7c62836086a34afd57a1f3d706dca00397692e"},"auth":"trust"}}
```

Each store command prints one JSON envelope. `--advance main` moved `main` to the new changeset in the
commit's own transaction. Reads need no identity; a commit, a land, a ref move and an undo need
one, from `--as` or `SWARMOPS_AGENT`. The [quick start guide](docs/guides/quick-start.md) goes on
to a land, a conflict and an undo.

## What it does

### Many agents commit into one store at once

Each agent process writes the store directly: object writes take no lock, metadata writes queue in
the kernel for one `flock`, which a killed agent never leaves behind, and refs move only by
compare-and-swap. Measured (G1, met): 50 processes × 1,000 commits through the binary, 74.0
commits/s, no `E_BUSY`, no commit lost ([perf.md](docs/perf.md#v060--2026-10-28);
[Parallel agents](docs/guides/parallel-agents.md)).

### Land with retry and a conflict report

`land` merges a change three-way onto a ref's head without a checkout and moves the ref in one
transaction, merging again and retrying when the ref moves; `--key` makes a rerun a replay. A
conflict records nothing and returns `E_CONFLICT` with each file's hunks and the agent that last
changed it; `land --resolve` settles it path by path. Measured (G4, met at 50 processes): 25.0
lands/s, p99 157.320 ms and 154.458 ms in two gated runs, no retry, nothing lost; the 32-process
runs are owed ([perf.md](docs/perf.md#d2-8-gated-runs--2026-11-09); [Commit and land](docs/guides/commit-and-land.md)).

### One JSON envelope per command, and an MCP server

Every command prints one envelope, `ok`, `op`, `api`, `result` or `error`, then `meta`
([exceptions](docs/develop/cli-scripting.md#the-envelope)). A failure carries a stable `E_*` code,
the exit code, `retryable`, `details` and a `hint` ([error reference](docs/reference/errors.md)).
`swarmops schema` prints the JSON Schema of every request and response, and `swarmops mcp` serves
the operations to an agent as 46 MCP tools ([MCP integration](docs/develop/mcp.md)).

### Git and GitHub round trip

`git import` brings a git history in; `git export` writes it back, an imported commit as itself,
byte for byte, a store changeset with a `SwarmOps-Changeset:` trailer. `clone`, `fetch`, `push`
and `sync` reach a git remote such as GitHub through your own git (2.39 or later), pushing by
compare-and-swap, never by force. Measured (G17, met): one commit's push on 12,000 commits 1.02×
its cost on 1,000 ([perf.md](docs/perf.md#g17-one-commit-on-12000-commits-against-1000);
[Git round trip](docs/guides/git-round-trip.md), [GitHub as a remote](docs/guides/github-remote.md)).

### Workspaces

`so ws create` writes a changeset's tree, or the paths under `--prefix`, into a directory where an
agent edits, builds and tests. `ws land --check` commits, rebases, runs the check and publishes
only if the target has not moved, so the landed tree is the tested one. An adapter gives Claude
Code's worktree hooks a workspace ([Build and test a change](docs/operate/build-and-test.md)).

[Features](https://swarmops.cc/product/features) runs each, plus the op log and undo, crash safety and `verify`.

## When not to use it

- You need macOS or Windows: SwarmOps builds for Linux only today (M12, planned).
- You need git parity: hooks, signing, submodules and LFS are a non-goal.
- Several hosts must write into one store at once: today one host writes and the others sync
  through a git remote (M11, planned).
- The store would sit on NFS, SMB, FUSE or under `/mnt/`: not supported
  ([where a store can live](docs/guides/install.md#where-can-a-store-live)).

[Non-goals](docs/goals.md#non-goals) and [known limitations](docs/known-limitations.md) list the
rest.

## SwarmOps and git

SwarmOps works beside git: it imports from git and exports to it, so review, CI and deployment
stay in git. Judged from the design and the code, it is ahead by design on the order of writes,
stale locks, durability and machine-readable errors, and behind on write throughput, storage on
disk, review and ecosystem. No benchmark against git exists. The full comparison, dimension by
dimension, is [SwarmOps and git](https://swarmops.cc/product/vs-git).

## Documentation

- [Install](https://swarmops.cc/docs/guides/install): the installer, packages, a source build.
- [Quick start](https://swarmops.cc/docs/guides/quick-start): a store, a commit, a land, an undo.
- [Parallel agents](https://swarmops.cc/docs/guides/parallel-agents): run a swarm on one repository.
- [Commit and land](https://swarmops.cc/docs/guides/commit-and-land): publish a change, retries and conflicts.
- [MCP integration](https://swarmops.cc/docs/develop/mcp): give agents the store as MCP tools.
- [Git round trip](https://swarmops.cc/docs/guides/git-round-trip): import a git repository, export the work back.
- [CLI reference](https://swarmops.cc/docs/reference/cli): every command, its output and its errors.
- [Operating SwarmOps](https://swarmops.cc/docs/operations): checks, backup and restore, upgrades, limits.

Every page, by group: [docs/README.md](docs/README.md).

## Roadmap

Each milestone's deliverable and exit criteria are in [docs/roadmap.md](docs/roadmap.md#milestones),
under "Milestones"; macOS and Windows builds are planned (M12).
[CHANGELOG.md](CHANGELOG.md) says what each release shipped, and
[docs/known-limitations.md](docs/known-limitations.md) what is still open, with the milestone that
takes each item.

## Contributing, security and licence

- [CONTRIBUTING.md](CONTRIBUTING.md): how changes are made; `just check` is the definition of done.
- [SECURITY.md](SECURITY.md): how to report a vulnerability privately.
- [SUPPORT.md](SUPPORT.md): where to ask a question or report a problem.
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md): the conduct expected in every project space.
- [LICENSE](LICENSE): MIT. Each release archive also carries
  [THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES), the licences of the crates compiled into `swarmops`
  and `so`.
