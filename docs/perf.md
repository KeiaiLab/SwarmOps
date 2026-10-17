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

**Decision.** p99 of a durable single-file write is 5.4 ms (max 7.4 ms over 1,000 writes), under the 20 ms threshold the M0 plan set, so **no `--durability fast` flag is introduced now**; the default stays full fsync (objects `sync_all` + one directory fsync per commit + the metadata store's syncs: SQLite `synchronous=FULL` in the M0 plan, and since the decision of 2026-10-15 for a native metadata store, the journal's two `fdatasync`s per transaction, storage spec §3). The rename + directory-fsync path has a 16.8 ms max, so a commit's single directory fsync can occasionally approach the threshold; that is one fsync per commit, not per object.

**Consequence for M1.** A commit that writes N new objects pays ~4 ms per object if the fsyncs are serial (100 files ≈ 0.4 s), which cannot meet the "≤ 5 ms + 0.3 ms per changed file" commit target. `FsObjectStore::put_batch` must issue the per-file fsyncs in parallel (rayon) and fsync each touched fan-out directory once at `flush()`. M1 measures `put_batch` throughput at 1, 8 and 32 concurrent fsyncs (`benches/put_batch.rs`); if the virtual disk serializes flushes (throughput does not scale), the `--durability fast` option (skip per-object fsync, keep the metadata journal's syncs — the SQLite WAL fsync in the M0 plan —, documented as "safe against process crash, not power loss") is reconsidered with that measurement attached here.

## M1 store level (2026-10-15)

The store-level half of the M1 Task 7 experiments; the CLI-level contention test follows with the CLI. Same host as M0: WSL2 (kernel 5.15.153.1), ext4 (`data=ordered`) on the virtual disk `/dev/sdd`, AMD Ryzen 9 7950X3D (16 cores, 32 threads), 106 GiB RAM; Rust 1.99.0, optimized builds (`cargo bench`; the contention test with `--release`). Every store lives in a fresh temporary directory under `~/.local/state/swarmops-bench`.

The host was not quiet. Three JetBrains remote-dev-server processes kept about 8.5 cores busy throughout, and other worktrees were compiling and testing. Before each run, the harness waited up to 10 minutes for the 1-minute load average to drop below 8. It did so three times, before `put_batch` runs 1, 2 and 4; every other run started at load 8.5–20.1. Each table gives the load at the start of each run, and the range sampled every 15 s during it. Threads blocked in fsync count towards the load average, so the I/O-bound runs raise it themselves. Every measurement ran twice. Where the two runs agree within 20 % the better one is shown; otherwise both are.

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

The "vs 1" ratios divide each cell's better run. Run by run (run 3 / run 4), 32 threads against 1 give 13.8× / 14.6× for `put_batch` + `flush`, 2.56× / 2.45× with the busy writer, and 14.9× / 13.2× for `put_batch` alone; 8 threads against 1 give 4.3× / 4.3×, 2.0× / 2.0× and 4.4× / 4.4×.

`flush` alone, after a 256-object batch: 4.1 ms (4.2 / 4.1 ms), and 571 ms with the busy writer (571 / 580 ms).

Runs 1 and 2 came before the busy-writer variants existed, and the host put them in different regimes. They disagree, so both count:

| run | load at start (range) | 32 threads, `put_batch` + `flush` | 8 threads | 1 thread | `flush` alone, median |
|---|---|---|---|---|---|
| 1 | 7.7 (7.5–21.4) | 587 ms | 872 ms | 1,174 ms | 412 ms, its samples spread from 4 ms to 568 ms |
| 2 | 7.6 (7.6–29.7) | 77 ms | 250 ms | 1,003 ms | 31 ms |

The per-object fsyncs scale: 14.8× on 32 threads, because the virtual disk and ext4's journal commit concurrent fsyncs together. `flush` scales only when nothing else is writing to the filesystem. On this kernel, a directory fsync becomes a full journal commit of its own whenever any process has metadata pending on the filesystem. That happens even for a directory that has not changed.

A side check bears this out (the two scripts are in the appendix below; the load was 11–12 throughout). 162 fsyncs of unchanged directories took 0.1 ms per round on a quiet filesystem. With one shell loop appending to a file, they took 567–616 ms per round over 5 rounds, every fsync a 3.7 ms commit (p50). The same fsyncs issued from 32 threads took 44–47 ms, against 567–585 ms serially in the same session.

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

The one cell with two values is where the runs differ by more than 20 % (run 1 / run 2). Run once more on the integration branch, where the bench lays the index out with `Index::create` (`Index::open` creates nothing), at load 23.7 (21.9–27.4 during the run): every median within 12 % of the value shown. Every lookup takes under 11 µs, so the spec's read target (< 1 ms) holds by two orders of magnitude, at 200,000 changesets as at 10,000. A lookup is about log2 n `read_at`s of one 32-byte key (14 at 10,000, 18 at 200,000), so going from 10,000 to 200,000 changesets adds about 2 µs. A full tail adds 0.5–3.2 µs: one 32 KiB `read_at` and a linear scan. `find_prefix` pays the tail scan even when the sorted view holds the match. `find` miss is what every insert of a new changeset asks, under the lock.

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

The two runs of each pacing disagree by more than 20 % on max and on the `Busy` count, so all four are shown. Under `just check-slow` (unoptimized, beside the other slow tests) the default run failed the same way, twice. The first gate run started at load 14.8: 270 `Busy`, p99 lock wait 8.45 s, max 49.7 s, spread 1.47× and 111.0 transactions/s. The run on the final commit of the measurement started at load 12.9: 260 `Busy`, p99 lock wait 8.11 s, max 40.7 s, spread 1.55× and 108.6 transactions/s. Neither gate run sampled the load during the run.

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

Side experiment (the program is in the appendix below): 32 processes × 300 transactions, each two small appends with `sync_data` under the lock, taken either by std's `File::try_lock` every 10 ms (what `MetaLock` did when these runs were made) or by the blocking `File::lock` (`flock(LOCK_EX)`).

Each cell gives run 1 / run 2, and the first row the load at the start of each. These runs did not wait for the load to drop, and nobody sampled it during them.

| | `try_lock` every 10 ms, back to back | `File::lock`, back to back | `File::lock`, 2 ms apart |
|---|---|---|---|
| load at start | 8.6 / 14.0 | 20.1 / 14.7 | 18.2 / 12.3 |
| transactions/s | 93.6 / 125.2 | 122.1 / 135.6 | 120.8 / 132.5 |
| lock wait p50 | 0.01 / 0.01 ms | 0.01 / 0.01 ms | 226 / 225 ms |
| lock wait p99 | 8.66 / 7.16 s | 1.06 / 0.80 s | 0.78 / 0.34 s |
| lock wait max | 36.6 / 27.7 s | 2.68 / 1.04 s | 1.45 / 0.37 s |
| `Busy` (10 s) | 108 / 64 | none (no timeout) | none (no timeout) |
| spread | 2.63× / 4.06× | 1.24× / 1.17× | 1.00× / 1.00× |

The polling column reproduces `MetaLock`'s behavior above.

A blocking `flock` wakes a waiter the moment the lock is released. That removes the 10 ms window in which a poller sleeps through the release. 2 ms apart, the lock rotates almost as a queue would: p50 225 ms ≈ 31 × 7.3 ms, and spread 1.00×. Back to back, the releaser still often takes the lock again before the woken waiter runs (p50 0.01 ms), but the tail drops from 7–9 s to about 1 s. The kernel's `flock` is not FIFO; a looping writer would need an explicit handoff, such as a ticket file, to be strictly fair.

### Decisions

- **(a) `--durability fast`: not justified by this data; parallel directory fsyncs in `flush` come first.** Applied in Task 7c, which parallelizes the directory fsyncs and adds no flag ("M1 store fixes (7c)" below). The rule: reconsider the flag if 32-way `put_batch` throughput is below 4× the 1-way.
  - The trigger is met only with another writer on the filesystem, which is the normal state of a swarm host: `put_batch` + `flush` on 32 threads against 1 then reaches 2.5× (405 against 161 objects/s; 2.56× and 2.45× in runs 3 and 4), below 4×. While nothing else writes it reaches 14.5× (3,666 against 253 objects/s; 13.8× and 14.6×).
  - The shortfall is not what a fast mode would skip. The per-object fsyncs scale 14.8× (14.9× and 13.2×); `flush`'s serial directory fsyncs take 571 of the 632 ms. A fast mode that skipped the object fsyncs would leave those in place.
  - Issuing the directory fsyncs in parallel keeps full durability, and took 162 of them from 575 ms to 45 ms in the side check. That comes first, and this data does not justify a `--durability fast` flag, which exists only with a measurement here that does.
- **(b) Parallel `Index::sync()`: recommended.** Applied in Task 7c (below). The rule: only if five parallel fsyncs are at least 2× faster than serial.
  - Measured: 7.5 ms against 18.8 ms, 2.5× (2.48× and 2.97× in the two runs).
  - It saves about 11 ms at every batched sync (every 32nd transaction, and before each compaction), all of it time under the lock.
- **(c) A blocking lock instead of polling: recommended.** Applied in Task 7c (below). The rule: if the fairness spread exceeds 2× or the p99 lock wait exceeds 1 s. The p99 lock wait carries the decision: it exceeds 1 s in every run, while the spread exceeds 2× only in back-to-back runs 1 and 2.
  - Back to back: p99 lock wait 8.2 s and 9.4 s, spread 2.18× and 2.01×, and 386 and 427 `Busy`, so both criteria are exceeded. The third back-to-back run (p99 9.3 s, spread 1.74×) and the two `just check-slow` runs (p99 8.5 s and 8.1 s, spreads 1.47× and 1.55×) exceed the p99 criterion alone.
  - 2 ms apart: p99 2.3 s and 1.9 s, with spread 1.19× and 1.09×. The p99 criterion is still exceeded, and `Busy` still occurs.
  - At 2 ms apart the spread understates the problem: over a whole run the lottery evens out, so every child finishes close to the others while single waits run to seconds. The wait tail is the indicator.
  - The side experiment says the change works: with the blocking `File::lock`, `Busy` disappears, and the p99 wait drops from 7–9 s to 0.8–1.1 s back to back and to 0.3–0.8 s 2 ms apart. The 10 s timeout can stay, with a thread that blocks in `File::lock` while the caller waits for it with a timeout; these runs never came near it. A strictly fair order for a writer that loops (a bulk import) needs a FIFO handoff on top of it.

### M1 store fixes (7c)

Task 7c applied the three decisions above, all at full durability:

- (c) `MetaLock` waits in the kernel's flock queue. An acquire tries the lock once without blocking; if another handle holds it, a helper thread blocks in `File::lock` (`flock(LOCK_EX)`) on the acquire's own open file, and the caller waits for it up to the 10 s timeout. A helper that gets the lock after its caller gave up lets it go at once.
- (b) `Index::sync` makes its five `sync_data`s from five threads at once (`MetaStore::set_parallelism`, 5 unless set).
- (a), the first step: `FsObjectStore::flush` makes its directory fsyncs at once, on the object store's pool (`FsObjectStore::set_parallelism`, else rayon's global pool). The flag stays out.

At parallelism 1 both kinds of sync run one after another in their old order. `LocalRepo` sets that under `SWARMOPS_TEST_CRASH_AT`, so crash sweeps keep meeting the same call at the same count.

The disk was 1.6–3.7× slower than M0's. Another process on the host was writing about 110 MB/s near the end of the lock-change run (seen at its 13th minute, gone 9 minutes later), and other worktrees' tests and benches ran during some runs (named with each). Beside the load, every run gives a raw `fdatasync` of a 120-byte append (the median of 40) at its start and end: 13–15 ms while that writer ran, 6.6–7.9 ms after it stopped, against M0's 4.1 ms. So absolute times here do not compare with the 7a section; waits counted in transactions, `Busy` and the spread do.

#### 32 writer processes, back to back

`crates/swarmops-store/tests/contention.rs` as above (32 × 1,000, `--release`), run once with the lock change alone and twice with all three changes. The 7a polling runs are the back-to-back runs 1 and 2 above. "In transactions" divides each wait by the run's mean transaction time (wall / 32,000), the time the lock takes to pass once.

