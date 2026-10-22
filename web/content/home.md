---
title: "SwarmOps: source control for coding-agent swarms"
description: "One embedded store that many coding agents commit into at once, one JSON envelope per command, and a round trip back to git. Linux, v0.1.0."
---

# SwarmOps

SwarmOps is a git replacement for coding-agent swarms: many agents commit into one embedded store
at once, every command answers in JSON, and the work moves back to git.
It is built for 10 to 50 Claude Code or LangGraph processes on one host, as one program with no
external database and no daemon.

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash
```

The installer puts `swarmops` and `so`, its short name, into `~/.local/bin` (`/usr/local/bin` as
root), checked against the release's `SHA256SUMS`. [Install options](/docs/operations/#install)

## Six things it does

Every example below ran against a scratch store, `acme`, with `swarmops` 0.1.0; the output is as
printed. `SWARMOPS_REPO=acme` and `SWARMOPS_AGENT=agent/worker-1` were set.

### 1. Many agents commit into one store

Each agent commits a JSON change list on the changeset it read; object writes take no lock, and
metadata writes queue in the kernel for one lock.

```console
$ so --as agent/worker-2 commit -m 'add g' --changes g.json --parent e71a197799a7
{"ok":true,"op":"commit","api":1,"result":{"changeset":"2ad559c4a3e1d1cba5d6d12da2cea3cf2276b8212eaa4eb6b7c9450bf154ae33","parent":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce","root":"c6952621b35e8f0a00f377c47c1c7f971b81bd8fa4cade1fa2c9d8aee447296e","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"acme","elapsed_ms":394,"resolved":{"e71a197799a7":"e71a197799a79a49b5f6cd7cb2d371609e0d7d97fb2f51630da453d9dff012ce"},"auth":"trust"}}
```

### 2. Land merges onto `main` and retries by itself

`land` merges a change three-way onto `main`'s head, whatever `main` gained since, and publishes it
as one changeset in one transaction. The same land run again with its key replays it.

```console
$ so --as agent/worker-3 land e556fc0d571f --key 019a0b6e-7c3d-7000-8000-000000000002 --human
landed 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093 on main (onto 09a8de96fe4c00c99d958d62e6d0009439463de886b4dbfe6c10d2a57a994454, attempt 1)
$ so --as agent/worker-3 land e556fc0d571f --key 019a0b6e-7c3d-7000-8000-000000000002 --human
replayed 997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093
```

### 3. Errors an agent can act on

A failure carries a stable `E_*` code, a fixed exit code, whether retrying can help, and a `hint`,
often the next command to run. This one exited with code 5.

```console
$ SWARMOPS_AGENT= so commit -m x --changes g.json
{"ok":false,"op":"commit","api":1,"error":{"code":"E_IDENTITY_REQUIRED","message":"this operation changes the store, so it needs the caller's identity","retryable":false,"exit":5,"details":{},"hint":"pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

### 4. An MCP server for agents

`swarmops mcp` serves the store on stdin and stdout to Claude Code or any MCP client, one tool per
operation; a tool's result is the envelope the CLI prints.

```console
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | so mcp | tail -n 1 | python3 -c 'import json,sys; print(" ".join(t["name"] for t in json.load(sys.stdin)["result"]["tools"]))'
repo_info resolve_rev read_file read_tree show log path_history diff op_log ref_list ref_set ref_delete undo commit land blob_put blob_has verify
```

### 5. An operation log with undo

Every commit, land and ref move is one numbered operation. `op undo` moves the refs an operation
moved back, all or none, by compare-and-swap; changesets are never removed.

```console
$ so op undo 10
{"ok":true,"op":"undo","api":1,"result":{"undone":10,"op":12,"ref_moves":[{"name":"main","old":"63eb0843d5ac02ccedb4eb6655d52979e84317b3203f65edd6f8d99c36e77313","new":"997ca54c77e1a673ae9ee34cbc2e1a1ff16ec82ff74c817251323d687aecd093","actor":"agent/worker-1","at":1792626985}],"already_applied":false},"meta":{"repo":"acme","elapsed_ms":542,"resolved":{},"auth":"trust"}}
```

### 6. A round trip with git

`git import` brings a git history in; `git export` writes the store's history back into a git
branch, so CI and deployments keep running on git. A commit imported from git exports as itself,
byte for byte (`68c3026` below); one made in the store gets its agent as author. Here the store
`shop` holds a one-commit git history, imported from `src-git`, and one commit made in the store:

```console
$ SWARMOPS_REPO=shop so git export out.git --branch stable --human
exported 2 commits to stable (none → d24bfc07e8608349c43c01bcb83c176dc50cd70b)
exported: 2
objects_written: 6
head_changeset: 69e0d93bdcd3bde4045a43befdcbb3266f939ce0a35c496ef577d3ba30d426f0
head_sha: d24bfc07e8608349c43c01bcb83c176dc50cd70b
branch: stable
branch_moved: true
incremental: false
rewritten: 0
$ git -C out.git log --format='%h %an %s' stable
d24bfc0 agent/worker-1 add world
68c3026 Dev hello
```

[Features](/product/features/) shows every other command the same way.

## Status

v0.1.0, the first release, for Linux on x86_64 and aarch64 (glibc 2.34 or newer; the aarch64
build is smoke-tested under QEMU, not on aarch64 hardware). It is
pre-release: usable by one agent, with a store that takes commits from many processes at once;
landing is built, but not yet measured under a swarm's load. [Roadmap](/roadmap/)

[Read the docs](/docs/) · [Product](/product/) · [How it works](/product/how-it-works/) ·
[SwarmOps and git](/product/vs-git/) · [GitHub](https://github.com/KeiaiLab/SwarmOps)
