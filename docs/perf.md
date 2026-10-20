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

**Decision.** p99 of a durable single-file write is 5.4 ms (max 7.4 ms over 1,000 writes), under the 20 ms threshold the M0 plan set, so **no `--durability fast` flag is introduced now**; the default stays full fsync (objects `sync_all` + the fsync of each touched fan-out directory, batched once per commit in `flush()` + the metadata store's syncs: SQLite `synchronous=FULL` in the M0 plan, and since the decision of 2026-10-15 for a native metadata store, the journal's two `fdatasync`s per transaction plus two `fsync`s per moved ref (one per deleted ref), storage spec §3). The rename + directory-fsync path has a 16.8 ms max, so a commit's directory fsyncs can occasionally approach the threshold; they are paid once per commit, not per object.

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

`crates/swarmops-store/tests/contention.rs`, ignored: `cargo nextest run --release -p swarmops-store --run-ignored all --test all -E 'test(/^contention::/)' --no-capture`; `just check-slow` runs it unoptimized.

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

`crates/swarmops-cli/tests/contention.rs`, ignored. `just perf` runs it optimized (`cargo nextest run --locked --release -p swarmops-cli --run-ignored ignored-only --test all -E 'test(/^contention::/)' --no-capture`), `just check-slow` unoptimized, both at the spec's 32 × 1,000 (at 32 × 250 for a while; see below).

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
- Caching pays only in a long-lived process that takes many snapshots, such as `swarmops mcp` or a network server after v1 (umbrella spec §17). There, a per-handle cache of the checkpoint's frame (its seq, checked with one 8-byte read that its entry still holds the checkpoint) would turn both searches into one read each.

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

**The lock pass, store against CLI.** At store level the lock passes every 8.27 ms (264.8 s / 32,000), about two raw `fdatasync`s, and an uncontended transaction takes 7.24 ms. The 3.8 ms probe is itself a 120-byte append and its `fdatasync`, so the inode size update that each journal append makes is already in it: growing the files is not the difference. Through the binary the lock passes every 13.75 ms (440.0 s / 32,000). The roughly 5.5 ms more appears only when the commits' own object writes run, outside the lock, on the same filesystem. Task 7b's "about 14 ms per pass at both levels" came from a store-level run that overlapped Task 8's processes, and is withdrawn. Whether the lock holder's two `fdatasync`s wait behind those object syncs on ext4's shared journal is not measured yet: the M1.6 measurement of the lock holder under object syncs will record it in a section of its own.

Decisions confirmed on this run: the kernel-queue lock keeps `Busy` at 0 with every longest wait under 1.2 s, so `lock_timeout` stays at 10 s (FIFO hand-off remains an M6 experiment); parallel `Index::sync` and parallel flush directory fsyncs hold their 7c gains; 32-thread `put_batch` scales 13.8× (15.5× with a busy writer), so a `--durability fast` flag is not justified by this data. The CLI harness now writes each change list to a file (7b fix round); the 21 ms uncontended commit matches the earlier stdin-based harness within noise.

## M1.5 — native store vs embedded databases (2026-10-17)

`bench/compare/` measures the native metadata store (`swarmops_store::MetaStore`, through its public API) against SQLite, redb, fjall and sled on SwarmOps' own workloads (M1.5 Task 8): `just bench-compare`, or `just bench-compare --quick` for a smoke run of every workload at small sizes (88.6 s of wall time here, 84.5 s inside the harness after a 4 s rebuild, at load 10.4–11.5 and a raw `fdatasync` of 3.0–5.1 ms, on b4d2208; the brief asks for under 2 minutes). It is a cargo workspace of its own, like `fuzz/`: `publish = false`, outside the root workspace, never built by `just check`. **SwarmOps depends on none of these databases**; they exist only in that harness, to measure the store the project chose to build instead. Its README gives the method in full.

