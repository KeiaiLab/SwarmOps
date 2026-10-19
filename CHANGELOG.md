# Changelog

Notable changes to SwarmOps. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and versions will follow
[the versioning policy](docs/policies/versioning-and-stability.md) (Semantic Versioning, read the
pre-1.0 way).

Nothing has been released or published yet: every crate is at 0.1.0 with `publish = false`, and
there are no tags. Until the first release ([RELEASE.md](RELEASE.md)) everything collects under
**Unreleased**, grouped by milestone of the design (umbrella spec §13).

## [Unreleased]

### Added

#### M0: bootstrap (2026-10-15)

- A Cargo workspace on Rust 1.99.0 (edition 2024, pinned by `rust-toolchain.toml`), with
  `unsafe` forbidden everywhere, clippy's `all` group denied and `pedantic` warned, and
  `just check` as the definition of done (fmt, clippy `-D warnings`, nextest, rustdoc
  `-D warnings`, cargo-deny); `scripts/check.sh` runs the same steps without `just`.
- `swarmops-core`: object ids that hash a domain-separated header with BLAKE3-256
  (`swarmops:<kind>:v1`), validated UTF-8 repository paths that are never normalized, and the
  canonical byte codec for trees and changesets, which is the hash contract. Four golden hash
  vectors pin it.
- Property tests and cargo-fuzz targets for the codec (`codec_roundtrip`, `tree_decode`).
- The fsync-latency experiment ([docs/perf.md](docs/perf.md)): durability defaults to full
  fsync, and no `--durability fast` flag is introduced.

#### M1: the single-agent core (2026-10-16)

- `swarmops-store`: self-verifying loose object files, zstd-compressed when that makes them
  smaller and checked against their id on every read, written temp file → `sync_all` →
  no-clobber link → directory fsync, in parallel batches.
- The native metadata store, in pure Rust with no external database: an append-only,
  CRC-framed journal (`journal.log`, `journal.off`, `journal.ckpt`) that is the source of truth;
  a columnar changeset index with sorted views and tail compaction; one file per ref. Writers
  are serialized by an `flock` that waits in the kernel's queue and gives up after 10 s
  (`E_BUSY`); readers take no lock. After a crash, the next writer cuts a torn tail and replays
  the journal.
- `swarmops-repo`: commits from change lists, each object durable before any journal frame
  names it; reads of files and directories at any revision without a checkout; revisions
  (`main`, `release/<name>`, ids and their prefixes of 12 or more digits, `~N`); first-parent
  history; ref moves by compare-and-swap; and `verify`, whose `--deep` re-hashes every object.
- `swarmops-api`: the operation contract, as serde and JSON Schema types: the request and
  response of every operation, the JSON envelope, 24 stable `E_*` error codes with fixed exit
  codes and retry advice, `Identity` and the `StoreOps` trait.
- `swarmops-ops`: `LocalOps`, the `StoreOps` implementation over a local store, which resolves
  identities and change-list contents, and reports each failure with its code, machine-readable
  details and a hint, often the next command to run.
- `swarmops-cli`: the `swarmops` binary, with `init`, `info`, `whoami`, `schema`, `commit`,
  `cat`, `ls`, `show`, `log`, `ref list`, `ref set`, `ref delete` and `verify`. Every run prints
  exactly one JSON envelope; default pages are sized for the spec's 64 KiB response target, with
  opaque cursors for the rest.
- Crash safety tested through the real binary: a sweep aborts `init` and `commit` at every one
  of their filesystem calls, as `kill -9` would, and the store must reopen and verify clean.
  Multi-process tests (8 processes committing at once, 16 racing to move one ref) end with
  `verify --deep`. Fuzz targets cover journal frames and journal recovery.
- The `test-hooks` cargo feature: the crash tests' hooks (`SWARMOPS_TEST_CRASH_AT`,
  `SWARMOPS_TEST_FS_CALLS`, `SWARMOPS_TEST_NOW`) work only in builds that turn it on, and
  `just check` proves that a release build ignores them.
- Measurements in [docs/perf.md](docs/perf.md): 32 processes × 1,000 commits through the binary
  without one `E_BUSY` (72.7 commits/s on the reference host), and a durable, uncontended commit
  in 21 ms.