| | 7a polling, run 1 | 7a polling, run 2 | 7c, lock change alone | 7c, run 1 | 7c, run 2 |
|---|---|---|---|---|---|
| load at start (range) | 8.5 (8.5–11.0) | 17.2 (13.8–27.6) | 33.3 (15.9–36.0) | 32.6 (10.5–33.4) | 10.5 (10.5–16.4) |
| raw `fdatasync` p50, start / end | not taken | not taken | 14.8 ms (mid-run) | 15.3 / 6.7 ms | 6.6 / 7.9 ms |
| wall, transactions/s | 264.8 s, 120.9/s | 291.3 s, 109.9/s | 794.9 s, 40.3/s | 590.9 s, 54.2/s | 444.4 s, 72.0/s |
| transaction p50 / p99 / max | 7.48 ms / 8.21 s / 72.8 s | 7.64 ms / 9.39 s / 61.3 s | 832 ms / 3.97 s / 9.02 s | 693 ms / 2.47 s / 5.26 s | 24.1 ms / 1.74 s / 2.61 s |
| lock wait p50 / p99 / max | 0.08 ms / 8.19 s / 72.8 s | 0.09 ms / 9.38 s / 61.2 s | 817 ms / 3.91 s / 8.92 s | 678 ms / 2.43 s / 5.22 s | 0.11 ms / 1.71 s / 2.57 s |
| lock wait in transactions | 0.01 / 990 / 8,800 | 0.01 / 1,030 / 6,720 | 33 / 157 / 359 | 37 / 131 / 283 | 0.01 / 123 / 185 |
| `Busy` | 386 | 427 | 0 | 0 | 0 |
| child total time min / max (spread) | 121.6 / 264.8 s (2.18×) | 144.6 / 291.3 s (2.01×) | 747.4 / 794.9 s (1.06×) | 535.8 / 590.9 s (1.10×) | 382.1 / 444.4 s (1.16×) |
| uncontended transaction p50 / p99 | not printed | not printed | 13.20 / 46.04 ms | 13.15 / 39.05 ms | 15.33 / 41.43 ms |
| test | fails (`Busy`) | fails (`Busy`) | passes | passes | passes |

Tests of other worktrees ran during some of these runs, on the same disk: Task 6's crash sweeps in 9 of the 39 load samples of run 1, and Task 7b's `fsync_latency` bench in 10 of the 29 samples of run 2. Task 7b's 32-process CLI contention test was running 8 minutes before the lock-change run started and had ended 11 minutes into it; that run did not log other processes, and neither did the 7a runs.

`Busy` is gone, in every run. Whether the writer that has just committed takes the lock straight back depends on the host. At load 32–33 the woken waiters won: the lock rotated, and the median wait was one round of the 31 other writers (33–37 transactions). At load 10–16 (run 2) the committing writer usually asked again and got the lock before a woken waiter ran (median 0.11 ms), as the blocking `flock` did in the 7a side experiment. Either way every child finished within 16 % of the others (spread 1.06–1.16×, from 2.0–2.2× with polling). Counted in transactions, the p99 wait is 6–8× shorter, and the longest 19–48× shorter.

The tail is bounded, not gone. At each release the kernel wakes every waiter and the first to run takes the lock, so a waiter can lose many releases in a row. A uniform random winner among 31 waiters predicts p99 140 and, over 32,000 waits, a longest wait of about 316 transactions; the rotating runs show 131–157 and 283–359. So the longest wait is a few hundred transactions: 2.6 s at run 2's 14 ms per transaction (it would be about 2.5 s at 7a's 8–9 ms), but 5.2–8.9 s at 18–25 ms. That stays inside the 10 s timeout, but on a slow disk not by much. An order fair to the transaction (at most 31) needs a FIFO handoff on top of `flock`.

Throughput follows the disk: an uncontended transaction took 13.2–15.3 ms at the end of these runs (7.4 ms in 7a), and the lock passed every 14–25 ms. The lock's own hand-off (a helper thread woken in the kernel, then a channel to its caller) does not show at that scale. In one process, 32 threads × 16 turns of 2 ms (`tests/lock.rs`, twice each) wait at most 0.12–0.13 s with the kernel queue (load 16.0), against 1.22–1.25 s with the polling lock (load 17.0).

#### `Index::sync`

`crates/swarmops-store/benches/fsync_fanout.rs`, once, with two new variants that time `Index::sync` itself: an index of its own gets the 32 records of a checkpoint's frames appended before every iteration (untimed), then syncs at parallelism 1 or 5. It started at load 13.1 (12.4–13.7 during the run), raw `fdatasync` 8.0 ms at its start and 7.1 ms at its end; nothing else was benchmarking.

| variant | 7a, runs 1 / 2 | 7c |
|---|---|---|
| serial, 5 × `sync_data` | 18.8 / 22.4 ms | 38.5 ms |
| parallel, 5 threads | 7.6 / 7.5 ms | 15.5 ms |
| one file | 3.8 / 4.0 ms | 6.9 ms |
| `Index::sync`, parallelism 1 | not measured | 24.1 ms |
| `Index::sync`, parallelism 5 (the default) | not measured | 9.2 ms |

One `sync_data` took 6.9 ms here against 3.8–4.0 ms in 7a, so the disk ran 1.8× slower; in those units the raw variants repeat 7a (serial 5.6 single syncs, 7a 4.9–5.6; parallel 2.2, 7a 1.9–2.0), 2.5× apart (2.48×; 7a 2.48× and 2.97×). `Index::sync` itself, after a checkpoint's 32 records, takes 24.1 ms one file after another and 9.2 ms at the default parallelism: 2.6× faster, and 14.9 ms less time under the lock at every batched sync (0.47 ms a transaction on average). It costs less than the raw serial variant because 32 records add about 1 KiB to a 32-byte column, not 64 KiB.

#### `flush`

`crates/swarmops-store/benches/put_batch.rs`, once, unchanged: `flush` now makes its directory fsyncs on the store's pool, so the 8- and 32-thread variants fsync 8 and 32 at a time and the 1-thread variants as before. It started at load 13.1 (13.3–24.9 during the run), raw `fdatasync` 7.2 ms at its start and 7.9 ms at its end; nothing else was benchmarking.

| variant | 7a, runs 3 / 4 | 7c |
|---|---|---|
| `put_batch` + `flush`, 1 thread | 1,011 ms / 1,017 ms | 1,857 ms |
| 8 threads | 235 ms / 234 ms | 481 ms |
| 32 threads | 73.1 ms / 69.8 ms | 72.3 ms |
| the same with the busy writer, 1 thread | 1,615 ms / 1,591 ms | 1,572 ms |
| 8 threads | 810 ms / 790 ms | 304 ms |
| 32 threads | 632 ms / 648 ms | 98.9 ms |
| `put_batch` alone, 1 thread | 999 ms / 990 ms | 984 ms |
| 8 threads | 230 ms / 228 ms | 228 ms |
| 32 threads | 67.0 ms / 74.9 ms | 66.3 ms |
| `flush` alone, on the 32-thread store (7a: one directory after another) | 4.2 ms / 4.1 ms | 5.3 ms |
| the same with the busy writer | 571 ms / 580 ms | 65.2 ms |

Each row is one median, and the host changed between rows: the two 8-thread rows disagree the wrong way round (481 ms, and 304 ms with the busy writer). The object fsyncs cost what they did in 7a: `put_batch` alone takes 984 / 228 / 66.3 ms on 1 / 8 / 32 threads (7a: 999 / 230 / 67.0 ms). But every directory fsync was a journal commit of its own today even without the bench's busy writer, so one thread's serial `flush` took about 870 ms (1,857 − 984 ms).

From 32 threads at once, `flush` takes 6 ms of the 72.3 ms that `put_batch` + `flush` take, and alone, with the busy writer, 65 ms against 571–580 ms one after another in 7a: 8.8–8.9× faster (the 7a side check had 575 against 45 ms).

Against decision (a)'s rule, `put_batch` + `flush` on 32 threads now reaches 25.7× the 1-thread throughput (3,542 against 138 objects/s; 7a 13.8× and 14.6×), and 15.9× with the busy writer (2,588 against 163 objects/s; 7a 2.56× and 2.45×). That is above 4× in both, so nothing here calls for a `--durability fast` flag. A 32-thread commit's 256 new files take 72 ms, inside the "≤ 5 ms + 0.3 ms per changed file" target (82 ms), and 99 ms with the busy writer on this slower disk.

### Appendix: the side-experiment programs

The side experiments above ran outside the workspace; here they are, so that they can be run again.

**Directory fsyncs** (Python 3). `python3 dirsync.py <dir>` times 162 fsyncs of unchanged directories in a fresh directory under `<dir>`, in 5 rounds; `python3 dirsync_par.py <dir>` times the same serially and from 32 threads. For the busy case, a shell loop appended to a file on the same filesystem meanwhile (`while :; do echo x >> f; done`).

`dirsync.py`:

```python
"""Times fsync of directories that have not changed: 162 in a row, 5 rounds."""
import os
import sys
import tempfile
import time

base = sys.argv[1]
with tempfile.TemporaryDirectory(dir=base) as d:
    dirs = [os.path.join(d, f"{i:02x}") for i in range(162)]
    for p in dirs:
        os.mkdir(p)
    fd = os.open(d, os.O_RDONLY)
    os.fsync(fd)
    os.close(fd)
    for rnd in range(5):
        times = []
        for p in dirs:
            fd = os.open(p, os.O_RDONLY)
            t = time.perf_counter()
            os.fsync(fd)
            times.append(time.perf_counter() - t)
            os.close(fd)
        times.sort()
        print(f"round {rnd}: total {sum(times)*1e3:.1f} ms, per fsync p50 {times[81]*1e6:.0f} us, "
              f"p90 {times[145]*1e6:.0f} us, max {times[-1]*1e6:.0f} us, "
              f"{sum(1 for x in times if x > 1e-3)} of 162 over 1 ms")
```

`dirsync_par.py`:

```python
"""Times fsync of 162 unchanged directories, serially and from 32 threads (5 rounds each)."""
import os
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

base = sys.argv[1]


def sync(p):
    fd = os.open(p, os.O_RDONLY)
    os.fsync(fd)
    os.close(fd)


with tempfile.TemporaryDirectory(dir=base) as d, ThreadPoolExecutor(32) as pool:
    dirs = [os.path.join(d, f"{i:02x}") for i in range(162)]
    for p in dirs:
        os.mkdir(p)
    sync(d)
    for mode in ("serial", "32 threads"):
        totals = []
        for _ in range(5):
            t = time.perf_counter()
            if mode == "serial":
                for p in dirs:
                    sync(p)
            else:
                list(pool.map(sync, dirs))
            totals.append((time.perf_counter() - t) * 1e3)
        print(f"{mode}: 162 directory fsyncs take " + ", ".join(f"{x:.1f}" for x in totals) + " ms")
```

**Lock fairness** (Rust, std only). Built with `rustc --edition 2024 -O flock_fairness.rs`, and run as `flock_fairness <dir> <poll|block> 32 300 <pause-us>`.

```rust
//! Side experiment for decision (c), in Rust with std's file locks (what MetaLock uses).
//! usage: flock_fairness <dir> <poll|block> <processes> <per-process> <pause-us>
//! Each process runs <per-process> transactions: open <dir>/lock, take it (poll: try_lock every
//! 10 ms, a Busy counted after 10 s and the wait restarted; block: File::lock), append + sync_data
//! 120 B to journal.log and 8 B to journal.off, unlock (drop). Pause between transactions.
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn append(path: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new().append(true).create(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_data().unwrap();
}

fn child(dir: &Path, mode: &str, per: u64, pause: Duration) {
    while !dir.join("go").exists() {
        std::thread::sleep(Duration::from_millis(1));
    }
    let started = Instant::now();
    let mut waits = Vec::new();
    let mut busy = 0;
    for i in 0..per {
        if i > 0 && !pause.is_zero() {
            std::thread::sleep(pause);
        }
        let begun = Instant::now();
        let lock = OpenOptions::new().write(true).create(true).truncate(false).open(dir.join("lock")).unwrap();
        if mode == "poll" {
            let mut deadline = begun + Duration::from_secs(10);
            loop {
                match lock.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) => {}
                    Err(TryLockError::Error(e)) => panic!("{e}"),
                }
                let now = Instant::now();
                if now >= deadline {
                    busy += 1;
                    deadline = now + Duration::from_secs(10);
                }
                std::thread::sleep(Duration::from_millis(10).min(deadline - now));
            }
        } else {
            lock.lock().unwrap();
        }
        waits.push(begun.elapsed().as_micros());
        append(&dir.join("journal.log"), &[0x78; 120]);
        append(&dir.join("journal.off"), &[0x79; 8]);
        let _ = lock.unlock();
        drop(lock);
    }
    let total = started.elapsed().as_micros();
    let w: Vec<String> = waits.iter().map(u128::to_string).collect();
    println!("{total} {busy} {}", w.join(","));
}

fn pct(sorted: &[u128], p: usize) -> u128 {
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let (mode, procs, per, pause_us) = (&args[2], args[3].parse::<u64>().unwrap(), args[4].parse::<u64>().unwrap(), args[5].parse::<u64>().unwrap());
    if std::env::var_os("FLOCK_CHILD").is_some() {
        child(&dir, mode, per, Duration::from_micros(pause_us));
        return;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..procs)
        .map(|_| Command::new(&exe).args(&args[1..]).env("FLOCK_CHILD", "1").stdout(Stdio::piped()).spawn().unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    File::create(dir.join("go")).unwrap();
    let outs: Vec<String> = children.into_iter().map(|c| String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap()).collect();
    let wall = started.elapsed().as_secs_f64();
    let (mut waits, mut totals, mut busy) = (Vec::new(), Vec::new(), 0);
    for out in &outs {
        let mut parts = out.trim().split(' ');
        totals.push(parts.next().unwrap().parse::<u128>().unwrap());
        busy += parts.next().unwrap().parse::<u64>().unwrap();
        waits.extend(parts.next().unwrap().split(',').map(|w| w.parse::<u128>().unwrap()));
    }
    waits.sort_unstable();
    let (min, max) = (*totals.iter().min().unwrap(), *totals.iter().max().unwrap());
    println!(
        "mode={mode} procs={procs} per={per} pause={pause_us}us: wall {wall:.1} s, {:.1} txn/s, Busy {busy}; lock wait p50 {:.2} ms p99 {:.1} ms max {:.1} ms; spread {:.2}x (min {:.1} s, max {:.1} s)",
        (procs * per) as f64 / wall,
        pct(&waits, 50) as f64 / 1e3, pct(&waits, 99) as f64 / 1e3, *waits.last().unwrap() as f64 / 1e3,
        max as f64 / min as f64, min as f64 / 1e6, max as f64 / 1e6
    );
}
```

