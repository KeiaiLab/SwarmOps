# Versioning and stability

SwarmOps 0.1.0 was the first release, tagged `v0.1.0`, and each release is tagged `v<version>`.
Every crate of the workspace is at the workspace version (`[workspace.package]` in `Cargo.toml`),
and none is published (`publish = false`). This page says what users and agents may rely on, what
they may not, and how a change to a public surface is made.
The release steps are in [RELEASE.md](../../RELEASE.md).

## Public surfaces

What SwarmOps promises lives in bytes and in JSON, not in Rust APIs. From the strongest promise
to the weakest:

1. **The hash contract.** The canonical byte layouts of blobs, trees and changesets, and the
   hash recipe `id = blake3("swarmops:<kind>:v1\0" ++ u64le(len) ++ payload)` (storage-core
   spec §1). Every object id and every changeset id depends on them, so they never change, not
   even before 1.0. The golden vectors in `crates/swarmops-core/tests/golden/` pin the bytes and
   ids of representative objects, and `just check` fails if either moves.
2. **The store format.** The on-disk layout of a store, named by its `FORMAT` file
   (`swarmops-store 1` or, since M6, `swarmops-store 2`): the object file header, the journal's
   frames and records, the index files and the ref files (storage-core spec §2, §3), and the
   entries of the external id index, which carry a version of their own (`SXID` version 1,
   storage-core spec §7). A binary opens only a store whose `FORMAT` it knows; any other, the
   CLI refuses as `E_NOT_A_REPO`.
   A release that writes a new `FORMAT` still opens, reads and writes the older ones it knows.
