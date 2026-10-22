# Clean-room development

SwarmOps is built from its own specification. Third-party designs, articles and code may inform
what SwarmOps should do; they never supply how it is written. This page says how that separation
works, what crosses it, what is recorded, and what is checked before a release. It applies to
every change from 2026-10-22 on.

## Why

- **Independent expression.** Ideas and functional designs are free to learn from; their
  expression (code, text, structure, figures) is not. Keeping the two apart keeps every line of
  SwarmOps its own.
- **One source of truth.** SwarmOps' specifications, code and measurements decide how it behaves.
  A requirement that cannot be stated in those terms is not ready to build.
- **A record that holds up.** Each feature can show where its requirement came from and who saw
  what, without relying on anyone's memory.

## Two sides

| | Specification side | Implementation side |
| --- | --- | --- |
| Reads | Public third-party material: articles, documentation, design descriptions, and code where a feature needs it, each one recorded | The repository's own specifications, code, tests and [docs/perf.md](../perf.md); the requirements handed across; the documentation of SwarmOps' dependencies; the published specification of a format or protocol SwarmOps interoperates with, such as git's |
| Writes | Functional requirements in its own words, and their provenance records | Code, tests, public documents and SwarmOps' specifications |
| Never | Writes code, tests or public text for the feature it specified | Reads the third-party material a requirement came from, or asks the specification side about it |

A person or an agent session belongs to one side per feature. One that has read the third-party
material behind a feature does not write that feature's code or public text. In the project's
agent workflow, the specification side is its own role, in sessions of its own; the requirement
document is the only thing passed to the implementation side.

## What a requirement may contain

A requirement says what SwarmOps does, in terms anyone could test.

May contain:

- behaviour a user or an agent can observe: inputs, outputs, error cases, ordering and
  durability guarantees;
- targets as numbers SwarmOps sets for itself, tied to its own goals and measurements
  ([docs/goals.md](../goals.md));
- facts needed to interoperate, such as a public file format;
- generic technical terms (compare-and-swap, write-ahead log, content addressing).

May not contain:

- code or pseudocode taken from a source, or a description of it line by line;
- quoted or closely paraphrased text;
- identifiers taken from a source: type, function, command, flag or file names;
- a source's structure: its module layout, section order, table layout or figures;
- names of third-party products or projects.

Examples:

- Allowed: "A ref moves only while it still names the changeset the caller read. Otherwise the
  call records nothing and reports what the ref names now."
- Allowed: "A commit's cost grows with the number of files it changes, not with the size of the
  repository."
- Not allowed: a sentence copied from an article, even with quotation marks.
- Not allowed: "Split the program into the three components the article describes, under the
  same names."
- Not allowed: "Name the error variant as project X does."

## The provenance record

Every feature built from a requirement keeps a record, held by the maintainers with the design
specifications:

- the requirement's id and its text, as handed across;
- who wrote it (the person, or the role and session);
- every source the specification side read for it: title, address, and the date read;
- the date it was handed across;
- the commits that implement it, each naming the requirement id in its body.

Commits and public documents name the requirement id only, never the sources.

## Checks before a release

A release is cut only when all four pass for everything since the previous release:

1. **No third-party code in the history.** Every path added is SwarmOps' own: no vendored or
   copied source tree. Dependencies enter only through `Cargo.lock`.
2. **No shared text.** The public files (the release tree and the site) are compared with every
   third-party text the specification side recorded since the last release. Words are lowercased
   and only alphanumeric tokens kept. A run of six or more consecutive words in common fails the
   check; runs of five are reviewed by hand and pass only as generic technical terms. A cited
   title is not a shared run.
3. **No shared code.** Where the specification side read third-party code, SwarmOps' code is
   compared with it: normalized lines of 40 characters or more, windows of 40 tokens, string
   literals of 12 characters or more and identifiers of 10 characters or more. Each match is
   explained (a fact of the function, prior art older than both, or SwarmOps' own earlier
   record) or rewritten.
4. **Licences.** `cargo deny check` passes over every lockfile with the licences
   [deny.toml](../../deny.toml) allows, and `THIRD-PARTY-LICENSES` matches `Cargo.lock`; both run
   in `just check`.

## On a finding

1. **Stop.** The affected change does not land, or the release is not cut.
2. **Record.** What matched, where, and with which source, in the feature's provenance record.
3. **Rewrite on the clean side.** The matching code or text is removed. Someone who has not read
   the source writes it again from the requirement, and the check runs again.
4. **Released already?** The fix goes into the next release, with a changelog line that says what
   was replaced.

## Work done before this policy

Work up to 2026-10-21 was not done under this separation: the same sessions read a public design
description, wrote the specification and wrote the code. An audit separate from that work, on
2026-10-21, checked the public tree, the full history and the live site:

- no third-party code was found in the history;
- no run of six or more words was shared with the third-party text the design drew on; the runs
  of five were generic version-control terms;
- one short path-validation routine resembles one in another public project; SwarmOps' own plan
  had defined it before anyone working on SwarmOps had seen that project;
- the design adapts ideas and functional structure from that public description, and its
  storage, hashing, durability, interfaces and git export are SwarmOps' own.

[docs/architecture.md](../architecture.md) states the design's lineage.
