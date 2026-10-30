//! Landing under contention through the binary (the M4 plan's Task 11; operations spec §3 and
//! §4): agents that land at once on one `main`, each through `swarmops land --key`, lose and
//! double nothing.
//!
//! Each agent is a thread of this test that runs one process of the binary at a time
//! (testkit's `CliLander`), as an agent does: it reads `main`, commits its change on the head it
//! read, and lands the commit with its key, running a land that ended retryable again with the
//! same key. Every run of the binary has a deadline and is killed and reaped however the test
//! ends (testkit's `cli` runner), and runs under the store's order checker. The test owns its
//! store, and ends with `verify --deep` through the binary.

use std::collections::BTreeMap;
use std::sync::Barrier;
use std::thread;
use std::time::Instant;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, ObjectId, ObjectKind, hash_object};
use swarmops_testkit::landing::{
    Change, CliLander, LandRecord, REGISTRY, Stats, Workload, base_files, check_invariants,
    own_line, registry_text, with_line,
};

use crate::support::{Store, agent};

/// How many agents land at once.
const LANDERS: usize = 8;

/// How many lands each agent makes, one after another.
const LANDS: usize = 4;

/// The tag of the line the test prints with the run's numbers ([`Stats::summary`]).
const TAG: &str = "landing: ";

/// The file lander `lander`'s land `land` puts, of its own, and its content.
fn own_file(lander: usize, land: usize) -> (String, Vec<u8>) {
    (
        format!("agents/{lander}/land-{land}.txt"),
        format!("lander {lander} land {land}\n").into_bytes(),
    )
}

/// Lander `lander`'s land `land`: its own file ([`own_file`]), and in every other land (the
/// even ones) its line of [`REGISTRY`], the file every lander shares ([`own_line`]). No two
/// landers' lines touch, so every land merges cleanly whatever landed before it.
fn change(lander: usize, land: usize) -> Change {
    Change {
        puts: vec![own_file(lander, land)],
        registry_line: land.is_multiple_of(2).then(|| own_line(lander)),
    }
}

/// Eight agents land at once on one `main` through the binary, four keyed lands each, each agent
/// running one process of the binary at a time: every land puts a file of its own, and every other
/// one also replaces the agent's own line of a file they all share ([`change`]). Up to eight `land`
/// processes race at any moment; a land whose target moved merges again and tries again, and one
/// that ends retryable (`E_BUSY`, or `E_CAS_RETRY_EXHAUSTED` after its 8 attempts) runs again with
/// the same key. Afterwards `main`'s first-parent chain holds exactly the landed changesets, each
/// once, each agent's in its own order, every move of `main` a one-step fast-forward, and the store
/// verifies clean ([`check_invariants`]); every land has landed; `log` lists each agent's landed
/// changesets on `main` exactly once; and `main` holds every land's file and, on each agent's line
/// of the shared file, its last land's text, so that no land's merge dropped another's change. None
/// of this depends on how the lands interleave.
#[test]
fn eight_processes_land_concurrently_none_lost() {
    let store = Store::init();
    let base = commit_base(&store, LANDERS);
    let start = Barrier::new(LANDERS);
    let started = Instant::now();
    let records: Vec<Vec<LandRecord>> = thread::scope(|scope| {
        let landers: Vec<_> = (0..LANDERS)
            .map(|lander| {
                let (store, start) = (&store, &start);
                scope.spawn(move || {
                    let cli = CliLander::new(store, lander, "main");
                    start.wait();
                    let lands = (0..LANDS).map(|land| cli.land(land, &change(lander, land)));
                    lands.collect::<Vec<LandRecord>>()
                })
            })
            .collect();
        (landers.into_iter())
            .map(|lander| {
                lander
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            })
            .collect()
    });
    let wall = started.elapsed();
    println!(
        "{TAG}{} wall_ms={}",
        Stats::from(&records, wall).summary(),
        wall.as_millis()
    );
    let base_id = ChangesetId::new(ObjectId::from_hex(&base).expect("a changeset id"));
    if let Err(rule) = check_invariants(Utf8Path::new(store.path()), "main", base_id, &records) {
        panic!("{rule}\n{records:#?}");
    }
    for (lander, lands) in records.iter().enumerate() {
        for (land, record) in lands.iter().enumerate() {
            assert!(
                record.id.is_some(),
                "lander {lander}'s land {land} did not land: {record:?}"
            );
        }
    }
    assert_each_landed_once(&store, &base, &records);
    assert_every_change_on_main(&store);
    store.assert_clean("after the lands");
}

