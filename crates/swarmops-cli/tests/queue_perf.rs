//! The landing queue under a swarm's load (the M22 core design, A22; K31): 50 processes each
//! enqueue a change onto `main` and wait for it, the repository's check taking 30 s. Prints one
//! `queue-perf: ` line per figure, A21's (lands/s, p99 gate latency, retries) among them, and
//! asserts what must hold, never a time but the lock's: every entry lands once, and no hold of
//! `meta/lock` reaches 1 s, so no serialised step contains a check.
//!
//! ```text
//!   50 processes ── enqueue ──▶ meta/lock (short holds) ──▶ entry durable
//!        │                                                    │ detached `queue run main`
//!        └── wait (poll) ◀── landed ◀── publish (one frame) ◀─┴─ checks of 30 s, no lock
//!
//!   sampler: /proc/locks every 1 ms ─▶ each pid's continuous hold of meta/lock's inode
//! ```

mod perf {
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::Value;
    use swarmops_testkit::measure::percentile;

    use crate::support::queue::{TARGET, checked, listed};
    use crate::support::{Run, Store, agent};

    /// What every line starts with.
    const TAG: &str = "queue-perf: ";

    /// The processes, each enqueuing and waiting for one entry.
    const PROCESSES: usize = 50;

    /// How long the repository's check takes, in seconds.
    const CHECK_S: u64 = 30;

    /// The bound on any hold of `meta/lock`: a hold that spans a check passes it.
    const HOLD_MAX: Duration = Duration::from_secs(1);

    /// How often the sampler reads `/proc/locks`.
    const SAMPLE_EVERY: Duration = Duration::from_millis(1);

    /// One `queue wait`'s timeout, in seconds: under the test kit's 120 s run timeout. A wait
    /// that times out is waited for again.
    const WAIT_S: &str = "100";

    /// The most `queue wait`s one process makes.
    const WAITS_MAX: u32 = 10;

    /// The kernel's table of file locks.
    const PROC_LOCKS: &str = "/proc/locks";

    /// What one process saw.
    struct Lane {
        /// Its entry's id.
        entry: String,
        /// From the start of its enqueue to its wait's answer: the gate latency.
        latency: Duration,
        /// The retryable errors its enqueue retried.
        retries: u32,
        /// Its `queue wait`s.
        waits: u32,
    }

