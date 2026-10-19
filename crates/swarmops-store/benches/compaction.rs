//! A compaction on its own (design-issue-6, Stage 0): how long one [`Index::compact`] takes,
//! with every sync it makes, when the unsorted tail has just passed the threshold, and how many
//! bytes it writes. A writer compacts after the commit point of the transaction that takes the
//! tail past [`Index::COMPACT_THRESHOLD`] records, while it still holds `meta/lock`, so every
//! writer queued on the lock waits it out (storage spec §3 "압축").
//!
//! For 100,000 and 1,000,000 records, an index is built outside the timed loop, in a fresh
//! temporary directory under `SWARMOPS_BENCH_DIR` (default `~/.local/state/swarmops-bench`, on
//! the native filesystem), through a filesystem that skips every sync ([`NoSyncFs`]): a chain
//! of real ids (BLAKE3 hashes), each changeset with its own delta hash, compacted into the
//! sorted views but for its last [`TAIL`] records, one past the threshold, which stay in the
//! unsorted tail as a writer finds them. The build is then synced once, and its four view files
//! are kept aside by hard links.
//!
//! Each iteration restores them in untimed setup (`iter_batched`): the view files the last
//! compaction wrote give way to the kept ones, `sorted/` is synced, and a handle on the real
//! filesystem ([`RealFs`]) opens the index. The timed part is [`Index::compact`] on it, every
//! sync on: the five index files' `sync_data`s (clean by then), the merge of each old view with
//! its sorted tail into temp files, the temp files' four `sync_data`s and their four durable
//! renames. It rewrites both views whole, 80 bytes per record (a 32-byte key and an 8-byte rec
//! in each), so what it writes grows with the index, not with the tail.
//!
//! Besides criterion's estimate, it prints a line per size with the nearest-rank percentiles of
//! every compaction it timed, warm-up included, and the bytes each one wrote
//! (`percentiles compaction/compact/<size>: …`, a line `just perf` shows).
//! `SWARMOPS_BENCH_COMPACTION_RECORDS=<n>[,<n>…]` measures other sizes (each above [`TAIL`]
//! records), for a quick check of the bench.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use criterion::{BatchSize, BenchmarkId, Criterion, SamplingMode, criterion_group, criterion_main};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
use swarmops_store::{ChangesetMeta, FsOps, Index, Layout, LinkOutcome, RealFs};
use swarmops_testkit::measure::{base_dir, percentile};
use tempfile::TempDir;

/// The index sizes measured, in records, unless [`RECORDS`] says otherwise.
const SIZES: [u64; 2] = [100_000, 1_000_000];

/// Set to measure other sizes: record counts, comma-separated, each above [`TAIL`].
const RECORDS: &str = "SWARMOPS_BENCH_COMPACTION_RECORDS";

/// The records in the unsorted tail when a writer compacts: one past
/// [`Index::COMPACT_THRESHOLD`], since it compacts once the tail holds more than that.
const TAIL: u64 = Index::COMPACT_THRESHOLD + 1;

/// The four files of the two sorted views, in `meta/sorted/`, which a compaction rewrites.
const VIEW_FILES: [&str; 4] = ["ids.key", "ids.rec", "dh.key", "dh.rec"];

/// What the two views hold per record: a 32-byte key and an 8-byte rec each.
const VIEW_BYTES_PER_RECORD: u64 = 2 * (32 + 8);

/// The real filesystem without a single flush to stable storage: for building the index, which
/// is setup, not what is timed. A copy of `read_snapshot.rs`'s (each bench is a crate of its
/// own).
struct NoSyncFs;

impl FsOps for NoSyncFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        RealFs.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        RealFs.write_durable_temp(dir, bytes)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        RealFs.link_noclobber(from, to)
    }

    fn fsync_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        RealFs.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OpenOptions::new().write(true).open(path)?.set_len(len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = path.parent().expect("a file in a directory");
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        temp.persist(path).map(drop).map_err(|e| e.error)
    }

    fn sync_data(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
}

/// The id of changeset `n`: a real hash, so the ids are spread as a store's are.
fn id(n: u64) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &n.to_le_bytes()))
}

/// Changeset `n` of the chain: the child of changeset `n - 1`, with a delta hash of its own.
fn changeset(n: u64) -> ChangesetMeta {
    ChangesetMeta {
        id: id(n),
        seq: 0,
        generation: n + 1,
        delta_hash: hash_object(ObjectKind::Blob, &n.to_le_bytes()).0,
        parents: n.checked_sub(1).map(id).into_iter().collect(),
    }
}

/// `records` as the bench names a size: `100k`, `1M`.
fn size_name(records: u64) -> String {
    if records.is_multiple_of(1_000_000) {
        format!("{}M", records / 1_000_000)
    } else if records.is_multiple_of(1_000) {
        format!("{}k", records / 1_000)
    } else {
        records.to_string()
    }
}

