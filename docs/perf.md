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