/// A key that landed a change onto `release/1` and is given for a land onto `main` is
/// `E_INVALID_ARG` (exit 2) through the binary, whether the land is another change or the same
/// one: the details name the key, the changeset it landed and `release/1`, and neither ref moves.
#[test]
fn a_key_reused_on_another_target_is_invalid_arg() {
    const KEY: &str = "other-target";
    let store = Store::init();
    let who = agent(0);
    let t0 = commit_base(&store, 1);
    let release = ["ref", "set", "release/1", &t0, "--create"];
    store.run_as(&who, &release, &[]).ok();
    let one = store.commit(&who, "one.txt", "one\n", Some(&t0));
    let land = ["land", one.as_str(), "--key", KEY, "--target", "release/1"];
    let landed = store.run_as(&who, &land, &[]).ok()["landed"]
        .as_str()
        .unwrap()
        .to_owned();
    let two = store.commit(&who, "two.txt", "two\n", Some(&t0));
    let refs = || store.read(&["ref", "list"]).ok();
    let before = refs();

    for head in [&two, &one] {
        let run = store.run_as(&who, &["land", head, "--key", KEY], &[]);
        let error = run.err("E_INVALID_ARG");
        assert_eq!(error["exit"], 2, "{run}");
        let details = serde_json::json!({"key": KEY, "landed": landed, "target": "release/1"});
        assert_eq!(error["details"], details, "{run}");
    }
    assert_eq!(refs(), before, "neither ref moved");
    store.assert_clean("after the refused lands");
}

/// Commits the base the landers start from ([`base_files`]: [`REGISTRY`] and a file under each
/// lander's directory, for `landers` landers) and creates `main` there: its id.
fn commit_base(store: &Store, landers: usize) -> String {
    let files = base_files(landers, Workload::Shared);
    let puts: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), std::str::from_utf8(content).unwrap()))
        .collect();
    let changes = store.changes_of("base", &puts);
    let who = agent(landers);
    let args = ["commit", "-m", "base", "--changes", &changes];
    let base = store.run_as(&who, &args, &[]).ok()["changeset"]
        .as_str()
        .unwrap()
        .to_owned();
    let main = ["ref", "set", "main", &base, "--create"];
    assert_eq!(store.run_as(&who, &main, &[]).ok()["new"], base.as_str());
    base
}

/// Asserts that `log main`, through the binary, lists each landed changeset of `records` exactly
/// once, and nothing else above `base`. It asks for twice the changesets that should be there,
/// so that any more show in the listing.
fn assert_each_landed_once(store: &Store, base: &str, records: &[Vec<LandRecord>]) {
    let limit = (2 * (LANDERS * LANDS + 1)).to_string();
    let log = store.read(&["log", "main", "-n", &limit]).ok();
    assert_eq!(log["truncated"], false, "{log}");
    let on_main: Vec<&str> = (log["entries"].as_array().unwrap().iter())
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(on_main.last(), Some(&base), "{on_main:?}");
    for (lander, lands) in records.iter().enumerate() {
        for (land, record) in lands.iter().enumerate() {
            let id = record.id.as_deref().unwrap();
            let times = on_main.iter().filter(|&&on| on == id).count();
            assert_eq!(
                times, 1,
                "lander {lander}'s land {land} landed {id}, which log lists on main {times} times"
            );
        }
    }
    assert_eq!(on_main.len(), LANDERS * LANDS + 1, "{on_main:?}");
}

