//! A sync that keeps pace with a swarm's landing (the M18 design §2.4; requirement
//! R-2026-10-28-A4): 50 agents land on `main` through the binary at G4's 20 lands/s for 10
//! minutes while one syncer runs `sync origin` again as soon as one ends, against the test kit's
//! loopback server, which only receives. The remote never moves on its own, so every sync
//! pushes `main` as it is (ops `sync.rs`'s `B = C_R → H = M` row).
//!
//! ```text
//!   50 × CliLander ── land ──▶ store main ── sync origin (loop) ──▶ GitServer main
//!                                 │                                      │
//!   sampler, every 1 s: lag = position of the remote head's changeset in `log main`
//! ```
//!
//! The case asserts what it ran: every landed changeset on the remote's first parents exactly
//! once and nothing else above the base, no sync rewrote an imported commit, the store's chain
//! and `verify --deep` clean. It prints the lag ratio (target ≤ 1.5) and the land p99 beside;
//! the verdict goes to `docs/perf.md`, never to an assertion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use camino::Utf8Path;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_testkit::landing::{LandRecord, Stats, Workload, base_files, check_invariants, load1};
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use swarmops_testkit::sync_load::{LagSample, RATIO_WINDOW, SyncLane, lag_ratio};
use tempfile::TempDir;

use crate::support::{Run, Store, retry_busy};

/// The agent the syncer and the setup run as.
const AGENT: &str = "agent/syncer";

/// The repository the store syncs with.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The trailer every native changeset's commit carries first (the git crate's export).
const CHANGESET_TRAILER: &str = "SwarmOps-Changeset";

/// How many entries one page of `log main` reads while the lag is looked for.
const LOG_PAGE: &str = "1000";

/// The lane: 50 agents, each one land every 2.5 s (20 lands/s in all), for 10 minutes, the lag
/// read every second.
const LANE: SyncLane = SyncLane {
    landers: 50,
    interval: Duration::from_millis(2_500),
    duration: Duration::from_secs(600),
    seed: 1,
    every: Duration::from_secs(1),
};

/// How the lines start.
const TAG: &str = "sync-load: ";

