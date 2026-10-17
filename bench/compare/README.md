# bench/compare: the native metadata store against embedded databases

This directory exists **only to measure SwarmOps' native metadata store** (`swarmops_store::MetaStore`) against embedded databases on SwarmOps' own workloads. **SwarmOps itself depends on none of these databases**: they appear only here, in a cargo workspace of its own (like `fuzz/`), `publish = false`, outside the root workspace. `just check` neither builds nor lints it, and the root `Cargo.lock` does not list them. `docs/perf.md` ("M1.5 — native store vs embedded databases") records the results and the verdicts.

```sh
just bench-compare --quick     # a smoke run of everything at small sizes: under 2 minutes once built
just bench-compare             # the full run at the plan's sizes: 3 h 18 min measured (2026-10-17), with W2 at about 7 ms per fdatasync
just bench-compare --only w1,w3 --engines native,sqlite --runs 5
just bench-compare --w1-txns 2000 --w2-txns 100   # smaller W1 and W2, everything else full
```

The full run's time goes almost all to waiting for the disk: W1 makes 10,000 commits per engine and run, W2 32,000, and the native store's W1 commit makes four syncs (see below). `--w1-txns`, `--w2-txns` and `--sizes` scale a run down; the report's headings give the sizes it ran.

The recipe runs `cargo run --release --locked --manifest-path bench/compare/Cargo.toml -- …`. The first build compiles SQLite's C sources (a C compiler is needed) and takes a minute or two. The report (Markdown) goes to stdout, progress to stderr, and every raw value to a JSON file (`bench/compare/target/results/`, or `--out`). The run needs no network.

The harness has tests of its own, which `just check` does not run: after a change here, run `cargo test --manifest-path bench/compare/Cargo.toml --locked`, `cargo clippy --manifest-path bench/compare/Cargo.toml --locked --all-targets -- -D warnings` and `cargo fmt --manifest-path bench/compare/Cargo.toml --check`.

## Engines

| name | engine | durability of a commit |
|---|---|---|
| `native` | `swarmops-store` (path dependency, this tree), its defaults | `journal.log` frame and `journal.off` entry, each `fdatasync`'d; a moved ref's file replaced atomically (`fsync` of a temp file, rename, directory `fsync`); index columns synced every 32 frames |
| `sqlite` | `rusqlite` 0.40.2, bundled SQLite 3.53.2 | WAL, `synchronous=FULL` (the WAL is `fsync`'d at every commit), `BEGIN IMMEDIATE`, `busy_timeout` 10 s, prepared statements |
| `sqlite-normal` | the same, `synchronous=NORMAL` | **not power-loss durable**: syncs only at checkpoints. A reference line, never a contender |
| `redb` | `redb` 4.3.0 | `Durability::Immediate` (the default), 1-phase commit with checksums: one `fdatasync` |
| `fjall` | `fjall` 3.1.12 (`lsm-tree` 3.1.10) | single-writer transaction, `durability(Some(PersistMode::SyncAll))`, the strongest it offers: the journal `fsync`'d. A seeded store is settled by flushing every keyspace's memtable into table files (below) |
| `fjall-unflushed` | the same | the same commits; a seeded store is settled with a journal sync alone, its data left in the journals. Measured in W3–W5 only, for the record |
| `sled` | `sled` 0.34.7, the last stable release (2021-09-12); unmaintained since, its 1.0 rewrite still in alpha | `Db::flush()` after each transaction: `pwrite` + `sync_file_range`, which writes no metadata and does not flush the device cache — weaker than the others |

Versions are pinned exactly, each released at least 7 days before the harness was written, as are the transitive dependencies in `Cargo.lock`, except `zstd` 0.14.2 (6 days), which `swarmops-store` itself pins. Every engine runs with its defaults apart from durability; no engine gets a tuning the others do not.

**fjall's settled state.** fjall writes a memtable to a table file only once it holds 64 MiB, and deletes a journal only when every keyspace has flushed past it. The small keyspaces (`meta`, `refs`, `reflog`) never fill a memtable, so after a bulk load every journal stays until 512 MiB of them force a flush: left that way, W5 would count the journals beside the tables. So `fjall` settles as the others do, by moving its log into its main structure, as SQLite's checkpoint does: it syncs the journal, then flushes every keyspace's memtable (`rotate_memtable_and_wait`, public but doc-hidden; fjall's own tests use it), and fjall deletes the journals no keyspace needs. One cost stays, and it is fjall's own: the active journal is rotated only when it passes 64 MB at a flush, and an open replays it whole, flushed or not. So the flush shrinks a large store but not its open time (W4), and a small store keeps its whole journal beside the new tables. `fjall-unflushed` shows the store as the load left it.

Only `native` and SQLite allow several processes to open one store. redb (whose multi-process mode is behind an `experimental-multiprocess` feature), fjall and sled allow one process, so W2 runs them on threads; `native` and SQLite run on processes, and on threads too, to show what that asymmetry is worth.