/// Asserts that `main`, read through the binary, holds [`REGISTRY`] with each lander's line as
/// the last of its lands that replaced it left it, and every land's own file with its content:
/// every file of the base, and nothing else.
fn assert_every_change_on_main(store: &Store) {
    let mut files = base_files(LANDERS, Workload::Shared);
    let (path, mut registry) = files.remove(0);
    assert_eq!(path, REGISTRY);
    for lander in 0..LANDERS {
        for land in 0..LANDS {
            files.push(own_file(lander, land));
            if let Some(line) = change(lander, land).registry_line {
                registry = with_line(&registry, line, &registry_text(lander, land));
            }
        }
    }
    let shared = store.read(&["cat", &format!("main:{REGISTRY}")]).ok();
    assert_eq!(
        shared["content"],
        std::str::from_utf8(&registry).unwrap(),
        "each lander's line of {REGISTRY}, as its last land left it"
    );
    files.push((REGISTRY.to_owned(), registry));
    let blob = |content: &[u8]| hash_object(ObjectKind::Blob, content).to_hex();
    let expected: BTreeMap<String, String> = (files.into_iter())
        .map(|(path, content)| (path, blob(&content)))
        .collect();
    let listed = store.read(&["ls", "main", "--depth", "3"]).ok();
    assert_eq!(listed["truncated"], false, "{listed}");
    let on_main: BTreeMap<String, String> = (listed["entries"].as_array().unwrap().iter())
        .filter(|entry| entry["kind"] == "file")
        .map(|entry| {
            let path = entry["path"].as_str().unwrap();
            (path.to_owned(), entry["id"].as_str().unwrap().to_owned())
        })
        .collect();
    assert_eq!(on_main, expected, "every land's own file, as it put it");
}

/// G4 (docs/goals.md): landing under a swarm's load through the binary, for `just perf` (a
/// release build, a quiet host). [`LANDERS_VAR`] agents (50 by default) land on one `main`, each
/// a thread running one process of the binary at a time as an agent does ([`CliLander`]), the
/// lands of [`change`]. In an open loop (the default, the M4 plan's scored cell) each agent
/// lands once every [`INTERVAL_VAR`] ms ± 20 % (2,000) until [`DURATION_VAR`] s (120) have
/// passed ([`slot`]); with an interval of 0, a closed loop, [`LANDS_VAR`] lands each (10), each as
/// soon as the last one ended. It prints one `landing-perf: ` line: the run's [`Stats`] (a land's
/// latency from its read of `main` to the end of its last `land`), the lands that did not land,
/// and the load before and after; then a second line with the gate's verdict ([`check_invariants`])
/// and that of its rules but the first, `slow land`, on their own (`chain`: nothing lost or
/// doubled, every move a fast-forward, a clean `verify`). It asserts the gate and a clean store,
/// never a target: a missed target is recorded in `docs/perf.md`.
///
/// With [`LOCK_SAMPLE_VAR`] set, a thread samples `meta/lock`'s holder and waiters in the
/// kernel's `/proc/locks` every that many microseconds while the agents land ([`locks`]): the
/// binary reports no lock times of its own, neither in its envelope nor in a log. It prints a
/// `landing-lock: ` line per process that waited for or held the lock (its subcommand, its wait,
/// its hold), then a `landing-perf: lock` line per subcommand with their p50 and p99, the share
/// of samples the lock was held in and the mean queue. Unset, the case runs as before.
mod perf {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use camino::Utf8Path;
    use swarmops_core::{ChangesetId, ObjectId};
    use swarmops_testkit::landing::{
        CliLander, LandRecord, Stats, check_invariants, load1, open_loop_lands, slot,
    };

    use super::{change, commit_base};
    use crate::support::Store;

    /// The interval, in microseconds, at which [`locks`] samples `meta/lock`; unset: not sampled.
    const LOCK_SAMPLE_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LOCK_SAMPLE_US";

    /// How many agents land at once.
    const LANDERS_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LANDERS";

    /// Each agent's interval between two lands, in milliseconds; 0 for a closed loop.
    const INTERVAL_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_INTERVAL_MS";

    /// How long an open loop offers lands, in seconds.
    const DURATION_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_DURATION_S";

