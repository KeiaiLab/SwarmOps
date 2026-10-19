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

### Security

#### M1.6: the code-review follow-ups (2026-10-19)

- An object file whose zstd frame records a huge decompressed size, in agreement with its header
  (17 bytes of frame are enough), no longer makes a read panic (`E_INTERNAL`) or abort the process
  while it reserves that size: a read reserves at most 64 MiB before the frame's output arrives,
  and such an object is `E_STORE_CORRUPT`, which `verify --deep` lists. An object whose frame
  really decompresses to a huge size is still decoded whole
  ([docs/known-limitations.md](docs/known-limitations.md), "A zstd bomb object is decoded
  whole").

[Unreleased]: https://github.com/KeiaiLab/SwarmOps/commits/stable