- The MIT License ([LICENSE](LICENSE)), for every crate.

#### M1.5: hardening and opening (2026-10-17)

- CI on GitHub Actions: `ci.yml` runs `just check` (the same gate as locally, `--locked`
  everywhere) and zizmor behind one `tests-pass` check; `scheduled.yml` runs the slow suite and
  a fuzz smoke weekly; Dependabot keeps the root and fuzz lockfiles valid; the nextest `ci`
  and `ci-slow` profiles kill hung tests.
- The project policies and the documentation set: [CONTRIBUTING.md](CONTRIBUTING.md),
  [SECURITY.md](SECURITY.md), [RELEASE.md](RELEASE.md), `AGENTS.md`, `docs/architecture.md`,
  `docs/known-limitations.md`, `docs/policies/`, the issue and pull request templates, and this
  changelog.
- `swarmops mcp`: a stdio MCP server over `StoreOps`, one tool per operation with the request
  type's JSON Schema as its `inputSchema`, and the CLI's envelope as the result
  (`structuredContent` plus a text block; `isError` on failure). MCP 2026-07-28 per request,
  and 2025-11-25 or 2025-06-18 after `initialize`. Each call resolves its revisions on its own
  (`meta.resolved` is no longer shared by a handle's requests), a mutation without an identity
  is refused, and `expected_old` of `ref_set` and `ref_delete` must be a full changeset id
  through MCP, and a `{"file": path}` content must be an absolute path that is not the server's
  own stdin or stdout.
- `server/discover` and the `-32022` error list every revision served (2026-07-28, 2025-11-25,
  2025-06-18), so a client that cannot speak 2026-07-28 can fall back to `initialize`.
- `ref set` to the target a ref already holds is a replay: `already_applied: true` and nothing
  written, in the CLI and through MCP, `--create` included; `old` and `new` are then the
  request's.
- The `--expect` help of `ref set` and `ref delete` states the README's rule: give the full
  changeset id you read, since a ref name or a relative revision such as `main~1` is read from
  the store when the command runs, and then the compare-and-swap protects nothing, and an id
  prefix stops resolving once another changeset shares it.
- `so`, the short form of `swarmops`: the same program, built and installed with it; its usage
  lines, help and clap's message for a missing subcommand (`'so ref' requires a subcommand`)
  name the binary as invoked, and everything else still says `swarmops`.
- I/O-error injection and write-ordering checks, behind `test-hooks`:
  `SWARMOPS_TEST_FAIL_AT=<k>[:once|persistent[:eio|enospc]]` fails the k-th filesystem call
  with an injected error (`FailAtFs`), and `SWARMOPS_TEST_ORDER_CHECK=1` checks every write
  against the store's write-ordering rules R1–R7 and panics at the first that breaks one
  (`OrderCheckFs`, on for every run of the binary in the CLI tests and in the store's and the
  repository's crash sweeps). A model of Linux's fsync failures (the bytes a failed sync did not
  cover stay lost, even after a later sync succeeds) tests the journal, and I/O-error sweeps fail
  every call of a checkpointing and of a compacting store transaction, of the recovery after a
  crash, and of the binary's `init` and `commit`, checking that each outcome is honest.
- The fsync-failure policy (storage spec §2 rule 3): a failed `journal.log` append or
  `fdatasync` is cut back off the journal under the lock, and nothing is committed; a failed
  `journal.off` listing after the commit point is cut back off `journal.off`, and the commit
  stands, with a warning; a tail check whose sync fails writes the unlisted frames afresh from
  the bytes it read before it lists them. Three documented double faults abort the process,
  saying why on stderr: a cut of a failed frame that fails too, a cut that fails while unlisted
  frames are written afresh, and an `init` that cannot take back its `FORMAT`. A long-lived
  process, such as `swarmops mcp`, ends with them.
- **Contract change:** `E_IO_TRANSIENT` now means that nothing was committed, with
  `details.committed: false` (it said `details.may_have_committed: true`), so running the same
  command again is safe; and a failed `journal.off` listing after the commit point is a success
  with a warning, as every later failure already was (it was `E_IO_TRANSIENT`). A journal append
  that fails before its frame is whole, which was `E_IO`, is now `E_IO_TRANSIENT` too, as is a
  write whose tail check can neither sync nor write afresh the frames `journal.off` does not
  list yet. In `swarmops-store`, `StoreError::Undecided` and `CrashAtFs::from_env` are removed.
  `StoreError::NotCommitted` (a journal append that failed and was cut back off the journal;
  retryable) is what `E_IO_TRANSIENT` now reports, and `HookedFs::from_env` replaces
  `CrashAtFs::from_env`.
- The schema describes `E_IO_TRANSIENT` by the journal append that failed (other failures before
  the commit point stay `E_IO`): the store cut what the append wrote back off the journal, so
  nothing was committed, and running the same operation again is safe.
- **Contract change (text only):** `retryable` is now defined as "can succeed when it is run
  again: as it is after a transient failure, or, after a lost compare-and-swap, with what the
  error names re-read"; it said "retried unchanged", which does not describe a lost
  compare-and-swap. No flag, exit code or schema shape changes.
- `init` takes `FORMAT` back (and syncs its removal) when the sync of the store's root after it
  fails, so that no store looks complete whose completion mark may not be durable; when it made
  the store but reading it back fails, the error says so with `details.created: true`.
- Power-loss crash images (`crates/swarmops-cli/tests/power_loss.rs`): every cut of a recorded
  run, with seeded choices of what a power loss keeps (unsynced data dropped, cut at 512 B or
  4 KiB, zero-filled, or with its first sector lost; directory entries that no fsync covered
  reverted), opened with the binary, must recover a prefix of the run's transactions that holds
  every committed one.
- Test infrastructure: the support the tests share is one crate, `swarmops-testkit` (a
  dev-dependency; no test includes another crate's file by its path any more); the store's
  `CrashAtFs` compiles only with the `test-hooks` feature, like `FailAtFs` and `OrderCheckFs`,
  so no build without it can name a fault double; and where a crate has several integration-test
  files, they are the modules of one test binary, `tests/all.rs`, their tests named
  `<file>::<test>` (the CLI's release proof, `test_hooks`, stays a binary of its own; the CLI's
  snapshots are now `all__cli__*.snap` and `all__mcp__*.snap`, their contents unchanged).
- Recovery after a power loss: an unlisted journal frame whose header's length reads 0 is a torn
  tail and is cut (it was `E_STORE_CORRUPT`); readers drop zeroed entries at the end of
  `journal.off`, as recovery did; past the checkpoint and the sorted views, the index counts only
  the records the journal vouches for, so zero-filled index records are replayed rather than
  counted (they gave wrong counts and seqs, or `E_STORE_CORRUPT`); and a snapshot that opens
  while a writer cuts the index counts every record the checkpoint passed or the sorted views
  cover.
- Where the journal cannot vouch for the index's records past the checkpoint (no journal, a
  damaged one, or a `journal.ckpt` that does not hold a u64le: damage verify reports), the
  index counts them by their lengths and now checks their `par.col` entries against the layout
  (each record's parents start where the previous record's end): a zeroed entry, which a power
  loss leaves, is `IndexCorrupt` (`E_STORE_CORRUPT`) and nothing is cut. Before, the next write
  cut `parents.log` at the zeroed entry and replayed over the parents of checkpointed records.
  `Index::open` returns any other error reading `journal.ckpt` instead of counting by length.
- `Journal::frame_count` drops the zeroed entries at the end of `journal.off` that readers and
  recovery drop; a reader's frame count after such a power loss was one too high until the next
  writer ran (the frames it read were right).
- A reader's snapshot compares the frames it reads again whole instead of counting them: a frame
  that a failed sync or rewrite cut back (storage spec §2 rule 3 (a)/(c)) and the next writer
  replaced at its seq no longer lays its records over the replacement's ref files (a ref at a
  target that never committed, at a version that never existed).
- `verify` reports a `journal.ckpt` that is not a frame boundary (every read and write fails on
  one); before, it reported nothing while the index counted by length.
- Store files that are not regular files (a FIFO, a device, a socket, or a symlink to one) are
  refused with an error instead of opened; a FIFO at `meta/journal.log` used to hang every
  command.
- A store path that leads under `/mnt/` is refused on open too (`E_UNSUPPORTED_FS`), not only
  by `init`: a store copied, moved or symlinked onto a Windows drive under WSL is not opened.
- `bench/compare/` (`just bench-compare`, `--quick` for a smoke run): the native metadata
  store, through its public API, against SQLite, redb, fjall and sled on SwarmOps' own
  workloads, in a cargo workspace of its own that `just check` never builds; SwarmOps depends on
  none of them. Its numbers, in the M1.5 section of [docs/perf.md](docs/perf.md), are the
  quiet-window run's at the plan's sizes (2026-10-17), no longer provisional.

#### M1.6: the code-review follow-ups (2026-10-19)

- The store-level 32-process contention measurement (`crates/swarmops-store/tests/contention.rs`)
  can add other writers' object syncs: with `SWARMOPS_TEST_CONTENTION_OBJECT_SYNCS=<n>` each
  writer puts and flushes n new 4 KiB blobs of its own before each transaction, outside the
  lock, as a CLI commit writes its objects, and the table gives n and each transaction's time
  in the lock once `write()` returned (p50, p99 and max). A test in the default gate checks that
  the objects are new each time and durable before the lock is taken, and that none of their
  calls is made under it.
- A criterion bench of a compaction on its own (`crates/swarmops-store/benches/compaction.rs`,
  which `just bench` and `just perf` run): `Index::compact()` with every sync on, on an index of
  100k and of 1M records whose unsorted tail has just passed the threshold (1,025 records), the
  sorted views restored before each iteration, untimed. It prints the bytes each compaction
  writes (both views whole, 80 per record); `SWARMOPS_BENCH_COMPACTION_RECORDS` sets other sizes
  for a quick check.
- `bench/compare` times each seeded store's first open after its load (`first_open_ms`, a W4
  section per size), once, before any workload opens it; and `fjall-unflushed`'s W4 opens are
  each of a fresh copy of its store as the load left it, taken at seeding, so that its row is
  that store's, not the one its first open settled. [docs/perf.md](docs/perf.md) keeps the
  2026-10-17 run's cells until the next quiet-window run measures the ones this changes.