## M1 (CLI level) (2026-10-15)

The CLI-level half of the M1 Task 7 experiments: 32 agents committing through the real `swarmops` binary, and the lock-free read path after the journal hardening. Same host as M0: WSL2 (kernel 5.15.153.1), ext4 (`data=ordered`) on the virtual disk `/dev/sdd`, 32 hardware threads; Rust 1.99.0, optimized builds (the contention test with `--release`, the bench with `cargo bench`). Every store lives in a fresh temporary directory under `~/.local/state/swarmops-bench`. `just perf` re-measures in three steps, the store-level contention test, this CLI test at 32 × 1,000, then every store bench, and prints only their summary lines.

**Every number here is provisional.** The host was never quiet: three JetBrains remote-dev-server processes (about 8.5 cores), another project's test suite, and other SwarmOps worktrees: Task 7c's 32-process store-level contention runs overlapped the end of contention run 2 and both bench runs before the merge, and Task 8's multi-process tests overlapped the runs in `just perf` after it ("After 7c" below). The 1-minute load average was 15.7–23.1 when each of the four 32 × 1,000 contention runs started and 15–39 during them (32 processes blocked in fsync count towards it), and 12–61 during the other runs. Every measurement ran twice, except the read snapshot after the merge (once, in `just perf`); where two runs disagree by more than 20 %, both are shown. The controller re-runs `just perf` in a quiet window before the final review.

### 32 agents committing through the binary

`crates/swarmops-cli/tests/contention.rs`, ignored. `just perf` runs it optimized (`cargo nextest run --release -p swarmops-cli --run-ignored all --test contention --no-capture`), `just check-slow` unoptimized, both at the spec's 32 × 1,000 (at 32 × 250 for a while; see below).

