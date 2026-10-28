# Security policy

## Reporting a vulnerability

Report a vulnerability privately, never in a public issue, pull request, discussion or commit.

The channel is GitHub's private vulnerability reporting: on the repository's **Security** tab,
choose **Report a vulnerability**. That opens a draft security advisory that only you and the
maintainers can see; we work out the fix with you there, and publish the advisory once the fix
has landed.

Please include what you found, how to reproduce it (a store, a command line, a crafted file),
what it lets an attacker do, and the commit you tested. A fix lands on `stable`, and
[CHANGELOG.md](CHANGELOG.md) notes it under **Security**.

## Supported versions

SwarmOps 0.1.0, 0.2.0, 0.3.0, 0.4.0, 0.5.0 and 0.6.0 are released as the tags `v0.1.0`,
`v0.2.0`, `v0.3.0`, `v0.4.0`, `v0.5.0` and `v0.6.0`, with their archives; no crate is published. Security fixes go to the main line, the `stable` branch, and nowhere else.

| Version | Supported |
| --- | --- |
| `stable` (the main line) | yes |
| `0.6.0`, `0.5.0`, `0.4.0`, `0.3.0`, `0.2.0`, `0.1.0` (released; fixes go to `stable`) | no |
| anything else | no |

## What is in scope

The trust model is M1's: SwarmOps runs on one host, as a library linked into each `swarmops`
process, with no daemon and no network service. Since M13, `clone`, `fetch`, `push` and `sync`
reach a git remote, through the user's own git as a child process: SwarmOps opens no connection
itself. Every agent and person that runs `swarmops`
against a store is a trusted local user acting for the same owner, and the store directory's
filesystem permissions are the access boundary. Within that model, these are vulnerabilities:

- **Crafted or damaged store files.** SwarmOps reads files it did not necessarily write: a
  store copied or restored from elsewhere, a damaged disk, a file someone placed there. Reading
  any of them must fail with an error, never panic, abort, hang, read out of bounds, allocate
  without bound or return wrong data as valid. That covers object files
  (`objects/<2hex>/<62hex>`: the header, the zstd frame and the payload, which is checked against
  its id on every read), the journal (`meta/journal.log`, `journal.off`, `journal.ckpt`), the
  changeset index (`meta/idx/*.col`, `meta/parents.log`, `meta/sorted/*`), ref files
  (`meta/refs/*`), derived values (`meta/derived/*`, a cache whose files carry a checksum
  over their slot and value, not a signature) and external id entries (`meta/external_ids/*`,
  a cache that maps a git commit to its changeset, whose files carry a checksum, not a
  signature). These cases are known and tracked
  ([docs/known-limitations.md](docs/known-limitations.md), "Hostile input"): an object whose
  zstd frame really decompresses to a large size (a compression bomb) is decompressed in full
  before its hash is checked, so reading it takes 512 MiB of payload, the most an object holds,
  plus the decoder window of up to 128 MiB, and about 1.1 GiB of address space (a
  header that records more is refused before any allocation, a frame that only records its size
  as corrupt); crafted `journal.off` entries that agree with each other, deeper than
  the two links recovery and readers check, pass as valid, so a read that starts past them
  numbers its frames by them until `verify` walks the file and reports them (recovery checks
  every listed frame before it cuts or lists anything, so no committed frame is lost); the
  generations a crafted index records (`meta/idx/gen.col`) are checked only against their
  parents' where a history walk uses them, so a run of them crafted to agree can still have a
  land's merge base or `log --path` answer wrongly, though
  every changeset they reach is on the first-parent chain the changeset objects name, and
  `verify` reports the damage; and a
  `journal.log`, `journal.off`, index column or sorted view whose length is huge (sparse or
  crafted) is read into one buffer of that length, so a read allocates without bound and aborts
  when the allocation fails instead of returning an error; and a derived value file crafted with
  a value its derivation does not give, whole and with its trailer right, is refused by a reader
  only where the reader's checks against the index and the objects tell (a skip list's first
  entry, length and each jump against the list one level down; a last touch's changeset against
  the path), and is otherwise read as valid until `verify --deep`, which checks every value of the
  built-in derivations against the objects, reports it (removing `meta/derived/` has every value
  derived again). A derived value's zstd frame is decoded whole within the object bound, 512
  MiB. `FORMAT`, `REPO` (an id and a name of
  at most 255 bytes), `journal.ckpt` and the ref files are capped at their sizes, and a store
  file that is not a regular file is refused before it is opened.
- **Requests.** Change lists, revisions and paths that get past their checks: a repository
  path with a `..` component, a leading `/` or a NUL byte; a request that makes SwarmOps write
  outside the store; one that corrupts the store; or one that makes it allocate without bound.
  A request's size is checked before it is read: its text (a `--changes` file or stdin, or one
  line of `swarmops mcp`'s input) holds at most 64 MiB. A commit's contents hold at most 1 GiB
  together and a put's content at most 512 MiB, both measured by the sizes they declare (a client
  file's length) before a byte of them is read, and again as each content is converted, in list
  order, for a file that grows meanwhile.