**The run.** One hold of the host's lock for heavy runs, 2026-10-17 02:49–06:07 UTC (11:49–15:07 KST), 11,865.8 s, no errors, on tree dcf4fa7, at the plan's sizes: W1 10,000 timed transactions after 200 untimed, W2 32 writers × 1,000 transactions, W3 and W5 on 100k and 1M changesets, W4 on 1M, W6 on 100k (20,000 lookups of each kind, 5 opens and 3 kills per run, 10 s of W6 writer), the sync count over 64 and 1,088 transactions, every measurement 3 times (the raw record, `bench-compare-1792217222.json`, is kept under `bench/compare/target/results/` on the benchmark host, git-ignored). The branch has moved to b4d2208 since (Task 6, test code): its store source differs from dcf4fa7's only in doc comments and in gating the `CrashAtFs` test double behind the `test-hooks` feature, which the harness does not enable, so the code measured is the code of b4d2208. The final fix wave then moved it past that: its store source differs from b4d2208's in the read loop's comparison (`store.rs`: the frames read again are compared whole instead of counted, fa2d0e3), in the error of a failed `Compacted` marker append (`store.rs`, 9ede780), in two stderr lines on the journal's abort paths (`journal.rs`, 6624cf7; the wave's third such line is in `swarmops-repo`, which the harness does not build), and in the reads of the store's files: every open of a store file is preceded by a `stat` that refuses anything but a regular file (`fs_ops.rs`, `journal.rs`, `column.rs`, `sorted.rs`, ed0eff5), `journal.ckpt` is read through a 9-byte cap and each ref file read is preceded by a `stat` of its length (`journal.rs`, `refs.rs`, 0f90b51); the rest is doc comments (`index.rs`, `order_check.rs`). So a snapshot pays one `stat` per journal file and sorted view it opens and one per ref file it reads, no sync and no allocation, and nothing was re-measured. The same host as M0 (WSL2, kernel 5.15, ext4 on `/dev/sdd`, 32 threads), under the host's lock for heavy runs: no other benchmark beside it, but the lock does not cover builds and tests, and the load shows that some ran. Load 11.57 and a raw `fdatasync` of 3.77 ms at the start (M0 measured 4.1 ms on a quiet disk). Outside W2, the 1-minute load was 7.7–15.2 at the start of every measurement and 7.7–25.6 during them, and the raw `fdatasync` probe 3.7–5.6 ms in every one of them but the first native W5 build (7.1–7.2 ms). W2 is the exception, in two ways. Its probe read 6.7–9.7 ms at the start and end of every run but its first's start (4.1 ms) and four readings under a load event (17–26 ms): the disk stayed there for the whole 2.8-hour W2 phase. And two events the harness's own work does not explain (runs 2 and 3 of the same workloads stayed at load ≤ 13) raised the load, to 89 during the native threads run 1 and to 85 during redb's threads run 1 (76 at the start of fjall's). The per-run rows of the raw record show both; a median over three runs is one of the other two runs' values, and the `[min–max]` beside it shows the outlier.

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

Every version was released at least 7 days before the harness was written, and so was every crate in its `Cargo.lock`, except `zstd` 0.14.2 (5.7 days), `zstd-safe` 8.1.0 and `zstd-sys` 2.1.1 (6.6 days), which `swarmops-store`'s own `zstd` brings in. sled builds cleanly on Rust 1.99, so it is measured, as a reference.

- **Equalized durability.** Every engine but two makes each commit durable before it returns: the native store, SQLite at `synchronous=FULL`, redb at `Durability::Immediate` and fjall at `PersistMode::SyncAll`. SQLite at `synchronous=NORMAL` syncs only at checkpoints, so a commit survives a process crash but not a power loss. sled's `flush` syncs with `sync_file_range`, which writes no metadata and does not flush the disk's cache. Those two are reference lines, never contenders.
- **Multi-process access.** Only the native store and SQLite let several processes open one store. redb, fjall and sled lock the database to one process, so W2 runs them on threads, and runs the native store and SQLite on threads too, which measures what the asymmetry is worth.
- **fjall's settled state.** fjall writes a memtable to a table file only once it holds 64 MiB, and drops a journal only when every keyspace has flushed past it. The small keyspaces (`meta`, `refs`, `reflog`) never fill a memtable, so a bulk-loaded store keeps every journal until 512 MiB of them force a flush (216 of the 1M store's 385 MiB). The `fjall` rows settle as the other engines do, by moving the log into the main structure as SQLite's checkpoint does: the journal is synced, then every keyspace's memtable is flushed into table files (`rotate_memtable_and_wait`, public but doc-hidden), and fjall deletes the journals no keyspace needs. The active journal stays: fjall rotates it only once it passes 64 MB at a flush, and every open replays it whole, flushed or not. So the flush shrinks the 1M store (W5); at open it saves only the first open after the load, which on the unflushed store replays all 216 MiB of journals, flushes what the load left in memtables and deletes the sealed journals (2.9 s in the second provisional hold, where W4 ran before any W3, against 0.55 s for the flushed store); every later open replays the same 43 MiB active journal (W4). A 100k store keeps its whole journal beside the new tables. The `fjall-unflushed` rows (W3–W5 only) show the store as the load left it in W5; W3 and W4 see it after fjall's own first open settled it.
- **Method.** Same host and filesystem as M0 (ext4 `data=ordered` on WSL2's virtual disk), stores in fresh directories under `~/.local/state/swarmops-bench`, warm page cache. Seeded, deterministic data with real BLAKE3 ids, in chains. Warm-up before timing (W1 200 transactions; W3 one untimed pass of every lookup kind; W6 one of the reader's). Each measurement runs 3 times, the sync count and W5's builds of each store included, the engines in a rotated order; every cell is the median over the runs with `[min–max]`. None of the open, first-lookup, first-write or commit timers includes a close (SQLite's open hands its connection to the first session: closing a WAL store's last connection checkpoints the WAL and deletes it); W4's `process ms` (a process from spawn to exit) and W5's `load s` do span one. Beside every row: the 1-minute load at the start of each run (and its range, sampled every 2 s) and a raw `fdatasync` probe (the median of 40 appends of 120 B, each `sync_data`'d) at the start and end of each run. No network.
- **This run's course.** The sync count first (0–191 s into the run), then the breakdown (to 252 s), W1 (to 1,028 s), W2 (to 10,991 s: 2 h 46 min, most of the run), W5's three builds of every store (to 11,455 s), W3 (to 11,580 s), W4 (to 11,668 s) and W6 (to 11,866 s). The raw `fdatasync` probe read 4.1 ms at the start of W2's first run (native, processes) and 7.0 ms at its end, then 6.7–9.7 ms at the start and end of every W2 run (the four readings under the second load event apart) until the first native W5 build (7.1–7.2 ms), after which it was back to 3.9 ms; the records do not say why. So W2 ran on a disk about twice as slow as W1's: its engines compare with one another, not with W1's rates. Two load events, which the harness's own work does not explain, fell into W2's first round of threads: one reached 89 during the native threads run 1 (its 59 `Busy`, its 28.8 s longest wait and its 24.5 transactions/s are that run's), the other 36 during SQLite's threads run 1, then 85 during redb's and 76 at the start of fjall's (probes of 17–26 ms; their run 1 rows: redb 92.7 transactions/s against 228–230 in runs 2–3, fjall 72.9 against 156–160). The medians of W2's rates are the two unaffected runs'.
- **What changed since the provisional run (2026-10-16).** That run, on the Task 8 tree in two holds of the lock on a busy host, had W1 at 1,000 transactions and W2 at 32 × 50, at a raw `fdatasync` of 3.7–18 ms. Its headlines, for the record: W1 29.9 commits/s against SQLite's 135 and redb's 218 (4.5–7.3×, at a 7.6–8.3 ms probe); W2 27–59 transactions/s; W3 3.3–12× SQLite's p50 and 8.2–57× redb's; W4 open + lookup 0.108 ms against SQLite's 0.377, recovery 17.2 ms against 5.14 (3.3×), 301 against 6.63 ms at worst (45×); W6 90.8 µs p50 beside the writer; W4's slow kills were there too (the second hold's run 2: a median of 192 ms and a worst of 301; the first hold's: 261, 310 and 190 ms). The harness changed in four ways between those holds and this run: SQLite's open hands its connection to the first session (the first hold's SQLite rows, open + lookup 0.679 ms and recovery 23.7 ms, came from an open that made a connection and closed it, which checkpoints a WAL store, with two syncs, and deletes its WAL: a tie that was the harness's; fixed before the second hold); W5's builds and the sync count run three times, as every measurement does (also before the second hold); each record names the tree it measured (`host.git` in the record, the report's `tree` line), where the provisional records named the base commit of an uncommitted harness; and W2 reaps its writers on an error, and fjall's settle fails at its timeout instead of returning. The store's source changed too: Task 8's tree predates the Task 2 and 3 store fixes and the code minors batch, and the native store now makes 165 system calls per commit where it made 157; its sync counts are the same.

### Sync calls per commit: durability, checked

`strace` and `perf` are not installed on this host, so the harness counts system calls with a ptrace tracer of its own (`bench/compare/src/trace.rs`: about 230 lines of safe Rust over `nix`'s ptrace calls, x86_64 Linux only). A child process creates a store, commits 32 untraced W1 transactions and waits on its stdin. The tracer seizes every thread, follows the threads they create, and counts every system call while the child makes n more commits, closes the store and exits, naming each sync call's file through `/proc/<pid>/fd`. It runs twice, with n = 64 and n = 1,088; the difference over 1,024 is the cost of one commit, with the batched work averaged in (the native index sync every 32 frames and a compaction every 1,024 records, SQLite's checkpoint every 1,000 WAL pages) and the open, the warm-up and the close cancelled out. The pair runs three times, once per run, the engines in a rotated order. Per commit, the median [min–max] of the three counts (the run's first 191 s, load 11.0–12.4 at the start of each count). Every count but sled's system calls came out the same three times:

| engine | fsync | fdatasync | sync_file_range | all syncs | syscalls | bytes written | sync calls by file (≥ 0.01) |
|---|---|---|---|---|---|---|---|
| native | 2.07 | 2.17 | 0.00 | 4.23 | 165 | 470 | `fdatasync meta/journal.log` 1.00; `fdatasync meta/journal.off` 1.00; `fsync meta/refs/` 1.00; `fsync meta/refs/.tmp*` 1.00; `fdatasync meta/idx/dh.col` 0.032; `fdatasync meta/idx/gen.col` 0.032; `fdatasync meta/idx/ids.col` 0.032; `fdatasync meta/idx/par.col` 0.032; `fdatasync meta/parents.log` 0.032; `fsync meta/` 0.031; `fsync meta/.tmp*` 0.031 |
| sqlite | 1.02 | 0.00 | 0.00 | 1.02 | 18.8 | 24,031 | `fsync db.sqlite-wal` 1.01 |
| sqlite-normal | 0.018 | 0.00 | 0.00 | 0.018 | 17.8 | 24,031 | `fsync db.sqlite-wal` 0.012 |
| redb | 0.00 | 1.00 | 0.00 | 1.00 | 15.1 | 49,752 | `fdatasync db.redb` 1.00 |
| fjall | 1.00 | 0.00 | 0.00 | 1.00 | 2.01 | 494 | `fsync fjall/#.jnl` 1.00 |
| sled | 0.003 | 0.00 | 2.26 | 2.26 | 29.5 [29.4–29.9] | 1,047 | `sync_file_range sled/db` 2.26 |

- **Every durable contender makes one sync per commit**: SQLite an `fsync` of its WAL (1.02 with its checkpoints averaged in; this SQLite build has no `fdatasync`), redb one `fdatasync`, fjall one `fsync` of its journal.
- **The native store makes 4.23**, as storage-core §3 specifies ("2 journal syncs per transaction, + 2 per moved ref"; every W1 transaction moves `main`): the frame's `fdatasync` (the commit point), the `journal.off` entry's, the ref file's temp-file `fsync` and the `fsync` of `refs/`, plus the batched index sync (5 files every 32 frames: 0.16), the checkpoint's replacement (2 every 32 frames: 0.06) and the compaction's syncs (every 1,024 records, under the table's 0.01 cut-off).
- **Not durable as the others are:** sled's 2.26 `sync_file_range` calls per commit write neither the file's metadata nor the disk's cache, and SQLite at `NORMAL` syncs only at its checkpoints (0.018 per commit).
- The native store writes the fewest bytes per commit (470, against SQLite's 24,031 in whole WAL pages and redb's 49,752 in copy-on-write pages) and makes the most system calls (165: the lock file and the index's nine files are opened in every transaction, and the ref file gets a temp file of its own). Neither matters beside its syncs (below).

### Where a native commit's time goes: the journal syncs

`breakdown` runs W1's transaction on a native store whose filesystem is `RealFs` with every call timed (through the store's public `MetaStore::create_with_fs`): 1,024 commits per run after a 200-commit warm-up, so that one compaction falls among them, 3 runs. `RealFs::append_durable` (the journal's append + `fdatasync`) and `atomic_replace` of a ref file (temp file, `fsync`, rename, directory `fsync`) are done step by step with `RealFs`'s calls in `RealFs`'s order, so that each sync is timed on its own; a kind of call counts for the time its calls cover, overlaps once (the index sync's five `sync_data`s run at once). Means per commit, so the parts add up. Load 11.9/12.1/11.5 at the start of the runs (11.5–12.6 during them), raw `fdatasync` 3.8–4.7 ms.

| part of the commit | calls per commit | ms per commit | share of the commit |
|---|---|---|---|
| ref file: fsync of the temp file | 1.00 | 3.87 [3.85–4.17] | 24.2 [23.9–24.2] % |
| fdatasync of journal.log (the commit point) | 1.00 | 3.80 [3.78–4.08] | 23.7 [23.5–23.7] % |
| fdatasync of journal.off | 1.00 | 3.77 [3.75–4.03] | 23.4 % |
| ref file: fsync of refs/ | 1.00 | 3.70 [3.61–3.86] | 22.6 [22.4–23.0] % |
| rest: lock, index open and lookups, encoding (outside filesystem calls) | – | 0.294 [0.293–0.334] | 1.84 [1.82–1.94] % |
| checkpoint replace (every 32 frames) | 0.032 | 0.251 [0.248–0.264] | 1.55 [1.53–1.56] % |
| index sync, 5 files (every 32 frames) | 0.166 | 0.176 [0.176–0.185] | 1.10 [1.08–1.10] % |
| ref file: rename over refs/main | 1.00 | 0.074 [0.072–0.084] | 0.459 [0.449–0.488] % |
| index appends, 5 files (no sync) | 5.00 | 0.056 [0.055–0.060] | 0.346 [0.345–0.350] % |
| ref file: temp file created + written | 1.00 | 0.047 [0.046–0.053] | 0.291 [0.288–0.308] % |
| journal appends: open + write, 2 files (no sync) | 4.00 | 0.036 [0.036–0.039] | 0.225 [0.224–0.229] % |
| compaction (every 1,024 records) | 0.016 | 0.029 [0.027–0.032] | 0.170 [0.166–0.201] % |
| ref file reads | 2.97 | 0.020 [0.020–0.024] | 0.127 [0.125–0.137] % |
| file length checks | 4.00 | 0.009 | 0.054 [0.054–0.056] % |
| **the whole commit** | | 16.1 [16.0–17.2] | 100 % |

- **94 % of a native commit is its four syncs, about a quarter each**: `journal.log`'s `fdatasync` 23.7 % [23.5–23.7], `journal.off`'s 23.4 %, the ref temp file's `fsync` 24.2 % [23.9–24.2] and the `fsync` of `refs/` 22.6 % [22.4–23.0] (93.8 % together, 93.7–93.9 in each run), each about one raw `fdatasync` (3.70–3.87 ms against a probe of 3.8–4.7 ms in these runs). The batched work takes 2.8 % (the checkpoint 1.55 %, the index sync 1.10 %, the compaction 0.17 %), and everything else (the lock, opening the index, the lookups, the encoding, the appends, the rename) 3.3 %.
- **The second journal sync is a quarter of a commit, not a half.** An independent code review suggested that the `fdatasync` of `journal.off` may be droppable. Without it, a W1 commit would take about 12.3 ms instead of 16.1 ms: 1.3× the commits/s. The ref file's replacement costs as much as both journal syncs together (its two syncs 46.7 %, the journal's two 47.1 %), although storage-core §2 rule 3 calls the ref files, like the index, a projection of the journal that replay restores from the frames past the checkpoint, and the index's syncs are batched every 32 frames. A commit with one sync, where the other engines are, needs both gone. Whether either can go is a durability question for the spec (what a reader and the tail check may trust after a power loss), which this measurement does not settle; it prices the options.