- `verify` reports each problem as a finding too: `VerifyReport.findings`, at the same index as
  its line in `problems`, gives the problem's `kind` (`missing_object`, `index_mismatch`, …) and
  what it names (`object`, `changeset`, `parent`, `seq`, `offset`, `ref`, `cause`), so that a
  program acts on the damage without parsing the lines. A failed `verify`'s `E_STORE_CORRUPT`
  carries it in `details` with the rest of the report, and the hint after a deep one names
  `details.findings`. `swarmops schema` lists `VerifyFinding`.
- `swarmops mcp`: `read_file`'s entry in `tools/list` carries
  `_meta: {"anthropic/maxResultSizeChars": 200000}`, which Claude Code reads to keep a
  `read_file` result of up to 200,000 characters in the conversation instead of saving one over
  50,000 to a file. No other tool carries `_meta`.
- `swarmops_store::FsMethod` names the 14 methods of the `FsOps` seam (`FsMethod::ALL`,
  `as_str`, `is_mutating`, `flushes`): the one list the fault doubles and the test kit's
  watchers classify calls with. It compares equal to the method's name.
- Test infrastructure: where a crate's integration tests are the modules of `tests/all.rs` (core,
  api, store, repo and the CLI), `every_test_file_is_a_module_of_all_rs` fails on a file of
  `tests/` that no `mod` line names, which `autotests = false` would leave unbuilt.