- The parent `init`s a store, commits a base changeset and creates main on it. Then 32 child processes start together behind a barrier (`ready-<n>` files, then `go`). Each runs `swarmops commit` 1,000 times in a row as agent `contention/agent-<n>`: one small file (`agents/<n>.txt`, about 20 bytes) per commit, on `--parent` its own previous changeset. So no child's commit depends on another's, and all they share is the store, and its `meta/lock`.
- Every commit is a process of its own: start, store open, two metadata snapshots (resolving `--parent`, checking the parents), the blob, two trees and the changeset written and synced (`put_batch` + `flush`), then the metadata write transaction under `meta/lock`, then the envelope.
- Per commit the child records the envelope's `meta.elapsed_ms` (all of that but process start, exit and printing) and the wall time from spawn to exit; the tables give nearest-rank percentiles over every commit's value. An `E_BUSY` (exit 4, retryable) is counted, and the same commit run again.
- A control comes first: one commit while the test itself holds `meta/lock` gets `E_BUSY` after 10,013 ms (10,014 in run 2), so the harness does see a `Busy` when there is one. The test also checks that every `E_BUSY` came after the 10 s lock timeout.
- After the children: `info` must count every changeset; 100 uncontended commits in a row time a commit alone on the final store; then `verify --deep` must find nothing, and `log -n 5` (from main, moved to child 0's last changeset) must list child 0's last five commits.

| 32 × 1,000 commits | run 1 | run 2 |
|---|---|---|
| load before / during / after | 16.1 / 15.5–35.7 / 32.0 | 15.7 / 15.1–36.0 / 22.8 |
| wall, commits/s | 500.7 s, 63.9/s | 867.2 s, 36.9/s |
| commit `meta.elapsed_ms` p50 / p90 / p99 / p99.9 / max | 265 ms / 1.13 s / 2.87 s / 5.19 s / 10.02 s | 432 ms / 2.05 s / 5.35 s / 8.53 s / 10.00 s |
| commit wall time, spawn to exit, p50 / p99 / max | 268 ms / 2.88 s / 10.02 s | 435 ms / 5.35 s / 10.00 s |
| `E_BUSY` (every one after the 10 s timeout) | 2, in 2 children | 19, in 14 children (each after 10.03–10.50 s) |
| other failures | 0 | 0 |
| child total time min / max (spread) | 453.0 / 500.7 s (1.11×) | 786.1 / 867.2 s (1.10×) |
| children with a commit over 1 s / over 5 s | 32 / 24 | 32 / 32 |
| uncontended commit afterwards, `meta.elapsed_ms` p50 / p99 | 30 / 54 ms | 58 / 110 ms |
| lock wait, estimated as `meta.elapsed_ms` − that p50: p50 / p99 / max | 235 ms / 2.84 s / 9.99 s | 374 ms / 5.29 s / 9.94 s |
| `verify --deep` | clean; 128,308 objects, 32,101 changesets in 2.06 s | clean; 128,325 objects, 32,101 changesets in 2.00 s |
| `log -n 5` | 5 entries, 0 ms | 5 entries, 0 ms |
| the whole test | 516.5 s | 886.1 s |
| test | fails (`E_BUSY`) | fails (`E_BUSY`) |

The two runs disagree by more than 20 % on nearly everything but the spread, so both are shown. The host's I/O load differed: Task 7c's store-level contention test started during run 2's last few minutes, and run 2's uncontended commits afterwards, timed while it ran, cost twice run 1's (58 against 30 ms). A 32 × 100 calibration run before them (load 24.4, 22.1–27.7 during) gave run 1's throughput: 63.3 commits/s, p50 261 ms, p99 2.96 s, max 6.78 s, no `E_BUSY`, and an uncontended commit of 20 ms.

**Throughput is the lock's.** The lock never idles: each of the 32 processes needs it once per commit, and between two of its turns does only its own work, which takes 20–60 ms alone (an uncontended commit), against the 0.4–0.9 s it waits on average. So commits/s is one over the time a commit holds `meta/lock`: about 15.6 ms in run 1 and 27 ms in run 2 (15–20 ms in the 32 × 250 runs below). That is 2–3.6× the store-level transaction of the M1 store-level section (Task 7a: 7.5 ms p50 alone, 100–121 transactions/s with 32 writers). It is the same transaction, with the journal's two `sync_data`s (and every 32nd commit the index sync and the checkpoint), but here ext4 commits each of them along with 31 other processes' new object files and fan-out directory fsyncs.

**The wait is the latency.** A contended commit is mostly its wait for the lock: p50 235 and 374 ms, p99 2.8 and 5.3 s. With the lock always busy, the mean is fixed by the throughput: each process takes one turn per round of all 32, so a commit takes 32 ÷ commits/s on average, 0.50 s in run 1 and 0.87 s in run 2, whatever the lock's discipline. The 32 × 250 runs, which print the mean, bear this out: 624 and 461 ms against 32 ÷ 49.4 = 648 and 32 ÷ 65.6 = 488 ms. The discipline only decides who waits how long, and the 10 ms poll makes it a lottery: the median commit waits half the mean, while one commit in a hundred waits 3–5 s. Every child waited over 1 s at least once, and 24 and 32 of them over 5 s. A commit that loses the lottery for 10 s straight gets `E_BUSY`: 2 and 19 of the 32,000. Its retry then commits, so nothing is lost and `verify --deep` stays clean. But the plan's "0 failures, `Busy` only after the 10 s timeout" holds only in its second half, and the test fails, as the store-level one does.

**The spread is not the indicator.** 1.11× and 1.10×: over 1,000 commits each child's luck evens out, while single commits wait for seconds. That matches the store-level runs 2 ms apart, the pacing closest to a CLI agent's.

**An uncontended commit costs 20–30 ms, nearly all of it fsyncs.** 20–30 ms p50 when the disk is calmer (58 ms in run 2). Every commit makes at least four fsyncs one after another: its new objects' (in parallel), then its fan-out directories' (one after another before Task 7c, at once since), then the journal's two `sync_data`s under the lock. At M0's ~4 ms per fsync on this disk, that alone is about 15 ms. The snapshots are not the cost: a commit takes two, about 0.1 ms (below). The design spec (§15) states the target ("≤ 5 ms + 0.3 ms per changed file") excluding the syncs, and a full durable commit at 20–30 ms on this host; nothing here measured a commit without its syncs.

`verify --deep` checked 32,101 changesets and about 128,300 objects in 2.0–2.1 s, about 63,000 objects/s. `log -n 5` took under 1 ms.

### 32 × 250 while the lock polled

With the polling lock the test took 516.5 s and 886.1 s at 32 × 1,000, over 10 minutes in run 2, so its default was cut to 32 × 250 until the lock changed; `just perf` kept the spec's 32 × 1,000. These are its runs at 32 × 250 then.

| 32 × 250 commits | run 1 | run 2 |
|---|---|---|
| load before / during / after | 24.8 / 23.8–61.1 / 37.8 | 26.6 / 18.3–26.6 / 18.3 |
| wall, commits/s | 162.1 s, 49.4/s | 122.0 s, 65.6/s |
| commit `meta.elapsed_ms` mean / p50 / p90 / p99 / p99.9 / max | 624 ms / 316 ms / 1.50 s / 4.61 s / 8.60 s / 9.81 s | 461 ms / 243 ms / 1.11 s / 3.00 s / 5.05 s / 8.42 s |
| `E_BUSY` | 0 | 0 |
| child total time min / max (spread) | 147.5 / 162.1 s (1.10×) | 100.1 / 122.0 s (1.22×) |
| children with a commit over 1 s / over 5 s | 32 / 28 | 32 / 9 |
| uncontended commit afterwards, p50 / p99 | 21 / 43 ms | 21 / 40 ms |
| `verify --deep` | clean; 32,306 objects, 8,101 changesets in 0.41 s | clean; the same in 0.40 s |
| the whole test | 175.5 s | 135.1 s |
| test | passes | passes |

No other SwarmOps test ran during either; run 1's load rose to 61 with other work on the host. The runs disagree by more than 20 %, so both are shown. Both passed, but with the polling lock that was luck: their slowest commits waited 9.8 s and 8.4 s of the 10 s timeout. At the 32 × 1,000 runs' rates (2 and 19 `E_BUSY` in 32,000 commits) 8,000 commits meet 0.5 to 4.75 on average, so `just check-slow` would have failed now and then until the lock changed. Under `just check-slow` then (unoptimized, beside every other slow test, load 18.9 at the start and 19.6–25.9 during) the test passed in 138.6 s.

### Read snapshot after the journal hardening

`crates/swarmops-store/benches/read_snapshot.rs` (`cargo bench -p swarmops-store --bench read_snapshot`). Since the hardening, a reader measures `journal.off` before `journal.log`, and `Journal::tail_frames_from` finds the checkpoint's frame by a binary search of `journal.off`, one 8-byte `read_at` per probe. `MetaStore::read()` calls it twice.

- One store per size, 10,000 and 100,000 changesets, each inserted by a write transaction of its own: a journal frame each, as commits make them. The stores are built outside the timed loops, through a filesystem that skips fsyncs, since a read does not depend on them; their files are in the page cache, as a hot store's are.
- Each store is timed twice through a handle on the real filesystem:
  - `ckpt_end`: after `MetaStore::flush`, the checkpoint at the end of the journal;
  - `tail31`: after 31 more transactions, 31 frames past the checkpoint, the most that the batched checkpoint leaves.
- The variants cycle through 1,024 scattered changesets:
  - `read_changeset`: `MetaStore::read()`, then `changeset(id)` on the snapshot;
  - `read`: `MetaStore::read()` alone;
  - `tail_frames`: one `Journal::tail_frames_from(journal.ckpt)`;
  - `off_search`: that call's binary search of `journal.off` alone (copied from `Journal`, whose search is private): what caching the checkpoint's frame would save.
- Every timed call is recorded, so these are nearest-rank percentiles over 57,000 to 999,000 calls per variant, warm-up included. Criterion's medians run 0–16 % above these p50s: they are medians of per-sample means, which the tail pulls up.

Run 1 started at load 11.7 (12.4–16.3 during), with Task 7c's 32-process store-level contention test running throughout; run 2 at load 22.6 (17.0–24.6), Task 7c's processes starting up again during it. p50 / p99:

| store | `read_changeset` | `read` | `tail_frames` | `off_search` |
|---|---|---|---|---|
| 10k, `ckpt_end` | 46 / 84 µs | 40 / 72 µs | 7.0 / 11.7 µs | 3.9 / 5.8 µs |
| 10k, `tail31` | 62 / 108 µs | 54 / 94 µs | 10.5 / 18.6 µs | 3.9 / 5.3 µs |
| 100k, `ckpt_end` | 53 / 94 µs | 42 / 76 µs | 8.0 / 13.6 µs | 4.9 / 7.4 µs |
| 100k, `tail31` | 69 / 127 µs | 58 / 105 µs | **11.7 / 20.3 µs; 14.3 / 25.0 µs** | 5.1 / 7.7 µs |

Each cell is run 1, which run 2 matches within 20 %, except the bold cell, where they differ by 22 % (p50) and 23 % (p99): run 1; run 2.

Task 2b's figure is a `find` p99 of 16 µs (`tests/index.rs`, `index_on_wide_history_is_fast`: `find` alone on an open 200,000-record index, unoptimized). That test, re-run on this code at load 17–29 with Task 6's tests running, gives p50 9.1 µs and p99 18.3 µs (2b: 10.3 and 16.1 µs), so the lookups did not change. The snapshot is the larger cost: `read_changeset`'s p99 of 84–128 µs is 5–8× that `find` p99, and `read` alone is most of it. The lookup on the snapshot adds 6–12 µs (p50).

**Should the `journal.off` lookup be cached? Not now.**

- The search costs 3.9–5.3 µs (p50; 14 probes at 10,000 frames, 17 at 100,000). `read()` runs it twice, 8–11 µs, about a fifth of the snapshot.
- A snapshot with its lookup is 46–70 µs p50 and under 130 µs p99, about 8× under the spec's read target (< 1 ms) at 100,000 changesets. A CLI command takes one or two: two for a commit, about 0.1 ms of a 20–60 ms commit, and less than its own process start.
- The rest of the snapshot is opening and reading files: `journal.ckpt`, the index's nine files and the refs, about 25 µs. The 31 frames past the checkpoint add 14–15 µs more: read and parsed twice, then overlaid. Each costs more than the search.
- Caching pays only in a long-lived process that takes many snapshots, such as a server (M10). There, a per-handle cache of the checkpoint's frame (its seq, checked with one 8-byte read that its entry still holds the checkpoint) would turn both searches into one read each.

### The lock: decision

The rule: recommend a blocking lock with a timeout thread if the fairness spread exceeds 2× or the p99 lock wait exceeds 1 s. Against the polling lock's runs above:

- **Spread: not exceeded.** 1.11× and 1.10×.
- **p99 lock wait: exceeded.** About 2.8 s and 5.3 s (estimated from `meta.elapsed_ms`, whose p99 is 2.87 and 5.35 s; the commit's own work is 20–60 ms of it).
- **`E_BUSY` occurs.** 2 and 19 of 32,000 commits, every one after the 10 s timeout. The CLI contention test fails on it, as the store-level test does.

**Recommended: a blocking lock** (`File::lock` on a thread of its own, the caller waiting for it with the 10 s timeout and returning `Busy` when that passes. In a CLI process the blocked thread dies with the process; in a long-lived one, a thread that gets the lock after its caller gave up must release it at once).

- A blocking `flock` wakes a waiter the moment the lock is released, and so removes the 10 ms window in which every poller sleeps through the release. The store-level side experiment shows the result: no `Busy`, and a p99 wait of 0.3–0.8 s with writers 2 ms apart.
- CLI commits are further apart than 2 ms: each process spends its 20–60 ms of own work between two lock requests.
- What it will not change is throughput: the lock stays busy, so the mean wait stays about 0.5 s at 32 back-to-back committers on this disk. A fair lock spreads that wait over every commit, as a queue would, instead of piling it on a few. More commits per second need a shorter transaction under the lock: 7.5 ms alone, 15–27 ms here.

Task 7c applied it, with the parallel `Index::sync` and `flush`; "After 7c" below has the CLI-level result.

### After 7c: the kernel-queue lock

Task 7c's changes (the store-level section's "M1 store fixes (7c)") are merged under the CLI: `MetaLock` waits in the kernel's flock queue, a helper thread blocked in `flock` while the caller waits up to the 10 s timeout; `Index::sync` makes its five syncs at once; and `flush` its directory fsyncs at once. The same test, 32 × 1,000, `--release`, twice:

- run 1 on its own, with no other SwarmOps test running;
- run 2 inside `just perf`, right after the store-level contention test, with Task 8's multi-process tests running for about 2 of its 8.4 minutes (33 processes, from its start) and in four later 15 s samples (2–46 processes).

7c's raw `fdatasync` probe (the median of 40 appends) took 3.8 ms at the start and end of run 1 and 3.7 ms at the start of `just perf`: M0's speed, unlike the 6.6–15 ms of the 7c runs. Percentiles are nearest-rank over every commit. "In lock passes" divides a wait by the run's mean time per commit (wall / 32,000), the time the lock takes to pass once. The polling runs did not print the mean; 32 ÷ commits/s stands in for it. The polling maxima leave out the `E_BUSY` waits, each past 10 s.

| 32 × 1,000 commits | polling, run 1 | polling, run 2 | kernel queue, run 1 | kernel queue, run 2 |
|---|---|---|---|---|
| load at start (range during) | 16.1 (15.5–35.7) | 15.7 (15.1–36.0) | 16.9 (16.9–28.4) | 23.1 (18.6–38.7) |
| raw `fdatasync` p50, start / end | not taken | not taken | 3.83 / 3.79 ms | 3.72 / 3.75 ms, at the start and end of `just perf` |
| wall, commits/s | 500.7 s, 63.9/s | 867.2 s, 36.9/s | 458.9 s, 69.7/s | 502.6 s, 63.7/s |
| commit `meta.elapsed_ms` mean / p50 / p99 / max | (0.50 s) / 265 ms / 2.87 s / 10.02 s | (0.87 s) / 432 ms / 5.35 s / 10.00 s | 455 / 441 / 659 ms / 2.83 s | 498 / 450 ms / 1.22 s / 7.93 s |
| `E_BUSY` | 2 | 19 | **0** | **0** |
| child total time min / max (spread) | 453.0 / 500.7 s (1.11×) | 786.1 / 867.2 s (1.10×) | 454.9 / 458.9 s (**1.01×**) | 500.1 / 502.6 s (**1.01×**) |
| children with a commit over 1 s / over 5 s | 32 / 24 | 32 / 32 | 32 / 0 | 32 / 32 |
| lock wait, estimated, p50 / p99 / max | 235 ms / 2.84 s / 9.99 s | 374 ms / 5.29 s / 9.94 s | 420 / 638 ms / 2.81 s | 429 ms / 1.20 s / 7.91 s |
| the same in lock passes | 15 / 181 / 638 | 14 / 195 / 367 | 29 / 45 / 196 | 27 / 76 / 503 |
| uncontended commit afterwards, p50 / p99 | 30 / 54 ms | 58 / 110 ms | 21 / 30 ms | 21 / 54 ms |
| the whole test | 516.5 s | 886.1 s | 473.2 s | 517.4 s |
| test | fails (`E_BUSY`) | fails (`E_BUSY`) | passes | passes |

The two kernel-queue runs agree within 20 % on throughput, the median and the spread, and disagree on the tail, so both are shown: run 2's p99 and longest wait are about twice and three times run 1's, with Task 8's tests on the disk.

- **`E_BUSY` is gone**, from 2 and 19 in 32,000 commits to none.
- **The tail is cut.** p99 0.66 and 1.22 s, from 2.87 and 5.35 s. In lock passes, p99 45 and 76 from 181 and 195, and the longest wait 196 and 503 passes from 367–638 (and the `E_BUSY` waits past 10 s).
- **The median rose and the mean did not move.** The mean is fixed by the throughput (455 and 498 ms against 32 ÷ 69.7 = 459 and 32 ÷ 63.7 = 502 ms). The lock now rotates: a commit waits about one round of the other 31 writers (27–29 passes), where the poll lottery let most commits through in a fraction of a round and made the unlucky ones wait for seconds. So p50 is 441–450 ms, from 265–432 ms.
- **Every child finished within 1 % of the others** (spread 1.01×, from 1.10–1.11×).
- **Throughput is still the lock's.** 63.7–69.7 commits/s, 14.3–15.7 ms per lock pass, on a disk at M0's fsync speed: about four raw `fdatasync`s for a transaction that makes two. What the rest of a pass costs under 32 writers is not measured here. Whether the commits' own object writes set that pace is for the controller's quiet-window `just perf` to show: the store-level contention test in this `just perf` (70.9 transactions/s) overlapped Task 8's 33–57 processes for about 6 of its 7.5 minutes, so it does not compare.
- **The tail is bounded, not gone**, as 7c found at store level. Run 1's longest wait, alone on the host, was 196 passes (2.8 s); run 2's about 500 (7.9 s at 15.7 ms a pass), with Task 8's tests on the disk for part of it. On a disk a third slower, 500 passes reach the 10 s timeout, which is why both contention tests now run one after the other (`.config/nextest.toml`). The tail is to be re-measured in the controller's quiet-window `just perf`. An order fair to the commit needs a FIFO hand-off on top of `flock` (7c).
- **An uncontended commit took 21 ms p50 in both runs** (p99 30 and 54 ms), about 5.5 raw `fdatasync`s. The four rounds in a row (the objects, their directories, now at once, then `journal.log` and `journal.off`) make about 15 ms of it.

**The read snapshot after the merge.** The merged reader also checks the frame listed just before where it reads. `just perf`'s `read_snapshot` run on it (load 15–28 during the benches, the disk at M0's speed) gives the same numbers within 3 % with the checkpoint at the journal's end (100k: `read_changeset` p50 54 µs, p99 97 µs). With 31 frames past the checkpoint they are 5–6 % slower (100k: 73 and 126 µs), as `tail_frames` takes 12.7–13.1 µs p50 against 10.5–11.7 µs before. `off_search` is unchanged (4.0–5.2 µs), and so is the answer on caching it.

**Size.** The test took 473.2 s and 517.4 s at 32 × 1,000, under 10 minutes both times. So its default is the spec's 32 × 1,000 again, and `just check-slow` runs that. Under `just check-slow`, unoptimized, it first passed in 513.5 s beside the store-level contention test (499.1 s; load 22.6–36.2 during). `.config/nextest.toml` now runs the two one after the other: the suite then took 725.1 s, this test 449.3 s and the store-level one 274.3 s after it (load 16.9 at the start and 12.0–37.1 during, raw `fdatasync` 3.8 ms at both ends).

## M1 — `just perf` after all merges (2026-10-16, controller run)

Head `a4da77d` (every M1 task merged), `just perf` (store-level contention → CLI contention → store benches, one after another), start load 16.4 → end load 14.6 (one concurrent debug build of another worktree during the benches; no other 32-process test). Raw log: the SDD workspace (`just-perf-after-merges.log`). Nearest-rank percentiles.

| Measurement | Result |
|---|---|
| Store-level 32 × 1,000 `MetaStore::write()` | wall 264.8 s, **120.9 tx/s**, Busy 0; transaction p50 8.12 ms, p99 899 ms, max 1,211 ms; lock wait p50 0.11 ms, p99 891 ms, max 1,198 ms; spread 1.12×; uncontended transaction p50 7.24 ms, p99 21.7 ms |
| CLI 32 × 1,000 `swarmops commit` | 32,000 commits in 440.0 s, **72.7 commits/s**; `meta.elapsed_ms` p50 427, p90 498, p99 656, p99.9 739, max 918 ms; spread 1.01×; uncontended commit p50 21 ms (wall 23.5 ms); estimated lock wait p50 406, p99 635, max 897 ms |
| `fsync_fanout` | serial 18.0 ms, parallel 7.2 ms (2.5×), one file 3.5 ms; `index_sync/1` 11.7 ms vs `index_sync/5` 4.7 ms |
| `index_lookup` (p50 ns, criterion) | 10k: find 4.9 µs compacted / 5.8 µs with a 1,000 tail; 100k: 6.3 / 7.6 µs; 200k: 6.9 / 8.3 µs; `find_prefix` ≤ 10.7 µs; `find_delta_hash` ≤ 9.0 µs |
| `put_batch` 256 × 4 KiB + flush | 1 thread 974 ms (263 obj/s), 8 threads 231 ms, **32 threads 70.4 ms (3,636 obj/s, 13.8×)**; with a busy writer 1,516 / 301 / 97.9 ms (15.5×); `flush_only` 5.3 ms (32.1 ms busy) |
| `read_snapshot` (`read()` + `changeset(id)`, p50) | 10k: 47.7 µs (ckpt at end) / 67.4 µs (31 frames past); 100k: 53.8 / 75.0 µs; p99 ≤ 126 µs; `journal.off` search 4.0–5.2 µs |

