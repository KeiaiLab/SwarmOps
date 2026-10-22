# Maintainers

SwarmOps is maintained by one project identity, SwarmOps Maintainers, for the KeiaiLab organisation.
This page says who decides and how a change lands.

## Who maintains SwarmOps?

The maintainers, as one project identity: `SwarmOps Maintainers <maintainers@swarmops.cc>`. Their
commits carry it as author and committer, and only the maintainers read that address. The
repository, `KeiaiLab/SwarmOps`, belongs to the KeiaiLab organisation, which holds the copyright
under the MIT License.

| Role | Who |
| --- | --- |
| Maintainers | `SwarmOps Maintainers <maintainers@swarmops.cc>` |
| Organisation | KeiaiLab: the repository's owner and the copyright holder |

## How are decisions made?

The maintainers decide what lands. Every change reaches `stable`, the trunk, the same way: on a
branch cut from `stable`, with `just check` passing on it, and with the `tests-pass` check, which
the ruleset on `stable` requires, green for the commit that `stable` moves to.

The ruleset also blocks force pushes and deleting `stable`. A change that changes behaviour updates
the spec paragraph that owns it in the same change, a change to a public surface follows the
[versioning policy](docs/policies/versioning-and-stability.md), and a release follows
[RELEASE.md](RELEASE.md). SwarmOps is pre-1.0: under that policy a minor release may break a public
surface other than the hash contract, and a patch release breaks none.

## Where are the design documents?

The maintainers keep them privately: the umbrella spec and its storage, operations and review specs,
in Korean, with the milestone plans. They are the authority on how SwarmOps behaves.
[docs/architecture.md](docs/architecture.md) is the public overview, in English.

[docs/known-limitations.md](docs/known-limitations.md) lists what is still open, and which
milestone takes each item.

## Can I become a maintainer?

Not yet: adding maintainers is not open. A change from anyone else comes as a pull request to
`stable`, made as [CONTRIBUTING.md](CONTRIBUTING.md) says, and contributions are accepted under the
MIT License.

## Governance

The maintainers decide, within the rules these documents set:

| Document | What it governs |
| --- | --- |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How changes are made, the rules every change follows, and the tests' rules. |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | How everyone takes part in the project, and how the maintainers act on a report. |
| [SECURITY.md](SECURITY.md) | How to report a vulnerability privately, and what counts as one. |
| [Versioning and stability](docs/policies/versioning-and-stability.md) | What stays compatible, and how a breaking change is made. |
| [RELEASE.md](RELEASE.md) | The release checklist. |
| [SUPPORT.md](SUPPORT.md) | Where to ask for help, and where to report a problem. |