/// A served repository that only receives, and the git environment the binary runs with.
struct Remote {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP.
    http: HttpServer,
    /// git's empty `HOME` for the product's children.
    _home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Remote {
    /// [`REPO`] served, its `main` one commit holding the landers' base files: its id.
    fn with_base(landers: usize) -> (Remote, String) {
        let server = GitServer::new();
        server.create(REPO);
        let files = base_files(landers, Workload::Disjoint);
        let texts: Vec<(&str, &str)> = (files.iter())
            .map(|(path, bytes)| (path.as_str(), std::str::from_utf8(bytes).expect("text")))
            .collect();

        // The remote's journal: every move of `main`, a push's too, in its reflog.
        let logged = bare_git(&server, &["config", "core.logAllRefUpdates", "always"]);
        assert!(logged.status.success(), "the reflog setting: {logged:?}");
        let base = server.commit_on(REPO, "main", "base", &texts);
        let http = server.http(HttpAuth::Anonymous);
        let home = tempfile::tempdir().expect("a temp HOME");
        let env = vec![
            ("SWARMOPS_GIT".to_owned(), text(&real_git(&server))),
            ("HOME".to_owned(), text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ];
        let remote = Remote {
            server,
            http,
            _home: home,
            env,
        };
        (remote, base)
    }

    /// A store that fetched the remote and set `main` at its head: that head's changeset.
    fn cloned(&self) -> (Store, String) {
        let vars: Vec<(&str, &str)> = (self.env.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let store = Store::init().with_env(&vars);
        let url = self.http.url(REPO);
        let line = ["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
        store.read(&line).ok();
        let fetched = store.read(&["fetch", ORIGIN]).ok();
        let head = fetched["head"]["changeset"].as_str().expect("a head");
        let line = ["ref", "set", "main", head, "--create"];
        store.run_as(AGENT, &line, &[]).ok();
        (store, head.to_owned())
    }

    /// The remote's `main`, first parents newest first: each commit's id and the changeset its
    /// [`CHANGESET_TRAILER`] names (empty for an imported commit).
    fn first_parents(&self) -> Vec<(String, String)> {
        let format =
            format!("--format=%H %(trailers:key={CHANGESET_TRAILER},valueonly,separator=)");
        let listed = self.git(&["log", "--first-parent", &format, "main"]);
        (listed.lines())
            .map(|line| {
                let (commit, changeset) = line.split_once(' ').unwrap_or((line, ""));
                (commit.to_owned(), changeset.trim().to_owned())
            })
            .collect()
    }

    /// The changeset the remote's head carries; `None` for the imported base.
    fn head_changeset(&self) -> Option<String> {
        let format = format!("--format=%(trailers:key={CHANGESET_TRAILER},valueonly,separator=)");
        let head = self.git(&["log", "-1", &format, "main"]);
        let head = head.trim();
        (!head.is_empty()).then(|| head.to_owned())
    }

    /// The moves of `main` the remote's reflog holds, each read against `base`, the imported
    /// commit: a move to a commit that does not descend from it is a push that rewrote it.
    fn moves(&self, base: &str) -> Moves {
        let listed = self.git(&["log", "--walk-reflogs", "--format=%H", "main"]);
        let commits: Vec<&str> = listed.lines().collect();
        let rewritten = (commits.iter())
            .filter(|commit| !self.descends(commit, base))
            .count();
        Moves {
            all: commits.len(),
            rewritten,
        }
    }

    /// Whether `commit` is `base` or has it among its ancestors.
    fn descends(&self, commit: &str, base: &str) -> bool {
        let output = bare_git(&self.server, &["merge-base", "--is-ancestor", base, commit]);
        match output.status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => panic!("git merge-base --is-ancestor {base} {commit}: {output:?}"),
        }
    }

    /// What plain git with `args` prints on the bare repository; a failure fails the case.
    fn git(&self, args: &[&str]) -> String {
        let output = bare_git(&self.server, args);
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).expect("UTF-8")
    }
}

/// The moves of the remote's `main` its reflog holds ([`Remote::moves`]).
struct Moves {
    /// Every move, the base's own included.
    all: usize,
    /// The moves to a commit that does not descend from the imported base.
    rewritten: usize,
}

/// Plain git with `args` on `server`'s bare repository: its output, whatever its status.
fn bare_git(server: &GitServer, args: &[&str]) -> Output {
    (server.command())
        .arg("--git-dir")
        .arg(server.bare(REPO))
        .args(args)
        .output()
        .expect("run git")
}

/// One sync as the syncer runs it: a sync that met a held lock runs again; any other failure
/// fails the case. Whether it rewrote an imported commit is read from the remote's reflog after
/// the lane ([`Remote::moves`]).
fn sync_once(store: &Store) {
    let line = ["sync", ORIGIN];
    let run = retry_busy(store.run_as(AGENT, &line, &[]), || {
        store.run_as(AGENT, &line, &[])
    });
    run.ok();
}

/// How many lands `main` holds above the changeset the remote's head carries (`base` while it
/// carries none): its position in `log main`, read a page at a time.
fn lag(remote: &Remote, store: &Store, base: &str) -> u64 {
    // 1. The remote first: `main` only moves forward from what was pushed.
    let pushed = remote.head_changeset().unwrap_or_else(|| base.to_owned());

    // 2. Its position on `main`'s first parents.
    let mut lands = 0_u64;
    let mut cursor: Option<String> = None;
    loop {
        let mut line = vec!["log", "main", "-n", LOG_PAGE];
        if let Some(cursor) = &cursor {
            line.extend(["--cursor", cursor.as_str()]);
        }
        let page = read(store, &line);
        for entry in page["entries"].as_array().expect("entries") {
            if entry["id"] == pushed.as_str() {
                return lands;
            }
            lands += 1;
        }
        let Some(next) = page["cursor"].as_str() else {
            panic!("the remote's head {pushed} is not on main");
        };
        cursor = Some(next.to_owned());
    }
}

/// A read through the binary, run again while it meets a held lock: its result.
fn read(store: &Store, args: &[&str]) -> serde_json::Value {
    let run: Run = retry_busy(store.read(args), || store.read(args));
    run.ok()
}

/// The mean lag of `samples` in each minute of the run, from the first.
fn per_minute(samples: &[LagSample]) -> Vec<f64> {
    let mut minutes: BTreeMap<u64, (u64, u32)> = BTreeMap::new();
    for sample in samples {
        let minute = sample.at.as_secs() / RATIO_WINDOW.as_secs();
        let (sum, count) = minutes.entry(minute).or_default();
        *sum += sample.lag;
        *count += 1;
    }
    (minutes.values())
        .map(|&(sum, count)| f64::from(u32::try_from(sum).unwrap_or(u32::MAX)) / f64::from(count))
        .collect()
}

#[test]
#[ignore = "slow, perf, ~11 min: 50 agents land through the binary for 10 minutes beside a \
            syncer, run by `just perf`"]
fn sync_keeps_pace_with_landing() {
    // 1. A remote holding the base, a store that cloned it.
    let (remote, base_commit) = Remote::with_base(LANE.landers);
    let (store, base) = remote.cloned();
    let load_start = load1();

    // 2. The lane, then one sync more, so that the remote holds every land.
    let run = LANE.run(&store, || sync_once(&store), || lag(&remote, &store, &base));
    sync_once(&store);
    let load_end = load1();

    // 3. The lines: the lag at each end, the ratio, the lands beside it, and each minute.
    let samples = &run.samples;
    let window = |at: fn(&[LagSample]) -> Option<&LagSample>| at(samples).map_or(0, |s| s.lag);
    let stats = Stats::from(&run.lands, LANE.duration);
    let unlanded = run
        .lands
        .iter()
        .flatten()
        .filter(|r| r.id.is_none())
        .count();
    println!(
        "{TAG}lag first {} last {} ratio {:.3} samples={} syncs={} landed={} unlanded={unlanded} \
         lands_per_s={:.2} land_p99_us={} wall_ms={} load_start={load_start:.2} load_end={load_end:.2}",
        window(<[LagSample]>::first),
        window(<[LagSample]>::last),
        lag_ratio(samples),
        samples.len(),
        run.syncs,
        stats.landed,
        stats.lands_per_s,
        stats.latency_p99_us,
        run.wall.as_millis(),
    );
    for (minute, mean) in per_minute(samples).iter().enumerate() {
        println!("{TAG}minute {minute} lag_mean {mean:.2}");
    }

    // 4. The remote: no push rewrote the base (its reflog), every landed changeset once on its
    //    first parents, above the base it started from.
    let moves = remote.moves(&base_commit);
    assert!(moves.all > 1, "no push reached the remote's reflog");
    assert_eq!(moves.rewritten, 0, "a sync rewrote the imported base");
    let chain = remote.first_parents();
    let (root, above) = chain.split_last().expect("the base");
    assert_eq!(
        root,
        &(base_commit, String::new()),
        "the base was rewritten"
    );
    let mut on_remote: BTreeMap<&str, usize> = BTreeMap::new();
    for (commit, changeset) in above {
        assert!(!changeset.is_empty(), "{commit} carries no changeset");
        *on_remote.entry(changeset.as_str()).or_default() += 1;
    }
    let landed: Vec<&str> = (run.lands.iter().flatten())
        .filter_map(|record| record.id.as_deref())
        .collect();
    assert_eq!(
        on_remote.len(),
        landed.len(),
        "the remote holds other changesets"
    );
    for id in &landed {
        assert_eq!(on_remote.get(id), Some(&1), "{id} on the remote");
    }

    // 5. The store: the chain's rules without the latency bound (a time, never asserted here),
    //    and `verify --deep` clean.
    let unslowed: Vec<Vec<LandRecord>> = (run.lands.iter())
        .map(|lands| {
            (lands.iter())
                .map(|r| LandRecord {
                    latency_us: 0,
                    ..r.clone()
                })
                .collect()
        })
        .collect();
    let base_id = ChangesetId::new(ObjectId::from_hex(&base).expect("a changeset id"));
    let path = Utf8Path::new(store.path());
    if let Err(rule) = check_invariants(path, "main", base_id, &unslowed) {
        panic!("{rule}");
    }
    store.assert_clean("after the lane");
}

/// The rewrite count's self-check: a move of `main` above the base reads kept, a push over HTTP
/// of another root reads rewritten, so that the lane's zero is a reading of pushes.
#[test]
fn rewrites_are_read_from_the_remote_reflog() {
    let (remote, base) = Remote::with_base(1);
    remote
        .server
        .commit_on(REPO, "main", "above", &[("above", "1")]);

    // A push of another root over `main`, as a rewrite of the base would land.
    let root = remote
        .server
        .commit_on(REPO, "other", "another root", &[("root", "1")]);
    let url = remote.http.url(REPO);
    let refspec = format!("+{root}:refs/heads/main");
    remote.git(&["push", url.as_str(), refspec.as_str()]);

    let moves = remote.moves(&base);
    assert_eq!(
        (moves.all, moves.rewritten),
        (3, 1),
        "base, above, the other root"
    );
}

/// The real git the server runs, as an absolute path: the product cleans its children's `PATH`.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// `path` as text.
fn text(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}
