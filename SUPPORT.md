# Support

This page says where to get help with SwarmOps: issues for bugs and feature requests, a private
report for a vulnerability, and email for a conduct problem.

## Where do I ask a question or report a bug?

In a GitHub issue on `KeiaiLab/SwarmOps`, through its templates: **Bug report** for something
SwarmOps did wrong, **Feature request** for something it should do. A question that fits neither
can be a blank issue. The repository has no GitHub Discussions.

Before you open one, check [docs/known-limitations.md](docs/known-limitations.md): the problem may
be known already.

## What should a bug report include?

What you ran and what SwarmOps printed. The **Bug report** form asks for these:

| Field | What to give |
| --- | --- |
| What happened | What you did, and what SwarmOps did. |
| What you expected | What SwarmOps should have done. |
| Commands and envelopes | The commands you ran and the JSON envelope each printed (`error.code`, `details` and `hint` matter most). Remove paths or content you cannot share. |
| `swarmops verify --deep` | Its envelope on the affected store, if a store is involved. |
| Area | The part of SwarmOps it concerns, from the form's list. |
| Did anything interrupt SwarmOps first? | A killed process, a power loss or a VM or WSL reset, a full disk or I/O errors, nothing, or not sure. |
| SwarmOps commit | The commit the binary was built from (`git rev-parse --short HEAD`). |
| Environment | Kernel (`uname -r`), the store's filesystem (`stat -f -c %T <store>`), and how many processes or agents shared the store. |

With a release binary, give the version that `swarmops --version` prints as the SwarmOps commit.

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

SwarmOps is pre-1.0, and its security fixes go to the main line, the `stable` branch, and nowhere
else. The `0.1.0` release is not supported on its own: its fixes go to `stable`
([Supported versions](SECURITY.md#supported-versions)).

## Where is the documentation?

In the repository: the [README](README.md), [Install SwarmOps on Linux](docs/guides/install.md),
the [quick start](docs/guides/quick-start.md), [Concepts](docs/concepts.md) and
[Operating SwarmOps](docs/operations.md), the guide for whoever runs SwarmOps for a swarm of coding
agents on one host.
