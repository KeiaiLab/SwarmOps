# Performance ledger

Numbers measured on this host (WSL2, kernel 5.15, ext4 on the virtual disk `/dev/sdd`, 32 cores). Re-measure after any storage-layer change; `just bench` fails on >20 % regression once `bench/thresholds.toml` exists (M9).

## M0 fsync latency (2026-10-15)

Two instruments, same 4 KiB-file-per-iteration workload, each variant in its own fresh temporary directory under `~/.local/state/swarmops-bench`:

- `crates/swarmops-store/tests/fsync_percentiles.rs` (`cargo nextest run -p swarmops-store --run-ignored all -E 'test(fsync_percentiles)' --no-capture`): **1,000 independent writes per variant, every write timed on its own** — the table below is from this run; percentiles are nearest-rank over the 1,000 per-write durations.
- `crates/swarmops-store/benches/fsync_latency.rs` (`cargo bench -p swarmops-store`): criterion means for regression tracking only (criterion reports per-sample means over tens of iterations, which smooths tails; it cannot give per-write percentiles).

| variant | p50 | p90 | p99 | max |
|---|---|---|---|---|
| write only, no fsync | 23 µs | 74 µs | 154 µs | 379 µs |
| write + `sync_data` | 4.12 ms | 4.70 ms | 5.91 ms | 26.6 ms |
| write + `sync_all` | 4.09 ms | 4.54 ms | 5.41 ms | 7.36 ms |
| write + `sync_all` + rename + directory fsync | 7.63 ms | 8.32 ms | 9.25 ms | 16.8 ms |

Criterion means for the same variants: 49 µs, 4.07 ms, 4.01 ms, 8.81 ms (consistent with the medians above).

**Decision.** p99 of a durable single-file write is 5.4 ms (max 7.4 ms over 1,000 writes), under the 20 ms threshold the M0 plan set, so **no `--durability fast` flag is introduced now**; the default stays full fsync (objects `sync_all` + one directory fsync per commit + SQLite `synchronous=FULL`). The rename + directory-fsync path has a 16.8 ms max, so a commit's single directory fsync can occasionally approach the threshold; that is one fsync per commit, not per object.

**Consequence for M1.** A commit that writes N new objects pays ~4 ms per object if the fsyncs are serial (100 files ≈ 0.4 s), which cannot meet the "≤ 5 ms + 0.3 ms per changed file" commit target. `FsObjectStore::put_batch` must issue the per-file fsyncs in parallel (rayon) and fsync each touched fan-out directory once at `flush()`. M1 measures `put_batch` throughput at 1, 8 and 32 concurrent fsyncs (`benches/put_batch.rs`); if the virtual disk serializes flushes (throughput does not scale), the `--durability fast` option (skip per-object fsync, keep the SQLite WAL fsync, documented as "safe against process crash, not power loss") is reconsidered with that measurement attached here.

## M1 store level (2026-10-15)

The store-level half of the M1 Task 7 experiments; the CLI-level contention test follows with the CLI. Same host as M0: WSL2 (kernel 5.15.153.1), ext4 (`data=ordered`) on the virtual disk `/dev/sdd`, AMD Ryzen 9 7950X3D (16 cores, 32 threads), 106 GiB RAM; Rust 1.99.0, optimized builds (`cargo bench`; the contention test with `--release`). Every store lives in a fresh temporary directory under `~/.local/state/swarmops-bench`.

The host was not quiet. Three JetBrains remote-dev-server processes kept about 8.5 cores busy throughout, and other worktrees were compiling and testing. Before each run, the harness waited up to 10 minutes for the 1-minute load average to drop below 8. It did so three times, before `put_batch` runs 1, 2 and 4; every other run started at load 8.5–17. Each table gives the load at the start of each run, and the range sampled every 15 s during it. Threads blocked in fsync count towards the load average, so the I/O-bound runs raise it themselves. Every measurement ran twice. Where the two runs agree within 20 % the better one is shown; otherwise both are.