**The lock pass, store against CLI.** At store level the lock passes every 8.27 ms (264.8 s / 32,000), about two raw `fdatasync`s, and an uncontended transaction takes 7.24 ms. The 3.8 ms probe is itself a 120-byte append and its `fdatasync`, so the inode size update that each journal append makes is already in it: growing the files is not the difference. Through the binary the lock passes every 13.75 ms (440.0 s / 32,000). The roughly 5.5 ms more appears only when the commits' own object writes run, outside the lock, on the same filesystem. Task 7b's "about 14 ms per pass at both levels" came from a store-level run that overlapped Task 8's processes, and is withdrawn. Whether the lock holder's two `fdatasync`s wait behind those object syncs on ext4's shared journal is not measured yet: the M1.5/M1.6 measurement of the lock holder under object syncs will record it in a section of its own.

Decisions confirmed on this run: the kernel-queue lock keeps `Busy` at 0 with every longest wait under 1.2 s, so `lock_timeout` stays at 10 s (FIFO hand-off remains an M6 experiment); parallel `Index::sync` and parallel flush directory fsyncs hold their 7c gains; 32-thread `put_batch` scales 13.8× (15.5× with a busy writer), so a `--durability fast` flag is not justified by this data. The CLI harness now writes each change list to a file (7b fix round); the 21 ms uncontended commit matches the earlier stdin-based harness within noise.

## M1.5 — native store vs embedded databases (2026-10-16, provisional)

`bench/compare/` measures the native metadata store (`swarmops_store::MetaStore`, through its public API) against SQLite, redb, fjall and sled on SwarmOps' own workloads (M1.5 Task 8): `just bench-compare`, or `just bench-compare --quick` for a smoke run of every workload at small sizes (87 s here once built, at load 9–10; the brief asks for under 2 minutes). It is a cargo workspace of its own, like `fuzz/`: `publish = false`, outside the root workspace, never built by `just check`. **SwarmOps depends on none of these databases**; they exist only in that harness, to measure the store the project chose to build instead. Its README gives the method in full.

**Every number in this section is provisional.** It was taken on the shared 32-core host while other agents built, tested and mutation-tested other worktrees, at the load and raw `fdatasync` latency printed beside it (M0 measured 4.1 ms for that `fdatasync` on a quiet disk). Heavy runs on the host were serialized by a lock, so no other benchmark ran beside these, but builds and test suites did. The controller re-runs `just bench-compare` at the plan's sizes in a quiet window before these numbers are published, and replaces the tables.

### Engines, durability and method

Every engine runs the same transaction on the same seeded data, in the layout a competent user of it would choose, with its defaults apart from durability. W1's transaction (which W2, W4 and W6 reuse) checks that the changeset is new, looks up its parent and computes its generation, stores it under its 32-byte id numbered in insertion order, indexes it by its 32-byte delta hash, moves `main` to it by compare-and-swap and records the move, and commits durably. The native store records the move in its journal frame; the others write a reflog row, as the M0 SQLite schema did. SQLite's schema mirrors the native index (rows in seq order, a unique index by id, an index by delta hash); redb, fjall and sled keep `changesets`, `by_delta_hash`, `refs`, `reflog` and `meta` (the counters).

| engine | version (exact pin) | a commit is durable when | syncs per commit, traced | processes per store |
|---|---|---|---|---|
| `native` | `swarmops-store` 0.1.0, this tree | its journal frame, then the frame's `journal.off` entry, are `fdatasync`'d, and the moved ref's file is replaced (temp file `fsync`'d, renamed, `refs/` `fsync`'d); the index and the checkpoint are synced every 32 frames | 4.23 | many |
| `sqlite` | `rusqlite` 0.40.2 (2026-08-08), SQLite 3.53.2 bundled | WAL, `synchronous=FULL`: the WAL `fsync`'d at every commit; `BEGIN IMMEDIATE`, `busy_timeout` 10 s | 1.02 | many |
| `sqlite-normal` | the same | WAL, `synchronous=NORMAL`: synced at checkpoints only, **not power-loss durable** | 0.018 | many |
| `redb` | `redb` 4.3.0 (2026-09-15) | `Durability::Immediate`: one `fdatasync` | 1.00 | one (threads) |
| `fjall` | `fjall` 3.1.12 (2026-10-03), `lsm-tree` 3.1.10 | single-writer transactions with `PersistMode::SyncAll`: the journal `fsync`'d | 1.00 | one (threads) |
| `sled` | `sled` 0.34.7 (2021-09-12), the last stable release; unmaintained since, its 1.0 rewrite still in alpha | `Db::flush` after every transaction: `sync_file_range`, which writes no metadata and does not flush the disk's cache, **weaker than the others** | 2.26 `sync_file_range` | one (threads) |

Every version was released at least 7 days before the harness was written, and so was every crate in its `Cargo.lock`, except `zstd` 0.14.2, which `swarmops-store` itself pins. sled builds cleanly on Rust 1.99, so it is measured, as a reference.