### W1: one writer, durable commits

W1's transaction back to back, 10,000 timed commits after 200 untimed. Bytes per transaction: the closed store's allocated size over the 10,200 commits.

| engine | commits/s | p50 ms | p99 ms | max ms | bytes/txn on disk | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|
| native | 59.5 [56.4–60.7] | 15.5 | 34.8 [31.9–70.0] | 106 [84.6–472] | 397 | 12.6/13.8/14.7 (11.6–25.6) | 3.9–5.1 ms |
| sqlite | 417 [399–419] | 1.82 [1.82–1.96] | 6.24 [5.80–7.00] | 14.0 [13.8–20.2] | 290 | 13.1/11.9/12.8 (11.1–13.1) | 3.9–4.7 ms |
| sqlite-normal | 8,920 [7,820–9,038] | 0.024 [0.023–0.030] | 3.29 [3.24–3.61] | 18.1 [17.5–28.5] | 290 | 12.1/12.6/11.7 (11.7–12.6) | 3.9–4.2 ms |
| redb | 450 [375–464] | 1.88 [1.82–2.34] | 4.90 [4.74–7.53] | 14.4 [12.3–30.0] | 382 | 12.2/12.6/11.7 (11.7–15.0) | 3.8–4.4 ms |
| fjall | 394 [364–400] | 2.34 [2.31–2.38] | 5.03 [4.82–8.06] | 38.4 [14.6–96.0] | 502 | 12.1/14.8/11.8 (11.5–15.2) | 3.8–4.1 ms |
| sled | 1,791 [1,532–1,836] | 0.508 [0.498–0.555] | 1.17 [1.11–1.87] | 5.01 [3.19–22.4] | 720 [669–819] | 11.5/13.7/15.2 (11.5–15.2) | 4.0–4.2 ms |