/// The sizes to measure: [`RECORDS`]'s, or [`SIZES`].
fn sizes() -> Vec<u64> {
    let Ok(list) = std::env::var(RECORDS) else {
        return SIZES.to_vec();
    };
    list.split(',')
        .map(|n| {
            let records: u64 = n
                .trim()
                .replace('_', "")
                .parse()
                .unwrap_or_else(|_| panic!("{RECORDS}: {n} is not a number of records"));
            assert!(
                records > TAIL,
                "{RECORDS}: an index of {records} records has no tail of {TAIL}"
            );
            records
        })
        .collect()
}

/// Syncs every file and directory under `dir`, and `dir` itself, on the real filesystem: the
/// build reaches the disk before anything is timed.
fn sync_tree(dir: &Path) {
    for entry in fs::read_dir(dir).expect("list a directory") {
        let entry = entry.expect("a directory entry");
        let path = entry.path();
        if entry.file_type().expect("a file type").is_dir() {
            sync_tree(&path);
        } else {
            RealFs.sync_data(&path).expect("sync a file");
        }
    }
    RealFs.fsync_dir(dir).expect("sync a directory");
}

/// An index of `records` records as a writer finds it when it compacts, its views kept aside:
/// see the module documentation.
struct Fixture {
    records: u64,
    layout: Layout,
    /// `meta/sorted/`.
    sorted: Utf8PathBuf,
    /// The view files as built, hard links of the ones `sorted/` held then.
    kept: Utf8PathBuf,
    /// The store root, removed when the fixture drops.
    _dir: TempDir,
}

impl Fixture {
    /// Builds the index of `records` records in a fresh directory under `base`, syncs it, and
    /// keeps its views aside.
    fn build(base: &Path, records: u64) -> Fixture {
        let dir = tempfile::tempdir_in(base).expect("a store dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 dir");
        let layout = Layout::new(root.clone());
        let mut index = Index::create(&layout, Arc::new(NoSyncFs)).expect("create the index");
        let covered = records - TAIL;
        for n in 0..covered {
            index.append(&changeset(n)).expect("append");
        }
        assert_eq!(index.compact().expect("compact"), covered);
        for n in covered..records {
            index.append(&changeset(n)).expect("append");
        }
        assert_eq!(index.tail_len(), TAIL);
        drop(index);
        let sorted = layout.meta_path("sorted");
        let kept = root.join("kept");
        fs::create_dir(&kept).expect("make the kept views' dir");
        for name in VIEW_FILES {
            fs::hard_link(sorted.join(name), kept.join(name)).expect("keep a view file");
        }
        sync_tree(dir.path());
        Fixture {
            records,
            layout,
            sorted,
            kept,
            _dir: dir,
        }
    }

    /// Puts the kept view files back in `sorted/`, syncs it, and opens the index on the real
    /// filesystem, its tail as built.
    fn restore(&self) -> Index {
        for name in VIEW_FILES {
            let view = self.sorted.join(name);
            fs::remove_file(&view).expect("remove a view file");
            fs::hard_link(self.kept.join(name), &view).expect("put a kept view file back");
        }
        RealFs
            .fsync_dir(self.sorted.as_std_path())
            .expect("sync sorted/");
        let index = Index::open(&self.layout, Arc::new(RealFs)).expect("open the index");
        assert_eq!((index.len(), index.tail_len()), (self.records, TAIL));
        index
    }

    /// The bytes the four view files hold.
    fn view_bytes(&self) -> u64 {
        VIEW_FILES
            .iter()
            .map(|name| {
                fs::metadata(self.sorted.join(name))
                    .expect("a view file")
                    .len()
            })
            .sum()
    }
}

/// `duration` in milliseconds.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn compaction(c: &mut Criterion) {
    let base = base_dir();
    let mut group = c.benchmark_group("compaction");
    group
        .sample_size(10)
        .sampling_mode(SamplingMode::Flat)
        .warm_up_time(Duration::from_secs(1));
    for records in sizes() {
        let name = size_name(records);
        let mut built: Option<Fixture> = None;
        let mut times: Vec<Duration> = Vec::new();
        group.bench_function(BenchmarkId::new("compact", &name), |b| {
            // Built at the first call, and only when this size is not filtered out.
            let fixture = &*built.get_or_insert_with(|| Fixture::build(&base, records));
            b.iter_batched(
                || fixture.restore(),
                |mut index| {
                    let started = Instant::now();
                    let covered = index.compact().expect("compact");
                    times.push(started.elapsed());
                    assert_eq!(covered, records, "the views cover every record");
                    index
                },
                BatchSize::PerIteration,
            );
        });
        let Some(fixture) = built else {
            continue;
        };
        let bytes = fixture.view_bytes();
        assert_eq!(bytes, VIEW_BYTES_PER_RECORD * records, "both views, whole");
        times.sort_unstable();
        println!(
            "percentiles compaction/compact/{name}: n {} p50 {:.1} ms p90 {:.1} ms max {:.1} ms; \
             {bytes} bytes written each ({records} records x {VIEW_BYTES_PER_RECORD} B, \
             {TAIL} of them in the tail)",
            times.len(),
            millis(percentile(&times, 50)),
            millis(percentile(&times, 90)),
            millis(times[times.len() - 1])
        );
    }
    group.finish();
}

criterion_group!(benches, compaction);
criterion_main!(benches);