### `put_batch` fsync scaling

`crates/swarmops-store/benches/put_batch.rs` (`cargo bench -p swarmops-store --bench put_batch`). Every iteration puts 256 new 4 KiB objects into a fresh store whose 256 fan-out directories already exist, as they do in a created store. The objects are incompressible, so each file holds 4 KiB. The batch runs on a pool of 1, 8 or 32 threads (`FsObjectStore::set_parallelism`). Per object that is a temp file, `sync_all` and a no-clobber link. `flush` then fsyncs each fan-out directory the batch touched (about 162), one after another. Each figure is criterion's median of 10 samples; objects/s = 256 / median.

The "busy writer" variants keep a thread appending 64 bytes to a file of its own every 100 µs, throughout, as some process always does on a swarm host. The other variants see whatever the host did meanwhile. These are runs 3 and 4, which agree within 12 % everywhere:

- run 3 started at load 12.0 (10.9–23.1 during the run);
- run 4 started at load 7.8 (7.8–18.6).

| threads | `put_batch` + `flush` | objects/s | vs 1 | with the busy writer | objects/s | vs 1 | `put_batch` alone | objects/s | vs 1 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 1,011 ms | 253 | 1.0× | 1,591 ms | 161 | 1.0× | 990 ms | 258 | 1.0× |
| 8 | 234 ms | 1,094 | 4.3× | 790 ms | 324 | 2.0× | 228 ms | 1,125 | 4.4× |
| 32 | 69.8 ms | 3,666 | 14.5× | 632 ms | 405 | 2.5× | 67.0 ms | 3,823 | 14.8× |

`flush` alone, after a 256-object batch: 4.1 ms, and 571 ms with the busy writer.

Runs 1 and 2 came before the busy-writer variants existed, and the host put them in different regimes. They disagree, so both count:

| run | load at start (range) | 32 threads, `put_batch` + `flush` | 1 thread | `flush` alone, median |
|---|---|---|---|---|
| 1 | 7.7 (7.5–21.4) | 587 ms | 1,174 ms | 412 ms, its samples spread from 4 ms to 568 ms |
| 2 | 7.6 (7.6–29.7) | 77 ms | 1,003 ms | 31 ms |

The per-object fsyncs scale: 14.8× on 32 threads, because the virtual disk and ext4's journal commit concurrent fsyncs together. `flush` scales only when nothing else is writing to the filesystem. On this kernel, a directory fsync becomes a full journal commit of its own whenever any process has metadata pending on the filesystem. That happens even for a directory that has not changed.

A side check bears this out. 162 fsyncs of unchanged directories took 0.1 ms on a quiet filesystem. With one shell loop appending to a file, they took 567–616 ms over 5 rounds, every fsync a 3.7 ms commit. The same fsyncs issued from 32 threads took 44–47 ms.

So on a busy host a commit's `flush` costs about 3.5 ms per touched fan-out directory: 571 ms for 256 objects. That breaks the "≤ 5 ms + 0.3 ms per changed file" commit target, which is 82 ms for 256 files, although `put_batch` alone on 32 threads (67 ms) meets it.

### Index lookups

`crates/swarmops-store/benches/index_lookup.rs` (`cargo bench -p swarmops-store --bench index_lookup`). One index per size is built outside the timed loops: a chain of real (BLAKE3) changeset ids, each changeset with a delta hash of its own. Its lookups are timed twice. First the last 1,000 records sit in the unsorted tail and the hits ask for tail records: the binary search misses, then the tail is scanned. Then the index is compacted whole and the hits ask for any of its records. The files are in the page cache, as a hot store's are. Each lookup cycles through 1,024 targets in a scattered order. p50 is criterion's median per lookup. Run 1 started at load 12.3 (12.1–20.3 during the run), run 2 at load 10.3 (10.3–11.6).

