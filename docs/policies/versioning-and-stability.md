# Versioning and stability

SwarmOps 0.1.0 is the first release, tagged `v0.1.0`. Every crate of the workspace is at version
0.1.0 (the workspace version), and none is published (`publish = false`). This page says what
users and agents may rely on, what they may not, and how a change to a public surface is made.
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
   frames and records, the index files and the ref files (storage-core spec §2, §3). A binary
   opens only a store whose `FORMAT` it knows; any other, the CLI refuses as `E_NOT_A_REPO`.
   A release that writes a new `FORMAT` still opens, reads and writes the older ones it knows.
3. **The command-line contract.** One JSON envelope per run on stdout,
   `{ok, op, api, result | error{code, message, retryable, exit, details, hint}, meta}`, with
   `api` 1; the `E_*` error codes, each with its fixed exit code (1 to 6, 3 for a conflict; 0
   is success) and its `retryable` flag; the commands, their flags and the grammar
   of their arguments, such as revisions and change lists (operations spec §0, §1); and the
   environment variables `SWARMOPS_REPO` (operations spec §1, umbrella §4), `SWARMOPS_FORMAT`
   (operations spec §0), `SWARMOPS_AGENT` (operations spec §5) and `SWARMOPS_RUN_ID`, which no
   spec names yet (the README, `docs/architecture.md` and the crate documentation describe it).
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

The contract surfaces (3 to 5) grow without breaking: a new field, a new optional argument, a new
command, a new error code or a new tool is not a breaking change. Clients must allow for that.
Requests and responses ignore fields they do not know (the operations spec forbids
`deny_unknown_fields`), and an unknown `E_*` code still carries its `exit` and `retryable`, so a
client can act on a code newer than its own table. A change is breaking when it removes or
renames a field, a command, a flag, a code or a tool; changes a field's type or meaning; changes
a code's exit code or `retryable` flag; or makes an optional input required.

## Internal surfaces

No compatibility is promised for:

- **The Rust crate APIs**, all nine of them (the eight product crates and `swarmops-testkit`).
  They are internal to the workspace and unpublished; `swarmops-api`'s types are a contract only
  through their JSON.
- **Performance.** The spec's targets and the numbers in [docs/perf.md](../perf.md) are goals
  and measurements, not guarantees. A regression is a bug to fix, not a breaking change.
- **Text meant for reading**: `error.message`, the wording of `hint`, `--human` output and
  anything on stderr. The `code`, `exit`, `retryable` and the keys of `details` are the
  contract; the words around them are not.
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
  changelog says when. `FORMAT 1` moves to `FORMAT 2` in place, with `swarmops upgrade` (M6),
  which no older release can read afterwards.
- From 1.0 on, the intent is that a release opens every store an earlier 1.x release wrote, or
  migrates it, and that a test proves it before each release.

## How a breaking change is made

- **A new object layout** gets a new kind/version header and its own golden vectors, beside the
  old ones. Objects already stored keep their bytes and their ids.
- **A new store layout** bumps `FORMAT`: an older binary then refuses to open the store rather
  than misread it, because there is no rule for skipping unknown records. `FORMAT 2` (M6) added a
  checksum to each `journal.off` entry, a frame header that carries the frame's sequence number
  and the previous frame's CRC, and ref files named by the hash of the ref's name, under new
  file names, with the record layouts unchanged. The first new journal record kind (annotations,
  M7) makes the next one, `swarmops-store 3`; whichever version first changes the layout takes
  the next number.
- **A contract change** shows up in a test the gate runs: `all__cli__schema_snapshot.snap` holds
  the schema of every type, the MCP server's tool list and its discovery result have snapshots
  of their own, and each error code's exit code and `retryable` flag, which no schema holds,
  have a row in `error_codes_map_to_spec_strings_and_exits`
  (`crates/swarmops-api/tests/schema.rs`). The gate fails until the snapshot or the row is
  updated in the same commit. That commit says that it changes the contract, and the changelog
  records it. A change that breaks the envelope or a response's shape also increments the
  envelope's `api` number.
- **An error code** that is removed is never reused for another meaning.