- Test infrastructure: the power-loss run places a frame whose length field straddles a 512 B
  sector boundary, which a lost first sector leaves a wrong length that is not 0, and its slow
  variant ends with a transaction of more than 1,024 changesets, so that its images hold the
  compaction a writer makes past that many records (an index sync, the compaction, the
  `Compacted` frame, the checkpoint), which the run's own compaction did not.
- `meta.recovered` in the envelope of `commit`, `ref set` and `ref delete` (the CLI's and the MCP
  server's alike): what the command's write repaired before its own work, of what earlier
  writers left when they crashed or failed after their commit point (a torn journal tail cut,
  frames listed or written afresh, `journal.off` bytes dropped, index records cut, a compaction
  finished, changesets and ref files replayed), in offsets and counts, on success and on
  `E_CAS_MISMATCH`; omitted when there was nothing to repair. It is no warning: `warnings` still
  names only the steps after the command's own commit point that failed. `swarmops schema`
  gains `RecoveredOut` and `LogCutOut`. In the library, `MetaWrite::recovered()`,
  `Journal::repair_tail()` (a `TailRepair`) and `recovered` on every `CommitOutcome` and
  `CasOutcome` report the same.
- `commit --advance REF` (`CommitReq.advance` and `CommitResp.advanced`; in Rust,
  `CommitRequest::advance` and `CommitOutcome::advanced`) publishes a changeset: the same
  transaction moves REF to it, so one journal frame holds both and they are recorded together or
  not at all, and only as a one-step fast-forward from the commit's parent (for a root commit,
  only if REF does not exist yet). When REF names anything else, the commit records nothing and
  fails with `E_CAS_MISMATCH` (exit 4, `details {name, actual, committed: false}`); the same
  request made again is a replay (`advanced.already_applied`). Without `--parent` it builds on
  REF's head. A publish takes one lock pass and four syncs, where a commit and then `ref set`
  take two and six. `swarmops mcp` refuses `advance` without a full 64-hex `parent`
  (`E_INVALID_ARG`), `swarmops schema` lists `Advanced`, and `swarmops_store::check_ref_name`
  checks a ref name as every ref operation does.