| changesets | layout | `find` hit | `find` miss | `find_prefix`, 12 hex | `find_delta_hash` |
|---|---|---|---|---|---|
| 10,000 | compacted | 5.1 µs | 4.8 µs | 5.5 µs | 6.4 / 5.2 µs |
| 10,000 | 1,000-record tail | 6.1 µs | 6.9 µs | 8.5 µs | 6.9 µs |
| 100,000 | compacted | 6.8 µs | 6.3 µs | 7.4 µs | 6.9 µs |
| 100,000 | 1,000-record tail | 7.9 µs | 8.1 µs | 10.2 µs | 8.6 µs |
| 200,000 | compacted | 7.1 µs | 6.6 µs | 7.7 µs | 7.6 µs |
| 200,000 | 1,000-record tail | 8.2 µs | 8.5 µs | 10.9 µs | 8.9 µs |

The one cell with two values is where the runs differ by more than 20 % (run 1 / run 2). Every lookup takes under 11 µs, so the spec's read target (< 1 ms) holds by two orders of magnitude, at 200,000 changesets as at 10,000. A lookup is about log2 n `read_at`s of one 32-byte key (14 at 10,000, 18 at 200,000), so going from 10,000 to 200,000 changesets adds about 2 µs. A full tail adds 0.5–3.2 µs: one 32 KiB `read_at` and a linear scan. `find_prefix` pays the tail scan even when the sorted view holds the match. `find` miss is what every insert of a new changeset asks, under the lock.

### fsync fan-out (`Index::sync`)

`crates/swarmops-store/benches/fsync_fanout.rs` (`cargo bench -p swarmops-store --bench fsync_fanout`): five files in a fresh directory. Before every iteration (untimed), each gets 64 KiB of new data appended, as the index's appends grow its files. The iteration then makes them durable:

- serially, one `sync_data` per file, each file opened for it, as `Index::sync` does;
- from five threads at once (`std::thread::scope`, thread start included);
- for scale, one such file alone.

Criterion median. Run 1 started at load 11.5 (11.5–12.6), run 2 at load 17.1 (17.1–22.3).

| variant | median | runs 1 / 2 |
|---|---|---|
| serial, 5 × `sync_data` (as `Index::sync`) | 18.8 ms | 18.8 / 22.4 ms |
| parallel, 5 threads | 7.5 ms | 7.6 / 7.5 ms |
| one file | 3.8 ms | 3.8 / 4.0 ms |

Serial costs five fsyncs (5 × 3.8 ms). Parallel costs about two, because ext4 commits the five files' metadata in a shared journal commit: 2.5× faster (2.48× in run 1, 2.97× in run 2). `Index::sync` runs at every 32nd transaction and before a compaction, under the lock. Parallel would save about 11 ms there, about 0.35 ms per transaction on average.

### 32 writer processes on one metadata store

`crates/swarmops-store/tests/contention.rs`, ignored: `cargo nextest run --release -p swarmops-store --run-ignored all --test contention --no-capture`; `just check-slow` runs it unoptimized.

- 32 child processes start together, and each runs 1,000 `MetaStore::write()` transactions. Each transaction inserts the next changeset of the child's own chain and commits, with the default 10 s lock timeout. A `Busy` is counted and the transaction tried again.
- The lock wait is the time until `write()` returns: the wait for `meta/lock`, plus the tail checks and replay under it. Those alone take 0.06–0.09 ms (p50; p99 0.12 ms) uncontended, on the final store.
- Two pacings were measured:
  - back to back, the default: a writer that loops, as a bulk import or a long-lived process does;
  - 2 ms between a child's transactions (`SWARMOPS_TEST_CONTENTION_PAUSE_US=2000`), as an agent leaves at least a process start between two commits.

