//! The FORMAT 3 measurements at 1,000,000 records (the M20 plan's task M20-7; the design §2.5
//! cost, §3.4, §7; amendment A7): G18's lock hold, A5's flat commit cost, A1's commit against a
//! small store, and the cost of `upgrade` on 200,000 changesets. Each case prints one
//! `format3-perf: ` line per figure and asserts what it ran, never a time: a missed target is
//! recorded in `docs/perf.md`, and becomes a known limitation, never a lowered target.
//!
//! ```text
//!   commits_1m_next_decile ──▶ $SWARMOPS_PERF_STORE_DIR/m20-1m/{store, results.txt, samples.txt}
//!        (ten calls, one decile of 100,000 commits each, resumed from results.txt alone)
//!   a5_verdict ────────────────▶ reads the ten lines: last p99 ≤ 1.5 × first, lookup and
//!                                snapshot p99 < 1 ms (in `just perf` only)
//!   lock_hold_at_1m_records ───▶ a copy of the scale fixture's large store: lock hold of a
//!                                commit, a land and a ref move, 1,000 each
//!   commit_cost_on_1m_against_1k ▶ commits of 1, 10, 100 files on the large and the small store
//!   upgrade_200k_measured ─────▶ a FORMAT 2 store of 200,000 changesets, `upgrade`
//! ```
//!
//! The cases on the scale fixtures run only with `SWARMOPS_SCALE=1` (`bulk::gate`): without it
//! each prints `format3-perf: <case> not run` and returns, so that a run of every ignored test
//! (`just check-slow`) does not build a store of a million changesets. The decile case is gated
//! by nothing but its store directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{
    BlobId, ChangesetId, LeafKind, ObjectKind, Signature, Tree, TreeEntry, hash_object,
};
use swarmops_repo::{Advance, Change, CommitRequest, LandRequest, LocalRepo, RawChangeset};
use swarmops_store::StoreFormat;
use swarmops_store::fs_ops::counters::Counters;
use swarmops_testkit::bulk::{self, Scale, Size};
use swarmops_testkit::measure::{base_dir, percentile};
use swarmops_testkit::repo::{put, request};

/// The variable that names the directory of the persistent 1,000,000-commit store.
const STORE_DIR_ENV: &str = "SWARMOPS_PERF_STORE_DIR";

/// Where that store lives unless [`STORE_DIR_ENV`] says otherwise: beside the runners' paths.
const DEFAULT_STORE_DIR: &str = "/data/swarmops/perf";

/// The variable that sets the commits of one decile, for a trial: the figures then name that
/// size, not A7's. Unset, it is [`DECILE_COMMITS`].
const COMMITS_ENV: &str = "SWARMOPS_PERF_COMMITS";

/// The commits of one decile of the 1,000,000.
const DECILE_COMMITS: usize = 100_000;

/// The deciles of a whole run.
const DECILES: usize = 10;

/// The directory of the persistent store under the perf directory.
const STORE_NAME: &str = "m20-1m";

/// The deciles' lines, in the perf directory of the store.
const RESULTS: &str = "results.txt";

/// The ids of every `SAMPLE_EVERY`-th commit, kept for the lookups of later deciles.
const SAMPLES: &str = "samples.txt";

/// Every how many commits a lookup and a snapshot are timed, and an id is kept.
const SAMPLE_EVERY: usize = 100;

/// The ref every case commits on.
const MAIN: &str = "main";

/// Who moves refs in the cases.
const ACTOR: &str = "perf/format3";

/// A5's bound on the growth of the commit's p99 from the first decile to the last, in
/// hundredths.
const GROWTH_MAX_HUNDREDTHS: u64 = 150;

/// A5's bound on a lookup's and a snapshot's p99, in microseconds.
const READ_MAX_US: u64 = 1_000;

/// G18's bound on a lock hold.
const HOLD_MAX: Duration = Duration::from_millis(100);

/// The operations of each kind the lock-hold case makes.
const HOLD_OPS: usize = 1_000;

/// The changesets of the upgrade case's FORMAT 2 store.
const UPGRADE_CHANGESETS: usize = 200_000;

/// The changesets of one frame of the upgrade case's build: the import's default.
const FRAME: usize = 8_192;

/// The files of A1's commits.
const FILE_COUNTS: [usize; 3] = [1, 10, 100];