    /// How many lands each agent makes in a closed loop.
    const LANDS_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LANDS";

    /// The seed of the open loop's slots.
    const SEED: u64 = 1;

    /// How the summary line starts.
    const TAG: &str = "landing-perf: ";

    /// `name`'s value as a number, or `default`.
    fn var(name: &str, default: u64) -> u64 {
        std::env::var(name).map_or(default, |n| n.parse().expect("a number"))
    }

    /// The lands of agent `lander` on `store`: in an open loop, each at its slot after `go`.
    fn lands(
        store: &Store,
        lander: usize,
        go: Instant,
        pace: (Duration, Duration, usize),
    ) -> Vec<LandRecord> {
        let (interval, duration, closed) = pace;
        let cli = CliLander::new(store, lander, "main");
        if interval.is_zero() {
            return (0..closed)
                .map(|land| cli.land(land, &change(lander, land)))
                .collect();
        }
        let count = open_loop_lands(SEED, lander, interval, duration);
        let mut records = Vec::with_capacity(count);
        for land in 0..count {
            let due = go + slot(SEED, lander, land, interval);
            thread::sleep(due.saturating_duration_since(Instant::now()));
            records.push(cli.land(land, &change(lander, land)));
        }
        records
    }

    #[test]
    #[ignore = "slow, perf: 50 agents landing through the binary for 2 minutes"]
    fn landing_under_load() {
        // 1. The run's shape, and the base every agent lands on.
        let landers = usize::try_from(var(LANDERS_VAR, 50)).expect("fits");
        let interval = Duration::from_millis(var(INTERVAL_VAR, 2_000));
        let duration = Duration::from_secs(var(DURATION_VAR, 120));
        let closed = usize::try_from(var(LANDS_VAR, 10)).expect("fits");
        let store = Store::init();
        let base = commit_base(&store, landers);

        // 2. Every agent at once, from one go; the lock sampled meanwhile if asked.
        let start = Barrier::new(landers + 1);
        let load_before = load1();
        let sample = std::env::var(LOCK_SAMPLE_VAR).ok();
        let sample = sample.map(|us| Duration::from_micros(us.parse().expect("a number")));
        let stop = AtomicBool::new(false);
        let mut lock_times = None;
        let (records, go): (Vec<Vec<LandRecord>>, Instant) = thread::scope(|scope| {
            let (store, start, stop) = (&store, &start, &stop);
            let sampler =
                sample.map(|every| scope.spawn(move || locks::sample(store, every, stop)));
            let threads: Vec<_> = (0..landers)
                .map(|lander| {
                    scope.spawn(move || {
                        start.wait();
                        lands(store, lander, Instant::now(), (interval, duration, closed))
                    })
                })
                .collect();
            start.wait();
            let go = Instant::now();
            let records = (threads.into_iter())
                .map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
                .collect();
            stop.store(true, Ordering::Relaxed);
            lock_times = sampler.map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
            (records, go)
        });
        let wall = go.elapsed();

        // 3. The numbers: lands/s over the open loop's duration, or the closed loop's wall.
        let over = if interval.is_zero() { wall } else { duration };
        let stats = Stats::from(&records, over);
        let unlanded = records.iter().flatten().filter(|r| r.id.is_none()).count();
        let pace = if interval.is_zero() {
            format!("closed lands={closed}")
        } else {
            format!(
                "open interval_ms={} duration_s={}",
                interval.as_millis(),
                duration.as_secs()
            )
        };
        println!(
            "{TAG}landers={landers} pace={pace} {} unlanded={unlanded} wall_ms={} load_start={load_before:.2} load_end={:.2}",
            stats.summary(),
            wall.as_millis(),
            load1(),
        );
        if let Some(lock_times) = &lock_times {
            lock_times.print(TAG);
        }

        // 4. The gate, then its rules after the first (`slow land`) on their own: a land past
        //    30 s still fails the run, and the line says whether anything was lost or doubled.
        let base_id = ChangesetId::new(ObjectId::from_hex(&base).expect("a changeset id"));
        let path = Utf8Path::new(store.path());
        let gate = check_invariants(path, "main", base_id, &records);
        let unslowed: Vec<Vec<LandRecord>> = (records.iter())
            .map(|lands| {
                (lands.iter())
                    .map(|r| LandRecord {
                        latency_us: 0,
                        ..r.clone()
                    })
                    .collect()
            })
            .collect();
        let chain = check_invariants(path, "main", base_id, &unslowed);
        println!(
            "{TAG}gate {} chain {}",
            gate.as_ref().map_or_else(String::as_str, |()| "ok"),
            chain.as_ref().map_or_else(String::as_str, |()| "ok"),
        );
        store.assert_clean("after the lands");
        if let Err(rule) = gate {
            panic!("{rule}");
        }
    }

