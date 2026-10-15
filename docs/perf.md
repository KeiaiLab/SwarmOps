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

Throughput follows the disk: an uncontended transaction took 13.2–15.3 ms at the end of these runs (7.4 ms in 7a), and the lock passed every 14–25 ms. The lock's own hand-off (a helper thread woken in the kernel, then a channel to its caller) does not show at that scale. In one process, 32 threads × 16 turns of 2 ms (`tests/lock.rs`) wait at most 0.12–0.13 s with the kernel queue, against 1.22–1.25 s with the polling lock.

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

The CLI-level half of the M1 Task 7 experiments: 32 agents committing through the real `swarmops` binary, and the lock-free read path after the journal hardening. Same host as M0: WSL2 (kernel 5.15.153.1), ext4 (`data=ordered`) on the virtual disk `/dev/sdd`, 32 hardware threads; Rust 1.99.0, optimized builds (the contention test with `--release`, the bench with `cargo bench`). Every store lives in a fresh temporary directory under `~/.local/state/swarmops-bench`. `just perf` re-runs both (the contention test at 32 × 1,000) and prints only their summary lines.

**Every number here is provisional.** The host was never quiet: three JetBrains remote-dev-server processes (about 8.5 cores), another project's test suite, and other SwarmOps worktrees, whose own 32-process store-level contention runs (Task 7c) overlapped the end of contention run 2 and both bench runs. The 1-minute load average was 15.7–16.1 when each 32 × 1,000 contention run started and 15–36 during them (32 processes blocked in fsync count towards it), and 18–61 during the other runs. Every measurement ran twice; where the runs disagree by more than 20 %, both are shown. The controller re-runs `just perf` in a quiet window before the final review.

### 32 agents committing through the binary

`crates/swarmops-cli/tests/contention.rs`, ignored. `just perf` runs it optimized at the spec's size (`SWARMOPS_TEST_CLI_CONTENTION_COMMITS=1000 cargo nextest run --release -p swarmops-cli --run-ignored all --test contention --no-capture`); `just check-slow` runs it unoptimized at 32 × 250 (below).

- The parent `init`s a store, commits a base changeset and creates main on it. Then 32 child processes start together behind a barrier (`ready-<n>` files, then `go`). Each runs `swarmops commit` 1,000 times in a row as agent `contention/agent-<n>`: one small file (`agents/<n>.txt`, about 20 bytes) per commit, on `--parent` its own previous changeset. So no child's commit depends on another's, and all they share is the store, and its `meta/lock`.
- Every commit is a process of its own: start, store open, two metadata snapshots (resolving `--parent`, checking the parents), the blob, two trees and the changeset written and synced (`put_batch` + `flush`), then the metadata write transaction under `meta/lock`, then the envelope.
- Per commit the child records the envelope's `meta.elapsed_ms` (all of that but process start, exit and printing) and the wall time from spawn to exit. An `E_BUSY` (exit 4, retryable) is counted, and the same commit run again.
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

**An uncontended commit misses the commit target.** 20–30 ms p50 when the disk is calmer (58 ms in run 2), against "≤ 5 ms + 0.3 ms per changed file" (5.3 ms for these commits). Every commit makes at least four fsyncs one after another: its new objects' (in parallel), then each of its fan-out directories' (one after another, the store-level section's finding), then the journal's two `sync_data`s under the lock. At M0's ~4 ms per fsync on this disk, that alone is over 15 ms. The snapshots are not the cost: a commit takes two, about 0.1 ms (below).

`verify --deep` checked 32,101 changesets and about 128,300 objects in 2.0–2.1 s, about 63,000 objects/s. `log -n 5` took under 1 ms.

### `just check-slow` runs 32 × 250

The test took 516.5 s and 886.1 s at 32 × 1,000, over 10 minutes in run 2. So its default is the fallback, 32 × 250, and its first summary line says so. `just perf`, or `SWARMOPS_TEST_CLI_CONTENTION_COMMITS=1000`, runs the spec's 32 × 1,000.

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

No other SwarmOps test ran during either; run 1's load rose to 61 with other work on the host. The runs disagree by more than 20 %, so both are shown. Both passed, but with the polling lock that is luck: their slowest commits waited 9.8 s and 8.4 s of the 10 s timeout. At the 32 × 1,000 runs' rates (2 and 19 `E_BUSY` in 32,000 commits) 8,000 commits meet 0.5 to 4.75 on average, so `just check-slow` fails now and then until the lock changes.

Under `just check-slow` (unoptimized, beside every other slow test, load 18.9 at the start and 19.6–25.9 during) the test passed in 138.6 s, which is also how long the whole suite took.

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

The rule: recommend a blocking lock with a timeout thread if the fairness spread exceeds 2× or the p99 lock wait exceeds 1 s. The lock (`crates/swarmops-store/src/meta/lock.rs`, `try_lock` every 10 ms) is unchanged; the controller decides.

- **Spread: not exceeded.** 1.11× and 1.10×.
- **p99 lock wait: exceeded.** About 2.8 s and 5.3 s (estimated from `meta.elapsed_ms`, whose p99 is 2.87 and 5.35 s; the commit's own work is 20–60 ms of it).
- **`E_BUSY` occurs.** 2 and 19 of 32,000 commits, every one after the 10 s timeout. The CLI contention test fails on it, as the store-level test does.

**Recommended: a blocking lock** (`File::lock` on a thread of its own, the caller waiting for it with the 10 s timeout and returning `Busy` when that passes. In a CLI process the blocked thread dies with the process; in a long-lived one, a thread that gets the lock after its caller gave up must release it at once).

- A blocking `flock` wakes a waiter the moment the lock is released, and so removes the 10 ms window in which every poller sleeps through the release. The store-level side experiment shows the result: no `Busy`, and a p99 wait of 0.3–0.8 s with writers 2 ms apart.
- CLI commits are further apart than 2 ms: each process spends its 20–60 ms of own work between two lock requests.
- What it will not change is throughput: the lock stays busy, so the mean wait stays about 0.5 s at 32 back-to-back committers on this disk. A fair lock spreads that wait over every commit, as a queue would, instead of piling it on a few. More commits per second need a shorter transaction under the lock: 7.5 ms alone, 15–27 ms here.