/// How many commits each size of A1's case times.
const A1_COMMITS: usize = 50;

/// The figures of one decile, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decile {
    commit_p50: u64,
    commit_p99: u64,
    commit_max: u64,
    step_p50: u64,
    step_p99: u64,
    step_max: u64,
    lookup_p99: u64,
    snapshot_p99: u64,
}

impl Decile {
    /// The line `results.txt` holds for decile `k`.
    fn line(&self, k: usize) -> String {
        format!(
            "decile {k}: commit_p50_us={} commit_p99_us={} commit_max_us={} step_p50_us={} \
             step_p99_us={} step_max_us={} lookup_p99_us={} snapshot_p99_us={}",
            self.commit_p50,
            self.commit_p99,
            self.commit_max,
            self.step_p50,
            self.step_p99,
            self.step_max,
            self.lookup_p99,
            self.snapshot_p99,
        )
    }

    /// The decile a line spells, or `None` for any other text.
    fn parse(line: &str) -> Option<(usize, Decile)> {
        let rest = line.strip_prefix("decile ")?;
        let (k, figures) = rest.split_once(": ")?;
        let mut values = figures.split(' ').map(|pair| {
            let (_, value) = pair.split_once('=')?;
            value.parse::<u64>().ok()
        });
        let mut next = || values.next().flatten();
        let decile = Decile {
            commit_p50: next()?,
            commit_p99: next()?,
            commit_max: next()?,
            step_p50: next()?,
            step_p99: next()?,
            step_max: next()?,
            lookup_p99: next()?,
            snapshot_p99: next()?,
        };
        Some((k.parse().ok()?, decile))
    }
}

/// What `a5_verdict` found.
#[derive(Debug, PartialEq, Eq)]
struct Verdict {
    /// The last decile's commit p99 over the first's, in hundredths.
    growth_hundredths: u64,
    /// Whether that is within A5's bound.
    growth_ok: bool,
    /// Whether every decile's lookup and snapshot p99 is below A5's bound.
    reads_ok: bool,
}

/// A5's verdict over the lines of `results.txt`; an error naming the missing deciles when it
/// holds fewer than [`DECILES`] (fail-closed: an empty file is no verdict).
fn verdict(lines: &[String]) -> Result<Verdict, String> {
    let mut found: Vec<Option<Decile>> = vec![None; DECILES];
    for line in lines {
        if let Some((k, decile)) = Decile::parse(line)
            && k < DECILES
        {
            found[k] = Some(decile);
        }
    }
    let missing: Vec<usize> = (0..DECILES).filter(|k| found[*k].is_none()).collect();
    if !missing.is_empty() {
        return Err(format!("deciles missing: {missing:?}"));
    }
    let all: Vec<Decile> = found.into_iter().flatten().collect();
    let first = all[0].commit_p99.max(1);
    let last = all[DECILES - 1].commit_p99;
    let growth_hundredths = last * 100 / first;
    Ok(Verdict {
        growth_hundredths,
        growth_ok: last * 100 <= first * GROWTH_MAX_HUNDREDTHS,
        reads_ok: all
            .iter()
            .all(|d| d.lookup_p99 < READ_MAX_US && d.snapshot_p99 < READ_MAX_US),
    })
}

/// The directory of the persistent store: [`STORE_DIR_ENV`]'s, or [`DEFAULT_STORE_DIR`], then
/// [`STORE_NAME`].
fn perf_dir() -> PathBuf {
    let base = std::env::var_os(STORE_DIR_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_STORE_DIR), PathBuf::from);
    base.join(STORE_NAME)
}

/// The commits of one decile this run: [`COMMITS_ENV`]'s, or [`DECILE_COMMITS`].
fn decile_commits() -> usize {
    match std::env::var(COMMITS_ENV) {
        Ok(value) => value.parse().expect("SWARMOPS_PERF_COMMITS is a count"),
        Err(_) => DECILE_COMMITS,
    }
}

/// The lines of `results.txt` in `dir`; none when it is not there.
fn read_results(dir: &Path) -> Vec<String> {
    match fs::read_to_string(dir.join(RESULTS)) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("read {RESULTS}: {e}"),
    }
}

/// Appends `line` to `file` in `dir`, synced.
fn append(dir: &Path, file: &str, text: &str) {
    use std::io::Write;
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file))
        .expect("open for append");
    out.write_all(text.as_bytes()).expect("append");
    out.sync_all().expect("sync");
}