3. **The command-line contract.** One JSON envelope per run on stdout,
   `{ok, op, api, result | error{code, message, retryable, exit, details, hint}, meta}` (but
   for `--help` and `--version`, which print plain text, `mcp` and `serve`, which speak MCP and
   git's protocol there, `auth git-credential`, which answers git as a credential helper, and
   `derive --follow`, which prints one envelope a line for each run that derived something, and
   a line that names no command at a terminal, which prints its help screen with the usage
   error's exit code 2 unless `--json` or `SWARMOPS_FORMAT=json` asks for the envelope), with
   `api` 1; the `E_*` error codes, each with its fixed exit code (1 to 6, 3 for a conflict; 0
   is success) and its `retryable` flag; the commands, their flags and the grammar
   of their arguments, such as revisions and change lists (operations spec §0, §1); and the
   environment variables `SWARMOPS_REPO` (operations spec §1, umbrella §4), `SWARMOPS_FORMAT`
   (operations spec §0), `SWARMOPS_AGENT` (operations spec §5) and `SWARMOPS_RUN_ID`, which no
   spec names yet (the README, `docs/architecture.md` and the crate documentation describe it),
   and `SWARMOPS_AUTO_UPDATE` with its two values, `on` and `off`
   ([What automatic update crosses](#what-automatic-update-crosses)).
   The line `--version` prints is part of it too: `swarmops <version>` on one line, followed by
   `+<commit>` as semver build metadata when the build set `SWARMOPS_BUILD_COMMIT`, a variable
   read at compile time, not at run time
   ([Install from a clone](../operate/install-and-upgrade.md#install-from-a-clone)). The
   variable's name and the values it takes are part of it as well: 7 to 40 lowercase hex digits,
   and any other value fails the build. The release workflow's smoke check, the packages' smoke
   check (`scripts/packages.py smoke`), the installer (`scripts/install.sh`) and `update`, which
   asks the installed and the downloaded binaries for it, read that line. A second variable is
   read at compile time: `SWARMOPS_BUILD_CHANNEL`, whose one value, `release`, marks a build as
   a release's. Only the project's release workflow sets it; any other value fails the build,
   and so does the variable together with `SWARMOPS_BUILD_COMMIT`. It changes nothing
   `--version` prints. `update` replaces only a build that has it, so a build from a clone is
   never replaced by a release.
4. **The API schema.** The JSON Schema of every request and response type that
   `swarmops schema` prints (`schema_for_all` in `swarmops-api`), and the `StoreOps` operations
   it describes.
5. **The MCP tools.** The tools `swarmops mcp` lists, their names, their `inputSchema` (each
   the request type's schema, kept a plain object, with the `expected_old` of `ref_set` and
   `ref_delete` narrowed to a full changeset id) and their results (the same envelope the CLI
   prints).
6. **The git export.** The git commit each changeset exports to (storage-core spec §7): an
   imported changeset's own commit while SwarmOps has not changed its lineage, and for any other
   changeset the commit its trees, its signatures and its message render to, a native
   changeset's message followed by SwarmOps' trailers (`SwarmOps-Changeset`, and the land, review
   and squash trailers). That rendering is version 1 of the `git_oid` derivation: git's tree order
   and modes, the commit layout, the signatures verbatim and the trailer block. It changes only
   as a new version, never in place, and every branch exported before a new version meets it as
   divergence, to be exported again once with `--force`: a deployment pipeline sees a rewritten
   history.
7. **The files SwarmOps reads from a tree.** `.swarmops/workspaces.json` (its `prefixes`,
   `bootstrap` and `check`) and, since M30, `.swarmops/checks.toml`, version 1. A file a release
   reads keeps its meaning in every later release. Version 1 of the check file gains keys as
   checks gain features; a build that does not know a key refuses the file with `E_CHECK_FILE`
   (`details.reason` `unknown_key`) instead of reading it without the key, so a newer file never
   means less to an older build. A key whose meaning changes makes a new `version`. The
   `definition_digest` that `check list` gives is not part of this surface yet: its recipe is
   provisional, and a later release may replace it before it is fixed.

The contract surfaces (3 to 5) grow without breaking: a new field, a new optional argument, a new
command, a new error code or a new tool is not a breaking change. Clients must allow for that.
Requests and responses ignore fields they do not know (the operations spec forbids
`deny_unknown_fields`), and an unknown `E_*` code still carries its `exit` and `retryable`, so a
client can act on a code newer than its own table. A change is breaking when it removes or
renames a field, a command, a flag, a code, a tool or an environment variable; changes a field's
type or meaning; changes a code's exit code or `retryable` flag; changes the form of the
`--version` line, `swarmops <version>` and the optional `+<commit>` on one line; narrows the
values `SWARMOPS_BUILD_COMMIT` takes; or makes an optional input required.

One surface more is a contract between releases, not with a script: what a released build
expects when it updates itself, which
[What automatic update crosses](#what-automatic-update-crosses) lists.

## Internal surfaces

No compatibility is promised for:

- **The Rust crate APIs**, of every crate of the workspace, `swarmops-testkit` included.
  They are internal to the workspace and unpublished; `swarmops-api`'s types are a contract only
  through their JSON.
- **Performance.** The spec's targets and the numbers in [docs/perf.md](../perf.md) are goals
  and measurements, not guarantees. A regression is a bug to fix, not a breaking change.
- **Text meant for reading**: `error.message`, the wording of `hint`, `--human` output and
  anything on stderr, the line a command prints at a terminal after an automatic update among
  it (`update --status` gives the same as `pending`, which is contract). The `code`, `exit`,
  `retryable` and the keys of `details` are the contract; the words around them are not.
- **Test-only behaviour**: the `test-hooks` cargo feature and the `SWARMOPS_TEST_*` variables.
- **Internal tuning** that no layout records, such as how often the index is synced and
  checkpointed or when the sorted views are compacted.

## Before 1.0

- Versions follow Semantic Versioning, with the pre-1.0 reading that uv and ruff use: a minor
  bump (0.1 to 0.2) may break a public surface other than the hash contract, and a patch bump
  (0.1.0 to 0.1.1) breaks none. Every crate shares the workspace version.
- Every breaking change is called out under its version in [CHANGELOG.md](../../CHANGELOG.md),
  with what to do about it.
- A store format change may need a store to be created again. (It happened once during M1:
  once `init` began to create every index file, stores created before that were refused.) The
  changelog says when. `FORMAT 1` moves to `FORMAT 2` (M6) and either to `FORMAT 3` (M20) in
  place, with `swarmops upgrade`, which no older release can read afterwards.
- From 1.0 on, the intent is that a release opens every store an earlier 1.x release wrote, or
  migrates it, and that a test proves it before each release.

## What automatic update crosses

From the first release after 0.11.0 on, an install the installer made updates itself unless it
is told not to ([Install, How do I update?](../guides/install.md#how-do-i-update)). It
therefore crosses version bumps that nobody at the host read the changelog for, and before 1.0
a minor bump may break a public surface. What it crosses unasked is decided per release:

- **A release is installed unasked only where it holds the mark for the installed minor
  line**, the file `auto-update-from-<major>.<minor>` among the release's files. A release
  without that mark is announced and not installed.
- **A release is marked for each earlier minor line of its major version**, a release with
  breaking changes too: a flag that was renamed, a code that moved. The line an update prints
  links the release's notes, where each breaking change is under its own group.
- **A release is withheld from a line** when the way from that line to it crosses a release
  that changes what stores older builds can open (`init`, or a writer by default, starts to
  make a store an older release refuses), or one that needs a step by hand. Its changelog
  section says so in a group's heading, with the words `no automatic update`. Such a release is
  installed by `so update` alone, by someone who read its notes.
- **An update never changes a store.** It replaces the two binaries. It never runs `upgrade`,
  and a store stays in its `FORMAT` under the new binary until someone upgrades it.
- **Another major version is never installed unasked**, and no pre-release is installed at all
  but by the installer's `--version`.

Until a release is marked, or where it is withheld, a host stays on what it has: only the
latest release is looked at ([known limitations](../known-limitations.md#updating-the-program)).
[RELEASE.md](../../RELEASE.md#checklist) (steps 4 and 9) is how the maintainers decide and
upload the marks, and how they withdraw them.

What a released build relies on to update itself is contract from that release on, since the
build cannot be changed afterwards. A change to any of it is a breaking change, and the release
that makes it is withheld:

- the mark's name, `auto-update-from-<major>.<minor>`, and that the release's files are at
  `https://github.com/KeiaiLab/SwarmOps/releases/download/v<version>/`, with `releases/latest`
  redirecting to `releases/tag/v<version>`;
- the archive's name, `swarmops-<version>-<target>.tar.gz`, its directory of the same name
  with `swarmops` and `so` in it, and the archive's line in `SHA256SUMS`;
- the installer's options `--version` and `--dir`, which the installer built into a release
  is run with, and the form of the `--version` line above;
- the files beside the binaries: the receipt's name, `.swarmops-receipt`, and its two keys,
  `receipt = 1` and `auto_update = true` or `false`, which the installer writes and every later
  build reads (a build refuses a receipt whose `receipt` is another number, and keeps keys it
  does not know); and the names `.swarmops-update.lock`, `.swarmops-update.note` and
  `.swarmops-update.d`, which the installer's `--uninstall` removes. What the lock file and the
  note hold is the build's own and no contract;
- `SWARMOPS_AUTO_UPDATE` and its values, the `CI` variable's reading (set to anything but the
  empty string, `0` or `false`), and the values of `auto_update` that `update --status` gives.

## How a breaking change is made

- **A new object layout** gets a new kind/version header and its own golden vectors, beside the
  old ones. Objects already stored keep their bytes and their ids.
- **A new store layout** bumps `FORMAT`: an older binary then refuses to open the store rather
  than misread it, because there is no rule for skipping unknown records. `FORMAT 2` (M6) added a
  checksum to each `journal.off` entry, a frame header that carries the frame's sequence number
  and the previous frame's CRC, and ref files named by the hash of the ref's name, under new
  file names, with the record layouts unchanged. `FORMAT 3` (M20) keeps FORMAT 2's frames,
  offsets and ref files, adds a nonce record to every frame and the fact records, and moves the
  sorted views to runs under a manifest; within it, new record kinds come behind named features,
  which a binary that lacks one refuses (`E_STORE_TOO_NEW`) rather than misread. Whichever
  version next changes a layout takes the next number.
- **A new external id entry layout** gets the next `SXID` version, beside version 1: entries
  already written keep their bytes. A binary reads only the versions it knows and reports an
  entry of any other as damaged (`E_STORE_CORRUPT`), so an older binary fails on such an entry
  rather than misreads it.
- **A contract change** shows up in a test the gate runs: `all__cli__schema_snapshot.snap` holds
  the schema of every type, the MCP server's tool list and its discovery result have snapshots
  of their own, and each error code's exit code and `retryable` flag, which no schema holds,
  have a row in `error_codes_map_to_spec_strings_and_exits`
  (`crates/swarmops-api/tests/schema.rs`). The gate fails until the snapshot or the row is
  updated in the same commit. That commit says that it changes the contract, and the changelog
  records it. A change that breaks the envelope or a response's shape also increments the
  envelope's `api` number.
- **An error code** that is removed is never reused for another meaning.
