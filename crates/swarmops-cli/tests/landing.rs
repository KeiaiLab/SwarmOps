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
    let base = commit_base(&store);
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
    let base_id = ChangesetId(ObjectId::from_hex(&base).expect("a changeset id"));
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
/// lander's directory) and creates `main` there: its id.
fn commit_base(store: &Store) -> String {
    let files = base_files(LANDERS, Workload::Shared);
    let puts: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), std::str::from_utf8(content).unwrap()))
        .collect();
    let changes = store.changes_of("base", &puts);
    let who = agent(LANDERS);
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