- The CLI-level 32-process contention measurement (`crates/swarmops-cli/tests/contention.rs`)
  can publish: with `SWARMOPS_TEST_CLI_CONTENTION_ADVANCE=1` each child publishes every commit
  on a ref of its own in one transaction (`commit --advance`), with `=0` in two (`commit`, then
  `ref set`), and the summary gives publishes/s and the journal frames, so the lock passes, each
  publish took. [docs/perf.md](docs/perf.md) has the section the quiet-window runs fill.

### Changed

#### M1.6: the code-review follow-ups (2026-10-19)

- `missing_docs` joins the workspace lints (M0's set was `unsafe` forbidden, clippy's `all`
  denied and `pedantic` warned): every exported item needs a `///` comment, and `just check`
  fails on one that lacks it. The lint is a warning, as `pedantic` is, so the gate's
  `-D warnings` in clippy and in rustdoc makes it an error while a local build of a draft
  still runs. A test fails if the root `Cargo.toml` drops the lint or a crate drops
  `[lints] workspace = true`.
- The write-ordering rules that `SWARMOPS_TEST_ORDER_CHECK=1` checks are W1–W7, stated in
  storage spec §2 rule 4, and a violation panics with `order check: W<n> violated by …`. They
  were R1–R7, defined only in `crates/swarmops-store/src/order_check.rs`, under the names the
  spec's review invariants (§6) also use.
- A raw object (one stored uncompressed: under 128 bytes, or content zstd cannot shrink, such
  as media or archives) is read back in the buffer its file was read into, the 16-byte header
  drained off the front, instead of being copied out of it: reading a large one peaks at about
  its size, not twice that. It is still one read of the file.
- A read gives up with the retryable `E_BUSY` when a commit lands during every one of its
  snapshot's 32 rounds (the store's `SnapshotContended`), where it used to read the refs and the
  frames again for as long as commits kept landing. `E_BUSY`'s description, which the schema
  carries, says so: another writer held the lock for the whole wait, or commits landed during
  every attempt to take a snapshot; either way nothing was committed (a commit's flushed objects
  may remain, for GC). It used to say only the first.
- The store's limit refusals (more than 255 parents, an actor over 255 bytes, more than 65,535
  records in one transaction) and malformed id prefixes are `E_INVALID_ARG`, with
  `details.store_limit`, `max` and `got`, or `details.prefix`; they were `E_IO`. A call the
  store's API does not take, which only a bug makes, is `E_INTERNAL`. No CLI, `StoreOps` or MCP
  request reaches them in M1 (storage spec §3, operations spec §0). In the Rust API,
  `StoreError` gains `Limit` (with `LimitKind`), `BadPrefix` and `BadCall`, its `Io` keeps the
  failures the filesystem reported, and `Record::encode` returns a `Result` instead of
  panicking on a record past the layout's lengths.