/// The signature of every commit of the cases.
fn signature(k: usize) -> Signature {
    let secs = 1_760_000_000 + i64::try_from(k).expect("an index fits");
    Signature::new(
        b"perf/format3".to_vec(),
        b"agent@perf.swarmops.local".to_vec(),
        secs,
        0,
        false,
    )
    .expect("a zero offset is valid")
}

/// The path of the file commit `n` puts: 100 directories of up to 1,000 files.
fn path_of(n: usize) -> String {
    format!("d{:02}/f{:04}.txt", n % 100, n / 100 % 1_000)
}

/// The bytes commit `n` puts.
fn text_of(n: usize) -> Vec<u8> {
    format!("commit {n}: the quick brown fox jumps over the lazy dog\n").into_bytes()
}

/// `main`'s head in `repo`.
fn head_of(repo: &LocalRepo) -> Option<ChangesetId> {
    repo.get_ref(MAIN).expect("read main").map(|r| r.target)
}

/// A request for one commit on `parent` (`None`: a root) that puts file `n` and moves `main`.
fn single(parent: Option<ChangesetId>, n: usize) -> CommitRequest {
    let parents: Vec<ChangesetId> = parent.into_iter().collect();
    let mut req = request(&parents, vec![put(&path_of(n), &text_of(n))], "perf commit");
    req.advance = Some(Advance::new(MAIN, ACTOR));
    req
}

/// A percentile of `samples` in microseconds; 0 for none.
fn micros(mut samples: Vec<Duration>, percent: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort();
    u64::try_from(percentile(&samples, percent).as_micros()).expect("fits")
}

/// The largest of `samples` in microseconds; 0 for none.
fn max_micros(samples: &[Duration]) -> u64 {
    let longest = samples.iter().max().copied().unwrap_or_default();
    u64::try_from(longest.as_micros()).expect("fits")
}

/// Every sampled id of `dir`'s `samples.txt`.
fn kept_samples(dir: &Path) -> Vec<ChangesetId> {
    let text = fs::read_to_string(dir.join(SAMPLES)).unwrap_or_default();
    text.lines()
        .filter_map(|hex| swarmops_core::ObjectId::from_hex(hex).ok())
        .map(ChangesetId::new)
        .collect()
}

/// Performs the next decile of `commits` commits in the store under `dir`, decided from
/// `results.txt` alone: with none, or with [`DECILES`] lines, it starts a new store; and
/// appends its line. The decile's index.
fn next_decile_in(dir: &Path, commits: usize) -> usize {
    // 1. The decile: the lines present; none, or all ten, start over.
    let mut lines = read_results(dir).len();
    if lines == 0 || lines >= DECILES {
        match fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove {}: {e}", dir.display()),
        }
        fs::create_dir_all(dir).expect("create the perf directory");
        lines = 0;
    }
    let k = lines;
    let store = Utf8PathBuf::from_path_buf(dir.join("store")).expect("a UTF-8 path");
    let repo = if store.join("FORMAT").exists() {
        LocalRepo::open(&store)
    } else {
        LocalRepo::create_as(&store, "perf", StoreFormat::V3)
    }
    .expect("open the perf store");

    // 2. The commits, each followed by the step the CLI makes after its reply.
    let mut head = head_of(&repo);
    let mut targets = kept_samples(dir);
    let (mut commit, mut step) = (Vec::new(), Vec::new());
    let (mut lookup, mut snapshot) = (Vec::new(), Vec::new());
    for n in 0..commits {
        let global = k * commits + n;
        let started = Instant::now();
        let made = repo.commit(single(head, global)).expect("commit").id;
        commit.push(started.elapsed());
        head = Some(made);

        let started = Instant::now();
        repo.step_views().expect("step the views");
        step.push(started.elapsed());

        if n % SAMPLE_EVERY != 0 {
            continue;
        }
        targets.push(made);
        append(dir, SAMPLES, &format!("{}\n", made.0.to_hex()));

        // The lookups of changesets spread over the history so far, in one snapshot.
        let started = Instant::now();
        let read = repo.snapshot().expect("a snapshot");
        snapshot.push(started.elapsed());
        let at = targets[(global * 7919) % targets.len()];
        let started = Instant::now();
        let found = read.changeset(at).expect("a lookup");
        lookup.push(started.elapsed());
        assert!(found.is_some(), "a sampled changeset is in the store");
    }
    assert_eq!(commit.len(), commits, "every commit ran");

    // 3. The line, durable.
    let decile = Decile {
        commit_p50: micros(commit.clone(), 50),
        commit_p99: micros(commit.clone(), 99),
        commit_max: max_micros(&commit),
        step_p50: micros(step.clone(), 50),
        step_p99: micros(step.clone(), 99),
        step_max: max_micros(&step),
        lookup_p99: micros(lookup, 99),
        snapshot_p99: micros(snapshot, 99),
    };
    let line = decile.line(k);
    append(dir, RESULTS, &format!("{line}\n"));
    println!("format3-perf: commits_1m_next_decile: {line}");
    k
}