**Verdict: the native store loses, with 6.6–7.6× fewer commits/s than the durable engines** (59.5 against fjall's 394, SQLite's 417 and redb's 450). Its p50 of 15.5 ms is four raw `fdatasync`s (3.87 ms each; the probe read 3.9–5.1 ms): the frame per transaction with two journal `fdatasync`s, plus the ref file's atomic replacement with two more, where every other durable engine makes one sync per commit. And that one sync costs the others about half of a raw append + `fdatasync` (SQLite's p50 1.82 ms, redb's 1.88, fjall's 2.34, against the probe's 3.8–4.7), where each native sync costs about one (3.70–3.87 ms in the breakdown above); most likely because every native sync follows an append or a newly created file, whose new length or inode ext4 must log through its own journal before the sync returns, while redb rewrites pages in place inside a file that does not grow at every commit, SQLite, after each checkpoint, rewrites its WAL from the start, and fjall writes into a journal it pre-sized to 64 MiB. The native tail is the longest too: p99 34.8 ms [31.9–70.0] and a worst commit of 106 ms [84.6–472] (run 3's 70.0 and 472 fell where the load rose to 25.6), against p99s of 4.90–6.24 ms and worst commits of 14.0–38.4 ms at the median for the three durable engines (fjall's run 3: 96.0, above the native store's best run, 84.6). sled (1,791/s) and SQLite at `NORMAL` (8,920/s) return before the commit is on the disk. On disk, the native store takes 397 bytes per transaction, between redb (382) and fjall (502); SQLite takes 290.

### W2: 32 writers

32 writers × 1,000 transactions each, every writer moving `main` with a compare-and-swap against its value inside the transaction. Processes for the native store and SQLite, threads for the others; the native store and SQLite on threads as well. Latency includes `Busy` retries (a writer that gets `Busy` after the 10 s timeout tries again); spread is the slowest writer's total time over the fastest's. No writer failed, and no engine reran a transaction. The raw `fdatasync` probe read 6.7–9.7 ms through W2 (above), against 3.8–5.1 during W1.

| engine | mode | txns/s | p50 ms | p99 ms | max ms | Busy | failed | spread | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|---|
| native | processes | 32.1 [32.0–34.8] | 1,089 [1,043–1,097] | 1,393 [1,383–2,372] | 1,614 [1,567–3,343] | 0.00 | 0.00 | 1.04 [1.03–1.06] | 11.5/10.6/9.4 (8.8–29.0) | 4.1–7.3 ms |
| sqlite | processes | 92.6 [89.7–95.0] | 23.8 [23.3–27.0] | 5,559 [5,146–5,674] | 40,702 [38,429–52,221] | 99.0 [88.0–116] | 0.00 | 2.98 [2.12–54.4] | 11.8/15.2/11.1 (8.7–15.4) | 6.8–7.9 ms |
| sqlite-normal | processes | 8,585 [7,535–9,166] | 0.027 [0.026–0.052] | 0.219 [0.141–0.749] | 3,534 [3,178–4,035] | 0.00 | 0.00 | 19.3 [18.2–25.9] | 28.4/9.3/11.3 (9.3–28.5) | 7.3–9.0 ms |
| native | threads | 32.0 [24.5–32.0] | 1,108 [1,097–1,111] | 1,435 [1,405–6,163] | 1,750 [1,709–28,774] | 0.00 [0.00–59.0] | 0.00 | 1.04 [1.03–1.05] | 17.8/10.7/11.7 (9.1–89.1) | 7.1–7.6 ms |
| sqlite | threads | 90.5 [73.2–90.8] | 27.1 [26.6–28.3] | 5,643 [5,556–6,950] | 30,295 [27,484–39,907] | 81.0 [71.0–139] | 0.00 | 2.29 [1.63–2.93] | 12.3/12.8/9.5 (7.3–35.6) | 6.7–9.7 ms |
| sqlite-normal | threads | 10,116 [9,295–10,912] | 0.033 [0.032–0.056] | 0.186 [0.177–1.56] | 2,933 [2,733–2,999] | 0.00 | 0.00 | 22.3 [13.4–27.2] | 28.5/9.7/10.9 (9.7–29.1) | 7.3–18.3 ms |
| redb | threads | 228 [92.7–230] | 3.95 [3.92–7.73] | 892 [18.0–8,101] | 60,036 [48,833–72,402] | 0.00 | 0.00 | 2.38 [1.60–4.08] | 29.1/10.2/10.7 (7.8–84.5) | 7.2–22.3 ms |
| fjall | threads | 156 [72.9–160] | 7.07 [7.00–11.9] | 3,463 [993–11,703] | 81,663 [55,544–117,985] | 0.00 | 0.00 | 2.37 [1.18–4.08] | 76.0/7.8/9.4 (7.5–76.0) | 6.8–25.5 ms |
| sled | threads | 2,110 [2,106–2,126] | 17.2 [17.1–17.2] | 27.0 [26.4–27.3] | 40.3 [37.7–43.5] | 0.00 | 0.00 | 1.02 [1.02–1.03] | 17.0/10.0/9.1 (9.1–17.0) | 6.9–8.1 ms |

**Verdict: the native store loses on throughput, 2.8–2.9× to SQLite and 4.9–7.1× to fjall and redb, and wins on fairness.**

- **Throughput.** Every durable engine serializes its writers, and none batches several commits into one sync, so W2's rate is set by each engine's per-commit sync cost on the disk it had (0.54 of the native store's W1 rate, 0.51 of redb's, 0.40 of fjall's, 0.22 of SQLite's): the native store at 32.0–32.1 transactions/s (four syncs at 7.8 ms, where W1's 59.5/s were four at about 4 ms), against SQLite's 90.5–92.6 (2.8–2.9×), fjall's 156 (4.9×) and redb's 228 (7.1×). The four run-1 rows under the load events are lower (native threads 24.5, SQLite threads 73.2, redb 92.7, fjall 72.9); the medians are the other two runs'.
- **Fairness.** The native store's writers wait in the kernel's flock queue (`MetaLock`, M1 Task 7c): spread 1.03–1.06 in all six runs, p50 1,043–1,111 ms over the six runs (medians 1,089 and 1,108; about one rotation of the 32 writers: 1.0 s at 32 commits/s), and the longest wait 1.57–1.75 s in four of the six runs, 3.34 s in the processes run 1 (its load rose to 29). The one run with `Busy` is the threads run 1 under the load of 89: 59 of them, after 10 s in the queue, and a longest wait of 28.8 s; the other five runs had none. SQLite's busy handler polls, with sleeps that grow to 100 ms: 71–139 `SQLITE_BUSY` per run after the 10 s timeout, on processes and threads alike, p99 5.1–7.0 s, longest waits 27–52 s, and a spread of 1.6–3.0 in five runs and 54 in one (the processes run 3, where a writer finished 54 times sooner than the slowest). redb's and fjall's in-process writer locks (a condition variable in redb, a mutex in fjall), neither first-come-first-served: spread 1.2–4.1, longest waits 49–118 s, p99 0.89 s [0.018–8.1] and 3.5 s [0.99–11.7]. sled's transactions are short and fair (spread 1.02, p50 17.2 ms, the longest 40 ms), at a durability the others do not have.
- **The asymmetry.** The native store runs the same on threads as on processes (32.0 against 32.1 transactions/s, spread 1.04 either way; every acquire flocks a file description of its own, so threads queue as processes do), and so does SQLite (90.5 against 92.6). redb, fjall and sled would need a server process in front of them to take SwarmOps' 32 agent processes.

### W3: lookups, warm

Bulk-loaded stores of 100,000 and 1,000,000 changesets (one chain, in transactions of 50,000, then settled and closed), opened in the measuring process; 20,000 lookups of each kind, after one untimed pass of every kind. On one open snapshot: by id (`get`), by the first 6 bytes of the id (`prefix`), by delta hash through the secondary index (`delta`), and an id the store does not hold (`miss`, what every insert asks first); and by id in a fresh read transaction each (`fresh`, what a CLI command pays; sled has no read snapshots, so its lookup reads the latest state and pays no snapshot cost). Every answer is checked. µs:

| engine | size | get p50 | get p99 | prefix p50 | prefix p99 | delta p50 | delta p99 | miss p50 | fresh p50 | fresh p99 | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| native | 100k | 10.00 [9.28–10.7] | 17.3 [15.8–18.3] | 11.4 [10.5–12.5] | 22.2 [21.9–25.2] | 10.3 [9.96–10.7] | 18.3 [15.9–19.7] | 6.25 [6.25–7.51] | 51.4 [51.0–62.9] | 101 [87.4–121] | 8.5/9.1/8.8 (8.5–9.1) | 3.8–4.0 ms |
| sqlite | 100k | 2.73 [2.69–2.75] | 6.33 [4.82–7.10] | 1.31 [1.28–1.84] | 2.31 [2.04–3.05] | 2.77 [2.58–2.98] | 6.21 [4.26–8.38] | 1.34 [1.16–1.69] | 4.66 [4.45–4.69] | 9.22 [6.88–9.47] | 8.7/8.9/8.8 (8.7–8.9) | 3.8–3.9 ms |
| redb | 100k | 0.390 [0.351–0.772] | 0.992 [0.672–1.59] | 0.491 [0.481–0.572] | 0.791 [0.661–1.29] | 0.561 | 1.38 [1.20–1.66] | 0.330 [0.301–0.390] | 0.702 [0.701–0.792] | 1.04 [1.04–1.54] | 8.7/8.9/8.9 (8.7–8.9) | 3.8–3.9 ms |
| fjall | 100k | 0.541 [0.521–1.48] | 1.04 [1.00–5.11] | 4.29 [3.30–5.97] | 11.5 [7.98–17.5] | 4.40 [4.39–6.13] | 10.6 [9.99–12.4] | 14.6 [14.0–15.9] | 0.962 [0.701–1.50] | 2.69 [2.21–4.12] | 8.7/8.9/8.9 (8.7–9.1) | 3.8–3.9 ms |
| fjall-unflushed | 100k | 0.631 [0.621–0.831] | 1.77 [1.16–2.65] | 1.08 [0.942–1.79] | 1.63 [1.55–4.10] | 1.35 [1.34–1.54] | 3.27 [3.11–3.65] | 0.762 [0.642–0.772] | 1.04 [0.702–1.42] | 2.53 [1.50–3.89] | 8.9/9.1/8.9 (8.9–9.1) | 3.8–4.1 ms |
| sled | 100k | 0.982 [0.601–1.01] | 3.30 [2.01–5.04] | 1.66 [0.992–2.45] | 4.04 [2.30–6.91] | 1.54 [1.48–1.70] | 4.99 [4.84–5.72] | 0.581 [0.581–0.672] | 0.742 [0.571–1.01] | 4.07 [2.27–4.44] | 8.9/9.1/8.8 (8.8–9.1) | 3.8–3.9 ms |
| native | 1M | 13.5 [13.0–14.2] | 27.0 [24.3–27.8] | 14.8 [14.6–14.9] | 28.8 [27.4–30.0] | 14.6 [14.3–15.1] | 26.7 [26.3–28.1] | 8.41 [8.08–8.55] | 56.3 [55.6–56.7] | 109 [105–110] | 8.9/9.2/8.4 (8.4–9.2) | 3.8–4.0 ms |
| sqlite | 1M | 3.60 [3.59–3.79] | 7.00 [5.68–7.83] | 2.03 [1.71–2.20] | 3.48 [3.14–4.74] | 3.84 [3.67–4.11] | 7.57 [6.38–9.00] | 2.19 [2.14–2.44] | 6.22 [6.03–6.67] | 14.0 [11.4–14.1] | 8.9/8.5/8.7 (8.5–8.9) | 3.7–3.9 ms |
| redb | 1M | 1.40 [1.25–1.42] | 2.84 [2.77–2.95] | 1.22 [1.20–1.83] | 2.89 [2.02–3.44] | 1.44 [1.17–2.08] | 3.19 [1.98–3.58] | 1.13 [0.992–1.67] | 1.97 [1.44–2.38] | 4.37 [2.27–5.13] | 8.9/8.5/8.9 (8.5–8.9) | 3.8–3.9 ms |
| fjall | 1M | 12.7 [11.9–13.4] | 163 [157–166] | 46.5 [46.1–47.2] | 99.6 [98.8–102] | 39.9 [39.5–40.1] | 81.3 [80.0–90.7] | 60.8 [60.7–61.1] | 14.1 [12.3–14.1] | 171 [158–171] | 8.9/8.5/8.9 (8.5–9.0) | 3.8–4.0 ms |
| fjall-unflushed | 1M | 13.3 [9.69–13.5] | 158 [155–165] | 40.6 [39.7–42.7] | 82.1 [76.1–92.9] | 38.8 [38.6–40.4] | 84.6 [73.2–88.1] | 59.9 [57.9–60.6] | 14.6 [11.7–14.8] | 173 [163–178] | 8.5/9.0/8.6 (8.1–9.2) | 3.8–3.9 ms |
| sled | 1M | 1.81 [1.79–2.14] | 9.38 [8.63–11.9] | 2.60 [2.39–3.23] | 10.2 [9.33–15.8] | 2.41 [2.25–2.93] | 11.1 [8.55–13.9] | 1.48 [1.35–1.74] | 1.56 [1.48–1.99] | 8.02 [6.90–11.6] | 8.5/9.2/8.4 (8.4–9.2) | 3.8–3.9 ms |

The native store's p50 over each engine's, kind by kind: the median over the three runs of each, as the harness prints it under each W3 table so that a verdict starts from every kind (`report::ratios`; the two tables of the report joined here, their cells as printed). Above 1, the native store is slower:

| size | native ÷ | get | prefix | delta | miss | fresh | range |
|---|---|---|---|---|---|---|---|
| 100k | sqlite | 3.67× | 8.67× | 3.74× | 4.66× | 11.0× | 3.67–11.0× |
| 100k | redb | 25.6× | 23.2× | 18.4× | 18.9× | 73.2× | 18.4–73.2× |
| 100k | fjall | 18.5× | 2.65× | 2.35× | 0.429× | 53.4× | 0.429–53.4× |
| 100k | fjall-unflushed | 15.8× | 10.5× | 7.65× | 8.20× | 49.3× | 7.65–49.3× |
| 100k | sled | 10.2× | 6.84× | 6.71× | 10.8× | 69.2× | 6.71–69.2× |
| 1M | sqlite | 3.77× | 7.30× | 3.81× | 3.83× | 9.05× | 3.77–9.05× |
| 1M | redb | 9.66× | 12.1× | 10.1× | 7.43× | 28.5× | 7.43–28.5× |
| 1M | fjall | 1.07× | 0.319× | 0.366× | 0.138× | 4.00× | 0.138–4.00× |
| 1M | fjall-unflushed | 1.02× | 0.366× | 0.377× | 0.140× | 3.85× | 0.140–3.85× |
| 1M | sled | 7.47× | 5.70× | 6.06× | 5.67× | 36.0× | 5.67–36.0× |

**Verdict: the native store loses every lookup kind to SQLite and to redb, by 3.7–11× against SQLite and 7.4–73× against redb, the fresh snapshot by the most; against fjall at 1M it wins the prefix, delta-hash and miss lookups, ties the `get` and loses the fresh snapshot.**

- **Against SQLite.** A `get` or a delta-hash lookup is 3.7–3.8× slower, a miss 3.8–4.7×, a prefix lookup 7.3–8.7× (14.8 µs p50 against 2.03 at 1M) and a fresh snapshot and lookup 9.1–11× (56.3 µs against 6.22 at 1M).
- **Against redb.** 18–73× at 100k (a `get` 10.0 µs against 0.390, a fresh snapshot 51.4 against 0.702) and 7.4–29× at 1M (a `get` 13.5 against 1.40, a fresh snapshot 56.3 against 1.97).
- **Against fjall.** At 1M the native store is the faster on the prefix, delta-hash and miss lookups (0.14–0.37× fjall's p50: fjall's prefix lookup takes 46.5 µs, its miss 60.8), about even on the `get` (13.5 against 12.7 µs, 1.07×) and the slower on the fresh snapshot (4.0×). At 100k it is the slower on every kind but the miss, where fjall's 14.6 µs is the slowest of all (the native store's 6.25). So at 100k the native store is the slowest engine at every kind but the miss; at 1M it is the slowest at the `get` and the fresh snapshot, and fjall at the other three.
- **Why.** A native lookup binary-searches a sorted view's key file with one `pread` per probe (about 20 at 1M), then reads the record's columns with four more. It keeps no cache of its own, nor maps the files (`unsafe` is forbidden), where redb and sled answer from a 1 GB in-process cache and SQLite from its page cache. And a native snapshot opens the index's nine files and reads the checkpoint, the journal tail and the ref files each time, because readers take no lock and share nothing with writers (the lock-free reader).
- **What the columnar index buys.** A prefix or delta-hash lookup costs about what a point lookup does (each kind has its sorted view), and a miss less. Going from 100k to 1M adds 31–41 % to a `get`, a prefix lookup, a delta-hash lookup and a miss (about three more probes) and 10 % to a fresh snapshot; SQLite's lookups grow 32–63 % over the same step, redb's 2.5–3.6×, and fjall's 4.2–23× (`get` 23×, fresh 15×, prefix 11×, delta 9.1×, miss 4.2×), past the native store's on the prefix, delta-hash and miss lookups: its 32 MiB block cache holds a sixth of its 1M tables (192 MiB).
- Every native lookup at 1M stays at or under 30 µs p99 (`get` 27.0, prefix 28.8 [27.4–30.0], delta 26.7), 0.11 ms with a fresh snapshot, nine times inside the spec's 1 ms read target. The gap matters to a long-lived process that takes many snapshots, not to a CLI command that takes one or two.

### W4: opening a 1M store, and recovery after a kill

Open: a fresh process opens the 1M store (warm: the files are in the page cache, and W3 has opened every store three times before W4, so these are re-opens; the first open after a load is not timed here) and looks one changeset up, 5 times per run (the JSON's `open_max_ms` holds the worst of the 5). Recovery: a writer process commits W1's transaction in a loop on the 1M store and is killed with SIGKILL 0.1–1 s after its first acknowledged commit (a seeded draw), most likely inside a commit; a fresh process then opens the store and commits one transaction (where most engines repair what the kill left), 3 kills per run. A killed process loses nothing the kernel holds, so this is recovery from a process crash, not from a power loss. None of the open, first-lookup or first-write timers includes a close: SQLite's open hands its connection to the first session, and the lookup's session is closed after the timing (`process ms`, the opening process from spawn to exit, spans its close). The last acknowledged commit was present after every kill, on every engine (each engine's ordered log implies the earlier ones). `open + lookup ms` is the sum of a run's median open and median first lookup; `open + write ms` is the median of the per-kill sums. ms:

| engine | open ms | worst open ms | first lookup ms | open + lookup ms | process ms | after a kill: open ms | first write ms | open + write ms | worst ms | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|---|---|
| native | 0.022 [0.020–0.028] | 0.036 [0.035–0.043] | 0.071 [0.069–0.103] | 0.091 [0.091–0.131] | 0.916 [0.881–1.14] | 0.022 [0.021–0.024] | 22.3 [16.2–231] | 22.4 [16.2–231] | 198 [20.9–235] | 8.7/9.6/9.2/9.2/10.3/10.3 (8.7–10.6) | 3.7–3.9 ms |
| sqlite | 0.334 [0.266–0.385] | 0.418 [0.412–0.678] | 0.034 [0.026–0.038] | 0.368 [0.292–0.423] | 1.51 [1.25–1.62] | 0.879 [0.584–1.34] | 4.31 [4.16–4.56] | 5.21 [5.03–5.50] | 5.58 [5.19–5.86] | 8.7/9.2/9.2/9.2/9.4/10.3 (8.7–10.5) | 3.7–4.3 ms |
| redb | 1.76 [1.56–2.49] | 1.82 [1.66–2.84] | 0.035 [0.030–0.036] | 1.79 [1.60–2.52] | 9.02 [8.85–9.67] | 285 [275–288] | 1.90 [1.89–1.93] | 287 [276–290] | 306 [290–342] | 8.7/9.2/9.6/9.1/9.9/10.6 (8.7–10.6) | 3.8–5.0 ms |
| fjall | 468 [462–481] | 494 [474–518] | 0.066 [0.055–0.071] | 468 [462–481] | 524 [524–529] | 496 [480–504] | 3.97 [3.73–4.33] | 499 [484–508] | 501 [490–529] | 8.7/9.2/9.6/8.9/9.9/10.6 (8.7–10.7) | 3.7–4.0 ms |
| fjall-unflushed | 463 [458–474] | 488 [474–495] | 0.070 [0.051–0.070] | 463 [458–474] | 512 [512–534] | 484 [482–506] | 3.89 [3.77–4.32] | 488 [486–510] | 501 [488–511] | 9.1/9.2/9.5/9.0/10.2/10.7 (9.0–10.7) | 3.8–4.0 ms |
| sled | 147 [145–155] | 161 [159–166] | 0.078 [0.075–0.082] | 147 [145–155] | 214 [210–218] | 166 [164–175] | 1.17 [0.937–6.31] | 172 [165–176] | 187 [185–188] | 9.1/9.6/9.2/9.4/10.3/10.5 (9.1–10.5) | 3.7–3.9 ms |

**Verdict: the native store opens fastest by far; it loses recovery to SQLite, 4.3× slower at the median and 40× at worst, and recovers 13–22× faster than redb and fjall at the median and 1.5–2.3× at worst (235 against 342 and 529 ms).**

- **Open.** 0.091 ms to open the 1M store and answer a first lookup, against SQLite's 0.368 ms (4.0×), redb's 1.79 ms and fjall's 468 ms (sled, a reference line: 147 ms). The native open reads nothing (0.022 ms); the first snapshot (0.071 ms) reads the checkpoint, the index's files and at most 31 frames past the checkpoint, because the index is synced and the checkpoint moved every 32 frames (the batched index sync). SQLite's open makes its first connection, which creates the WAL and its index again (a clean close deleted them), 0.334 ms; its first lookup and redb's are the fastest of all (0.034 and 0.035 ms). fjall replays its active journal (43 MiB here) into its memtables at every open, flushed or not; by W4 the unflushed store holds the same active journal, its first open (in W3) having replayed and flushed its 216 MiB of journals and deleted the sealed ones, so the two open alike (463 ms against 468). That first open is not in the table: in the second provisional hold, which ran W4 with no W3 before it, it took 2.9 s (fjall-unflushed, run 1's worst open), and sled's first open 1.2 s against 162 ms at the median.
- **Recovery.** 22.4 ms [16.2–231] for the next process's open and first write, against SQLite's 5.21 [5.03–5.50] (4.3×), redb's 287 (without quick-repair, its default, an open after a crash walks the database to rebuild the allocator state) and fjall's 499 (its journal replay): 13× and 22× faster than redb and fjall (sled, a reference line, 172). SQLite's open after a kill rebuilds the WAL's index from the WAL (0.879 ms: a read, no sync), and its first write is one ordinary commit appended to that WAL: an `fsync` of the WAL, which also `fsync`s the directory, as every connection's first sync does (4.31 ms, about one raw `fdatasync`). The native open reads nothing (0.022 ms), and its first write is an ordinary commit too: its four syncs (16.2 ms in run 3, at a 3.8–3.9 ms probe), with the tail repair and the replay of at most 31 frames costing next to nothing when the kill left no torn frame.
- **The worst case.** 235 ms for the native store against SQLite's 5.86 ms (40×): ahead of redb's (342) and fjall's (529); sled, a reference line, took 188. Run 1's median kill took 231 ms and its worst 235; run 2's worst 198 ms against a median of 22.3; run 3's worst 20.9: at least three of the nine kills. Most likely kills that left a torn or unlisted frame: before it cuts or lists one, the next writer checks every listed frame (storage-core §2 rule 3; `check_listed_chain`), and at 1M that reads the whole 101 MiB journal. The check grows with the journal, in bytes and in listed frames (two reads, a CRC and a decode each), and only a crash's repair pays for it. The seeded store was bulk-loaded in 20 transactions of 50,000 changesets, each followed by its `Compacted` frame, so its journal holds 40 frames (plus the killed writers' own commits); a store built one commit at a time holds a frame per commit, about a million at 1M commits, and its repair would take longer than this worst case (not measured; docs/known-limitations.md, "Recovery after a crash walks all of `journal.off`").

### W5: size on disk

The bulk-loaded stores, settled (native: `flush`; SQLite: `wal_checkpoint(TRUNCATE)`; redb: nothing to do; fjall: memtables flushed, `fjall-unflushed`: the journal synced only; sled: `flush`) and closed; allocated bytes, as `du` counts them. Each store was built three times, a run each (the first build is the store W3, W4 and W6 read). The native store, SQLite, redb and fjall came out the same to the byte in all three builds (the native store 29,839,360 bytes at 100k and 298,065,920 at 1M), `fjall-unflushed`'s 1M store within one 4 KiB block, and sled 2.3 % apart at 100k (64.0–65.5 MiB) and 1.0 % at 1M (534–539 MiB); fjall's 1M store holds 35 files in every build. Log: the native journal (`journal.{log,off,ckpt}`), SQLite's WAL, fjall's journals; redb has none, and sled's log is its data file.

| engine | size | MiB | bytes/record | of which log MiB | apparent MiB | files | load s | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|
| native | 100k | 28.5 | 298 | 10.1 | 28.4 | 14.0 | 3.70 [3.29–3.74] | 11.2/10.0/8.9 (8.9–11.2) | 3.8–7.2 ms |
| sqlite | 100k | 19.5 | 205 | 0.00 | 19.5 | 1.00 | 0.873 [0.717–1.15] | 9.6/9.4/7.7 (7.7–9.6) | 3.7–4.0 ms |
| redb | 100k | 39.9 | 418 | 0.00 | 64.3 | 1.00 | 0.634 [0.518–0.722] | 10.0/10.5/8.3 (8.3–10.5) | 3.8–3.9 ms |
| fjall | 100k | 40.9 | 429 | 21.6 | 83.3 | 32.0 | 2.19 [1.31–2.52] | 10.0/10.5/8.7 (8.4–10.5) | 3.8–4.0 ms |
| fjall-unflushed | 100k | 21.6 | 227 | 21.6 | 64.0 | 24.0 | 1.05 [1.02–1.08] | 10.7/9.4/7.9 (7.9–10.7) | 3.7–3.9 ms |
| sled | 100k | 65.4 [64.0–65.5] | 686 [672–687] | 0.00 | 65.4 [64.0–65.5] | 2.00 | 2.08 [2.04–2.23] | 9.6/10.4/9.7 (9.5–10.4) | 3.8–4.0 ms |
| native | 1M | 284 | 298 | 101 | 284 | 14.0 | 39.8 [39.1–40.6] | 11.2/9.7/8.9 (7.7–11.2) | 3.8–7.2 ms |
| sqlite | 1M | 197 | 206 | 0.00 | 197 | 1.00 | 17.5 [16.0–17.9] | 9.6/9.4/7.7 (7.7–10.5) | 3.7–3.9 ms |
| redb | 1M | 460 | 483 | 0.00 | 514 | 1.00 | 13.0 [12.2–13.1] | 10.0/10.5/8.3 (8.2–10.9) | 3.8–3.9 ms |
| fjall | 1M | 235 | 247 | 43.1 | 256 | 35.0 | 15.1 [14.8–15.6] | 10.2/10.5/8.4 (7.9–10.7) | 3.8–4.2 ms |
| fjall-unflushed | 1M | 385 | 403 | 216 | 406 | 31.0 | 15.4 [15.2–15.6] | 10.7/9.6/7.9 (7.9–10.7) | 3.8–3.9 ms |
| sled | 1M | 538 [534–539] | 565 [559–565] | 0.00 | 538 [534–539] | 2.00 | 39.5 [39.2–41.6] | 9.5/10.4/9.7 (8.4–10.4) | 3.7–3.9 ms |

**Verdict: the native store is third of the four durable engines: 284 MiB for 1M changesets, 298 bytes each: 1.45× SQLite's 197 MiB (206 bytes), 1.21× fjall's 235 after its flush (247), 0.62× redb's 460 (483) (sled, a reference line: 538, at 565 bytes).** Over a third of it is the journal (101 MiB), which the native store keeps whole as the record of truth, the index being a projection of it that replay can rebuild (storage-core §2). The rest is the columnar index, which stores each id and each delta hash twice more, in its column and in its sorted view: 112 bytes of columns and parents and 80 of sorted views per record (from the 1M seed's file list: `meta/idx/{ids,dh}.col` and `meta/parents.log` 32 bytes each, `meta/idx/{gen,par}.col` 8; `meta/sorted/{ids,dh}.key` 32, `meta/sorted/{ids,dh}.rec` 8), the same per record at 100k. SQLite's B-trees hold each key once per index; redb's copy-on-write file keeps free pages (514 MiB apparent, 460 allocated); fjall's active journal is 18 % of its flushed 1M store, and at 100k all of its journal stays beside the new tables (40.9 MiB, against 21.6 MiB unflushed, all journal).

### W6: a reader while a writer commits

On a copy of the 100k store: a reader looks a changeset up by id in a fresh read transaction each time (sled has no read snapshots: its reader looks up the latest state), 20,000 times idle, then for 10 s while a writer commits W1's transaction back to back. Reader and writer are two threads of one process, each with a session of its own (SQLite: two connections), for every engine. µs:

| engine | idle p50 | idle p99 | busy p50 | busy p99 | busy p99.9 | busy max | writer commits/s | load at start (range) | raw fdatasync |
|---|---|---|---|---|---|---|---|---|---|
| native | 51.4 [51.1–51.4] | 92.8 [88.5–103] | 87.3 [86.3–87.7] | 179 [178–182] | 1,289 [1,261–1,299] | 4,992 [4,440–5,284] | 64.1 [63.8–64.1] | 10.5/10.9/10.4 (9.9–10.9) | 3.7–4.1 ms |
| sqlite | 4.37 [4.15–4.57] | 7.00 [6.43–11.9] | 3.21 [3.17–3.29] | 9.59 [9.29–10.0] | 83.7 [81.5–85.6] | 1,026 [986–1,075] | 374 [357–387] | 10.8/11.1/9.9 (9.9–11.7) | 3.7–5.6 ms |
| sqlite-normal | 4.73 [4.25–4.80] | 10.2 [9.55–11.6] | 4.80 [4.80–4.89] | 46.3 [44.9–47.2] | 118 [116–121] | 648 [614–1,295] | 4,585 [4,427–4,662] | 11.7/10.8/10.1 (9.6–11.7) | 3.8–4.0 ms |
| redb | 1.01 [0.642–1.06] | 1.96 [1.10–2.19] | 1.03 [0.992–1.04] | 2.83 [2.73–2.85] | 20.8 [17.6–29.2] | 468 [458–735] | 486 [481–490] | 10.6/10.6/9.6 (9.3–10.9) | 3.9–4.1 ms |
| fjall | 0.721 [0.712–0.822] | 2.73 [1.41–2.81] | 0.782 [0.781–0.802] | 2.51 [2.47–2.56] | 5.40 [5.36–7.40] | 963 [536–5,788] | 257 [256–257] | 10.4/10.7/9.6 (9.6–11.1) | 3.8–4.6 ms |
| sled | 0.861 [0.661–1.42] | 3.00 [2.28–6.13] | 1.34 [1.34–1.35] | 7.14 [6.99–7.20] | 136 [135–137] | 1,154 [965–1,333] | 1,771 [1,764–1,776] | 10.7/11.1/10.4 (10.1–11.1) | 3.8–4.1 ms |

**Verdict: the native reader is never blocked, and stays the slowest.** Idle, a fresh snapshot and lookup takes 51.4 µs p50 (SQLite 4.37, redb 1.01, fjall 0.721; sled, a reference line, 0.861). Beside the writer's 64 commits/s it takes 87.3 µs p50, 179 µs p99, 1.3 ms p99.9 and 5.0 ms at worst [4.4–5.3]. Readers take no lock, so the writer never stalls them; what the writer adds is work per snapshot: the frames past the checkpoint (up to 31) are read and overlaid on every snapshot, and the ref files and the frames are read again when a commit lands meanwhile. The other engines' readers stay at or below 1.1× their idle p50 (SQLite 0.73×, redb 1.02×, fjall 1.08×; sled's 1.34 against an idle 0.861 [0.661–1.42]): SQLite's WAL readers 3.21 µs, redb's 1.03, fjall's 0.782 beside their writers, with worst cases of 0.47–1.2 ms at the median of the runs (fjall's 5.8 ms in one run).

### The verdicts in one place

At the loads and raw `fdatasync` latencies in the tables above: outside W2, load 7.7–15.2 at the start of the runs and 3.7–5.6 ms per `fdatasync` (7.1–7.2 in one W5 build); W2 at 6.7–9.7 ms, with the two load events named above in its first round of threads.

| workload | the native store against the best durable alternative | the design choice behind it |
|---|---|---|
| W1, one writer | **loses**: 59.5 commits/s against fjall's 394 (6.6×), SQLite's 417 (7.0×) and redb's 450 (7.6×) | a frame per transaction with two `fdatasync`s, plus the moved ref's file replaced with two more syncs: 4.23 syncs per commit where the others make one, each about one raw `fdatasync` where the others' one costs about half |
| W2, 32 writers | **loses throughput** (32 transactions/s against SQLite's 92.6, fjall's 156 and redb's 228: 2.8–7.1×), **wins fairness**: spread 1.03–1.06 against 1.2–4.1 (54 once), no `Busy` in five of six runs against SQLite's 71–139 per run, the longest wait 1.6–3.3 s against 27–118 s (outside the load event) | the same four syncs; the flock writer lock's kernel queue |
| W3, lookups | **loses every kind to SQLite and redb**: 3.7–11× SQLite's p50 and 7.4–73× redb's, by kind and size (a `get` 3.7–3.8× SQLite's and 9.7–26× redb's, a prefix lookup 7.3–8.7× SQLite's, a fresh snapshot 9.1–11× SQLite's and 29–73× redb's); against fjall at 1M wins the prefix, delta-hash and miss lookups, ties the `get`, loses the fresh snapshot | the columnar index read with one `pread` per probe and no cache; lock-free readers that open the index for every snapshot |
| W4, open and recovery | **wins the open**: 0.091 ms against SQLite's 0.368 (4.0×), redb's 1.79 and fjall's 468; **loses recovery** to SQLite: 22.4 ms against 5.21 at the median (4.3×), 235 against 5.86 ms at worst (40×); recovers 13–22× faster than redb and fjall at the median and 1.5–2.3× at worst (235 against 342 and 529 ms) | the batched index sync and a checkpoint every 32 frames bound what an open or a recovery replays; the recovering write is a four-sync commit, and a repair after a torn frame checks every listed frame |
| W5, size | **third of the four durable engines**: 284 MiB for 1M, 1.45× SQLite's 197, 1.21× fjall's 235, 0.62× redb's 460 (sled, a reference line: 538) | the journal kept whole beside a columnar index that stores each key twice more |
| W6, a reader beside a writer | **never blocked, slowest**: 87.3 µs p50 against fjall's 0.782, redb's 1.03 and SQLite's 3.21 (sled, a reference line: 1.34) | lock-free readers that read files for every snapshot and overlay the frames past the checkpoint |

**The journal syncs.** Of a native W1 commit (16.1 ms here), the `fdatasync` of `journal.log` takes 23.7 % and that of `journal.off` 23.4 %; the ref file's temp-file `fsync` and directory `fsync` take 24.2 % and 22.6 %.

### What would change the picture

- **Fewer syncs per commit.** 94 % of a native commit is its four syncs, and the storage-core spec's own target is two per transaction (§3.1, principle 7). Making the ref files durable at the batched checkpoint, as the index already is, would take 46.7 % off a commit (the temp file's `fsync` and the directory's, 24.2 % and 22.6 % in the table, 46.7 before rounding); dropping the `fdatasync` of `journal.off`, as the code review suggests, 23.4 %. Both would leave one sync per commit, where SQLite, redb and fjall are, and a commit of about 5 ms here (16.1 ms less three syncs of 3.70–3.87). Each touches what a reader and the next writer's tail check may trust after a power loss, so each is a spec decision; these shares are what it would buy.
- **Group commit.** No durable engine here puts several writers' commits under one sync, so each one's W2 rate is bounded by its own per-commit sync cost (sled, a reference line, runs W2 above W1: 2,110 against 1,791 transactions/s). With 32 writers that is where the next factor lies, for the native store as for the others.
- **The disk.** Every durable engine here waits on `fdatasync`; the ratios follow the number of syncs per commit for as long as a sync costs more than the rest of a commit (94 % of it here), and the other engines' one sync costs about half of a raw append + `fdatasync` (W1: 1.82–2.34 ms against 3.8–4.7) where each native sync costs about one (3.70–3.87 ms). Most likely every native sync pays for a new length or a new inode that ext4 logs before the sync returns, while redb rewrites pages in place, SQLite rewrites its WAL from the start after each checkpoint and fjall writes into a journal it pre-sized to 64 MiB; journal files preallocated in place would test that. W2 ran on a disk about twice as slow as W1's (a 6.7–9.7 ms probe against 3.8–5.1); the native store's rate fell to 0.54 of its W1 rate and redb's to 0.51, about as much as the disk, fjall's to 0.40 and SQLite's to 0.22, so the ratios between the engines moved (against the native store: SQLite 7.0× in W1, 2.8–2.9× in W2; fjall 6.6×, 4.9×; redb 7.6×, 7.1×); the records do not say why.
- **A long-lived reader.** A long-lived server process (`swarmops mcp` today, a network server after v1) could keep the index's files open and cache the sorted views' upper levels, closing most of W3's and W6's gap; mapping the files is ruled out while `unsafe` is forbidden. A CLI command, which takes one or two snapshots, pays 0.1–0.2 ms.

## M1.6 — publishes under 32 writers: one transaction against two (not measured yet)

`commit --advance` makes a publish one write transaction: the changeset and the move of its ref
in one journal frame, where `commit` and then `ref set --expect` take two. The CLI contention
test measures what that saves under 32 writers (`crates/swarmops-cli/tests/contention.rs`): with
`SWARMOPS_TEST_CLI_CONTENTION_ADVANCE=1` each child publishes its 1,000 commits in one
transaction each, `commit --parent <its previous> --advance release/agent-<n>`, and with `=0` the
same children publish in two, `commit --parent <its previous>`, then `ref set release/agent-<n>
<it> --expect <its previous>`; the parent creates the 32 refs at the base changeset before the
children start. The summary gives publishes/s and the journal frames each publish wrote, so its
**lock passes per publish**: 1 against 2 by construction (a compaction's frame every 1,025
changesets aside), which the test asserts.

The per-child refs are the measurement's artefact, not a ref model: agents publish to `main`
(umbrella decision 7), where every publish in a round but one loses its compare-and-swap,
records nothing and runs again on the winner. So these numbers are never landing throughput;
the shared-`main` compare-and-swap loop is M4's to measure.

Not measured yet: the two runs wait until after the pilot, when they run once, in a quiet window
under the host's heavy-run lock, and fill this section. Each is

```sh
SWARMOPS_TEST_CLI_CONTENTION_ADVANCE=1 cargo nextest run --locked --release -p swarmops-cli \
  --run-ignored ignored-only --test all -E 'test(/^contention::thirty_two/)' --no-capture
```

with `=1`, and again with `=0`.

## M4 — a land's incremental re-merge (not measured yet)

A land that loses its race re-merges incrementally (operations spec §3 (g)): what it merged
before, `L`, onto what the target gained since, over `L`'s delta paths alone
(`Repo::merge_trees`), where the naive retry merges the whole change again from its base
(`Repo::merge`); operations spec §9 item 1 asks for the two side by side. `benches/remerge.rs` of
`swarmops-repo` times both, `incremental/<files>x<gained>` and `full/<files>x<gained>`, for a
change of 1, 10 and 100 files out of a base of 1,000, against a target that gained 1, 10 and 100
changesets elsewhere since the land's first attempt (a store built without syncs, the objects in
the page cache).

Not measured yet: M4's measurements wait until after the pilot, when they run once, in a quiet
window under the host's heavy-run lock (the M4 plan's execution policy), with

```sh
cargo bench --locked -p swarmops-repo --bench remerge
```

## v0.1.0 — `just perf` in a quiet window (2026-10-21)

Head: the v0.1.0 release commit (the one tagged `v0.1.0`), 556 commits after the 2026-10-16 run's,
with M1.5, M1.6, M2, M3, M4, M8 and the v0.1.0 release batch merged in between. `just perf` ran once
(store-level contention → CLI contention → store benches, one after another) under the host's lock
for heavy runs, its three steps built beforehand so that the timed run compiled nothing. Wall time
1,463.7 s: 255.6 s, 463.6 s and 744.5 s for the three steps (the benches: 7 bench binaries, 92
benchmarks). Same host as M0: WSL2 (kernel 5.15.153.1), ext4 (`data=ordered`) on the same virtual
disk, AMD Ryzen 9 7950X3D (16 cores, 32 threads), 106 GiB RAM; Rust 1.99.0, optimized builds. Every
store lives in a fresh temporary directory under `~/.local/state/swarmops-bench`.

The window was quiet: start load 0.93 → end load 2.27, and no other test, build or bench on the
host. The 1-minute load of 7–12 during the CLI test is its own 32 fsyncing processes, and the disk
under the stores was busy 99 % of the store-level test (813 writes/s) and 97 % of the CLI test
(1,086 writes/s), its 32 writers' own load, and 45 % of the benches. The raw `fdatasync` probe (the
median of 40 appends of 120 B, each `sync_data`'d) took 3.56 ms at the start and 3.71 ms at the end,
M1's quiet-disk speed (3.7–3.8 ms; M0 measured 4.1 ms). One run (the M1 experiments above ran each
measurement twice), so every p99 and max below is a single run's. Nearest-rank percentiles;
criterion figures are medians.

| Measurement | Result |
|---|---|
| Store-level 32 × 1,000 `MetaStore::write()` | wall 254.2 s, **125.9 tx/s**, Busy 0; transaction p50 288.35 ms, p99 418.68 ms, max 548.43 ms; lock wait p50 280.74 ms, p99 408.66 ms, max 526.56 ms; in lock (transaction minus lock wait) p50 7.16 ms, p99 21.36 ms, max 89.75 ms; spread 1.03×; uncontended `write()` p50 0.07 ms, p99 0.18 ms; uncontended transaction p50 7.01 ms, p99 20.50 ms; journal 32,031 frames (31 `Compacted`), 1 past `journal.ckpt`, `journal.log` 3,711,565 bytes |
| CLI 32 × 1,000 `swarmops commit` | 32,000 commits in 446.1 s, **71.7 commits/s**, `E_BUSY` 0 (control: `E_BUSY` after 10,009 ms); `meta.elapsed_ms` mean 442, p50 433, p90 487, p99 621, p99.9 680, max 712 ms; spawn to exit p50 436.2, p99 624.0, max 715.8 ms; spread 1.00× (children 444.6–446.0 s); no commit over 1 s; uncontended commit p50 19 ms (p99 32, max 32; wall p50 22.1 ms); estimated lock wait mean 423, p50 414, p99 602, max 693 ms; `verify --deep` clean, 128,306 objects and 32,101 changesets in 1,758 ms; `log -n 5` 5 entries in 0 ms; the whole test 460.3 s |
| `fsync_latency` (M0's instrument) | no fsync 22.5 µs; `sync_data` 3.53 ms; `sync_all` 3.56 ms; `sync_all` + rename + directory fsync 7.04 ms |
| `fsync_fanout` | serial 17.3 ms, parallel 7.09 ms (2.44×), one file 3.51 ms; `index_sync/1` 11.4 ms vs `index_sync/5` 4.62 ms (2.47×) |
| `index_lookup` (criterion median) | 10k: find 2.26 µs compacted / 3.07 µs with a 1,000-record tail; 100k: 2.52 / 3.46 µs; 200k: 2.54 / 3.49 µs; `find_prefix` ≤ 6.13 µs; `find_delta_hash` ≤ 4.50 µs |
| `put_batch` 256 × 4 KiB + flush | 1 thread 954 ms (268 obj/s), 8 threads 224 ms, **32 threads 72.9 ms (3,509 obj/s, 13.1×)**; with a busy writer 1,493 / 296 / 100 ms (**14.9×**); `put_batch` alone 940 / 223 / 66.3 ms (14.2×); `flush_only` 5.08 ms (33.7 ms busy) |
| `read_snapshot` (`read()` + `changeset(id)`, p50 / p99) | 10k: 50.9 / 101.5 µs (ckpt at end), 68.4 / 132.7 µs (31 frames past); 100k: 55.2 / 107.6 µs, 71.9 / 152.9 µs; `journal.off` search 3.50–4.60 µs. Refs on 10k changesets: 0 refs 55.8 / 113.6 µs, 16 refs 145 / 282 µs, **256 refs 1.55 ms / 3.00 ms**; `show main` as two snapshots / as one (p50) at 0, 16 and 256 refs: 110 / 59.1 µs, 289 / 149 µs, 3.13 / 1.55 ms |
| `compaction` (criterion median; p50, p90, max over every compaction) | 100k records: 52.1 ms (p50 51.7, p90 53.4, max 55.3 ms over 131 compactions, 8,000,000 bytes written each); 1M records: 201 ms (p50 200.0, p90 211.5, max 221.4 ms over 37, 80,000,000 bytes each) |
| `history` (200k / 1M changesets, p50) | `find` hit 2.44 / 2.58 µs, miss 1.80 / 1.81 µs; the bisection copy: hit 5.86 / 7.23 µs, miss 5.51 / 6.50 µs; `insert_changeset` with no tail 4.81 / 4.74 µs, with a 1,024-record tail 8.03 / 8.59 µs; 100-step first-parent walk 419 / 479 µs by `changeset`, 375 / 411 µs by `first_parent`; `reflog(100)` with 31 frames past the checkpoint 30.9 / 154 ms (criterion median) |

`compaction`, `history`, and `read_snapshot`'s store with refs have no earlier number in this file.
`compaction` times one `Index::compact`, every sync on, as a writer makes it while it holds
`meta/lock` once the unsorted tail has passed its threshold: it rewrites both sorted views whole, 80
bytes per record. `history` times, on linear chains of 200k and 1M changesets, `Index::find` (an
interpolation search) against a plain bisection the bench copies, on the same reads, an insert that
scans an empty and a full (1,024-record) unsorted tail, 100 first-parent steps by whole records
(`changeset`) and by parent entries (`first_parent`), and the reflog of `main`'s last 100 moves.
`read_snapshot`'s third store holds 0, 16 or 256 `release/*` refs besides `main`; `show main`
resolved `main` in one snapshot and read its changeset in a second, and makes both reads in one now.

**The lock pass, store against CLI.** At store level the lock passes every 7.94 ms (254.2 s /
32,000), 2.2 raw `fdatasync`s of the 3.56 ms probe; an uncontended transaction takes 7.01 ms and a
contended one 7.16 ms in the lock. Through the binary it passes every 13.94 ms (446.1 s / 32,000),
3.9 raw `fdatasync`s: 6.0 ms more (2026-10-16: 8.27 and 13.75 ms, 5.5 ms more). The 2026-10-16
section puts that difference down to the commits' own object writes on the same filesystem; this run
did not isolate it.

**The wait.** The lock rotated at both levels. At store level the wait's p50, 280.74 ms, is 35.3
passes (about one round of the other 31 writers), its p99 51.4 passes and its max 66.3. Through the
binary, estimated as `meta.elapsed_ms` minus 19 ms: p50 29.7, p99 43.2, max 49.7 passes. 7c's
rotating runs had a p99 of 131–157 passes and a longest wait of 283–359, and a uniformly random
winner among 31 waiters gives a p99 of 140 and a longest wait near 316 ("M1 store fixes (7c)"). Both
tails stayed under 0.7 s, against the 10 s timeout. The 2026-10-16 store-level run was in the other
regime, the committing writer taking the lock straight back (wait p50 0.11 ms, p99 108 passes, max
145). The lock's queueing is the code it was then (only its open and its error messages changed),
and 7c found that the regime depends on the host.

**Against the 2026-10-16 run.**

- **Unchanged within 5 %:** the store-level and CLI throughput (125.9 transactions/s against 120.9,
  71.7 commits/s against 72.7), the uncontended transaction (7.01 ms p50 against 7.24),
  `fsync_fanout`, `put_batch` with and without the busy writer, and `flush`. The `read_snapshot`
  medians moved by at most 7 %, and the uncontended commit went from 21 to 19 ms (whole
  milliseconds). The disk ran at M1's quiet-disk speed; the host's start load was 0.93, against 16.4
  then.
- **The lock wait changed regime, not throughput**, on the same queueing code (above). Its tails are
  the shortest recorded for either test (store p99 0.41 s, longest 0.53 s; CLI p99 0.60 s, longest
  0.69 s, estimated), at the lowest start load of any run (M1's runs started at 7.6–33).
- **`index_lookup` is about 2× faster** (−43 % to −63 %). Two commits between the runs aim at it,
  `72f9112` (a guarded interpolation search over the sorted views) and `092d077` (`find` without
  re-reading the key it found, and lookups that read one field), and the 2026-10-16 run had a busier
  host (start load 16.4, a debug build of another worktree during its benches); this run does not
  separate the causes.
- **`read_snapshot`'s p99 is higher** with 31 frames past the checkpoint (132.7 and 152.9 µs,
  against ≤ 126 µs): single-run tails, while its p50s moved by at most 7 %.

**M0's fsync, inside `just perf`.** The criterion means, 22.4 µs, 3.54 ms, 3.58 ms and 7.07 ms, are
54 %, 13 %, 11 % and 20 % below M0's (49 µs, 4.07, 4.01 and 8.81 ms): the same speed class. M0's
no-fsync mean was twice its own per-write p50 (23 µs), where this run's mean and median agree (22.4
and 22.5 µs). `just perf` takes no per-write percentiles, so M0's per-write table is not
re-measured; the probe's 40 appends at the start had a p90 of 4.09 ms and a worst of 5.56 ms.

Decisions confirmed on this run: the kernel-queue lock keeps `Busy` and `E_BUSY` at 0 of 32,000 at
both levels, the longest waits 0.53 s (store) and 0.69 s (CLI, estimated), so `lock_timeout` stays
at 10 s; parallel `Index::sync` is 2.47× serial (decision (b) asks at least 2×); 32-thread
`put_batch` + `flush` reaches 13.1× the 1-thread rate, and 14.9× with the busy writer (decision (a)
reconsiders the flag below 4×), so a `--durability fast` flag is not justified by this data.

Not in `just perf`, so not refreshed: M0's per-write percentiles (`fsync_percentiles`), the M1.5
comparison (`just bench-compare`), and the M1.6 publish pair and M4's re-merge bench above, which
stay not measured.
