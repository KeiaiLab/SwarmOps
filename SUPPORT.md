# Support

This page says where to get help with SwarmOps: issue forms for questions, bugs and feature
requests, a private report for a vulnerability, and email for a conduct problem.

## Where do I ask a question or report a bug?

At <https://github.com/KeiaiLab/SwarmOps/issues/new/choose>, through its forms: **Question** for
anything about using or running SwarmOps, **Bug report** for something SwarmOps did wrong,
**Feature request** for something it should do. The repository has no GitHub Discussions.

Before you open one, check [docs/known-limitations.md](docs/known-limitations.md): the problem may
be known already.

## What should a bug report include?

What you ran and what SwarmOps printed. The **Bug report** form asks for these; the first three
are required:

| Field | What to give |
| --- | --- |
| What happened | The commands you ran and the JSON envelope each printed (`error.code`, `details` and `hint` matter most). Remove paths or content you cannot share. |
| Version | What `swarmops --version` prints. |
| Platform | What `uname -srm` prints. |
| What you expected | What SwarmOps should have done. |
| `swarmops verify --deep` | What `swarmops verify --deep` prints on the affected store, if a store is involved. |
| Area | The part of SwarmOps it concerns: store, cli, mcp, git, remote, workspace, install, docs or site. |
| Did anything interrupt SwarmOps first? | A killed process, a power loss or a VM or WSL reset, a full disk or I/O errors, nothing, or not sure. |
| SwarmOps commit | For a binary built from source, what `git rev-parse --short HEAD` prints in that checkout. |
| Environment | The store's filesystem, as `stat -f -c %T <store>` prints it, and how many processes or agents shared the store. |

## How do I report a security vulnerability?

Privately, never in a public issue, pull request, discussion or commit: on the repository's
**Security** tab, choose **Report a vulnerability**. That opens a draft security advisory that only
you and the maintainers can see.

[SECURITY.md](SECURITY.md) says what to include, and what counts as a vulnerability.

## How do I report a conduct problem?

Privately, by email to the maintainers at <maintainers@swarmops.cc>; only the maintainers read that
address. Never in a public issue, pull request or discussion.

[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) says what the project expects; the maintainers keep reports
confidential and review each one. A security problem goes through [SECURITY.md](SECURITY.md)
instead.

## How soon will I get an answer?

No time is promised: SwarmOps has no service-level agreement, for issues, vulnerability reports or
email.

## Which versions get fixes?

Fixes land on `stable`, the main line, and ship in the next release; no earlier release gets a
fix ([SECURITY.md](SECURITY.md#which-versions-get-fixes)).

## Where is the documentation?

In the repository: the [README](README.md), [Install SwarmOps on Linux](docs/guides/install.md),
the [quick start](docs/guides/quick-start.md), [Concepts](docs/concepts.md) and
[Operating SwarmOps](docs/operations.md), the guide for whoever runs SwarmOps for a swarm of coding
agents on one host.
