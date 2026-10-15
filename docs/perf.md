# Performance ledger

Numbers measured on this host (WSL2, kernel 5.15, ext4 on the virtual disk `/dev/sdd`, 32 cores). Re-measure after any storage-layer change; `just bench` fails on >20 % regression once `bench/thresholds.toml` exists (M9).

## M0 fsync latency (2026-10-15)

`cargo bench -p swarmops-store` (`crates/swarmops-store/benches/fsync_latency.rs`), one fresh 4 KiB file per iteration under `~/.local/state/swarmops-bench`, 60 samples each; percentiles are per iteration.

| variant | p50 | p90 | p99 | max |
|---|---|---|---|---|
| write only, no fsync | 50 µs | 55 µs | 60 µs | 60 µs |
| write + `sync_data` | 4.05 ms | 4.27 ms | 4.68 ms | 4.68 ms |
| write + `sync_all` | 3.99 ms | 4.25 ms | 4.57 ms | 4.57 ms |
| write + `sync_all` + rename + directory fsync | 7.99 ms | 10.8 ms | 14.5 ms | 14.5 ms |

**Decision.** p99 of a durable single-file write is 4.6 ms, under the 20 ms threshold the M0 plan set, so **no `--durability fast` flag is introduced now**; the default stays full fsync (objects `sync_all` + one directory fsync per commit + SQLite `synchronous=FULL`).

**Consequence for M1.** A commit that writes N new objects pays ~4 ms per object if the fsyncs are serial (100 files ≈ 0.4 s), which cannot meet the "≤ 5 ms + 0.3 ms per changed file" commit target. `FsObjectStore::put_batch` must issue the per-file fsyncs in parallel (rayon) and fsync each touched fan-out directory once at `flush()`. M1's acceptance tests measure `put_batch` throughput at 1, 8 and 32 concurrent fsyncs; if the virtual disk serializes flushes (throughput does not scale), the `--durability fast` option (skip per-object fsync, keep the SQLite WAL fsync, documented as "safe against process crash, not power loss") is reconsidered with that measurement attached here.
