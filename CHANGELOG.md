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

[Unreleased]: https://github.com/KeiaiLab/SwarmOps/commits/stable