    /// K31: 50 processes each enqueue and wait with a 30 s check. Every entry lands once, the
    /// longest `meta/lock` hold is under 1 s, and A21's figures are printed.
    #[test]
    #[ignore = "slow, ~2 min: 50 processes on a check of 30 s"]
    fn fifty_entries_with_thirty_second_checks() {
        // 1. A store whose check sleeps 30 s, and one change per process on its head.
        let store = Store::init();
        let base = checked(&store, &format!("sleep {CHECK_S}"));
        let heads: Vec<String> = (0..PROCESSES)
            .map(|i| {
                let path = format!("change-{i}.txt");
                store.commit(&agent(i), &path, &format!("{i}\n"), Some(&base))
            })
            .collect();

        // 2. The processes, started together, beside the lock sampler.
        let stop = AtomicBool::new(false);
        let start = Barrier::new(PROCESSES);
        let began = Instant::now();
        let (lanes, holds) = thread::scope(|scope| {
            let sampler = scope.spawn(|| sample(&store, &stop));
            let racers: Vec<_> = (heads.iter().enumerate())
                .map(|(i, head)| {
                    let (store, start) = (&store, &start);
                    scope.spawn(move || {
                        start.wait();
                        lane(store, i, head)
                    })
                })
                .collect();
            let lanes: Vec<Lane> = (racers.into_iter())
                .map(|racer| racer.join().expect("a process's thread"))
                .collect();
            stop.store(true, Ordering::Relaxed);
            (lanes, sampler.join().expect("the sampler"))
        });
        let wall = began.elapsed();

        // 3. Every entry landed once, and the store is whole.
        let entries: Vec<String> = lanes.iter().map(|lane| lane.entry.clone()).collect();
        landed_once(&store, &base, &entries);
        store.assert_clean("after the swarm");

        // 4. A21's figures and the lock's.
        let mut latencies: Vec<Duration> = lanes.iter().map(|lane| lane.latency).collect();
        latencies.sort_unstable();
        let retries: u32 = lanes.iter().map(|lane| lane.retries).sum();
        let waits: u32 = lanes.iter().map(|lane| lane.waits).sum();
        let lands_per_s = f64::from(u32::try_from(PROCESSES).expect("fits")) / wall.as_secs_f64();
        let mut sorted = holds.clone();
        sorted.sort_unstable();
        let longest = sorted.last().copied().unwrap_or_default();
        println!(
            "{TAG}processes={PROCESSES} check_s={CHECK_S} landed={} wall_s={:.1} \
             lands_per_s={lands_per_s:.2}",
            entries.len(),
            wall.as_secs_f64(),
        );
        println!(
            "{TAG}gate_latency p50_s={:.1} p99_s={:.1} max_s={:.1}",
            percentile(&latencies, 50).as_secs_f64(),
            percentile(&latencies, 99).as_secs_f64(),
            latencies.last().copied().unwrap_or_default().as_secs_f64(),
        );
        println!(
            "{TAG}retries={retries} retry_rate_pct={:.1} waits={waits}",
            f64::from(retries) * 100.0 / f64::from(u32::try_from(PROCESSES).expect("fits")),
        );
        println!(
            "{TAG}lock holds={} hold_p50_ms={:.1} hold_p99_ms={:.1} hold_max_ms={:.1} \
             (bound {HOLD_MAX:?})",
            sorted.len(),
            ms(sorted.first().map(|_| percentile(&sorted, 50))),
            ms(sorted.first().map(|_| percentile(&sorted, 99))),
            ms(Some(longest)),
        );

        // 5. The lock was seen, and never held across a check.
        assert!(!holds.is_empty(), "the sampler saw no hold of meta/lock");
        assert!(
            longest < HOLD_MAX,
            "a hold of meta/lock took {longest:?}, past {HOLD_MAX:?}: a check ran under it"
        );
    }

    /// One process: enqueues `head` onto [`TARGET`] as agent `i`, retrying retryable errors,
    /// then waits until its entry lands.
    fn lane(store: &Store, i: usize, head: &str) -> Lane {
        let who = agent(i);
        let started = Instant::now();

        // 1. The entry, durable once the enqueue answers.
        let mut retries = 0;
        let entry = loop {
            let args = ["queue", "enqueue", TARGET, "--head", head];
            let run = store.run_as(&who, &args, &[]);
            if run.exit() == 0 {
                break run.ok()["entry"]["id"].as_str().expect("an id").to_owned();
            }
            assert!(retryable(&run), "process {i}'s enqueue: {run}");
            retries += 1;
        };

        // 2. Its wait, again after each timeout.
        let mut waits = 0;
        loop {
            waits += 1;
            assert!(waits <= WAITS_MAX, "process {i}: {entry} never landed");
            let run = store.run_as(&who, &["queue", "wait", &entry, "--timeout", WAIT_S], &[]);
            if run.exit() == 0 {
                assert_eq!(run.ok()["state"], "landed", "process {i}: {run}");
                break;
            }
            run.err("E_QUEUE_WAIT_TIMEOUT");
        }
        Lane {
            entry,
            latency: started.elapsed(),
            retries,
            waits,
        }
    }

