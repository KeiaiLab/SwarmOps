# Security policy

## Reporting a vulnerability

Report a vulnerability privately, never in a public issue, pull request, discussion or commit.

The channel is GitHub's private vulnerability reporting: on the repository's **Security** tab,
choose **Report a vulnerability**. That opens a draft security advisory that only you and the
maintainers can see; we work out the fix with you there, and publish the advisory once the fix
has landed. GitHub offers this for public repositories only, and SwarmOps' repository is private
while SwarmOps is pre-release: it will be turned on when the repository goes public. Until then,
the people who can see the repository report to a maintainer directly, outside the issue
tracker.

Please include what you found, how to reproduce it (a store, a command line, a crafted file),
what it lets an attacker do, and the commit you tested. A fix lands on `stable`, and
[CHANGELOG.md](CHANGELOG.md) notes it under **Security**.

## Supported versions

SwarmOps has not been released: there are no tags, binaries or published crates. Security fixes
go to the main line, the `stable` branch, and nowhere else.

| Version | Supported |
| --- | --- |
| `stable` (pre-release, 0.1.0) | yes |
| anything else | no |

## What is in scope

The trust model is M1's: SwarmOps runs on one host, as a library linked into each `swarmops`
process, with no daemon and no network service. Every agent and person that runs `swarmops`
against a store is a trusted local user acting for the same owner, and the store directory's
filesystem permissions are the access boundary. Within that model, these are vulnerabilities:

- **Crafted or damaged store files.** SwarmOps reads files it did not necessarily write: a
  store copied or restored from elsewhere, a damaged disk, a file someone placed there. Reading
  any of them must fail with an error, never panic, abort, hang, read out of bounds, allocate
  without bound or return wrong data as valid. That covers object files
  (`objects/<2hex>/<62hex>`: the header, the zstd frame and the payload, which is checked against
  its id on every read), the journal (`meta/journal.log`, `journal.off`, `journal.ckpt`), the
  changeset index (`meta/idx/*.col`, `meta/parents.log`, `meta/sorted/*`) and ref files
  (`meta/refs/*`). These cases are known and tracked
  ([docs/known-limitations.md](docs/known-limitations.md), "Hostile input"): a crafted zstd frame
  whose recorded size is huge makes the read panic or abort while it allocates, instead of
  failing with an error; crafted `journal.off` entries that agree with each other, deeper than
  the two links recovery and readers check, pass as valid, so a read that starts past them
  numbers its frames by them until `verify` walks the file and reports them (recovery checks
  every listed frame before it cuts or lists anything, so no committed frame is lost); a
  `journal.log`, `journal.off`, index column or sorted view whose length is huge (sparse or
  crafted) is read into one buffer of that length, so a read allocates without bound and aborts
  when the allocation fails instead of returning an error; and `REPO`, whose name `init` does
  not bound, is read whole with no size cap, so a huge one costs its length in memory and fails
  with `E_IO` only when the allocation fails (`FORMAT`, `journal.ckpt` and the ref files are
  capped at their fixed sizes, and a store file that is not a regular file is refused before it
  is opened).
- **Requests.** Change lists, revisions and paths that get past their checks: a repository
  path with a `..` component, a leading `/` or a NUL byte; a request that makes SwarmOps write
  outside the store; or one that corrupts the store.
- **Test hooks in a release build.** The `SWARMOPS_TEST_*` variables make a process crash or
  fail at a chosen filesystem call, count its calls, or pin its clock. A build without the
  `test-hooks` feature, as `cargo build --release` is, must ignore them; one that honours them
  is a vulnerability.
- **The git import (M3, planned).** Importing a git repository brings in content and paths that
  an attacker controls. Path traversal and symlink attacks through crafted repositories are a
  known class in this field: jj's critical
  [GHSA-88h5-6w7m-5w56](https://github.com/jj-vcs/jj/security/advisories/GHSA-88h5-6w7m-5w56),
  "Path traversal via crafted Git repositories", and gitoxide's
  [GHSA-6p9q-f2xg-6pr5](https://github.com/GitoxideLabs/gitoxide/security/advisories/GHSA-6p9q-f2xg-6pr5),
  a symlink planted during a forced checkout that sends writes outside the worktree, and
  [GHSA-x862-c2wj-4mwr](https://github.com/GitoxideLabs/gitoxide/security/advisories/GHSA-x862-c2wj-4mwr),
  an allocation sized by a pack header's object count that aborts the process. An import that
  writes outside the store, follows a symlink out of a workspace, or exhausts memory on a
  crafted repository will be a vulnerability. Workspaces (M5), which write files to disk, are in
  scope the same way.
- **The SwarmOps supply chain.** A dependency or a CI workflow that could change what we build
  or run. `cargo deny check` (advisories, licenses, bans, sources) runs in every `just check`,
  `Cargo.lock` is committed and builds use `--locked`, and the GitHub Actions workflows pin every
  action by commit, start from `permissions: {}`, keep no credentials after checkout and are
  checked with zizmor. A known advisory in a dependency that is already public can go to a
  normal issue.

## What is not a vulnerability under M1's model

- **One agent acting as another.** In M1 an identity (`--as`, `SWARMOPS_AGENT`) is a claim that
  is recorded, not verified: every envelope's `meta.auth` says `"trust"`. Tokens arrive later
  (`E_AUTH` is reserved for them).
- **Anything a local user with write access to the store can do directly**: delete, rewrite or
  corrupt its files, hold its lock, fill its disk. Filesystem permissions are the boundary.
  SwarmOps detecting such damage is in scope (`swarmops verify --deep` re-hashes every object);
  preventing it is not.
- **Reading files the caller can read.** A change list's `{"file": "<path>"}` content is read
  on the client, with the caller's own permissions, as any program the caller runs could read
  it; the stdio MCP server runs as its caller and reads it the same way. A network server (M10)
  must refuse such content, and will.
- **Durability failures.** Data lost after a crash, a power loss or a disk error is a serious
  bug, not a vulnerability: report it as an issue. [docs/architecture.md](docs/architecture.md)
  describes the durability model and its tests.
- **Builds with the `test-hooks` feature**, which exist for the test suite only.

When SwarmOps gains a network server (M10), the trust model changes, and this policy with it.