    /// `meta/lock` seen from outside the binary: its holder and waiters in `/proc/locks`.
    ///
    /// ```text
    /// 1: FLOCK  ADVISORY  WRITE 4242 08:10:435976 0 EOF      the holder, pid 4242
    /// 1: -> FLOCK  ADVISORY  WRITE 4243 08:10:435976 0 EOF   a waiter, pid 4243
    /// ```
    ///
    /// A process's wait and hold are the time from the sample that first saw it waiting (or
    /// holding) to the first that did not, summed over its turns: exact to one interval.
    mod locks {
        use std::collections::BTreeMap;
        use std::os::unix::fs::MetadataExt as _;
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        use crate::support::Store;

        /// The kernel's table of file locks.
        const PROC_LOCKS: &str = "/proc/locks";

        /// The subcommands told apart; any other is `other`.
        const KINDS: [&str; 4] = ["land", "commit", "ref", "cat"];

        /// Where a process stands with the lock in one sample.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Seen {
            /// Blocked in `flock`, behind the holder.
            Waiting,
            /// Holding it.
            Holding,
            /// Neither.
            Gone,
        }

        /// One process's time with the lock.
        struct Track {
            /// Its subcommand ([`KINDS`]).
            kind: &'static str,
            /// What the last sample saw, since when.
            seen: (Seen, Instant),
            /// Its waits, summed.
            wait: Duration,
            /// Its holds, summed.
            hold: Duration,
            /// How many times it took the lock.
            holds: u32,
        }

        impl Track {
            /// Ends what the last sample saw at `now` and starts `seen`.
            fn move_to(&mut self, seen: Seen, now: Instant) {
                let (was, since) = self.seen;
                if was == seen {
                    return;
                }
                match was {
                    Seen::Waiting => self.wait += now - since,
                    Seen::Holding => self.hold += now - since,
                    Seen::Gone => {}
                }
                if seen == Seen::Holding {
                    self.holds += 1;
                }
                self.seen = (seen, now);
            }
        }

        /// What [`sample`] saw.
        pub(super) struct Sampled {
            /// Each process that waited for or held the lock, by pid.
            tracks: BTreeMap<u32, Track>,
            /// How many samples it took.
            samples: u32,
            /// In how many of them the lock was held.
            held: u32,
            /// The waiters it saw, summed over the samples.
            waiters: u32,
        }