    /// Asserts that every entry of `entries` landed once: each listed as landed, on a changeset
    /// of its own, and `main`'s history `base` and one changeset per entry, none twice. The log
    /// is read whole: its default page (50) is shorter than the swarm's history.
    #[track_caller]
    fn landed_once(store: &Store, base: &str, entries: &[String]) {
        // 1. Each entry landed, on a changeset of its own.
        let listed = listed(store);
        let mut landed: Vec<&str> = (entries.iter())
            .map(|entry| {
                let found = listed.iter().find(|e| e["id"] == entry.as_str());
                let found = found.unwrap_or_else(|| panic!("{entry} not queued: {listed:?}"));
                assert_eq!(found["state"], "landed", "{found}");
                found["landed"].as_str().expect("a landed id")
            })
            .collect();
        landed.sort_unstable();
        landed.dedup();
        assert_eq!(landed.len(), entries.len(), "{landed:?}");

        // 2. Main: the base and one changeset per entry.
        let limit = (entries.len() + 2).to_string();
        let log = store.read(&["log", TARGET, "--limit", &limit]).ok();
        let ids: Vec<&str> = (log["entries"].as_array().expect("entries").iter())
            .map(|e| e["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(ids.len(), entries.len() + 1, "{ids:?}");
        assert_eq!(ids.last(), Some(&base), "{ids:?}");
        for id in &landed {
            assert!(ids.contains(id), "{id} not on main: {ids:?}");
        }
    }

    /// Whether `run` failed with an error its envelope calls retryable.
    fn retryable(run: &Run) -> bool {
        serde_json::from_str::<Value>(&run.stdout)
            .is_ok_and(|envelope| envelope["error"]["retryable"] == true)
    }

    /// Samples `store`'s `meta/lock` in [`PROC_LOCKS`] every [`SAMPLE_EVERY`] until `stop`:
    /// each pid's holds, from the sample that first saw it holding to the first that did not.
    ///
    /// ```text
    /// 1: FLOCK  ADVISORY  WRITE 4242 08:10:435976 0 EOF      the holder, pid 4242
    /// 1: -> FLOCK  ADVISORY  WRITE 4243 08:10:435976 0 EOF   a waiter, passed over
    /// ```
    fn sample(store: &Store, stop: &AtomicBool) -> Vec<Duration> {
        let lock = Path::new(store.path()).join("meta").join("lock");
        let ino = std::fs::metadata(lock).expect("meta/lock").ino();
        let mut holding: BTreeMap<u32, Instant> = BTreeMap::new();
        let mut holds = Vec::new();
        loop {
            // 1. Who holds it now; a holder no longer listed ends its hold.
            let now = Instant::now();
            let stopping = stop.load(Ordering::Relaxed);
            let holders = if stopping {
                Vec::new()
            } else {
                let table = std::fs::read_to_string(PROC_LOCKS).expect("read /proc/locks");
                holders_of(&table, ino)
            };
            holding.retain(|pid, since| {
                let held = holders.contains(pid);
                if !held {
                    holds.push(now - *since);
                }
                held
            });
            for pid in holders {
                holding.entry(pid).or_insert(now);
            }

            // 2. The next sample, until told to stop.
            if stopping {
                return holds;
            }
            thread::sleep(SAMPLE_EVERY.saturating_sub(now.elapsed()));
        }
    }

    /// The pids `table` lists holding the flock on inode `ino`; waiters (`->`) are not holders.
    fn holders_of(table: &str, ino: u64) -> Vec<u32> {
        let mut pids = Vec::new();
        for line in table.lines() {
            // `<n>: FLOCK <mode> <access> <pid> <maj>:<min>:<ino> <start> <end>`.
            let words: Vec<&str> = line.split_whitespace().skip(1).collect();
            let [kind, _, _, pid, file, ..] = words.as_slice() else {
                continue;
            };
            let on = file.rsplit(':').next().and_then(|n| n.parse::<u64>().ok());
            if *kind != "FLOCK" || on != Some(ino) {
                continue;
            }
            if let Ok(pid) = pid.parse::<u32>() {
                pids.push(pid);
            }
        }
        pids
    }

    /// `hold` in milliseconds; zero for none.
    fn ms(hold: Option<Duration>) -> f64 {
        hold.unwrap_or_default().as_secs_f64() * 1_000.0
    }

    /// The parse of `/proc/locks`: the holder of the inode counts, its waiter and another
    /// inode's holder do not.
    #[test]
    fn holders_are_read_from_proc_locks() {
        let table = "1: FLOCK  ADVISORY  WRITE 4242 08:10:435976 0 EOF\n\
                     1: -> FLOCK  ADVISORY  WRITE 4243 08:10:435976 0 EOF\n\
                     2: FLOCK  ADVISORY  WRITE 4244 08:10:111 0 EOF\n\
                     3: POSIX  ADVISORY  WRITE 4245 08:10:435976 0 EOF\n";
        assert_eq!(holders_of(table, 435_976), [4242]);
    }
}