/// A7: the next decile of the 1,000,000 commits (100,000 back to back through `LocalRepo`, a
/// view step after each as the CLI makes it after its reply); `just perf` calls it ten times.
#[test]
#[ignore = "slow, ~12 to 35 min: 100,000 durable commits in one decile (est. 7.5 to 21 ms each)"]
fn commits_1m_next_decile() {
    next_decile_in(&perf_dir(), decile_commits());
}

/// A5's verdict over the ten deciles (`just perf` only; never in `ci_perf_cases`).
#[test]
#[ignore = "perf recipe only: reads the ten deciles of commits_1m_next_decile"]
fn a5_verdict() {
    let lines = read_results(&perf_dir());
    let found = verdict(&lines).unwrap_or_else(|missing| panic!("a5_verdict: {missing}"));
    let growth = found.growth_hundredths;
    let word = |ok: bool| if ok { "met" } else { "MISSED" };
    println!(
        "format3-perf: a5_verdict: last decile p99 over first {}.{:02} (<= 1.50 {}); lookup and \
         snapshot p99 < 1 ms in every decile {}",
        growth / 100,
        growth % 100,
        word(found.growth_ok),
        word(found.reads_ok),
    );
}

/// A copy of the store at `from` under `to`, which `to` makes anew: `objects/` hard-linked (a
/// store never writes into an object file), the rest copied.
fn copy_store(from: &Path, to: &Path) {
    match fs::remove_dir_all(to) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", to.display()),
    }
    copy_dir(from, to, false);
}

/// Copies directory `from` to `to`; under `objects/` files are hard-linked.
fn copy_dir(from: &Path, to: &Path, linked: bool) {
    fs::create_dir_all(to).expect("create a directory of the copy");
    for entry in fs::read_dir(from).expect("read a directory of the store") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            let below = linked || entry.file_name() == "objects";
            copy_dir(&entry.path(), &target, below);
        } else if linked {
            fs::hard_link(entry.path(), &target).expect("link an object file");
        } else {
            fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
}

/// The meta-lock hold, in microseconds, that `work` made: the counters' delta.
fn held<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let before = Counters::process().snapshot();
    let out = work();
    let after = Counters::process().snapshot();
    let us = after.since(&before).lock_hold_us;
    (out, Duration::from_micros(us))
}

/// One line of figures over the holds of one kind of operation.
fn hold_line(kind: &str, holds: &[Duration]) -> String {
    let max = holds.iter().max().copied().unwrap_or_default();
    let word = if max <= HOLD_MAX { "met" } else { "MISSED" };
    format!(
        "{kind}: n={} p50={:?} p99={:?} max={max:?} (G18 <= 100 ms {word})",
        holds.len(),
        percentile_of(holds, 50),
        percentile_of(holds, 99),
    )
}

/// A percentile of `holds`; zero for none.
fn percentile_of(holds: &[Duration], percent: usize) -> Duration {
    let mut sorted = holds.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    percentile(&sorted, percent)
}

/// The write transactions the process began, where the build counts them.
fn write_calls() -> Option<u64> {
    #[cfg(feature = "test-hooks")]
    return Some(swarmops_store::MetaStore::write_calls());
    #[cfg(not(feature = "test-hooks"))]
    None
}

/// A copy of the large fixture, which a case may change, under the bench directory, opened; or
/// `None` once the case's `not run (fixture k of n)` line is out.
fn large_copy(case: &str) -> Option<(Utf8PathBuf, LocalRepo)> {
    let Some(fixture) = bulk::open(Size::Large) else {
        let (k, n) = bulk::parts(Size::Large);
        println!("format3-perf: {case} not run (fixture {k} of {n})");
        return None;
    };
    copy_of(fixture.store.as_std_path(), case)
}