        /// Samples `store`'s `meta/lock` every `every` until `stop`.
        pub(super) fn sample(store: &Store, every: Duration, stop: &AtomicBool) -> Sampled {
            let lock = Path::new(store.path()).join("meta").join("lock");
            let ino = std::fs::metadata(&lock).expect("meta/lock").ino();
            let mut sampled = Sampled {
                tracks: BTreeMap::new(),
                samples: 0,
                held: 0,
                waiters: 0,
            };
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                let table = std::fs::read_to_string(PROC_LOCKS).expect("/proc/locks");
                let current = on_lock(&table, ino);
                sampled.add(&current, now);
                thread::sleep(every.saturating_sub(now.elapsed()));
            }
            sampled.add(&BTreeMap::new(), Instant::now());
            sampled
        }

        /// The pids `table` shows holding or waiting for the flock on inode `ino`.
        fn on_lock(table: &str, ino: u64) -> BTreeMap<u32, Seen> {
            let mut current = BTreeMap::new();
            for line in table.lines() {
                // `<n>: [->] FLOCK <mode> <access> <pid> <maj>:<min>:<ino> <start> <end>`.
                let mut words = line.split_whitespace().skip(1).peekable();
                let seen = if words.next_if_eq(&"->").is_some() {
                    Seen::Waiting
                } else {
                    Seen::Holding
                };
                let words: Vec<&str> = words.collect();
                let [kind, _, _, pid, file, ..] = words.as_slice() else {
                    continue;
                };
                let on = file.rsplit(':').next().and_then(|n| n.parse::<u64>().ok());
                if *kind != "FLOCK" || on != Some(ino) {
                    continue;
                }
                let Ok(pid) = pid.parse::<u32>() else {
                    continue;
                };
                current.insert(pid, seen);
            }
            current
        }

        /// `pid`'s subcommand, from its command line ([`KINDS`]).
        fn kind_of(pid: u32) -> &'static str {
            let line = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let mut args = line.split(|&b| b == 0).skip(1);
            let found = args.find_map(|arg| KINDS.iter().find(|kind| kind.as_bytes() == arg));
            found.copied().unwrap_or("other")
        }

        impl Sampled {
            /// Adds one sample, `current`, taken at `now`.
            fn add(&mut self, current: &BTreeMap<u32, Seen>, now: Instant) {
                // 1. The lock's state in this sample.
                self.samples += 1;
                let holding = current.values().filter(|&&s| s == Seen::Holding).count();
                self.held += u32::from(holding > 0);
                let waiting = current.len() - holding;
                self.waiters += u32::try_from(waiting).expect("fits");

                // 2. Each process seen before and not now has let go or stopped waiting.
                for (pid, track) in &mut self.tracks {
                    if !current.contains_key(pid) {
                        track.move_to(Seen::Gone, now);
                    }
                }

                // 3. Each process seen now, met for the first time or not.
                for (&pid, &seen) in current {
                    let track = self.tracks.entry(pid).or_insert_with(|| Track {
                        kind: kind_of(pid),
                        seen: (Seen::Gone, now),
                        wait: Duration::ZERO,
                        hold: Duration::ZERO,
                        holds: 0,
                    });
                    track.move_to(seen, now);
                }
            }

            /// Prints a `landing-lock: ` line per process, then a `<tag>lock` line per
            /// subcommand.
            pub(super) fn print(&self, tag: &str) {
                let ms = |d: Duration| d.as_secs_f64() * 1_000.0;
                for (pid, track) in &self.tracks {
                    println!(
                        "landing-lock: pid={pid} cmd={} wait_ms={:.1} hold_ms={:.1} holds={}",
                        track.kind,
                        ms(track.wait),
                        ms(track.hold),
                        track.holds
                    );
                }
                let share = |n: u32| f64::from(n) / f64::from(self.samples.max(1));
                for kind in KINDS.iter().copied().chain(["other"]) {
                    let of_kind = || self.tracks.values().filter(|t| t.kind == kind);
                    let mut waits: Vec<Duration> = of_kind().map(|t| t.wait).collect();
                    let mut holds: Vec<Duration> = of_kind().map(|t| t.hold).collect();
                    if waits.is_empty() {
                        continue;
                    }
                    waits.sort_unstable();
                    holds.sort_unstable();
                    let at = |all: &[Duration], pct: usize| all[(all.len() - 1) * pct / 100];
                    println!(
                        "{tag}lock cmd={kind} processes={} wait_p50_ms={:.1} wait_p99_ms={:.1} \
                         hold_p50_ms={:.1} hold_p99_ms={:.1}",
                        waits.len(),
                        ms(at(&waits, 50)),
                        ms(at(&waits, 99)),
                        ms(at(&holds, 50)),
                        ms(at(&holds, 99)),
                    );
                }
                println!(
                    "{tag}lock samples={} held_pct={:.1} mean_waiters={:.2}",
                    self.samples,
                    share(self.held) * 100.0,
                    share(self.waiters)
                );
            }
        }
    }
}
