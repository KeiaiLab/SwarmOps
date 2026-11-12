//! Landing under contention through the binary (the M4 plan's Task 11; operations spec §3 and
//! §4): agents that land at once on one `main`, each through `swarmops land --key`, lose and
//! double nothing.
//!
//! Each agent is a thread of this test that runs one process of the binary at a time
//! (testkit's `CliLander`), as an agent does: it reads `main`, and lands its change on the head
//! it read with its key in one `land --changes` (the M16 design §4), running a land that ended
//! retryable again with the same key. Every run of the binary has a deadline and is killed and reaped however the test
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
/// landers' lines touch at any pool size, so every land merges cleanly whatever landed before
/// it ([`landers_0_and_67_merge_cleanly`]).
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

    // Another change and the same change, each given the key for `main`: refused, with the
    // ref that holds the land named.
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

/// A key reused on its own target names that target in `details.target`.
#[test]
fn a_key_reused_on_its_target_names_it() {
    const KEY: &str = "same-target";
    let store = Store::init();
    let who = agent(0);
    let t0 = commit_base(&store, 1);

    // The key lands one change onto `main`; another change is given it for `main` too.
    let one = store.commit(&who, "one.txt", "one\n", Some(&t0));
    let landed = store.run_as(&who, &["land", &one, "--key", KEY], &[]).ok()["landed"]
        .as_str()
        .unwrap()
        .to_owned();
    let two = store.commit(&who, "two.txt", "two\n", Some(&t0));

    let run = store.run_as(&who, &["land", &two, "--key", KEY], &[]);
    let details = serde_json::json!({"key": KEY, "landed": landed, "target": "main"});
    assert_eq!(run.err("E_INVALID_ARG")["details"], details, "{run}");
}