/// A copy of the store at `from`, under the bench directory, named for `case`; opened.
fn copy_of(from: &Path, case: &str) -> Option<(Utf8PathBuf, LocalRepo)> {
    let to = base_dir().join(format!("m20-7-{case}"));
    copy_store(
        from.parent()
            .expect("the fixture directory")
            .join("store")
            .as_path(),
        &to,
    );
    let store = Utf8PathBuf::from_path_buf(to).expect("a UTF-8 bench path");
    let repo = LocalRepo::open(&store).expect("open the copy");
    Some((store, repo))
}

/// G18 at 1,000,000 records: the longest hold of `meta/lock` by a commit, a land and a ref move
/// on a copy of the large store, [`HOLD_OPS`] each, a view step after each reply (a merge falls
/// in the window).
#[test]
#[ignore = "slow, ~5 min plus the fixture: 3,000 operations on a copy of the 1M store"]
fn lock_hold_at_1m_records_stays_under_100ms() {
    const CASE: &str = "lock_hold_at_1m_records_stays_under_100ms";
    if bulk::gate(CASE) == Scale::NotRun {
        return;
    }
    let Some((_store, repo)) = large_copy(CASE) else {
        return;
    };
    let mut head = head_of(&repo).expect("the fixture has a main");
    let writes_before = write_calls();

    // 1. Commits that move main.
    let mut commits = Vec::new();
    for n in 0..HOLD_OPS {
        let (made, hold) = held(|| repo.commit(single(Some(head), n)).expect("commit").id);
        repo.step_views().expect("step");
        commits.push(hold);
        head = made;
    }

    // 2. Lands: a change on main's head that moves no ref, landed onto main.
    let mut lands = Vec::new();
    for n in 0..HOLD_OPS {
        let parents = [head];
        let req = request(
            &parents,
            vec![put(&path_of(n + HOLD_OPS), &text_of(n))],
            "change",
        );
        let change = repo.commit(req).expect("a change").id;
        let land = LandRequest::new(change, MAIN, signature(n));
        let (landed, hold) = held(|| repo.land(land).expect("land").landed);
        repo.step_views().expect("step");
        lands.push(hold);
        head = landed;
    }

    // 3. Ref moves: a release ref back and forth between two changesets.
    let other = repo
        .commit(request(
            &[head],
            vec![put("other.txt", b"other\n")],
            "other",
        ))
        .expect("a second changeset")
        .id;
    let name = "release/perf";
    repo.cas_ref(name, None, Some(head), ACTOR)
        .expect("create the ref");
    let mut moves = Vec::new();
    let (mut at, mut to) = (head, other);
    for _ in 0..HOLD_OPS {
        let (_, hold) = held(|| repo.cas_ref(name, Some(at), Some(to), ACTOR).expect("move"));
        repo.step_views().expect("step");
        moves.push(hold);
        std::mem::swap(&mut at, &mut to);
    }

    for (kind, holds) in [("commit", &commits), ("land", &lands), ("ref move", &moves)] {
        assert_eq!(holds.len(), HOLD_OPS, "every {kind} ran");
        assert!(
            holds.iter().all(|hold| !hold.is_zero()),
            "every {kind} held the lock"
        );
        println!("format3-perf: {CASE}: {}", hold_line(kind, holds));
    }
    if let (Some(before), Some(after)) = (writes_before, write_calls()) {
        assert!(
            after - before >= u64::try_from(3 * HOLD_OPS).expect("fits"),
            "every operation was a write transaction: {} of {}",
            after - before,
            3 * HOLD_OPS
        );
    }
}

/// The time of one commit of `files` files on the head of `repo`, advancing `main`, and the new
/// head; the timed part ends with the commit's result.
fn timed_commit(
    repo: &LocalRepo,
    head: ChangesetId,
    files: usize,
    round: usize,
) -> (Duration, ChangesetId) {
    let changes: Vec<Change> = (0..files)
        .map(|i| put(&path_of(round * 131 + i * 7), &text_of(round * 1_000 + i)))
        .collect();
    let mut req = request(&[head], changes, "a1 commit");
    req.advance = Some(Advance::new(MAIN, ACTOR));
    let started = Instant::now();
    let made = repo.commit(req).expect("commit").id;
    (started.elapsed(), made)
}