- **Equalized durability.** Every engine but two makes each commit durable before it returns: the native store, SQLite at `synchronous=FULL`, redb at `Durability::Immediate` and fjall at `PersistMode::SyncAll`. SQLite at `synchronous=NORMAL` syncs only at checkpoints, so a commit survives a process crash but not a power loss. sled's `flush` syncs with `sync_file_range`, which writes no metadata and does not flush the disk's cache. Those two are reference lines, never contenders.
- **Multi-process access.** Only the native store and SQLite let several processes open one store. redb, fjall and sled lock the database to one process, so W2 runs them on threads, and runs the native store and SQLite on threads too, which measures what the asymmetry is worth.
- **fjall's settled state.** fjall writes a memtable to a table file only once it holds 64 MiB, and drops a journal only when every keyspace has flushed past it. The small keyspaces (`meta`, `refs`, `reflog`) never fill a memtable, so a bulk-loaded store keeps every journal until 512 MiB of them force a flush (216 of the 1M store's 385 MiB). The `fjall` rows settle as the other engines do, by moving the log into the main structure as SQLite's checkpoint does: the journal is synced, then every keyspace's memtable is flushed into table files (`rotate_memtable_and_wait`, public but doc-hidden), and fjall deletes the journals no keyspace needs. The active journal stays: fjall rotates it only once it passes 64 MB at a flush, and every open replays it whole, flushed or not. So the flush shrinks the 1M store (W5) and leaves its open time (W4) as it was; a 100k store keeps its whole journal beside the new tables. The `fjall-unflushed` rows (W3–W5 only) show the store as the load left it.
- **Method.** Same host and filesystem as M0 (ext4 `data=ordered` on WSL2's virtual disk), stores in fresh directories under `~/.local/state/swarmops-bench`, warm page cache. Seeded, deterministic data with real BLAKE3 ids, in chains. Warm-up before timing (W1 200 transactions; W3 one untimed pass of every lookup kind; W6 one of the reader's). Each measurement runs 3 times, the sync count and W5's builds of each store included, the engines in a rotated order; every cell is the median over the runs with `[min–max]`. None of the open, first-lookup, first-write or commit timers includes a close (SQLite's open hands its connection to the first session: closing a WAL store's last connection checkpoints the WAL and deletes it); W4's `process ms` (a process from spawn to exit) and W5's `load s` do span one. Beside every row: the 1-minute load at the start of each run (and its range, sampled every 2 s) and a raw `fdatasync` probe (the median of 40 appends of 120 B, each `sync_data`'d) at the start and end of each run. No network.
- **Scale of these runs.** Two holds of the host's lock for heavy runs. The first, 25 minutes (05:53–06:18 UTC): the write workloads first (the sync count, the breakdown, W1, W2), then the seeded ones (W3–W6). A raw `fdatasync` probe taken once the lock was held (7.6 ms) set W1 to 1,000 timed transactions (the plan's 10,000 is a tenth) and W2 to 32 writers × 50 (the plan's × 1,000 is a twentieth), so that the hold stayed within an hour; W3–W6 ran at the plan's sizes (100k and 1M changesets, 20,000 lookups of each kind, 5 opens and 3 kills per run, 10 s of W6 writer). The disk then sped up to M0's 4 ms and the hold took 25 minutes. The second, 16 minutes (07:03–07:19 UTC), after the review's fixes (SQLite's open hands its connection to the first session; W5 builds every store three times; the sync count runs three times): `--quick`, then the sync count, W4 and W5 again on every engine at the plan's sizes, at load 9.1–13.6 (up to 29 during W5's builds) and a raw `fdatasync` of 3.7–6.7 ms (up to 18 during W5's builds). The sync, W4 and W5 tables come from the second hold, the others from the first. The plan's sizes stay the harness's default for the quiet-window re-run.

### Sync calls per commit: durability, checked

`strace` and `perf` are not installed on this host, so the harness counts system calls with a ptrace tracer of its own (`bench/compare/src/trace.rs`: about 230 lines of safe Rust over `nix`'s ptrace calls, x86_64 Linux only). A child process creates a store, commits 32 untraced W1 transactions and waits on its stdin. The tracer seizes every thread, follows the threads they create, and counts every system call while the child makes n more commits, closes the store and exits, naming each sync call's file through `/proc/<pid>/fd`. It runs twice, with n = 64 and n = 1,088; the difference over 1,024 is the cost of one commit, with the batched work averaged in (the native index sync every 32 frames and a compaction every 1,024 records, SQLite's checkpoint every 1,000 WAL pages) and the open, the warm-up and the close cancelled out. The pair runs three times, once per run, the engines in a rotated order. Per commit, the median [min–max] of the three counts, taken in the second hold (07:05–07:08 UTC, load 9.1–11.1, raw `fdatasync` 3.9 ms). Every count but sled's system calls came out the same three times, and the same as the first hold's single count (at load 27.8 falling to 14.8 and a raw `fdatasync` of 7.9 ms) but for SQLite's bytes written (24,031 against 24,039):

| engine | fsync | fdatasync | sync_file_range | all syncs | syscalls | bytes written | sync calls by file (≥ 0.01) |
|---|---|---|---|---|---|---|---|
| native | 2.07 | 2.17 | 0.00 | 4.23 | 157 | 470 | `fdatasync meta/journal.log` 1.00; `fdatasync meta/journal.off` 1.00; `fsync meta/refs/` 1.00; `fsync meta/refs/.tmp*` 1.00; `fdatasync meta/idx/dh.col` 0.032; `fdatasync meta/idx/gen.col` 0.032; `fdatasync meta/idx/ids.col` 0.032; `fdatasync meta/idx/par.col` 0.032; `fdatasync meta/parents.log` 0.032; `fsync meta/` 0.031; `fsync meta/.tmp*` 0.031 |
| sqlite | 1.02 | 0.00 | 0.00 | 1.02 | 18.8 | 24,031 | `fsync db.sqlite-wal` 1.01 |
| sqlite-normal | 0.018 | 0.00 | 0.00 | 0.018 | 17.8 | 24,031 | `fsync db.sqlite-wal` 0.012 |
| redb | 0.00 | 1.00 | 0.00 | 1.00 | 15.1 | 49,752 | `fdatasync db.redb` 1.00 |
| fjall | 1.00 | 0.00 | 0.00 | 1.00 | 2.01 | 494 | `fsync fjall/#.jnl` 1.00 |
| sled | 0.003 | 0.00 | 2.26 | 2.26 | 29.9 [29.4–30.1] | 1,047 | `sync_file_range sled/db` 2.26 |

- **Every durable contender makes one sync per commit**: SQLite an `fsync` of its WAL (1.02 with its checkpoints averaged in; this SQLite build has no `fdatasync`), redb one `fdatasync`, fjall one `fsync` of its journal.
- **The native store makes 4.23**, as storage-core §3 specifies ("2 journal syncs per transaction, + 2 per moved ref"; every W1 transaction moves `main`): the frame's `fdatasync` (the commit point), the `journal.off` entry's, the ref file's temp-file `fsync` and the `fsync` of `refs/`, plus the batched index sync (5 files every 32 frames: 0.16), the checkpoint's replacement (2 every 32 frames: 0.06) and the compaction's syncs (every 1,024 records, under the table's 0.01 cut-off).
- **Not durable as the others are:** sled's 2.26 `sync_file_range` calls per commit write neither the file's metadata nor the disk's cache, and SQLite at `NORMAL` syncs only at its checkpoints (0.018 per commit).
- The native store writes the fewest bytes per commit (470, against SQLite's 24,031 in whole WAL pages and redb's 49,752 in copy-on-write pages) and makes the most system calls (157: the lock file and the index's nine files are opened in every transaction, and the ref file gets a temp file of its own). Neither matters beside its syncs (below).

### Where a native commit's time goes: the journal syncs

`breakdown` runs W1's transaction on a native store whose filesystem is `RealFs` with every call timed (through the store's public `MetaStore::create_with_fs`): 1,024 commits per run after a 200-commit warm-up, so that one compaction falls among them, 3 runs. `RealFs::append_durable` (the journal's append + `fdatasync`) and `atomic_replace` of a ref file (temp file, `fsync`, rename, directory `fsync`) are done step by step with `RealFs`'s calls in `RealFs`'s order, so that each sync is timed on its own; a kind of call counts for the time its calls cover, overlaps once (the index sync's five `sync_data`s run at once). Means per commit, so the parts add up. Load 14.8/12.9/13.1 (12.2–14.8) during the runs, raw `fdatasync` 7.5–8.5 ms.

| part of the commit | calls per commit | ms per commit | share of the commit |
|---|---|---|---|
| ref file: fsync of the temp file | 1.00 | 8.20 [7.59–8.23] | 24.1 [24.0–24.5] % |
| fdatasync of journal.log (the commit point) | 1.00 | 8.08 [7.50–8.20] | 24.0 [23.8–24.1] % |
| fdatasync of journal.off | 1.00 | 7.86 [7.54–8.13] | 23.8 [23.5–23.9] % |
| ref file: fsync of refs/ | 1.00 | 7.75 [7.39–7.99] | 23.4 [23.1–23.4] % |
| checkpoint replace (every 32 frames) | 0.032 | 0.510 [0.481–0.529] | 1.52 [1.52–1.55] % |
| rest: lock, index open and lookups, encoding (outside filesystem calls) | – | 0.372 [0.364–0.372] | 1.11 [1.09–1.15] % |
| index sync, 5 files (every 32 frames) | 0.166 | 0.323 [0.319–0.331] | 0.987 [0.933–1.02] % |
| ref file: rename over refs/main | 1.00 | 0.106 [0.102–0.108] | 0.322 [0.310–0.322] % |
| index appends, 5 files (no sync) | 5.00 | 0.081 [0.077–0.081] | 0.241 [0.236–0.243] % |
| ref file: temp file created + written | 1.00 | 0.064 [0.062–0.067] | 0.196 [0.191–0.198] % |
| compaction (every 1,024 records) | 0.016 | 0.059 [0.057–0.062] | 0.181 [0.175–0.182] % |
| journal appends: open + write, 2 files (no sync) | 4.00 | 0.057 [0.056–0.058] | 0.170 [0.170–0.176] % |
| ref file reads | 2.97 | 0.025 [0.024–0.026] | 0.077 [0.074–0.077] % |
| file length checks | 4.00 | 0.011 [0.010–0.011] | 0.032 [0.031–0.034] % |
| **the whole commit** | | 33.5 [31.6–34.2] | 100 % |

- **95 % of a native commit is its four syncs, about a quarter each**: `journal.log`'s `fdatasync` 24.0 % [23.8–24.1], `journal.off`'s 23.8 % [23.5–23.9], the ref temp file's `fsync` 24.1 % and the `fsync` of `refs/` 23.4 %, each about one raw `fdatasync` (7.5–8.5 ms in these runs). The batched work takes 2.7 % (the checkpoint 1.5 %, the index sync 1.0 %, the compaction 0.2 %), and everything else (the lock, opening the index, the lookups, the encoding, the appends) about 2 %.
- **The second journal sync is a quarter of a commit, not a half.** An independent code review suggested that the `fdatasync` of `journal.off` may be droppable. Without it, a W1 commit would take about 25.5 ms instead of 33.5 ms: 1.3× the commits/s. The ref file's replacement costs as much as both journal syncs together (47.5 %), although storage-core §2 rule 3 calls the ref files, like the index, a projection of the journal that replay restores from the frames past the checkpoint, and the index's syncs are batched every 32 frames. A commit with one sync, where the other engines are, needs both gone. Whether either can go is a durability question for the spec (what a reader and the tail check may trust after a power loss), which this measurement does not settle; it prices the options.

### W1: one writer, durable commits

W1's transaction back to back, 1,000 timed commits after 200 untimed (a tenth of the plan's 10,000: see the run's scale above). Bytes per transaction: the closed store's allocated size over the 1,200 commits.

| engine | commits/s | p50 ms | p99 ms | max ms | bytes/txn on disk | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|
| native | 29.9 [29.6–31.2] | 31.5 [30.4–33.1] | 62.4 [60.8–63.4] | 167 [154–188] | 437 | 12.2/13.8/13.3 (11.6–14.4) | 7.6–8.3 ms |
| sqlite | 135 [129–161] | 7.93 [6.82–8.31] | 16.6 [12.4–17.0] | 22.3 [19.2–26.5] | 311 [311–314] | 12.8/15.1/13.4 (12.8–15.1) | 7.0–8.5 ms |
| sqlite-normal | 5,978 [5,366–6,114] | 0.028 [0.028–0.029] | 0.333 [0.238–0.342] | 20.5 [20.1–23.6] | 314 [311–314] | 14.6/14.4/13.6 (13.6–14.6) | 7.4–8.5 ms |
| redb | 218 [158–231] | 4.07 [3.94–5.44] | 9.41 [8.76–16.7] | 14.5 [11.4–18.4] | 457 | 14.6/14.4/13.6 (13.1–15.4) | 7.6–16.1 ms |
| fjall | 144 [138–158] | 7.40 [7.05–7.83] | 11.6 [9.53–13.6] | 18.1 [15.3–19.7] | 573 | 15.8/14.2/13.1 (13.1–15.8) | 7.3–16.1 ms |
| sled | 1,442 [1,370–1,622] | 0.612 [0.552–0.664] | 1.56 [1.41–2.91] | 5.17 [4.67–7.35] | 1,072 | 15.1/13.8/13.3 (13.3–15.1) | 7.4–8.2 ms |

**Verdict: the native store loses, with 4.5–7.3× fewer commits/s than the durable engines** (29.9 against SQLite's 135, fjall's 144 and redb's 218). Its p50 of 31.5 ms is four raw `fdatasync`s (7.6–8.3 ms): the frame per transaction with two journal `fdatasync`s, plus the ref file's atomic replacement with two more, where every other durable engine makes one sync per commit. redb's commit costs about half of one raw append + `fdatasync` (4.1 against about 7.6 ms), most likely because redb rewrites pages inside a file that does not grow at every commit, so its `fdatasync` has no new file size to log, while every native sync follows an append or a newly created file. sled (1,442/s) and SQLite at `NORMAL` (5,978/s) return before the commit is on the disk. On disk, the native store takes 437 bytes per transaction, between SQLite (311) and redb (457).

### W2: 32 writers

32 writers × 50 transactions each (a twentieth of the plan's 1,000: see the run's scale above), every writer moving `main` with a compare-and-swap against its value inside the transaction. Processes for the native store and SQLite, threads for the others; the native store and SQLite on threads as well. Latency includes `Busy` retries (a writer that gets `Busy` after the 10 s timeout tries again); spread is the slowest writer's total time over the fastest's. No writer failed, and sled reran no transaction.

| engine | mode | txns/s | p50 ms | p99 ms | max ms | Busy | failed | spread | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|---|
| native | processes | 31.1 [26.9–59.3] | 1,049 [578–1,190] | 1,196 [694–1,823] | 1,240 [707–2,094] | 0.00 | 0.00 | 1.12 [1.11–1.17] | 13.2/17.6/13.3 (12.4–17.6) | 3.9–8.4 ms |
| sqlite | processes | 137 [42.8–247] | 6.75 [2.52–29.5] | 5,269 [3,139–10,596] | 11,354 [6,343–18,028] | 5.00 [0.00–15.0] | 0.00 | 29.7 [26.1–59.8] | 16.6/26.0/12.9 (12.3–35.4) | 4.0–14.4 ms |
| sqlite-normal | processes | 657 [657–739] | 0.027 [0.025–0.035] | 831 [633–831] | 2,433 [2,133–2,433] | 0.00 | 0.00 | 218 [111–370] | 31.8/21.1/13.6 (13.1–31.8) | 3.9–8.6 ms |
| native | threads | 53.1 [30.6–59.2] | 594 [580–1,046] | 1,238 [677–1,805] | 1,286 [694–1,876] | 0.00 | 0.00 | 1.12 [1.11–1.12] | 13.2/15.3/12.8 (12.8–16.6) | 3.9–8.4 ms |
| sqlite | threads | 128 [83.5–239] | 7.17 [2.62–9.03] | 5,667 [3,244–8,299] | 12,154 [6,544–17,077] | 6.00 [0.00–7.00] | 0.00 | 27.2 [18.7–30.2] | 34.6/22.9/12.3 (12.3–34.6) | 3.9–14.3 ms |
| sqlite-normal | threads | 716 [685–872] | 0.029 [0.027–0.034] | 631 [330–931] | 2,233 [1,832–2,334] | 0.00 | 0.00 | 197 [162–276] | 31.8/20.3/13.1 (13.1–31.8) | 4.0–8.6 ms |
| redb | threads | 213 [202–414] | 4.20 [2.13–4.44] | 2,945 [1,808–3,157] | 5,676 [3,435–6,684] | 0.00 | 0.00 | 12.5 [11.1–18.3] | 30.8/20.3/13.1 (13.1–30.8) | 3.9–8.5 ms |
| fjall | threads | 148 [142–391] | 7.54 [2.38–7.69] | 4,662 [1,865–4,881] | 10,162 [3,626–10,559] | 0.00 | 0.00 | 33.5 [19.0–33.9] | 28.7/18.9/13.3 (13.3–28.7) | 3.9–8.8 ms |
| sled | threads | 1,953 [1,896–1,999] | 17.9 [17.7–18.4] | 28.7 [27.7–28.9] | 35.8 [33.3–41.4] | 0.00 | 0.00 | 1.13 [1.11–1.19] | 26.0/17.6/13.3 (13.3–26.0) | 4.0–8.0 ms |

**Verdict: the native store loses on throughput and wins on fairness.**

- **Throughput.** Every engine serializes its writers, and none batches several commits into one sync, so W2 runs at W1's pace: the native store at 26.9–59.3 transactions/s, about four raw `fdatasync`s a commit, against SQLite's 42.8–247 and redb's 202–414. The ranges are the disk's: the raw `fdatasync` probe fell from about 8 ms in runs 1–2 to 4 ms in run 3, and every engine's throughput about doubled with it (the native store's from 27–31 to 59/s).
- **Fairness.** The native store's writers wait in the kernel's flock queue (`MetaLock`, M1 Task 7c): spread 1.11–1.17 in all six runs, no `Busy`, and the longest wait 0.69–2.1 s, one to two rotations of the 31 other writers (p50 1,049 ms ≈ 31 × 33.5 ms). SQLite's busy handler polls, with sleeps that grow to 100 ms: spread 18.7–59.9, p99 3.1–10.6 s, longest waits 6.3–18.0 s, and up to 15 `SQLITE_BUSY` per run after the 10 s timeout, on processes and threads alike. redb's and fjall's in-process writer locks (a condition variable in redb, a mutex in fjall), neither first-come-first-served: spread 11–34, longest waits 3.4–10.6 s. sled's transactions are short and fair (spread 1.13), at a durability the others do not have.
- **The asymmetry.** The native store runs the same on threads as on processes (spread 1.11–1.12 either way; every acquire flocks a file description of its own, so threads queue as processes do), and so does SQLite. redb, fjall and sled would need a server process in front of them to take SwarmOps' 32 agent processes.

### W3: lookups, warm

Bulk-loaded stores of 100,000 and 1,000,000 changesets (one chain, in transactions of 50,000, then settled and closed), opened in the measuring process; 20,000 lookups of each kind, after one untimed pass of every kind. On one open snapshot: by id (`get`), by the first 6 bytes of the id (`prefix`), by delta hash through the secondary index (`delta`), and an id the store does not hold (`miss`, what every insert asks first); and by id in a fresh read transaction each (`fresh`, what a CLI command pays). Every answer is checked. µs:

| engine | size | get p50 | get p99 | prefix p50 | prefix p99 | delta p50 | delta p99 | miss p50 | fresh p50 | fresh p99 | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| native | 100k | 10.8 [10.7–10.8] | 33.3 [31.8–34.0] | 10.4 [9.69–13.4] | 26.4 [26.3–42.2] | 11.8 [11.5–12.1] | 42.1 [25.8–46.9] | 7.26 [6.95–7.36] | 54.3 [52.0–68.6] | 176 [161–213] | 10.8/10.8/10.9 (10.8–10.9) | 3.9–4.0 ms |
| sqlite | 100k | 3.22 [3.01–3.67] | 10.8 [7.06–11.4] | 1.39 [1.36–1.50] | 4.35 [4.29–4.96] | 3.30 [2.88–3.71] | 8.44 [7.19–12.7] | 1.33 [1.14–1.43] | 4.63 [4.47–4.97] | 11.7 [10.6–19.0] | 10.9/10.6/10.9 (10.4–10.9) | 3.8–4.1 ms |
| redb | 100k | 0.441 [0.401–0.932] | 1.83 [1.21–2.35] | 0.531 [0.491–0.651] | 1.24 [0.762–1.87] | 0.752 [0.662–0.772] | 2.25 [2.01–2.67] | 0.390 [0.341–0.511] | 0.961 [0.852–1.00] | 2.79 [1.88–2.91] | 10.9/10.6/10.9 (10.6–10.9) | 3.9–4.0 ms |
| fjall | 100k | 1.02 [0.801–1.34] | 3.00 [2.25–5.78] | 4.25 [3.21–5.83] | 10.9 [7.88–19.8] | 4.61 [4.41–6.16] | 14.2 [13.2–25.7] | 18.2 [17.9–18.9] | 1.25 [1.04–1.56] | 4.21 [3.31–4.74] | 11.0/10.6/10.9 (10.6–11.0) | 3.9–4.0 ms |
| fjall-unflushed | 100k | 0.862 [0.551–1.10] | 3.32 [1.00–3.40] | 1.11 [1.07–1.53] | 2.88 [2.65–5.12] | 1.37 [1.29–1.75] | 4.29 [3.82–6.82] | 0.772 [0.691–1.04] | 0.852 [0.762–0.941] | 3.05 [1.80–3.60] | 11.0/10.6/10.9 (10.6–11.0) | 3.8–3.9 ms |
| sled | 100k | 1.07 [0.982–1.26] | 5.09 [4.49–5.11] | 2.14 [2.09–2.28] | 7.55 [7.16–8.17] | 2.49 [2.38–2.84] | 10.5 [9.68–11.4] | 1.19 [0.942–1.42] | 1.28 [1.18–1.73] | 6.12 [5.40–9.54] | 11.0/10.6/10.9 (10.6–11.0) | 3.9–4.1 ms |
| native | 1M | 15.4 [14.0–15.6] | 60.4 [32.3–67.3] | 18.0 [17.6–18.2] | 71.9 [65.7–80.2] | 16.5 [16.4–16.5] | 60.6 [58.4–70.0] | 10.5 [10.3–10.6] | 64.9 [61.1–67.1] | 237 [199–241] | 10.4/9.6/9.5 (9.2–10.4) | 4.0–4.6 ms |
| sqlite | 1M | 4.55 [4.48–5.83] | 15.9 [14.8–31.1] | 2.38 [2.32–2.77] | 8.31 [7.54–10.6] | 4.95 [4.49–5.27] | 15.5 [15.5–17.8] | 2.58 [1.93–2.61] | 6.34 [5.60–7.09] | 25.3 [13.4–25.3] | 10.0/9.8/9.3 (9.3–10.0) | 3.9–4.1 ms |
| redb | 1M | 1.51 [1.03–1.69] | 3.68 [1.90–6.08] | 1.56 [1.18–1.60] | 4.44 [3.62–4.64] | 1.54 [1.36–1.65] | 3.84 [2.69–4.99] | 1.28 [1.13–1.47] | 1.76 [1.45–2.56] | 8.04 [3.99–8.35] | 10.0/9.8/9.2 (9.2–10.0) | 3.9–4.3 ms |
| fjall | 1M | 20.6 [18.6–21.2] | 370 [349–375] | 58.2 [55.7–59.1] | 253 [252–255] | 49.9 [47.5–52.1] | 224 [219–240] | 70.2 [63.8–71.8] | 22.0 [21.8–23.8] | 357 [348–503] | 10.0/9.8/9.2 (9.0–10.3) | 3.9–4.1 ms |
| fjall-unflushed | 1M | 17.5 [16.8–20.6] | 344 [335–400] | 49.0 [48.1–51.3] | 205 [191–231] | 48.0 [46.8–48.5] | 201 [196–203] | 69.5 [67.4–71.9] | 23.2 [22.1–23.8] | 402 [363–432] | 10.3/10.0/10.1 (9.5–10.3) | 3.8–4.3 ms |
| sled | 1M | 2.33 [2.15–2.45] | 15.6 [12.9–16.3] | 3.56 [3.34–3.95] | 19.3 [19.1–20.2] | 3.59 [3.25–3.74] | 25.4 [21.0–27.1] | 2.76 [2.25–2.95] | 2.24 [2.03–2.78] | 15.3 [11.5–22.9] | 10.2/9.8/9.5 (9.5–10.2) | 3.9–4.2 ms |

The native store's p50 over each engine's, kind by kind: the median over the three runs of each, as the harness now prints it under each W3 table so that a verdict starts from every kind (`report::ratios`), here computed from the records of the table above. Above 1, the native store is slower:

| size | native ÷ | get | prefix | delta | miss | fresh | range |
|---|---|---|---|---|---|---|---|
| 100k | sqlite | 3.35× | 7.45× | 3.57× | 5.45× | 11.7× | 3.35–11.7× |
| 100k | redb | 24.4× | 19.5× | 15.7× | 18.6× | 56.5× | 15.7–56.5× |
| 100k | fjall | 10.5× | 2.44× | 2.56× | 0.399× | 43.4× | 0.399–43.4× |
| 100k | fjall-unflushed | 12.5× | 9.33× | 8.58× | 9.41× | 63.8× | 8.58–63.8× |
| 100k | sled | 10.1× | 4.83× | 4.72× | 6.09× | 42.4× | 4.72–42.4× |
| 1M | sqlite | 3.38× | 7.59× | 3.34× | 4.05× | 10.2× | 3.34–10.2× |
| 1M | redb | 10.2× | 11.5× | 10.7× | 8.16× | 36.8× | 8.16–36.8× |
| 1M | fjall | 0.748× | 0.310× | 0.331× | 0.149× | 2.95× | 0.149–2.95× |
| 1M | fjall-unflushed | 0.878× | 0.368× | 0.344× | 0.150× | 2.80× | 0.150–2.80× |
| 1M | sled | 6.58× | 5.07× | 4.61× | 3.80× | 28.9× | 3.80–28.9× |

**Verdict: the native store loses every lookup kind to SQLite and to redb, by 3.3–12× against SQLite and 8.2–57× against redb, the fresh snapshot by the most; it beats fjall at 1M on every kind but the fresh snapshot.**

- **Against SQLite.** A `get` or a delta-hash lookup is 3.3–3.6× slower, a miss 4.1–5.5×, a prefix lookup 7.5–7.6× (18.0 µs p50 against 2.38 at 1M) and a fresh snapshot and lookup 10–12× (64.9 µs against 6.34 at 1M).
- **Against redb.** 16–57× at 100k (a `get` 10.8 µs against 0.441, a fresh snapshot 54.3 against 0.961) and 8.2–37× at 1M (a `get` 15.4 against 1.51, a fresh snapshot 64.9 against 1.76).
- **Against fjall.** At 1M the native store is the faster on four kinds (0.15–0.88× fjall's p50: fjall's `get` takes 20.6 µs, its prefix lookup 58.2) and the slower on the fresh snapshot (2.8–3.0×). At 100k it is the slower on every kind but the miss, where fjall's 18.2 µs is the slowest of all (the native store's 7.26); so at 100k the native store is the slowest engine at every kind but the miss.
- **Why.** A native lookup binary-searches a sorted view's key file with one `pread` per probe (about 20 at 1M), then reads the record's columns with four more. It keeps no cache of its own, nor maps the files (`unsafe` is forbidden), where redb and sled answer from a 1 GB in-process cache and SQLite from its page cache. And a native snapshot opens the index's nine files and reads the checkpoint, the journal tail and the ref files each time, because readers take no lock and share nothing with writers (the lock-free reader).
- **What the columnar index buys.** A prefix or delta-hash lookup costs about what a point lookup does (each kind has its sorted view), and a miss less. Going from 100k to 1M adds 40–44 % to a `get`, a delta-hash lookup and a miss (about three more probes), 74 % to a prefix lookup and 19 % to a fresh snapshot. fjall's lookups grow 3.9–20× over the same step (`get` 20×, fresh 18×, prefix 14×, delta 11×, miss 3.9×), past the native store's on every kind but the fresh snapshot: its 32 MiB block cache holds a sixth of its 1M tables (192 MiB).
- Every native lookup stays under 72 µs p99, 0.24 ms with a fresh snapshot, four times inside the spec's 1 ms read target. The gap matters to a long-lived process that takes many snapshots, not to a CLI command that takes one or two.

### W4: opening a 1M store, and recovery after a kill

Open: a fresh process opens the 1M store (warm: the files are in the page cache) and looks one changeset up, 5 times per run. Recovery: a writer process commits W1's transaction in a loop on the 1M store and is killed with SIGKILL 0.1–1 s after its first acknowledged commit (a seeded draw), most likely inside a commit; a fresh process then opens the store and commits one transaction (where most engines repair what the kill left), 3 kills per run. A killed process loses nothing the kernel holds, so this is recovery from a process crash, not from a power loss. None of the open, first-lookup or first-write timers includes a close: SQLite's open hands its connection to the first session, and the lookup's session is closed after the timing (`process ms`, the opening process from spawn to exit, spans its close). Every acknowledged commit was present after every kill, on every engine. ms, from the second hold:

| engine | open ms | first lookup ms | open + lookup ms | process ms | after a kill: open ms | first write ms | open + write ms | worst ms | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|---|
| native | 0.024 [0.022–0.046] | 0.084 [0.075–0.116] | 0.108 [0.097–0.162] | 1.40 [0.997–1.47] | 0.026 [0.026–0.031] | 17.2 [15.6–192] | 17.2 [15.6–192] | 24.1 [23.4–301] | 9.8/10.8/11.0/11.0/10.4/13.6 (9.8–13.6) | 3.8–4.3 ms |
| sqlite | 0.336 [0.240–0.362] | 0.041 [0.026–0.044] | 0.377 [0.266–0.406] | 1.41 [1.30–1.56] | 1.10 [0.612–1.18] | 4.21 [4.00–4.43] | 5.14 [5.08–5.35] | 6.36 [5.34–6.63] | 9.8/10.3/11.0/11.0/10.0/13.6 (9.8–13.7) | 3.9–4.1 ms |
| redb | 2.17 [1.89–2.71] | 0.058 [0.052–0.061] | 2.22 [1.94–2.77] | 10.3 [10.2–12.0] | 346 [330–362] | 2.21 [2.11–2.25] | 348 [333–364] | 361 [350–369] | 9.8/10.3/10.8/10.7/10.1/10.4 (9.8–10.8) | 3.9–4.6 ms |
| fjall | 545 [529–558] | 0.056 [0.055–0.083] | 545 [529–558] | 598 [594–609] | 545 [544–570] | 4.27 [4.21–5.02] | 549 [549–574] | 564 [549–622] | 9.8/10.3/10.8/10.7/10.2/10.8 (9.8–10.9) | 3.7–4.1 ms |
| fjall-unflushed | 554 [529–603] | 0.070 [0.069–0.072] | 554 [529–603] | 622 [615–656] | 574 [533–625] | 5.05 [4.12–6.84] | 579 [537–630] | 608 [557–745] | 9.8/10.5/10.9/10.4/10.5/10.9 (9.8–12.6) | 3.8–6.7 ms |
| sled | 176 [162–189] | 0.100 [0.094–0.122] | 176 [162–189] | 246 [225–283] | 195 [195–210] | 1.78 [1.58–3.70] | 200 [197–212] | 200 [199–224] | 10.3/10.8/10.9/10.0/10.7/12.6 (10.0–13.6) | 3.8–5.8 ms |

**Verdict: the native store opens fastest by far; it loses recovery to SQLite, 3.3× slower at the median and 45× at worst, and recovers 12–32× faster than sled, redb and fjall.**

- **Open.** 0.11 ms to open the 1M store and answer a first lookup, against SQLite's 0.38 ms (3.5×), redb's 2.22 ms, sled's 176 ms and fjall's 545 ms. The native open reads nothing; the first snapshot reads the checkpoint, the index's files and at most 31 frames past the checkpoint, because the index is synced and the checkpoint moved every 32 frames (the batched index sync). SQLite's open makes its first connection, which creates the WAL and its index again (a clean close deleted them); its first lookup is the fastest of all (0.041 ms). fjall replays its active journal (43 MB here) into its memtables at every open, flushed or not: the unflushed store, with 216 MiB of journals, opens no slower (554 ms).
- **Recovery.** 17.2 ms [15.6–192] for the next process's open and first write, against SQLite's 5.14 [5.08–5.35] (3.3×), sled's 200, redb's 348 (without quick-repair, its default, an open after a crash walks the database to rebuild the allocator state) and fjall's 549 (its journal replay). SQLite's open after a kill rebuilds the WAL's index from the WAL (1.10 ms: a read, no sync), and its first write is one ordinary commit appended to that WAL: an `fsync` of the WAL, which also `fsync`s the directory, as every connection's first sync does (4.21 ms, about one raw `fdatasync`). The native open reads nothing (0.026 ms), and its first write is an ordinary commit too: its four syncs (17.2 ms), with the tail repair and the replay of at most 31 frames costing next to nothing.
- **The worst case.** 301 ms for the native store against SQLite's 6.63 ms (45×). Run 2's median native kill took 192 ms and its worst 301 ms, as in the first hold, where run 2's kills (with the same seeded delays) took 261 and 310 ms and run 3's worst 190 ms: most likely kills that left a torn or unlisted frame. Before it cuts or lists one, the next writer checks every listed frame (storage-core §2 rule 3; `check_listed_chain`), and at 1M that reads the whole 101 MiB journal. The check grows with the journal, and only a crash's repair pays for it.
- **The first hold's tie was the harness's.** Its SQLite rows (open + lookup 0.679 ms, recovery 23.7 ms) came from an open that made a connection and closed it. Closing a WAL store's last connection checkpoints the WAL into the database, with a sync of each, and deletes the WAL: after a kill, the "open" of 15.1 ms was mostly that checkpoint, of the killed writer's 113–219 commits. The first session's new connection then started a new WAL, whose first commit also syncs its header (8.55 ms). With the open's connection handed to the first session, SQLite's recovery fell to 5.14 ms and its open + lookup to 0.38 ms.

### W5: size on disk

The bulk-loaded stores, settled (native: `flush`; SQLite: `wal_checkpoint(TRUNCATE)`; redb: nothing to do; fjall: memtables flushed, `fjall-unflushed`: the journal synced only; sled: `flush`) and closed; allocated bytes, as `du` counts them. Each store was built three times, a run each, in the second hold (the first build is the store W3, W4 and W6 read). SQLite and redb came out the same to the byte in all three builds and in the first hold's, the native store within three 4 KiB blocks, both fjall rows within one (fjall's 1M store holds 35 or 36 files), and sled 0.7 % apart at 100k and 4.6 % at 1M (525–549 MiB). Log: the native journal (`journal.{log,off,ckpt}`), SQLite's WAL, fjall's journals; redb has none, and sled's log is its data file.

| engine | size | MiB | bytes/record | of which log MiB | apparent MiB | files | load s | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|
| native | 100k | 28.5 | 298 | 10.1 | 28.4 | 14.0 | 4.05 [3.36–4.22] | 11.2/17.0/10.4 (10.4–17.0) | 3.8–4.0 ms |
| sqlite | 100k | 19.5 | 205 | 0.00 | 19.5 | 1.00 | 1.16 [0.871–1.44] | 10.0/9.8/9.4 (9.4–10.1) | 3.8–4.1 ms |
| redb | 100k | 39.9 | 418 | 0.00 | 64.3 | 1.00 | 0.725 [0.516–0.795] | 9.4/10.7/13.7 (9.4–13.7) | 3.8–4.2 ms |
| fjall | 100k | 40.9 | 429 | 21.6 | 83.3 | 32.0 | 2.62 [1.77–3.47] | 9.4/13.6/13.4 (9.4–13.9) | 3.9–4.2 ms |
| fjall-unflushed | 100k | 21.6 | 227 | 21.6 | 64.0 | 24.0 | 1.39 [1.30–1.96] | 9.6/29.1/12.6 (9.6–29.1) | 3.9–12.8 ms |
| sled | 100k | 64.4 [64.2–64.6] | 675 [673–678] | 0.00 | 64.4 [64.2–64.6] | 2.00 | 2.60 [2.49–2.62] | 10.0/23.6/11.3 (10.0–23.6) | 3.8–4.1 ms |
| native | 1M | 284 | 298 | 101 | 284 | 14.0 | 45.6 [41.0–46.2] | 10.9/17.0/10.5 (9.4–17.0) | 3.9–4.2 ms |
| sqlite | 1M | 197 | 206 | 0.00 | 197 | 1.00 | 21.6 [18.4–21.8] | 10.0/10.1/9.4 (9.4–10.7) | 3.8–4.0 ms |
| redb | 1M | 460 | 483 | 0.00 | 514 | 1.00 | 15.4 [14.9–18.5] | 9.4/10.7/13.7 (9.3–13.7) | 3.9–4.2 ms |
| fjall | 1M | 235 | 247 | 43.1 | 256 | 35.0 [35.0–36.0] | 19.5 [18.5–26.1] | 9.7/13.9/13.3 (9.3–29.3) | 3.8–18.1 ms |
| fjall-unflushed | 1M | 385 | 403 | 216 | 406 | 31.0 | 18.5 [17.9–20.0] | 10.0/29.1/12.6 (10.0–29.1) | 3.8–9.8 ms |
| sled | 1M | 532 [525–549] | 558 [550–576] | 0.00 | 532 [525–549] | 2.00 | 47.8 [46.1–50.2] | 10.2/22.7/11.3 (9.2–22.7) | 3.8–4.1 ms |

**Verdict: the native store sits in the middle: 284 MiB for 1M changesets, 298 bytes each, against SQLite's 197 MiB (206 bytes), fjall's 235 after its flush (247), redb's 460 (483) and sled's 532 (558).** A third of it is the journal (101 MiB), which the native store keeps whole as the record of truth, the index being a projection of it that replay can rebuild (storage-core §2). The rest is the columnar index, which stores each id and each delta hash twice more, in its column and in its sorted view: 112 bytes of columns and parents and 80 of sorted views per record, the same per record at 100k. SQLite's B-trees hold each key once per index; redb's copy-on-write file keeps free pages (514 MiB apparent, 460 allocated); fjall's active journal is a fifth of its flushed 1M store, and at 100k all of its journal stays beside the new tables (40.9 MiB, against 21.6 MiB unflushed, all journal).

### W6: a reader while a writer commits

On a copy of the 100k store: a reader looks a changeset up by id in a fresh read transaction each time, 20,000 times idle, then for 10 s while a writer commits W1's transaction back to back. Reader and writer are two threads of one process, each with a session of its own (SQLite: two connections), for every engine. µs:

| engine | idle p50 | idle p99 | busy p50 | busy p99 | busy p99.9 | busy max | writer commits/s | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|
| native | 53.8 [53.8–58.2] | 186 [175–193] | 90.8 [90.0–93.6] | 322 [321–341] | 1,335 [1,321–1,559] | 6,715 [6,566–8,140] | 60.5 [60.2–60.8] | 10.6/10.9/11.1 (10.6–11.3) | 4.1–4.2 ms |
| sqlite | 5.30 [4.75–5.53] | 14.4 [10.8–16.0] | 3.51 [3.48–3.59] | 17.1 [16.4–20.7] | 129 [128–133] | 943 [890–1,657] | 345 [314–346] | 10.8/11.2/11.0 (10.6–11.5) | 3.9–4.1 ms |
| sqlite-normal | 4.93 [4.67–5.16] | 17.2 [13.4–18.5] | 5.59 [5.46–5.61] | 61.0 [55.7–62.9] | 177 [167–190] | 1,095 [952–2,504] | 4,089 [4,063–4,145] | 11.1/10.6/11.0 (10.6–11.1) | 3.9–4.2 ms |
| redb | 1.39 [1.07–1.47] | 3.78 [2.83–4.15] | 1.28 [1.22–1.33] | 3.83 [3.75–3.93] | 44.3 [38.7–48.8] | 1,358 [749–2,479] | 447 [442–449] | 10.9/11.0/10.7 (10.4–11.2) | 3.8–4.4 ms |
| fjall | 1.15 [1.12–1.23] | 3.82 [3.69–3.94] | 0.982 [0.922–0.991] | 3.69 [3.60–3.73] | 27.4 [25.7–28.0] | 1,013 [393–1,580] | 240 [229–243] | 11.1/11.1/10.4 (10.4–11.5) | 3.9–4.3 ms |
| sled | 1.61 [0.701–1.61] | 8.19 [2.60–8.21] | 1.66 [1.62–1.74] | 11.7 [10.9–13.1] | 165 [161–168] | 1,973 [946–4,459] | 1,587 [1,543–1,612] | 11.5/11.3/11.1 (10.7–11.7) | 3.9–4.3 ms |

**Verdict: the native reader is never blocked, and stays the slowest.** Idle, a fresh snapshot and lookup takes 53.8 µs p50 (SQLite 5.30, redb 1.39, fjall 1.15, sled 1.61). Beside the writer's 60 commits/s it takes 90.8 µs p50, 322 µs p99, 1.3 ms p99.9 and 6.7 ms at worst. Readers take no lock, so the writer never stalls them; what the writer adds is work per snapshot: the frames past the checkpoint (up to 31) are read and overlaid on every snapshot, and the ref files and the frames are read again when a commit lands meanwhile. The other engines' readers keep their idle p50 beside their writers (SQLite's WAL readers 3.51 µs, redb's 1.28, fjall's 0.98, sled's 1.66), with worst cases of 0.9–2.0 ms.

### The verdicts in one place

Provisional, at the loads and raw `fdatasync` latencies in the tables above (load 9–35, `fdatasync` 3.7–18 ms).

| workload | the native store against the best durable alternative | the design choice behind it |
|---|---|---|
| W1, one writer | **loses**: 29.9 commits/s against redb's 218 (7.3×) and SQLite's 135 (4.5×) | a frame per transaction with two `fdatasync`s, plus the moved ref's file replaced with two more syncs: 4.23 syncs per commit where the others make one |
| W2, 32 writers | **loses throughput** (27–59/s against redb's 202–414), **wins fairness**: spread 1.11–1.17 against 11–60, no `Busy` against SQLite's up to 15, longest wait ≤ 2.1 s against 3.4–18 s | the same four syncs; the flock writer lock's kernel queue |
| W3, lookups | **loses every kind**: 3.3–12× SQLite's p50 and 8.2–57× redb's, by kind and size (prefix 7.5–7.6× and a fresh snapshot 10–12× SQLite's; `get` 3.4× SQLite's and 10× redb's at 1M); beats fjall at 1M but for the fresh snapshot | the columnar index read with one `pread` per probe and no cache; lock-free readers that open the index for every snapshot |
| W4, open and recovery | **wins the open**: 0.11 ms against SQLite's 0.38 (3.5×) and fjall's 545; **loses recovery** to SQLite: 17.2 ms against 5.14 at the median (3.3×), 301 against 6.63 ms at worst (45×), 12–32× faster than sled, redb and fjall | the batched index sync and a checkpoint every 32 frames bound what an open or a recovery replays; the recovering write is a four-sync commit, and a repair after a torn frame checks every listed frame |
| W5, size | **middle**: 284 MiB for 1M against SQLite's 197 (1.44×), ahead of redb's 460 and sled's 532 | the journal kept whole beside a columnar index that stores each key twice more |
| W6, a reader beside a writer | **never blocked, slowest**: 90.8 µs p50 against fjall's 0.98 and redb's 1.28 | lock-free readers that read files for every snapshot and overlay the frames past the checkpoint |

**The journal syncs.** Of a native W1 commit (33.5 ms here), the `fdatasync` of `journal.log` takes 24.0 % and that of `journal.off` 23.8 %; the ref file's temp-file `fsync` and directory `fsync` take 24.1 % and 23.4 %.

### What would change the picture

- **Fewer syncs per commit.** 95 % of a native commit is its four syncs, and the storage-core spec's own target is two per transaction (§3.1, principle 7). Making the ref files durable at the batched checkpoint, as the index already is, would take 47.5 % off a commit; dropping the `fdatasync` of `journal.off`, as the code review suggests, 23.8 %. Both would leave one sync per commit, where SQLite, redb and fjall are. Each touches what a reader and the next writer's tail check may trust after a power loss, so each is a spec decision; these shares are what it would buy.
- **Group commit.** No engine here puts several writers' commits under one sync, so W2 runs at W1's pace everywhere. With 32 writers that is where the next factor lies, for the native store as for the others.
- **The disk.** Every durable engine here waits on `fdatasync`; the ratios follow the number of syncs per commit for as long as a sync costs more than the rest of a commit (95 % of it here). redb's commit costs about half of a raw append + `fdatasync` (4.1 against about 7.6 ms), most likely because its `fdatasync` has no new file size to log, while every native sync follows an append or a new file; journal files preallocated in place would test that.
- **A long-lived reader.** A server process (M10) could keep the index's files open and cache the sorted views' upper levels, closing most of W3's and W6's gap; mapping the files is ruled out while `unsafe` is forbidden. A CLI command, which takes one or two snapshots, pays 0.1–0.3 ms.
- **This run's scale and host.** W1 and W2 ran at a tenth and a twentieth of the plan, on a host shared with other agents' builds and tests. The controller's quiet-window `just bench-compare` at the plan's sizes replaces these tables.