## The transaction (every engine does the same work)

W1's transaction, which W2, W4 and W6 reuse:

1. checks that the changeset is new and looks up its parent (an error if it is missing), computing its generation as 1 + the parent's;
2. stores the changeset under its 32-byte id, numbered in insertion order, and indexes it by its 32-byte delta hash;
3. moves `main` to it by compare-and-swap and records the move (the native frame holds it; the others write a reflog row, as the M0 SQLite schema did);
4. commits durably.

SQLite's schema mirrors the native index: rows in seq order (the rowid), a unique index on the id and an index on the delta hash, `refs` and `reflog` tables. redb, fjall and sled keep `changesets` (id → seq, generation, delta hash, parents), `by_delta_hash` ((delta hash, seq) → id), `refs`, `reflog` and `meta` (the counters).

## Workloads

- **sync**: the sync calls per commit, by file, counted with ptrace by a small tracer in `src/trace.rs` (safe Rust through `nix`; `strace` is not installed on the measuring host). Two traced runs of different lengths; the difference per transaction, so batched work is averaged in and the open and close drop out. The pair runs once per run.
- **breakdown**: where a native commit's time goes: 1,024 W1 commits per run after the warm-up (so that one compaction falls among them), every filesystem call timed by a timing `FsOps` around `RealFs` (the store's public `MetaStore::create_with_fs`). A kind of call counts for the time its calls cover, overlaps once: the index sync's five `sync_data`s run at once, and the commit waits for them as for one. The journal appends (`append_durable`) and the ref file's replacement (`atomic_replace`) are done step by step, with `RealFs`'s calls in `RealFs`'s order, so that each sync is timed on its own: the `fdatasync` of `journal.log`, that of `journal.off`, the ref temp file's `fsync`, the `refs/` directory's `fsync`.
- **W1**: one writer, 10,000 transactions back to back after a warm-up: commits/s, p50/p99/max, and the closed store's bytes per transaction.
- **W2**: 32 writers × 1,000 transactions, `main` compared against its value inside each transaction: throughput, p50/p99/max (a `Busy` retry included), `Busy` and failures, sled's conflict reruns, and the spread of the writers' total times (fairness).
- **W3**: lookups on bulk-loaded stores of 100,000 and 1,000,000 changesets, warm: by id, by a 6-byte id prefix, by delta hash (the secondary index), a miss, all on one open snapshot; and by id in a fresh read transaction each.
- **W4**: a fresh process opening the 1M store and looking up one changeset; and recovery after a writer process is killed with SIGKILL mid-loop: the next process's open plus first write, with every acknowledged commit checked present. None of the open, first-lookup or first-write timers includes a close: SQLite's open hands its connection to the first session, because closing a WAL store's last connection checkpoints the WAL into the database and deletes it, and the lookup's session is closed after the timing (the opening process's time from spawn to exit, which the report also gives, spans its close).
- **W5**: the bulk-loaded stores' size on disk, settled and closed, and how much of it is the engine's log (the native journal, SQLite's WAL, fjall's journals). Each store is built once per run; the first build is the store W3, W4 and W6 read, the others are deleted once measured.
- **W6**: a reader's lookup latency (a fresh read transaction each) idle and while a writer commits W1's transaction continuously; reader and writer are two threads of one process, for every engine.

## Method

- Stores live in fresh temporary directories under `$SWARMOPS_BENCH_DIR`, or `~/.local/state/swarmops-bench` (the native filesystem, as the store's own benches use; a path under `/mnt` is refused). Data is seeded and deterministic: real BLAKE3 ids, in chains.
- Every measurement records the 1-minute load average at its start, its range during (sampled every 2 s) and at its end, and a raw `fdatasync` probe (the median of 40 appends of 120 B, each `sync_data`'d) at its start and end; the tables print them beside every number.
- Each measurement runs 3 times (`--runs`), the sync count and W5's builds included, the engines in a rotated order each run; the tables give the median and `[min–max]` over the runs, and a per-run table under each.
- Warm-up: W1 commits 200 untimed transactions first; W3 makes one untimed pass of every lookup kind; W6 warms the reader before timing it.
- Numbers taken on a busy host are provisional: the disk is shared, and every engine here waits on it.

## Limits

- One host, one filesystem (ext4 `data=ordered` on WSL2's virtual disk), warm page cache (dropping it needs root): no cold-cache figures.
- A killed process loses nothing the kernel holds, so W4 measures recovery from a process crash, not from a power loss.
- Latencies are per call with `Instant`; the tracer slows the traced process, so the sync run's timings are not used.
- The tracer is x86_64 Linux only; elsewhere the sync step reports itself unavailable. It needs to be allowed to trace its own children (Yama `ptrace_scope` 0 or 1).
- `--quick` runs every workload at 5,000 and 20,000 changesets, 50 W1 transactions (and 50 in the breakdown) and 32 × 4 in W2: a check that everything runs, not numbers to publish.