/// A1, the commit's cost: commits of 1, 10 and 100 files on a copy of the 1,000,000-changeset
/// store against the same on the 1,000-changeset one (each within 1.5 x, and within 5 ms + 0.3 ms
/// per file).
#[test]
#[ignore = "slow, ~2 min plus the fixture: 300 durable commits on two copies"]
fn commit_cost_on_1m_against_1k() {
    const CASE: &str = "commit_cost_on_1m_against_1k";
    if bulk::gate(CASE) == Scale::NotRun {
        return;
    }
    let Some((_big_store, big)) = large_copy(CASE) else {
        return;
    };
    let small_fixture = bulk::build_small();
    let (_small_store, small) =
        copy_of(small_fixture.store.as_std_path(), &format!("{CASE}-small"))
            .expect("a copy of the small store");
    let mut heads = [
        head_of(&big).expect("main on the large store"),
        head_of(&small).expect("main on the small store"),
    ];
    for files in FILE_COUNTS {
        let mut times: [Vec<Duration>; 2] = [Vec::new(), Vec::new()];
        for round in 0..A1_COMMITS {
            for (side, repo) in [&big, &small].into_iter().enumerate() {
                let (took, made) = timed_commit(repo, heads[side], files, round);
                times[side].push(took);
                heads[side] = made;
            }
        }
        let p50 = |side: usize| micros(times[side].clone(), 50);
        let (large_us, small_us) = (p50(0), p50(1));
        let allowed = (small_us * 3 / 2).max(small_us + 5_000 + 300 * files as u64);
        let word = if large_us <= allowed { "met" } else { "MISSED" };
        assert_eq!(times[0].len(), A1_COMMITS, "every commit ran");
        println!(
            "format3-perf: {CASE}: {files} files: large p50={large_us} us, small p50={small_us} us, \
             allowed {allowed} us (A1 {word})"
        );
    }
}

/// The text of the upgrade case's file `k`.
fn filler(k: usize) -> Vec<u8> {
    format!("upgrade file {k}\n").into_bytes()
}

/// A FORMAT 2 store of `changesets` changesets in one chain, `main` on its head, under `dir`.
fn format2_store(dir: &Utf8Path, changesets: usize) -> LocalRepo {
    let repo =
        LocalRepo::create_as(dir, "upgrade", StoreFormat::V2).expect("create a FORMAT 2 store");
    let text = filler(0);
    let blob = BlobId::new(hash_object(ObjectKind::Blob, &text));
    let tree = Tree::new(vec![TreeEntry::leaf("a.txt", LeafKind::File, blob)]).expect("a tree");
    repo.put_objects(vec![
        (ObjectKind::Blob, text),
        (ObjectKind::Tree, tree.encode()),
    ])
    .expect("put the objects");
    let mut parent: Option<ChangesetId> = None;
    for from in (0..changesets).step_by(FRAME) {
        let mut batch = repo.raw_batch();
        for k in from..changesets.min(from + FRAME) {
            let mut raw = RawChangeset::new(
                Vec::new(),
                tree.id(),
                signature(k),
                signature(k),
                format!("upgrade {k}").into_bytes(),
                Vec::new(),
                Vec::new(),
            );
            raw.parents = parent.into_iter().collect();
            parent = Some(batch.push(raw).expect("push a changeset"));
        }
        batch.commit().expect("record a frame");
    }
    repo.cas_ref(MAIN, None, parent, ACTOR).expect("move main");
    repo.flush_metadata().expect("flush the metadata");
    repo
}

/// The operations of `repo`, as their ids.
fn op_ids(repo: &LocalRepo) -> Vec<(u64, u32)> {
    let (ops, older) = repo.op_log(None, usize::MAX / 2).expect("the op log");
    assert!(!older, "the whole op log");
    ops.iter().map(|op| (op.seq, op.crc)).collect()
}

