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
mod perf {
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, Instant};

    use camino::Utf8Path;
    use swarmops_core::{ChangesetId, ObjectId};
    use swarmops_testkit::landing::{
        CliLander, LandRecord, Stats, check_invariants, load1, open_loop_lands, slot,
    };

    use super::{change, commit_base};
    use crate::support::Store;

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

        // 2. Every agent at once, from one go.
        let start = Barrier::new(landers + 1);
        let load_before = load1();
        let (records, go): (Vec<Vec<LandRecord>>, Instant) = thread::scope(|scope| {
            let (store, start) = (&store, &start);
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
}