/// Lander 67, the first whose line of [`REGISTRY`] would wrap past 200 lines onto lander 0's
/// neighbour, and lander 0 each replace their own line in a land made on the same head of a
/// 68-lander base: the second land merges over the first, and `main`'s [`REGISTRY`] holds both
/// lines.
#[test]
fn landers_0_and_67_merge_cleanly() {
    const LATE: usize = 67;
    let store = Store::init();
    let base = commit_base(&store, LATE + 1);
    let mut files = base_files(LATE + 1, Workload::Shared);
    let (path, registry) = files.swap_remove(0);
    assert_eq!(path, REGISTRY);

    // 1. Each lander lands its own line onto the base head, in its own `land` process.
    let mut expected = registry.clone();
    for lander in [LATE, 0] {
        let text = registry_text(lander, 0);
        let line = with_line(&registry, own_line(lander), &text);
        expected = with_line(&expected, own_line(lander), &text);
        let changes = store.changes_of(
            &format!("lander-{lander}"),
            &[(REGISTRY, std::str::from_utf8(&line).unwrap())],
        );
        let key = format!("lander-{lander}");
        let args = [
            "land",
            &base,
            "--changes",
            &changes,
            "--key",
            &key,
            "-m",
            &key,
        ];
        store.run_as(&agent(lander), &args, &[]).ok();
    }

    // 2. `main` holds both lines.
    let shared = store.read(&["cat", &format!("main:{REGISTRY}")]).ok();
    assert_eq!(
        shared["content"],
        std::str::from_utf8(&expected).unwrap(),
        "lander {LATE}'s line and lander 0's"
    );
    store.assert_clean("after the two lands");
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
/// The run is four lanes (the M17 design §7), one after another, each on a store of its own
/// ([`Lane`]):
///
/// ```text
/// landers   the agents alone, no access.toml            G4, and the land p99 without preparation
/// prepared  the agents, access.toml in the store        the land p99 with preparation (G20)
/// movers    [`MOVERS_VAR`] branch movers alone (50)     the move p99 without landers (R-1)
/// beside    the movers beside the agents                the move p99 with landers (R-1)
/// ```
///
/// A mover ([`CliMover`]) moves `release/mover-<n>` with one `ref set --expect` at the agents'
/// pace and slots. Each lane's line starts `landing-perf: lane=<lane> what=lands` or
/// `what=moves`; a `landing-window: ` line per 10 s window ([`Stats::per_window`]) follows, then
/// how many windows had an attempt that was neither a success nor a retryable refusal
/// ([`flagged`]), which must be none. The movers' branches end at their last acknowledged move
/// ([`check_moved`]). In `prepared` the detached preparations after the lands
/// (`serve --prepare`) must have left `serve.git` current within [`PREPARED_WITHIN`]; the other
/// lanes must have prepared nothing. The last two lines give R-1's ratio and G20's, each when
/// both its lanes ran: [`LANES_VAR`] names the lanes to run (G4's verdict runs `landers` alone),
/// all four when unset.
///
/// With [`LOCK_SAMPLE_VAR`] set, a thread samples `meta/lock`'s holder and waiters in the
/// kernel's `/proc/locks` every that many microseconds while the agents land ([`locks`]): the
/// binary reports no lock times of its own, neither in its envelope nor in a log. It prints a
/// `landing-lock: ` line per process that waited for or held the lock (its subcommand, its wait,
/// its hold, the answers its holds wrote), then a `landing-perf: lock` line per subcommand with
/// their p50 and p99 and a `lock batch` line with the batch size `b` per hold and the hold
/// ([`Holds`](swarmops_testkit::landing::Holds)), the share of samples the lock was held in and
/// the mean queue, and the lane's `U`: lands/s times the median hold over `b` of a `land`'s hold
/// (the M18 D2 design §8), beside the held share and the former `U` of a `land` process's median
/// hold. Unset, the lanes run without it.
mod perf {
    use std::fs;
    use std::path::Path;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use camino::{Utf8Path, Utf8PathBuf};
    use swarmops_core::{ChangesetId, ObjectId};
    use swarmops_git::serve::repo::ServeRepo;
    use swarmops_ops::LocalOps;
    use swarmops_ops::serve::refresh::{Freshness, freshness};
    use swarmops_testkit::landing::{
        CliLander, CliMover, LandRecord, Stats, check_invariants, check_moved, flagged, load1,
        open_loop_lands, slot,
    };

    use super::{change, commit_base};
    use crate::support::{Store, Swarmops, agent};

    /// The interval, in microseconds, at which [`locks`] samples `meta/lock`; unset: not sampled.
    const LOCK_SAMPLE_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LOCK_SAMPLE_US";

    /// How many agents land at once.
    const LANDERS_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LANDERS";

    /// How many branch movers move at once in the `movers` and `beside` lanes.
    const MOVERS_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_MOVERS";

    /// Each agent's interval between two lands, in milliseconds; 0 for a closed loop.
    const INTERVAL_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_INTERVAL_MS";

    /// How long an open loop offers lands, in seconds.
    const DURATION_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_DURATION_S";

    /// How many lands each agent makes in a closed loop.
    const LANDS_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LANDS";

    /// The lanes to run, by name, comma-separated ([`Lane::name`]); unset: all four.
    const LANES_VAR: &str = "SWARMOPS_TEST_CLI_LANDING_LANES";

    /// The seed of the open loop's slots.
    const SEED: u64 = 1;

    /// How the summary line starts.
    const TAG: &str = "landing-perf: ";

    /// How a window's line starts.
    const WINDOW_TAG: &str = "landing-window: ";

    /// The windows a lane's records are cut into (the M17 design §7).
    const WINDOW: Duration = Duration::from_secs(10);

    /// The access file that makes the `prepared` lane's store a served one: its lands start a
    /// preparation each (the M17 design §4.3).
    const ACCESS: &str = "version = 1\n\n[[grant]]\nwho = [\"person/ada\"]\nread = true\n";

    /// How long the detached preparations may take, after the last land, to leave `serve.git`
    /// current: far past one refresh of these stores, for a loaded host.
    const PREPARED_WITHIN: Duration = Duration::from_secs(120);

    /// How often `serve.git` is looked at while waiting.
    const POLL: Duration = Duration::from_millis(100);

    /// The mark a preparation that found `serve.lock` held leaves for its holder.
    const SERVE_DIRTY: &str = "git/serve.dirty";

    /// The file every export writes beside `serve.git`: there only once one ran.
    const SERVE_STATE: &str = "git/serve.state";

    /// The store's write queue (the M18 D2 design §3.4).
    const QUEUE: &str = "meta/queue";

    /// `name`'s value as a number, or `default`.
    fn var(name: &str, default: u64) -> u64 {
        std::env::var(name).map_or(default, |n| n.parse().expect("a number"))
    }

    /// Whether a lane's store holds the access file, so that its lands are prepared after.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Preparation {
        /// It does.
        On,
        /// It does not.
        Off,
    }

    /// One lane of the case: who runs, and whether the store is prepared after each land.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Lane {
        /// The agents alone, unprepared: G4's run.
        Landers,
        /// The agents alone, prepared.
        Prepared,
        /// The movers alone.
        Movers,
        /// The movers beside the agents, unprepared.
        Beside,
    }

    impl Lane {
        /// The four, in the order they run.
        const ALL: [Lane; 4] = [Lane::Landers, Lane::Prepared, Lane::Movers, Lane::Beside];

        /// Its name on the lines.
        fn name(self) -> &'static str {
            match self {
                Lane::Landers => "landers",
                Lane::Prepared => "prepared",
                Lane::Movers => "movers",
                Lane::Beside => "beside",
            }
        }

        /// The lane named `name`, if one is.
        fn from_name(name: &str) -> Option<Lane> {
            Lane::ALL.into_iter().find(|lane| lane.name() == name)
        }

        /// The lanes [`LANES_VAR`] names, in [`Lane::ALL`]'s order; all of them if it is unset.
        /// A name that is no lane's fails the run.
        fn chosen() -> Vec<Lane> {
            let Ok(list) = std::env::var(LANES_VAR) else {
                return Lane::ALL.to_vec();
            };
            let named: Vec<Lane> = (list.split(','))
                .map(|name| {
                    Lane::from_name(name.trim()).unwrap_or_else(|| panic!("no lane {name:?}"))
                })
                .collect();
            Lane::ALL
                .into_iter()
                .filter(|lane| named.contains(lane))
                .collect()
        }

        /// Whether the agents land in it.
        fn lands(self) -> bool {
            self != Lane::Movers
        }

        /// Whether the movers move in it.
        fn moves(self) -> bool {
            matches!(self, Lane::Movers | Lane::Beside)
        }

        /// Whether its store is prepared after each land.
        fn preparation(self) -> Preparation {
            match self {
                Lane::Prepared => Preparation::On,
                Lane::Landers | Lane::Movers | Lane::Beside => Preparation::Off,
            }
        }
    }

    /// The run's shape, from the variables.
    #[derive(Debug, Clone, Copy)]
    struct Shape {
        /// The agents.
        landers: usize,
        /// The movers.
        movers: usize,
        /// Each one's interval; zero for a closed loop.
        interval: Duration,
        /// How long an open loop runs.
        duration: Duration,
        /// A closed loop's lands (or moves) each.
        closed: usize,
        /// The lock sampler's interval, if asked.
        sample: Option<Duration>,
    }

    /// Which records of a lane a line or a check is about.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Side {
        /// The agents' lands.
        Lands,
        /// The movers' moves.
        Moves,
    }

    impl Side {
        /// Its name on the lines.
        fn name(self) -> &'static str {
            match self {
                Side::Lands => "lands",
                Side::Moves => "moves",
            }
        }

        /// Its records in `measured`.
        fn records(self, measured: &Measured) -> &[Vec<LandRecord>] {
            match self {
                Side::Lands => &measured.lands,
                Side::Moves => &measured.moves,
            }
        }
    }

    /// What a lane measured: its agents' and its movers' records, and the host around them.
    struct Measured {
        /// The agents' lands, by agent; empty in `movers`.
        lands: Vec<Vec<LandRecord>>,
        /// The movers' moves, by mover; empty in `landers` and `prepared`.
        moves: Vec<Vec<LandRecord>>,
        /// From the go to the last one's end.
        wall: Duration,
        /// The 1-minute load before the go.
        load_start: f64,
        /// The same after the end.
        load_end: f64,
        /// What the lock sampler saw, if it ran.
        lock: Option<locks::Sampled>,
    }

    /// When the attempt beginning now began, in microseconds after `go`.
    fn at_us(go: Instant) -> u64 {
        u64::try_from(go.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// The `count` steps of one agent or mover `who` of an open loop at `shape`'s pace, each at
    /// its slot after `go` (all at once in a closed loop), by `step`; each record's start set.
    fn paced(
        shape: &Shape,
        who: usize,
        go: Instant,
        mut step: impl FnMut(usize) -> LandRecord,
    ) -> Vec<LandRecord> {
        let count = if shape.interval.is_zero() {
            shape.closed
        } else {
            open_loop_lands(SEED, who, shape.interval, shape.duration)
        };
        let mut records = Vec::with_capacity(count);
        for n in 0..count {
            if !shape.interval.is_zero() {
                let due = go + slot(SEED, who, n, shape.interval);
                thread::sleep(due.saturating_duration_since(Instant::now()));
            }
            let at = at_us(go);
            let mut record = step(n);
            record.at_us = at;
            records.push(record);
        }
        records
    }

    #[test]
    #[ignore = "slow, perf, ~9 min: four lanes of 50 agents or branch movers through the binary, \
                2 minutes each"]
    fn landing_under_load() {
        // 1. The run's shape, then each lane on a store of its own.
        let shape = Shape {
            landers: usize::try_from(var(LANDERS_VAR, 50)).expect("fits"),
            movers: usize::try_from(var(MOVERS_VAR, 50)).expect("fits"),
            interval: Duration::from_millis(var(INTERVAL_VAR, 2_000)),
            duration: Duration::from_secs(var(DURATION_VAR, 120)),
            closed: usize::try_from(var(LANDS_VAR, 10)).expect("fits"),
            sample: std::env::var(LOCK_SAMPLE_VAR)
                .ok()
                .map(|us| Duration::from_micros(us.parse().expect("a number"))),
        };
        let chosen = Lane::chosen();
        let mut land_p99 = [0_u64; 2];
        let mut move_p99 = [0_u64; 2];
        for &lane in &chosen {
            let (lands, moves) = run_lane(lane, &shape);
            match lane {
                Lane::Landers => land_p99[0] = lands.latency_p99_us,
                Lane::Prepared => land_p99[1] = lands.latency_p99_us,
                Lane::Movers => move_p99[0] = moves.latency_p99_us,
                Lane::Beside => move_p99[1] = moves.latency_p99_us,
            }
        }

        // 2. R-1's ratio (a move's p99 beside the agents over alone) and G20's (a land's p99
        //    prepared over unprepared), each when both its lanes ran: recorded, never asserted.
        let ratio = |of: [u64; 2]| {
            let secs = |us: u64| Duration::from_micros(us).as_secs_f64();
            secs(of[1]) / secs(of[0].max(1))
        };
        let ran = |pair: [Lane; 2]| pair.iter().all(|lane| chosen.contains(lane));
        if ran([Lane::Movers, Lane::Beside]) {
            println!(
                "{TAG}r1 move_p99_alone_us={} move_p99_beside_us={} ratio={:.3}",
                move_p99[0],
                move_p99[1],
                ratio(move_p99)
            );
        }
        if ran([Lane::Landers, Lane::Prepared]) {
            println!(
                "{TAG}g20 land_p99_unprepared_us={} land_p99_prepared_us={} ratio={:.3}",
                land_p99[0],
                land_p99[1],
                ratio(land_p99)
            );
        }
    }

    /// Runs `lane` at `shape` on a fresh store, prints its lines and asserts what it ran: its
    /// lands' and its moves' [`Stats`] (empty for the side that did not run).
    fn run_lane(lane: Lane, shape: &Shape) -> (Stats, Stats) {
        // 1. The base, the movers' branches, and the access file when prepared.
        let store = Store::init();
        let base = commit_base(&store, shape.landers);
        let count = if lane.moves() { shape.movers } else { 0 };
        let mut moving: Vec<CliMover<'_, Swarmops>> = (0..count)
            .map(|m| CliMover::create(&store, m, &agent(mover_id(shape, m)), &base))
            .collect();
        if lane.preparation() == Preparation::On {
            fs::write(Path::new(store.path()).join("access.toml"), ACCESS).expect("access.toml");
        }

        // 2. The race, then its numbers: per second over the open loop's duration, or the
        //    closed loop's wall.
        let measured = race(lane, shape, &store, &mut moving);
        let over = if shape.interval.is_zero() {
            measured.wall
        } else {
            shape.duration
        };
        let lands = Stats::from(&measured.lands, over);
        let moves = Stats::from(&measured.moves, over);
        let name = lane.name();

        // 3. The lines: a lane's lands, its moves, and its lock times when sampled. `U` is the
        //    lands/s times the median hold of a land's batch over its members (the M18 D2
        //    design §8); `u_process` the same times a land process's median hold, as before.
        print_side(lane, shape, Side::Lands, &measured, &lands);
        print_side(lane, shape, Side::Moves, &measured, &moves);
        if let Some(lock_times) = &measured.lock {
            lock_times.print(TAG);
            if let (Some(hold), Some(batches)) =
                (lock_times.hold_p50("land"), lock_times.holds_of("land"))
            {
                let u_process = lands.lands_per_s * hold.as_secs_f64();
                println!(
                    "{TAG}lane={name} u={:.3} held_share={:.3} u_process={u_process:.3}",
                    batches.u(lands.lands_per_s),
                    lock_times.held_share(),
                );
            }
        }

        // 4. The windows, which must add up to the lane and hold no failure.
        for (side, whole) in [(Side::Lands, &lands), (Side::Moves, &moves)] {
            let records = side.records(&measured);
            if records.is_empty() {
                continue;
            }
            assert_windows(name, side.name(), records, whole);
        }

        // 5. Preparation: the prepared lane's serve.git current once its runs are done; none in
        //    the others.
        let root = Utf8PathBuf::from(store.path());
        match lane.preparation() {
            Preparation::On => wait_prepared(&root),
            Preparation::Off => {
                let state = root.join(SERVE_STATE);
                assert!(
                    !state.exists(),
                    "lane {name}: a preparation ran without access.toml"
                );
            }
        }

        // 6. The gate for the agents, the acknowledged value for each mover, a clean store.
        if lane.lands() {
            assert_gate(name, &store, &base, &measured.lands);
        }
        let branches: Vec<(String, String)> = (moving.iter())
            .map(|mover| (mover.branch().to_owned(), mover.acknowledged().to_owned()))
            .collect();
        if let Err(rule) = check_moved(&root, &branches) {
            panic!("lane {name}: {rule}");
        }
        store.assert_clean("after the lane");

        // 7. What the write queue left once every process ended (the M18 D2 design §10: none),
        //    recorded, not asserted.
        let left = fs::read_dir(root.join(QUEUE)).map_or(0, Iterator::count);
        println!("{TAG}lane={name} queue_left={left}");
        (lands, moves)
    }

    /// Every agent and mover of `lane` at once on `store`, from one go; the lock sampled
    /// meanwhile if `shape` asks.
    fn race(
        lane: Lane,
        shape: &Shape,
        store: &Store,
        moving: &mut [CliMover<'_, Swarmops>],
    ) -> Measured {
        let landers = if lane.lands() { shape.landers } else { 0 };
        let start = Barrier::new(landers + moving.len() + 1);
        let load_start = load1();
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            // 1. The sampler, the agents and the movers, each waiting for the go.
            let (start, stop) = (&start, &stop);
            let sampler =
                (shape.sample).map(|every| scope.spawn(move || locks::sample(store, every, stop)));
            let agents: Vec<_> = (0..landers)
                .map(|lander| {
                    scope.spawn(move || {
                        let cli = CliLander::new(store, lander, "main");
                        start.wait();
                        let go = Instant::now();
                        paced(shape, lander, go, |n| cli.land(n, &change(lander, n)))
                    })
                })
                .collect();
            let shifters: Vec<_> = (moving.iter_mut().enumerate())
                .map(|(m, mover)| {
                    let who = mover_id(shape, m);
                    scope.spawn(move || {
                        start.wait();
                        let go = Instant::now();
                        paced(shape, who, go, |_| mover.shift())
                    })
                })
                .collect();

            // 2. The go, then every record once all have ended.
            start.wait();
            let go = Instant::now();
            let join = |t: thread::ScopedJoinHandle<'_, Vec<LandRecord>>| {
                t.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
            };
            let lands = agents.into_iter().map(join).collect();
            let moves = shifters.into_iter().map(join).collect();
            let wall = go.elapsed();
            stop.store(true, Ordering::Relaxed);
            let lock = sampler.map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
            Measured {
                lands,
                moves,
                wall,
                load_start,
                load_end: load1(),
                lock,
            }
        })
    }

    /// Prints lane `lane`'s line for `what` (`lands` or `moves`), its numbers `stats`; none for
    /// a side that did not run in it.
    fn print_side(lane: Lane, shape: &Shape, side: Side, measured: &Measured, stats: &Stats) {
        let records = side.records(measured);
        if records.is_empty() {
            return;
        }
        let what = side.name();
        let (who, missed) = match side {
            Side::Lands => ("landers", "unlanded"),
            Side::Moves => ("movers", "unmoved"),
        };
        let count = records.iter().flatten().filter(|r| r.id.is_none()).count();
        println!(
            "{TAG}lane={} what={what} {who}={} pace={} {} {missed}={count} wall_ms={} load_start={:.2} load_end={:.2}",
            lane.name(),
            records.len(),
            pace(shape),
            stats.summary(),
            measured.wall.as_millis(),
            measured.load_start,
            measured.load_end,
        );
    }

    /// Mover `m`'s number among the run's identities and draws: past the agents and the base's
    /// committer (`agent(landers)`), so that none shares another's.
    fn mover_id(shape: &Shape, m: usize) -> usize {
        shape.landers + 1 + m
    }

    /// The pace on the lines.
    fn pace(shape: &Shape) -> String {
        if shape.interval.is_zero() {
            return format!("closed lands={}", shape.closed);
        }
        format!(
            "open interval_ms={} duration_s={}",
            shape.interval.as_millis(),
            shape.duration.as_secs()
        )
    }

    /// Prints `records`' windows ([`WINDOW`]) as lane `name`'s `what`, and asserts that their
    /// counts add up to `whole`'s and that none is flagged.
    fn assert_windows(name: &str, what: &str, records: &[Vec<LandRecord>], whole: &Stats) {
        let windows = Stats::per_window(records, WINDOW);
        for (window, stats) in &windows {
            println!(
                "{WINDOW_TAG}lane={name} what={what} start_s={} landed={} retryable={} other={} p99_us={}",
                window.start.as_secs(),
                stats.landed,
                stats.retryable,
                stats.other,
                stats.latency_p99_us,
            );
        }
        let sum = |pick: fn(&Stats) -> u64| windows.iter().map(|(_, s)| pick(s)).sum::<u64>();
        let summed = (sum(|s| s.landed), sum(|s| s.retryable), sum(|s| s.other));
        assert_eq!(
            summed,
            (whole.landed, whole.retryable, whole.other),
            "lane {name} {what}"
        );
        let flags = flagged(&windows);
        println!(
            "{TAG}lane={name} what={what} windows={} flagged={}",
            windows.len(),
            flags.len()
        );
        assert!(
            flags.is_empty(),
            "lane {name} {what}: failures in {flags:?}"
        );
    }

    /// Waits, at most [`PREPARED_WITHIN`], until the store at `root` holds no preparation mark
    /// and its `serve.git` is current: the detached preparations after its lands ran.
    fn wait_prepared(root: &Utf8Path) {
        let deadline = Instant::now() + PREPARED_WITHIN;
        let current = || {
            let ops = LocalOps::open(root).expect("open the store");
            let repo = ServeRepo::open(root).expect("open serve.git");
            let fresh = freshness(&ops, &repo).expect("read the freshness");
            matches!(fresh, Freshness::Current)
        };
        while root.join(SERVE_DIRTY).exists() || !root.join(SERVE_STATE).exists() || !current() {
            assert!(
                Instant::now() < deadline,
                "serve.git not prepared within {PREPARED_WITHIN:?}"
            );
            thread::sleep(POLL);
        }
    }

    /// The gate on `records`, lane `name`'s lands on `store` from `base`, then its rules after the
    /// first (`slow land`) on their own: a land past 30 s still fails the run, and the line says
    /// whether anything was lost or doubled.
    fn assert_gate(name: &str, store: &Store, base: &str, records: &[Vec<LandRecord>]) {
        let base_id = ChangesetId::new(ObjectId::from_hex(base).expect("a changeset id"));
        let path = Utf8Path::new(store.path());
        let gate = check_invariants(path, "main", base_id, records);
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
            "{TAG}lane={name} gate {} chain {}",
            gate.as_ref().map_or_else(String::as_str, |()| "ok"),
            chain.as_ref().map_or_else(String::as_str, |()| "ok"),
        );
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
    ///
    /// Each sample also lists `meta/queue/` (the M18 D2 design §3.4, §8): a request first seen
    /// claimed (`<rid>.com`) is one answer of the hold that claimed it, the one of the leader its
    /// manifest (`<rid>.batch`) names, else of the holder, else of the last one. A hold's batch
    /// is then `b` = 1 + its answers.
    ///
    /// ```text
    /// <ns>-4242-<nonce>.batch   the manifest of pid 4242's batch
    /// <ns>-4243-<nonce>.com     a request claimed into it: one answer of 4242's hold
    /// ```
    mod locks {
        use std::collections::{BTreeMap, BTreeSet};
        use std::os::unix::fs::MetadataExt as _;
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        use swarmops_testkit::landing::{Hold, Holds};

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
            /// The answers its holds wrote ([`Sampled::claim`]).
            answers: u32,
            /// Its current or last hold, in [`Sampled::holds`].
            last: Option<usize>,
        }

        impl Track {
            /// Ends what the last sample saw at `now` and starts `seen`; a hold that ends is
            /// timed in `holds`, one that starts is added to it.
            fn move_to(&mut self, seen: Seen, now: Instant, holds: &mut Vec<(&'static str, Hold)>) {
                let (was, since) = self.seen;
                if was == seen {
                    return;
                }

                // 1. What ends.
                match was {
                    Seen::Waiting => self.wait += now - since,
                    Seen::Holding => {
                        self.hold += now - since;
                        if let Some((_, hold)) = self.last.and_then(|at| holds.get_mut(at)) {
                            hold.us = u64::try_from((now - since).as_micros()).unwrap_or(u64::MAX);
                        }
                    }
                    Seen::Gone => {}
                }

                // 2. What starts: a hold gets its entry.
                if seen == Seen::Holding {
                    self.holds += 1;
                    self.last = Some(holds.len());
                    holds.push((self.kind, Hold { us: 0, answers: 0 }));
                }
                self.seen = (seen, now);
            }
        }

        /// What one sample saw in `meta/queue/`.
        #[derive(Default)]
        struct Queued {
            /// The pid of the leader a manifest names, if one is there.
            leader: Option<u32>,
            /// The requests claimed, by id.
            claimed: Vec<String>,
        }

        /// The pid a request id names: `<ns>-<pid>-<nonce>`.
        fn pid_of(rid: &str) -> Option<u32> {
            rid.split('-').nth(1)?.parse().ok()
        }

        /// What the names of `meta/queue/`'s entries show ([`Queued`]); temporary files and
        /// other names are passed over.
        fn on_queue<'n>(names: impl Iterator<Item = &'n str>) -> Queued {
            let mut queued = Queued::default();
            for name in names {
                if let Some(rid) = name.strip_suffix(".batch") {
                    queued.leader = queued.leader.or_else(|| pid_of(rid));
                    continue;
                }
                if let Some(rid) = name.strip_suffix(".com") {
                    queued.claimed.push(rid.to_owned());
                }
            }
            queued
        }

        /// The names in `dir`, none if it is not there.
        fn names(dir: &Path) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            let names = entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok());
            names.collect()
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
            /// Every hold, with its holder's subcommand, in the order they started.
            holds: Vec<(&'static str, Hold)>,
            /// The requests seen claimed so far.
            claimed: BTreeSet<String>,
            /// The pid that held the lock last.
            last_holder: Option<u32>,
        }

        /// Samples `store`'s `meta/lock` every `every` until `stop`.
        pub(super) fn sample(store: &Store, every: Duration, stop: &AtomicBool) -> Sampled {
            let meta = Path::new(store.path()).join("meta");
            let ino = std::fs::metadata(meta.join("lock"))
                .expect("meta/lock")
                .ino();
            let queue = meta.join("queue");
            let mut sampled = Sampled::new();
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                let table = std::fs::read_to_string(PROC_LOCKS).expect("/proc/locks");
                let current = on_lock(&table, ino);
                let listed = names(&queue);
                let queued = on_queue(listed.iter().map(String::as_str));
                sampled.add(&current, &queued, now, kind_of);
                thread::sleep(every.saturating_sub(now.elapsed()));
            }
            sampled.add(
                &BTreeMap::new(),
                &Queued::default(),
                Instant::now(),
                kind_of,
            );
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
            /// Nothing sampled yet.
            fn new() -> Sampled {
                Sampled {
                    tracks: BTreeMap::new(),
                    samples: 0,
                    held: 0,
                    waiters: 0,
                    holds: Vec::new(),
                    claimed: BTreeSet::new(),
                    last_holder: None,
                }
            }

            /// Adds one sample taken at `now`: `current` from `/proc/locks`, `queued` from
            /// `meta/queue/`; `kind` names a pid's subcommand ([`KINDS`]).
            fn add(
                &mut self,
                current: &BTreeMap<u32, Seen>,
                queued: &Queued,
                now: Instant,
                kind: fn(u32) -> &'static str,
            ) {
                // 1. The lock's state in this sample.
                self.samples += 1;
                let holding = current.values().filter(|&&s| s == Seen::Holding).count();
                self.held += u32::from(holding > 0);
                let waiting = current.len() - holding;
                self.waiters += u32::try_from(waiting).expect("fits");

                // 2. Each process seen before and not now has let go or stopped waiting.
                for (pid, track) in &mut self.tracks {
                    if !current.contains_key(pid) {
                        track.move_to(Seen::Gone, now, &mut self.holds);
                    }
                }

                // 3. Each process seen now, met for the first time or not.
                for (&pid, &seen) in current {
                    let track = self.tracks.entry(pid).or_insert_with(|| Track {
                        kind: kind(pid),
                        seen: (Seen::Gone, now),
                        wait: Duration::ZERO,
                        hold: Duration::ZERO,
                        holds: 0,
                        answers: 0,
                        last: None,
                    });
                    track.move_to(seen, now, &mut self.holds);
                }

                // 4. The requests claimed since the last sample, each an answer of its hold.
                let holder = current.iter().find(|(_, s)| **s == Seen::Holding);
                self.last_holder = holder.map(|(&pid, _)| pid).or(self.last_holder);
                for rid in &queued.claimed {
                    if self.claimed.insert(rid.clone()) {
                        self.claim(queued.leader);
                    }
                }
            }

            /// Counts one answer for the hold of `leader`, the manifest's, if it was seen; else
            /// for the current or last holder's.
            fn claim(&mut self, leader: Option<u32>) {
                let seen = leader.filter(|pid| self.tracks.contains_key(pid));
                let Some(pid) = seen.or(self.last_holder) else {
                    return;
                };
                let Some(track) = self.tracks.get_mut(&pid) else {
                    return;
                };
                let Some((_, hold)) = track.last.and_then(|at| self.holds.get_mut(at)) else {
                    return;
                };
                hold.answers += 1;
                track.answers += 1;
            }

            /// The holds of the processes of subcommand `kind` ([`KINDS`]), summed up; `None`
            /// if none was seen.
            pub(super) fn holds_of(&self, kind: &str) -> Option<Holds> {
                let of_kind = self.holds.iter().filter(|(k, _)| *k == kind);
                let holds: Vec<Hold> = of_kind.map(|(_, hold)| *hold).collect();
                Holds::of(&holds)
            }

            /// The share of samples the lock was held in, from 0 to 1.
            pub(super) fn held_share(&self) -> f64 {
                f64::from(self.held) / f64::from(self.samples.max(1))
            }

            /// The median hold of the processes of subcommand `kind` ([`KINDS`]), each its
            /// holds summed; `None` if none was seen.
            pub(super) fn hold_p50(&self, kind: &str) -> Option<Duration> {
                let of_kind = self.tracks.values().filter(|t| t.kind == kind);
                let mut holds: Vec<Duration> = of_kind.map(|t| t.hold).collect();
                holds.sort_unstable();
                holds.get(holds.len().checked_sub(1)? / 2).copied()
            }

            /// Prints a `landing-lock: ` line per process, then a `<tag>lock` line per
            /// subcommand.
            pub(super) fn print(&self, tag: &str) {
                let ms = |d: Duration| d.as_secs_f64() * 1_000.0;
                for (pid, track) in &self.tracks {
                    println!(
                        "landing-lock: pid={pid} cmd={} wait_ms={:.1} hold_ms={:.1} holds={} \
                         answers={}",
                        track.kind,
                        ms(track.wait),
                        ms(track.hold),
                        track.holds,
                        track.answers
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
                    if let Some(batches) = self.holds_of(kind) {
                        println!("{tag}lock batch cmd={kind} {}", batches.line());
                    }
                }
                println!(
                    "{tag}lock samples={} held_pct={:.1} mean_waiters={:.2}",
                    self.samples,
                    share(self.held) * 100.0,
                    share(self.waiters)
                );
            }
        }

        /// The subcommand every pid of the tests below runs.
        fn land(_: u32) -> &'static str {
            "land"
        }

        /// One sample of the tests below: `holder` holding the lock, if any, and `names` in
        /// `meta/queue/`, at `ms` after `go`.
        fn step(sampled: &mut Sampled, go: Instant, ms: u64, holder: Option<u32>, names: &[&str]) {
            let current: BTreeMap<u32, Seen> =
                holder.map(|pid| (pid, Seen::Holding)).into_iter().collect();
            let queued = on_queue(names.iter().copied());
            sampled.add(&current, &queued, go + Duration::from_millis(ms), land);
        }

        /// The probe's `b` per hold: pid 10 leads a batch whose manifest it names and claims
        /// two requests, then a third seen claimed only after it let go; pid 20 leads the next
        /// and claims one, beside a claim of 10's batch not yet removed. The `lock batch` line
        /// reads 2 holds, 4 answers, `b` 4 and 2.
        #[test]
        fn a_lock_line_counts_answers_per_hold() {
            let mut sampled = Sampled::new();
            let go = Instant::now();
            let ten = ["1-10-a.batch", "1-11-a.com", "1-12-a.com"];
            step(&mut sampled, go, 0, Some(10), &ten);
            step(&mut sampled, go, 1, Some(10), &ten);
            step(&mut sampled, go, 3, None, &["1-11-a.com", "1-13-a.com"]);
            step(
                &mut sampled,
                go,
                4,
                Some(20),
                &["1-13-a.com", "2-20-a.batch", "2-21-a.com"],
            );
            step(&mut sampled, go, 6, None, &[]);

            let holds: Vec<(u64, u64)> = (sampled.holds.iter())
                .map(|(_, hold)| (hold.us, hold.members()))
                .collect();
            assert_eq!(holds, [(3_000, 4), (2_000, 2)]);
            let line = sampled.holds_of("land").expect("holds").line();
            assert!(
                line.starts_with("holds=2 answers=4 b_p50=2 b_p99=4 "),
                "{line}"
            );
            assert_eq!(sampled.tracks[&10].answers, 3);
        }

        /// The self-check of the one above: a hold whose queue shows no claim reads `b` = 1.
        #[test]
        fn a_hold_without_answers_reads_one() {
            let mut sampled = Sampled::new();
            let go = Instant::now();
            step(&mut sampled, go, 0, Some(30), &["1-31-a.req"]);
            step(&mut sampled, go, 2, None, &[]);

            let line = sampled.holds_of("land").expect("holds").line();
            assert!(
                line.starts_with("holds=1 answers=0 b_p50=1 b_p99=1 hold_p50_ms=2.0 "),
                "{line}"
            );
        }
    }
}