/// The upgrade of a FORMAT 2 store of 200,000 changesets: its time and its lock hold, the op-id
/// list equal before and after (recorded, no target).
#[test]
#[ignore = "slow, ~3 min plus the build of the FORMAT 2 store"]
fn upgrade_200k_measured() {
    const CASE: &str = "upgrade_200k_measured";
    if bulk::gate(CASE) == Scale::NotRun {
        return;
    }
    let changesets = UPGRADE_CHANGESETS.min(bulk::large());
    let dir = base_dir().join("m20-7-upgrade");
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", dir.display()),
    }
    fs::create_dir_all(&dir).expect("create the directory");
    let store = Utf8PathBuf::from_path_buf(dir.join("store")).expect("a UTF-8 path");
    let repo = format2_store(&store, changesets);
    assert_eq!(repo.format(), StoreFormat::V2, "the store is FORMAT 2");
    let before = op_ids(&repo);

    // 1. The upgrade, timed, with the lock's hold it made.
    let started = Instant::now();
    let (report, hold) = held(|| repo.upgrade().expect("upgrade"));
    let took = started.elapsed();
    assert!(report.upgraded, "the store was upgraded");
    drop(repo);

    // 2. A handle opened afresh reads FORMAT 3 and the same operations.
    let after_repo = LocalRepo::open(&store).expect("open the upgraded store");
    assert_eq!(
        after_repo.format(),
        StoreFormat::V3,
        "the store is FORMAT 3"
    );
    assert_eq!(op_ids(&after_repo), before, "the op-id list is the same");
    println!(
        "format3-perf: {CASE}: {changesets} changesets: upgrade {took:?}, lock hold {hold:?}, {} \
         operations, the op-id list equal",
        before.len()
    );
}

#[test]
fn a_verdict_names_the_deciles_it_misses() {
    let lines: Vec<String> = (0..4).map(|k| flat(k, 1_000).line(k)).collect();
    let missing = verdict(&lines).expect_err("four deciles are no verdict");
    assert!(
        missing.contains("[4, 5, 6, 7, 8, 9]"),
        "it names the missing ones: {missing}"
    );
    assert!(verdict(&[]).is_err(), "an empty file is no verdict");
}

#[test]
fn a_verdict_meets_flat_deciles_and_misses_growth() {
    let flat_run: Vec<String> = (0..DECILES).map(|k| flat(k, 1_000).line(k)).collect();
    let met = verdict(&flat_run).expect("ten deciles");
    assert_eq!(met.growth_hundredths, 100);
    assert!(met.growth_ok && met.reads_ok, "{met:?}");

    // The last decile's p99 at 2 x the first's, and one slow lookup.
    let mut grown: Vec<String> = (0..DECILES).map(|k| flat(k, 1_000).line(k)).collect();
    grown[DECILES - 1] = flat(DECILES - 1, 2_000).line(DECILES - 1);
    assert!(
        !verdict(&grown).expect("ten deciles").growth_ok,
        "2 x is a miss"
    );
    let mut slow = flat(3, 1_000);
    slow.lookup_p99 = READ_MAX_US;
    grown[3] = slow.line(3);
    assert!(
        !verdict(&grown).expect("ten deciles").reads_ok,
        "1 ms is a miss"
    );
}

/// A decile whose commit p99 is `p99` microseconds.
fn flat(_k: usize, p99: u64) -> Decile {
    Decile {
        commit_p50: p99 / 2,
        commit_p99: p99,
        commit_max: p99 * 2,
        step_p50: 10,
        step_p99: 20,
        step_max: 30,
        lookup_p99: 100,
        snapshot_p99: 100,
    }
}

#[test]
fn a_decile_line_reads_back() {
    let decile = flat(2, 1_234);
    assert_eq!(Decile::parse(&decile.line(2)), Some((2, decile)));
    assert_eq!(Decile::parse("decile 2: nonsense"), None);
    assert_eq!(Decile::parse(""), None);
}

#[test]
fn deciles_resume_from_results_and_start_over_after_ten() {
    let dir = tempfile::tempdir_in(base_dir()).expect("a directory");
    let at = dir.path().join("m20-1m");
    let commits = 12;
    for expected in 0..DECILES {
        assert_eq!(next_decile_in(&at, commits), expected, "the next decile");
    }
    assert_eq!(read_results(&at).len(), DECILES, "ten lines");
    let found = verdict(&read_results(&at)).expect("a verdict");
    assert!(found.growth_hundredths > 0, "{found:?}");

    // The eleventh call starts a new store: one line, decile 0.
    assert_eq!(
        next_decile_in(&at, commits),
        0,
        "a rerun starts at decile 0"
    );
    assert_eq!(read_results(&at).len(), 1, "no accumulation past ten");
}