- **Test hooks in a release build.** The `SWARMOPS_TEST_*` variables make a process crash or
  fail at a chosen filesystem call, count its calls, or pin its clock. A build without the
  `test-hooks` feature, as `cargo build --release` is, must ignore them; one that honours them
  is a vulnerability.
- **The git import (M3).** Importing a git repository brings in content and paths that
  an attacker controls. Path traversal and symlink attacks through crafted repositories are a
  known class in this field: jj's critical
  [GHSA-88h5-6w7m-5w56](https://github.com/jj-vcs/jj/security/advisories/GHSA-88h5-6w7m-5w56),
  "Path traversal via crafted Git repositories", and gitoxide's
  [GHSA-6p9q-f2xg-6pr5](https://github.com/GitoxideLabs/gitoxide/security/advisories/GHSA-6p9q-f2xg-6pr5),
  a symlink planted during a forced checkout that sends writes outside the worktree, and
  [GHSA-x862-c2wj-4mwr](https://github.com/GitoxideLabs/gitoxide/security/advisories/GHSA-x862-c2wj-4mwr),
  an allocation sized by a pack header's object count that aborts the process. An import that
  writes outside the store, follows a symlink out of a workspace, or exhausts memory on a
  crafted repository is a vulnerability.
- **The workspaces (M5).** `swarmops ws` writes a changeset's files into a directory on the
  caller's host, and the Claude Code hook runs the repository's bootstrap command there, as a build
  would. A write outside the workspace's directory, a write through a symlink, or an update that
  overwrites an edit the agent made is a vulnerability.
- **The git export (M8).** `swarmops git export` writes git objects and one branch into a git
  repository at a path that the request names, on the caller's host, so the export is a trust
  boundary. A request that makes SwarmOps write outside that repository, move a branch other than
  the one it names, or write through a name that git does not take as a branch is a vulnerability.
- **The git remotes (M13): credentials and the child's environment.** SwarmOps stores no
  credential: `remotes.toml` holds a token's variable name (`token_env`), never its value, and a URL
  with a password in it is refused. The token reaches git only through the child's environment, as
  an HTTP header scoped to the remote's URL, never as an argument or in a file, and is removed, with
  its base64 forms, from whatever git prints before it enters an error. The child runs
  without a shell, its arguments after `--end-of-options`, with `GIT_TERMINAL_PROMPT=0`,
  `GIT_ALLOW_PROTOCOL` set to the remote URL's scheme alone (`file://`, `ext::` and local paths are
  refused as remotes), an empty hooks directory, and the variables that point git at another
  repository or trace its traffic removed; the user's credential helpers, ssh agent, `GIT_SSH_COMMAND`
  and proxy variables pass through, as they do for the user's own `git fetch`. A token or
  credential that reaches a file under the store, an argument, an envelope or an error; a git
  child that runs a hook or a command a remote or a fetched repository chose; or a mirror write
  outside `git/mirror.git`, is a vulnerability. What the remote sends is imported as the git
  import above takes any repository. A `SwarmOps-` trailer line in a remote commit is trusted as
  proof that the remote holds a change (the known limitations' "A trailer line is taken as
  proof"): anyone who can push to the remote is trusted with its content.
- **The SwarmOps supply chain.** A dependency or a CI workflow that could change what we build
  or run. `cargo deny check` (advisories, licenses, bans, sources) runs over every lockfile the
  project builds: the workspace's and the fuzz crate's in every `just check`, and
  `bench/compare`'s, which `just check` does not build, wherever it is built
  (`just bench-compare` and the weekly scheduled run). The lockfiles are committed and builds use
  `--locked`, and the GitHub Actions workflows pin every action by commit, start from
  `permissions: {}`, keep no credentials after checkout and are checked with zizmor. A known
  advisory in a dependency that is already public can go to a normal issue.

## What is not a vulnerability under M1's model

- **One agent acting as another.** In M1 an identity (`--as`, `SWARMOPS_AGENT`) is a claim that
  is recorded, not verified: every envelope's `meta.auth` says `"trust"`. Tokens arrive later
  (`E_AUTH` is reserved for them).
- **Anything a local user with write access to the store can do directly**: delete, rewrite or
  corrupt its files, hold its lock, fill its disk. Filesystem permissions are the boundary.
  SwarmOps detecting such damage is in scope (`swarmops verify --deep` re-hashes every object
  and checks the derived values against them); preventing it is not.
- **Reading files the caller can read.** A change list's `{"file": "<path>"}` content is read
  on the client, with the caller's own permissions, as any program the caller runs could read
  it, and `land --resolve` reads its `{"file"}` content on the client the same way; the stdio MCP
  server runs as its caller and reads it the same way. A server that takes requests from another
  host, M11's `swarmops serve --stdio` over ssh first, must refuse such content, and will
  (operations spec §7).
- **Durability failures.** Data lost after a crash, a power loss or a disk error is a serious
  bug, not a vulnerability: report it as an issue. [docs/architecture.md](docs/architecture.md)
  describes the durability model and its tests.
- **Builds with the `test-hooks` feature**, which exist for the test suite only.

When SwarmOps gains a server that takes requests from another host (M11's `serve` first), the
trust model changes, and this policy with it.