- `StoreError::JournalCorrupt`'s `offset` is always a position in `journal.log`.
  `Record::decode`, which gave it the position in its input where decoding stopped, returns a
  `BadRecord { at }` with that position instead (Rust API only; the readers report a frame
  that does not decode at its own offset, as before).
- **Schema only, the wire unchanged:** `Envelope`'s schema says with a `oneOf` that exactly one
  of `result` (when `ok`) and `error` (when not) is present, and its `error` is no longer
  nullable; `RefSetReq`'s says with a `oneOf` that exactly one of `expected_old` and `create`
  is given. Through `swarmops mcp`, every tool's `inputSchema` stays a plain object, with no
  `oneOf`, `anyOf` or `allOf` at its top, which the Claude API refuses for the whole session,
  and the `expected_old` of `ref_set` and `ref_delete` there is a string of the full-id
  pattern `^[0-9a-fA-F]{64}$` that the server enforces, in place of the revision text that
  offered a ref name.
- The `expected_old` descriptions in `swarmops schema`, the `ref_set` and `ref_delete` tool
  descriptions and the MCP server's instructions say what an id prefix does, apart from a ref
  name and a relative revision: it pins the id read only until another changeset shares it,
  then it is `E_AMBIGUOUS_ID`. The instructions also say that a read may show a commit that a
  failed sync then takes back, and that a mutation naming it fails under the lock instead of
  building on it; they drop the revision grammar, which each tool's schema and a bad
  revision's hint give, and the parenthesis on Claude Code's result limits (1,990 of Claude
  Code's 2,048 characters; they were 2,043).
- Test infrastructure: one `NoSyncFs` in `swarmops-testkit`, the real filesystem without its
  flushes that fails where it fails (a sync of a missing path is `NotFound`), replaces the test
  kit's `FastFs` and the copies in the `read_snapshot` bench and the `journal_recovery` fuzz
  target, which had drifted apart (two of the three answered `Ok` to a sync of a missing path).
  `Watched(watcher, fs)` runs its watcher over any filesystem, and a watcher sees a call's
  method as an `FsMethod`. The test kit's CLI and repository helpers sit behind its default
  features `cli` and `repo`, so the fuzz crate takes it with `default-features = false`.
- The order checker's W1 also refuses a cut below what the journal vouches for: of
  `journal.log` into the frames `journal.off` lists, or of `journal.off` below an entry the
  process synced or found there (the zeroed entries past frame 0 at its end aside, which list no
  frame). Before, `truncate` was only followed, and a wrong cut showed only when a sweep's oracle
  found the frames lost.
- Test infrastructure: the crash sweeps of the journal, the index and the repository recover
  under the order checker too, as the sweeps through the binary did (they recovered on the real
  filesystem, unchecked), and so does the fsync-failure model's sweep of the journal, every run.
- `init` refuses a repository name of more than 255 bytes with `E_INVALID_ARG` (`details`
  `{"store_limit": "repo_name_bytes", "max": 255, "got": <bytes>}`) before it creates
  anything, and `open` refuses a `REPO` longer than an id and a 255-byte name
  (`E_NOT_A_REPO`): a store created with a longer name must be created again.
- `swarmops mcp`: the instructions and the `commit` tool's description say what `advance` does
  and to check `result.advanced`. To make room, the instructions' sentence on result sizes drops
  that each result holds its envelope twice, which the envelope's sentence says, and its note on
  binary files, which `read_file`'s description gives: 2,034 of Claude Code's 2,048 characters
  (they were 1,990).
- `E_IO`, `E_IO_TRANSIENT` and the `E_STORE_CORRUPT` of a metadata file that does not parse
  name the file operation that failed and the store file it was on: `details.fs_op` (one of 22
  names, such as `"open"` or `"append_durable"`) and `details.store_file` (relative to the
  store's root, such as `meta/journal.off`; absent for a path outside it). Their messages say
  the same before the operating system's text, no error carries the store's host path, and the
  schema's descriptions of the three codes say so. A missing index file's `E_STORE_CORRUPT`
  names it (`meta/idx/ids.col is missing`). In the Rust API, `StoreError::Io` and
  `NotCommitted` carry an `IoFailure` (`op`, `file`, `source`, `also`) in place of an
  `io::Error`, the new `StoreFs` locates the failure of every filesystem call the stores make,
  and neither `StoreError` nor `RepoError` converts from a bare `io::Error` any more.

### Fixed

#### M1.6: the code-review follow-ups (2026-10-19)

- A reader that measured `journal.off` before a writer cut an entry off it (a listing whose sync
  failed, or the zeroed entries a crash left at its end) reads it again instead of reporting the
  journal corrupt: snapshots, paging, `reflog`, `journal_after` and `verify` no longer give a
  transient `E_STORE_CORRUPT` or report a `JournalCorrupt` problem that a second run does not.
- A snapshot or `verify` whose index counts its records by their lengths (the journal cannot
  vouch for them) no longer fails with an I/O error when a writer cuts the index as it opens:
  it counts the records the cut keeps, and fails only if the index is cut again meanwhile.
- A sorted-view entry that points at a record its view does not cover is `E_STORE_CORRUPT`
  (`IndexCorrupt`) on every lookup that reads it. Before, a lookup by id, prefix or delta hash
  answered with another changeset's record where the entry pointed into the unsorted tail, and
  failed with `E_IO` past the index's end.
- No error from the store carries the store's absolute path any more. A malformed
  `journal.ckpt` or ref file, and a ref file over its size cap, are named by their place in the
  store (`meta/journal.ckpt`, `meta/refs/<name>`), and a store file that is not a regular file
  by its name. A temp file that the store cannot create or write (an object write, or the
  replacement of a ref file or of `journal.ckpt`) fails with the operating system's own error,
  errno included, where tempfile's wrapper named the temp file's absolute path and dropped the
  errno.
- `verify` reports an index record that no journal frame holds, past the journal's last
  changeset, as an `IndexMismatch` (cause `Journal`) while changeset frames follow the
  checkpoint too. The index does not count such a record there, so verify reported nothing, and
  the next write cut it without a word.
- `swarmops mcp` refuses a `commit` whose `{"file": path}` content is not a regular file (a
  FIFO, a socket, a device or a directory, or a symlink to one) with `E_INVALID_ARG` before it
  reads anything: a FIFO no longer holds up the session until something writes to it, and a
  device such as `/dev/zero` is no longer read up to the 512 MiB limit. The CLI still reads such
  files, a pipe included.
- A `ref set` whose new target is the one it expects (`--expect` and the revision name the same
  changeset) is decided under the store's metadata lock, after the tail check, as every other
  `ref set` is: it answers `already_applied` only from the journal as the next writer finds it, no
  longer from a snapshot that can show another writer's frame before that frame's sync, which a
  failed sync or a power loss could then take back. It can now wait for the lock (`E_BUSY` after
  10 s) and repair what a crashed writer left; it still writes no journal frame.
- When the tail check's rewrite of the frames `journal.off` does not list yet fails, its error
  names those frames by seq; a tail check that rewrote them no longer cuts and syncs
  `journal.log` again to the length the rewrite left; and a rewrite that does not land where the
  frames were (another writer's bytes in between) is cut back and refused as damage
  (`JournalCorrupt` at the frames' offset, `E_STORE_CORRUPT`) before anything is listed, where it
  used to list entries that pointed into other bytes.

### Security

#### M1.6: the code-review follow-ups (2026-10-19)

- An object file whose zstd frame records a huge decompressed size, in agreement with its header
  (17 bytes of frame are enough), no longer makes a read panic (`E_INTERNAL`) or abort the process
  while it reserves that size: a read reserves at most 64 MiB before the frame's output arrives,
  and such an object is `E_STORE_CORRUPT`, which `verify --deep` lists. An object whose frame
  really decompresses to a huge size is still decoded whole
  ([docs/known-limitations.md](docs/known-limitations.md), "A zstd bomb object is decoded
  whole").
- `open` measures `REPO` before it reads it: a crafted `REPO` of huge length is refused by its
  length (`E_NOT_A_REPO`), where it used to be read whole, at the cost of its length in memory
  (M1.5 final review, security-2).

[Unreleased]: https://github.com/KeiaiLab/SwarmOps/commits/stable
