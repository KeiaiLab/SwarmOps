//! G9's `ws commit` row as G2 is measured (the M19 design §8.2, decision D1): one `ws commit` of
//! `k` changed files (1, 10 and 100) of the 50,000-file tree, the workspace's durable files on
//! the test kit's `NoSyncFs`, and apart from it the hashing of those `k` files alone.
//!
//! ```text
//!  SWARMOPS_BENCH_DIR/<tmp>/store   50,000 files in 2,500 directories, `main` on them
//!  SWARMOPS_BENCH_DIR/<tmp>/w       a workspace of all of it, its index warm
//!       each iteration: k files rewritten (untimed) ──▶ swarmops_ws::commit (timed)
//! ```
//!
//! - `ws_commit/nosync/<k>`: the commit's wall time, the scan of the tree included, with no sync
//!   of the workspace's files. The store under it syncs as the product does: `LocalOps` takes no
//!   other filesystem outside its own tests, so this is an upper bound of G2's "syncs
//!   excluded".
//! - `ws_commit/hashing/<k>`: reading and hashing the `k` files, which "plus hashing" names.
//!
//! Each variant also prints the nearest-rank percentiles of every commit it timed, warm-up
//! included (`percentiles ws_commit/…`, a line `just perf` shows). The product's default stays
//! full fsync: the no-sync filesystem exists only in this bench. `docs/perf.md` records it; the
//! gate only builds it.

use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use criterion::{Criterion, criterion_group, criterion_main};
use swarmops_api::{
    ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, ModeInput, StatusAfter, StoreOps,
    WsCommitReq, WsCreateReq, WsStatusReq,
};
use swarmops_core::ObjectKind;
use swarmops_ops::LocalOps;
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::measure::{base_dir, percentile};
use swarmops_testkit::ws::{TREE_DIRS, TREE_FILES, content_of, path_of, rewrite_first};
use swarmops_ws::{Ctx, RealHost};

/// The changed files per commit measured.
const FILES: [usize; 3] = [1, 10, 100];

/// The files of one seeding commit.
const CHUNK: usize = 5_000;

/// Who commits.
const AGENT: &str = "bench/ws-commit";

/// The workspace's name.
const WS: &str = "w";

/// Fewer timed commits than this, as criterion's test mode makes, print no percentiles.
const MIN_SAMPLES: usize = 10;

/// A store holding the 50,000-file tree and a workspace of all of it.
struct Fixture {
    /// The directory of both, removed when the fixture is dropped.
    _dir: tempfile::TempDir,
    /// The store's root.
    root: Utf8PathBuf,
    /// The workspace's root.
    ws: Utf8PathBuf,
    /// The store's operations.
    ops: LocalOps,
}

impl Fixture {
    /// The store seeded in chunks, `main` on the tree, and the workspace made with a warm index.
    fn build() -> Fixture {
        // 1. The store, under the bench directory.
        let dir = tempfile::tempdir_in(base_dir()).expect("a temp dir");
        let at = Utf8Path::from_path(dir.path()).expect("UTF-8");
        let root = at.join("store");
        let (ops, _) = LocalOps::init(&root, "bench").expect("init");

        // 2. The tree, one commit per chunk.
        let who = Identity::parse(AGENT).expect("an identity");
        let mut head: Option<String> = None;
        for start in (0..TREE_FILES).step_by(CHUNK) {
            let changes = (start..TREE_FILES.min(start + CHUNK))
                .map(|i| put(path_of(i, TREE_DIRS), content_of(i, 0)))
                .collect();
            let mut req = CommitReq::new(changes, "seed");
            req.parent = head.take().map(Into::into);
            req.advance = Some("main".to_owned());
            head = Some(ops.commit(&who, req).expect("seed").changeset);
        }

        // 3. The workspace, then a status: its index written.
        let fixture = Fixture {
            ws: at.join(WS),
            _dir: dir,
            root,
            ops,
        };
        let mut req = WsCreateReq::new(WS.to_owned());
        req.path = Some(fixture.ws.to_string());
        req.all = true;
        req.status_after = StatusAfter::None;
        swarmops_ws::create(&fixture.ctx(), req).expect("create");
        swarmops_ws::status(&fixture.ctx(), WsStatusReq::new(WS)).expect("status");
        fixture
    }

    /// The workspace operations over the real host, the workspace's files never synced.
    fn ctx(&self) -> Ctx<'_> {
        let who = Identity::parse(AGENT).expect("an identity");
        Ctx::new(
            &self.ops,
            self.root.clone(),
            Some(who),
            Arc::new(NoSyncFs),
            Arc::new(RealHost::default()),
        )
    }

    /// Rewrites the first `files` files at `version` (untimed), then commits them: how long the
    /// commit took.
    fn commit(&self, files: usize, version: u32) -> Duration {
        rewrite_first(self.ws.as_std_path(), TREE_DIRS, files, version).expect("rewrite");
        let mut req = WsCommitReq::new(WS.to_owned(), "a bench commit".to_owned());
        req.status_after = StatusAfter::None;
        let ctx = self.ctx();
        let started = Instant::now();
        let made = swarmops_ws::commit(&ctx, req).expect("commit");
        let took = started.elapsed();
        black_box(made);
        took
    }
}

/// A put of inline `text` at `path`, a plain file.
fn put(path: String, text: String) -> ChangeInput {
    let mut change = ChangeInput::new(path, ChangeOp::Put);
    change.content = Some(ContentInput::Inline(text));
    change.mode = Some(ModeInput::File);
    change
}

/// Reads and hashes the first `files` files of the tree below `root`, as a commit hashes them.
fn hash_first(root: &Path, files: usize) {
    for i in 0..files {
        let bytes = std::fs::read(root.join(path_of(i, TREE_DIRS))).expect("read");
        black_box(swarmops_core::hash_object(ObjectKind::Blob, &bytes));
    }
}

/// Milliseconds, for the printed lines.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Every variant, on one fixture: the commits of `k` files, then their hashing.
fn ws_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("ws_commit");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    let fixture = Fixture::build();
    let mut version = 1;
    for files in FILES {
        // 1. Each commit of `files` changed files, timed alone.
        let mut samples: Vec<Duration> = Vec::new();
        let name = format!("nosync/{files}");
        group.bench_function(&name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let took = fixture.commit(files, version);
                    total += took;
                    samples.push(took);
                    version += 1;
                }
                total
            });
        });

        // 2. The hashing of those files alone.
        let root = fixture.ws.as_std_path();
        group.bench_function(format!("hashing/{files}"), |b| {
            b.iter(|| hash_first(root, files));
        });

        // 3. The percentiles, once criterion has measured (its test mode runs once).
        if samples.len() < MIN_SAMPLES {
            continue;
        }
        samples.sort_unstable();
        let target = 5.0 + 0.3 * f64::from(u32::try_from(files).unwrap_or(u32::MAX));
        println!(
            "percentiles ws_commit/{name}: n {} p50 {:.2} ms p90 {:.2} ms p99 {:.2} ms max \
             {:.2} ms; G2's target {target:.1} ms plus hashing",
            samples.len(),
            millis(percentile(&samples, 50)),
            millis(percentile(&samples, 90)),
            millis(percentile(&samples, 99)),
            millis(samples[samples.len() - 1]),
        );
    }
    group.finish();
}

criterion_group!(benches, ws_commit);
criterion_main!(benches);