| | back to back, run 1 | back to back, run 2 | 2 ms apart, run 1 | 2 ms apart, run 2 |
|---|---|---|---|---|
| load at start (range) | 8.5 (8.5–11.0) | 17.2 (13.8–27.6) | 10.8 (10.8–15.6) | 9.9 (9.9–15.2) |
| wall, transactions/s | 264.8 s, 120.9/s | 291.3 s, 109.9/s | 320.8 s, 99.7/s | 281.7 s, 113.6/s |
| transaction p50 / p99 / max | 7.48 ms / 8.21 s / 72.8 s | 7.64 ms / 9.39 s / 61.3 s | 158.6 ms / 2.28 s / 10.1 s | 138.7 ms / 1.93 s / 6.20 s |
| lock wait p50 / p99 / max | 0.08 ms / 8.19 s / 72.8 s | 0.09 ms / 9.38 s / 61.2 s | 141.8 ms / 2.27 s / 10.1 s | 131.3 ms / 1.93 s / 6.19 s |
| `Busy` | 386 | 427 | 1 | 0 |
| child total time min / max (spread) | 121.6 / 264.8 s (2.18×) | 144.6 / 291.3 s (2.01×) | 269.8 / 320.8 s (1.19×) | 258.6 / 281.7 s (1.09×) |
| test | fails (`Busy`) | fails (`Busy`) | fails (`Busy`) | passes |

The two runs of each pacing disagree by more than 20 % on max and on the `Busy` count, so all four are shown. Under `just check-slow` (unoptimized, beside the other slow tests, load about 14) the default run failed the same way: 270 `Busy`, p99 lock wait 8.45 s, max 49.7 s, spread 1.47×, and 111 transactions/s.

The journal at the end of every run holds the same:

- 32,031 frames: 32,000 transactions and 31 `Compacted` markers;
- 1 frame past `journal.ckpt`;
- `journal.log` of 3,711,565 bytes (116 bytes per one-parent changeset frame).

The four runs above checked the store only after the `Busy` assertion, so its checks ran there only in 2 ms run 2, where they passed. The test now checks the store first. A third back-to-back run made that way:

- passed every store check: 32,000 changesets, each chain intact, one frame per transaction, and seqs contiguous in journal order, so the `Busy` retries lose nothing;
- then failed on `Busy`: 449 of them, p99 lock wait 9.29 s, max 78.7 s, spread 1.74×, 111.9 transactions/s, starting at load 12.3 (11.1–25.0).

A transaction holds the lock for its two journal `sync_data`s: 7.5 ms p50 when it does not wait, M0's 2 × 3.8 ms. Every 32nd transaction adds the index sync and the checkpoint (uncontended p99 30 ms), and about every 1,025th adds a compaction. The lock is never idle, so throughput stays at 100–121 transactions/s whatever the pacing.

What differs is who waits, and `MetaLock`'s poll makes that unfair. A writer that fails `try_lock` sleeps 10 ms, so the writer that has just released the lock takes it again, a few microseconds later, before any poller wakes:

- Back to back, that lets one writer keep the lock for runs of transactions while the others time out. The lock-wait p50 is 0.08 ms, but p99 is 8.2 s, and the worst transaction waited through seven 10 s timeouts.
- 2 ms apart, the lock goes to whichever poller wakes first: a lottery rather than a queue, and pollers that sleep a fixed 10 ms can stay unlucky. p50 is 130–140 ms and p99 1.9–2.3 s. Every child waited over 1 s at least once in both runs, 20 and 5 of them over 5 s, and in run 1 one wait reached the 10 s timeout.

So the spec's "32 processes are serialized fairly by the flock queue" does not hold for this lock. Nor does the plan's "Busy only after the 10 s lock timeout, 0 failures": `Busy` comes at either pacing.

Side experiment (not in the repo; the program is in the Task 7a report): 32 processes × 300 transactions, each two small appends with `sync_data` under the lock, taken either by std's `File::try_lock` every 10 ms (`MetaLock` today) or by the blocking `File::lock` (`flock(LOCK_EX)`).

Each cell gives run 1 and run 2. The load at the start of the six runs was 8.6, 20.1, 18.2, 14.0, 14.7 and 12.3. The controller ruled that these side runs not wait for it.

| | `try_lock` every 10 ms, back to back | `File::lock`, back to back | `File::lock`, 2 ms apart |
|---|---|---|---|
| transactions/s | 93.6 / 125.2 | 122.1 / 135.6 | 120.8 / 132.5 |
| lock wait p50 | 0.01 / 0.01 ms | 0.01 / 0.01 ms | 226 / 225 ms |
| lock wait p99 | 8.66 / 7.16 s | 1.06 / 0.80 s | 0.78 / 0.34 s |
| lock wait max | 36.6 / 27.7 s | 2.68 / 1.04 s | 1.45 / 0.37 s |
| `Busy` (10 s) | 108 / 64 | none (no timeout) | none (no timeout) |
| spread | 2.63× / 4.06× | 1.24× / 1.17× | 1.00× / 1.00× |

The polling column reproduces `MetaLock`'s behavior above.

A blocking `flock` wakes a waiter the moment the lock is released. That removes the 10 ms window in which a poller sleeps through the release. 2 ms apart, the lock rotates almost as a queue would: p50 225 ms ≈ 31 × 7.3 ms, and spread 1.00×. Back to back, the releaser still often takes the lock again before the woken waiter runs (p50 0.01 ms), but the tail drops from 7–9 s to about 1 s. The kernel's `flock` is not FIFO; a looping writer would need an explicit handoff, such as a ticket file, to be strictly fair.

### Decisions

The controller decides what changes, and in which task.

- **(a) `--durability fast`: recommended to reconsider in M2.** The rule: reconsider it if 32-way `put_batch` throughput is below 4× the 1-way.
  - `put_batch` + `flush` on 32 threads against 1 reaches 14.5× (3,666 against 253 objects/s) while nothing else writes to the filesystem.
  - It reaches only 2.5× (405 against 161 objects/s) with another writer, which is the normal state of a swarm host. That is below 4×.
  - What to reconsider follows from the same data. The per-object fsyncs, which a fast mode would skip, scale 14.8×. The shortfall is `flush`'s serial directory fsyncs: 571 of the 632 ms. A fast mode that skips object fsyncs would leave those in place.
  - Issuing the directory fsyncs in parallel keeps full durability, and took 162 of them from 575 ms to 45 ms in the side check. Try that first.
- **(b) Parallel `Index::sync()`: recommended.** The rule: only if five parallel fsyncs are at least 2× faster than serial.
  - Measured: 7.5 ms against 18.8 ms, 2.5× (2.48× and 2.97× in the two runs).
  - It saves about 11 ms at every batched sync (every 32nd transaction, and before each compaction), all of it time under the lock.
- **(c) A blocking lock instead of polling: recommended.** The rule: if the fairness spread exceeds 2× or the p99 lock wait exceeds 1 s.
  - Back to back: p99 lock wait 8.2 s and 9.4 s, spread 2.18× and 2.01×, and 386 and 427 `Busy`, so both criteria are exceeded.
  - 2 ms apart: p99 2.3 s and 1.9 s, with spread 1.19× and 1.09×. The p99 criterion is still exceeded, and `Busy` still occurs.
  - At 2 ms apart the spread understates the problem: over a whole run the lottery evens out, so every child finishes close to the others while single waits run to seconds. The wait tail is the indicator.
  - The side experiment says the change works: with the blocking `File::lock`, `Busy` disappears, and the p99 wait drops from 7–9 s to 0.8–1.1 s back to back and to 0.3–0.8 s 2 ms apart. The 10 s timeout can stay, with a thread that blocks in `File::lock` while the caller waits for it with a timeout; these runs never came near it. A strictly fair order for a writer that loops (a bulk import) needs a FIFO handoff on top of it.
