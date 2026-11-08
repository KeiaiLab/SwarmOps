//! The kill sweep of `ws land` (the M19 design §7, B3 acceptance 3): a real `kill -9` ends a
//! `ws land -m` at a random moment while an editor keeps writing fresh files beside it, and
//! no edit made before the kill is lost.
//!
//! ```text
//!  per format: a store, workspace W of 5,000 files
//!  each round:
//!    another lander rewrites 2,000 files on main   (W's land then writes them after its frame)
//!    W commits one file                            (its chain of one commit)
//!    spawn `ws land -m m` ── editor thread: notes/<round>/<i>.txt until the kill
//!    delay d ~ U[0.3·T, T] (T: the last uncut land) ── SIGKILL
//!    classify: before (nothing landed) · window (landed, a record left) · after
//!    `ws land -m m` again: completes, or E_WS_STALE ── `ws rebase` (the update)
//!    every note on disk byte for byte · the rest equal to W's head's tree
//!    verify --deep clean · W's commit landed on main once
//! ```
//!
//! Each kill is classified by what it left, read through the binary (`show` of what `main`
//! gained, `ws info` for the head) and the workspace's record files
//! (`.swarmws/journal/{pending,materialize}.json`). Each test owns its temp dir.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use swarmops_core::{ObjectKind, hash_object};
use swarmops_store::StoreFormat;
use swarmops_testkit::child::Child;
use swarmops_testkit::per_format;
use swarmops_testkit::ws::{Rng, content_of, path_of};
use swarmops_ws::pending::{JOURNAL_DIR, MATERIALIZE_JSON, PENDING_JSON};
use swarmops_ws::state::SWARMWS_DIR;

use crate::support::{self, Store};
use crate::ws::{AGENT, Out, inside};

/// The signal `Child::kill` sends on Unix.
const SIGKILL: i32 = 9;

/// The kills the fast variant makes per format (`just check`).
const FAST_KILLS: usize = 10;

/// The kills the slow variant makes per format.
const SLOW_KILLS: usize = 500;

/// Of the slow variant's kills, how many at least end a land in its window.
const SLOW_IN_WINDOW: usize = 250;

/// How many lands per kill a test may spend: a land that ends before its delay is no kill.
const RUNS_PER_KILL: usize = 4;

/// The seed of every test's delays; a failure prints it with the round.
const SEED: u64 = 0x19_4c11_1a4d;

/// The files of W's tree.
const TREE: usize = 5_000;

/// The directories they are in.
const DIRS: usize = 250;

/// The files the other lander rewrites on `main` each round: W's land writes them.
const LANDED: usize = 2_000;

/// The ref every land lands on.
const MAIN: &str = "main";

/// The workspace's name.
const W: &str = "w";

/// The directory the editor writes its notes in.
const NOTES: &str = "notes";

/// The shortest delay, in tenths of the window: d ~ U[0.3·T, T].
const SHORTEST_TENTHS: u64 = 3;

/// How long the editor waits between two notes.
const NOTE_EVERY: Duration = Duration::from_millis(20);

/// Where a kill hit, by what it left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// `main` holds no land of W's commit: before the store's land.
    Before,
    /// `main` holds it and W still holds a record (pending or intent): between the land's
    /// journal append and its last file write.
    Window,
    /// `main` holds it and W holds no record: after the land's last write.
    After,
}

/// The kills counted per [`Hit`].
#[derive(Debug, Default)]
struct Counts {
    /// Before the store's land.
    before: usize,
    /// In the window.
    window: usize,
    /// After it.
    after: usize,
}

impl Counts {
    /// Counts `hit`.
    fn add(&mut self, hit: Hit) {
        match hit {
            Hit::Before => self.before += 1,
            Hit::Window => self.window += 1,
            Hit::After => self.after += 1,
        }
    }
}

/// The editor beside a land: fresh notes under `notes/<round>/` until stopped.
struct Editor {
    /// Set to stop it.
    done: Arc<AtomicBool>,
    /// The thread; what it wrote, by repository path.
    thread: JoinHandle<BTreeMap<String, String>>,
}

impl Editor {
    /// Starts writing notes of `round` in the workspace at `dir`.
    fn start(dir: &Path, round: usize) -> Editor {
        let done = Arc::new(AtomicBool::new(false));
        let (dir, stop) = (dir.to_owned(), Arc::clone(&done));
        let thread = thread::spawn(move || {
            let mut wrote = BTreeMap::new();
            let folder = dir.join(NOTES).join(round.to_string());
            fs::create_dir_all(&folder).expect("make the notes' directory");
            for i in 0.. {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let text = format!("note {round} {i}\n");
                fs::write(folder.join(format!("{i}.txt")), &text).expect("write a note");
                wrote.insert(format!("{NOTES}/{round}/{i}.txt"), text);
                thread::sleep(NOTE_EVERY);
            }
            wrote
        });
        Editor { done, thread }
    }

    /// Stops it; every note it wrote, whole.
    fn stop(self) -> BTreeMap<String, String> {
        self.done.store(true, Ordering::Relaxed);
        self.thread.join().expect("the editor")
    }
}

/// How the killed land ended.
enum Ended {
    /// SIGKILLed.
    Killed,
    /// Before its delay, after this long: an uncut land.
    Exited(Duration),
}

/// One format's sweep: the store, W, and the window delays are drawn from.
struct Sweep {
    /// The store.
    store: Store,
    /// W's directory.
    dir: PathBuf,
    /// The last uncut land's wall time.
    window: Duration,
    /// The delays' generator.
    rng: Rng,
    /// What says which run failed.
    what: String,
}

impl Sweep {
    /// A store of `format` whose `main` holds the tree, and W of all of it.
    fn new(format: StoreFormat) -> Sweep {
        // 1. The tree on `main`.
        let store = Store::init_as(format);
        let paths: Vec<String> = (0..TREE).map(|i| path_of(i, DIRS)).collect();
        let texts: Vec<String> = (0..TREE).map(|i| content_of(i, 0)).collect();
        let puts: Vec<(&str, &str)> = paths
            .iter()
            .map(String::as_str)
            .zip(texts.iter().map(String::as_str))
            .collect();
        let changes = store.changes_of("tree", &puts);
        let made = store.run_as(AGENT, &["commit", "-m", "tree", "--changes", &changes], &[]);
        let first = made.ok()["changeset"].as_str().expect("an id").to_owned();
        let moved = store.run_as(AGENT, &["ref", "set", MAIN, &first, "--create"], &[]);
        moved.ok();

        // 2. W, of the whole tree.
        let dir = store.tmp().join(W);
        let path = dir.to_str().expect("UTF-8 temp paths");
        let line = [
            "--repo",
            store.path(),
            "--as",
            AGENT,
            "ws",
            "create",
            W,
            "--path",
            path,
            "--all",
        ];
        let created = crate::ws::run_in(store.tmp(), &[], &line, None);
        assert_eq!(created.ok()["created"], true, "{created}");

        Sweep {
            store,
            dir,
            window: Duration::ZERO,
            rng: Rng::new(SEED ^ u64::from(format.version())),
            what: format!("{format:?} seed {SEED:#x}"),
        }
    }

    /// `main`'s head.
    fn main_head(&self) -> String {
        let log = self.store.read(&["log", MAIN, "-n", "1"]).ok();
        log["entries"][0]["id"].as_str().expect("an id").to_owned()
    }

    /// The other lander: the first [`LANDED`] files at version `round % 2 + 1`, alternating
    /// so that the store keeps two copies of them, not one a round.
    fn other_lands(&self, round: usize) {
        let version = u32::try_from(round % 2 + 1).expect("a small version");
        let paths: Vec<String> = (0..LANDED).map(|i| path_of(i, DIRS)).collect();
        let texts: Vec<String> = (0..LANDED).map(|i| content_of(i, version)).collect();
        let puts: Vec<(&str, &str)> = paths
            .iter()
            .map(String::as_str)
            .zip(texts.iter().map(String::as_str))
            .collect();
        let changes = self.store.changes_of(&format!("other-{round}"), &puts);
        let args = [
            "commit",
            "-m",
            "other",
            "--changes",
            &changes,
            "--advance",
            MAIN,
        ];
        self.store.run_as(AGENT, &args, &[]).ok();
    }

    /// W's chain of one commit, of `w/<round>.txt`: the commit.
    fn chain(&self, round: usize) -> String {
        crate::ws::write(
            &self.dir,
            &format!("w/{round}.txt"),
            &format!("w {round}\n"),
        );
        let made = inside(&self.dir, &["ws", "commit", "-m", "c"]);
        made.ok()["changeset"].as_str().expect("an id").to_owned()
    }

    /// Spawns `ws land -m m` in W and SIGKILLs it `delay` after, unless it ended before.
    fn kill_after(&self, delay: Duration) -> Ended {
        let args = ["--as", AGENT, "ws", "land", "-m", "m"];
        let mut command = support::swarmops(&args, &[]);
        command.current_dir(&self.dir);
        let started = Instant::now();
        let mut child = Child::spawn(&mut command);
        let (output, _) = child.finish(started + delay);
        if output.status.signal() == Some(SIGKILL) {
            return Ended::Killed;
        }
        assert!(
            output.status.success(),
            "{}: an unkilled land: {output:?}",
            self.what
        );
        Ended::Exited(started.elapsed())
    }

    /// Whether W's journal holds `name`.
    fn holds(&self, name: &str) -> bool {
        self.dir
            .join(SWARMWS_DIR)
            .join(JOURNAL_DIR)
            .join(name)
            .exists()
    }

    /// The changesets `main` gained above `from` that squash `commit`.
    fn lands_of(&self, from: &str, commit: &str) -> usize {
        let log = self.store.read(&["log", MAIN, "-n", "8"]).ok();
        let entries = log["entries"].as_array().expect("entries").clone();
        let above = entries
            .iter()
            .map_while(|entry| entry["id"].as_str().filter(|id| *id != from));
        let squashed = |id: &str| {
            let shown = self.store.show(id);
            let chain = shown["squashed_from"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            chain.iter().any(|c| c == commit)
        };
        above.filter(|id| squashed(id)).count()
    }

    /// Where the kill of the land of `commit`, onto `main` at `from`, hit.
    fn classify(&self, from: &str, commit: &str) -> Hit {
        if self.lands_of(from, commit) == 0 {
            return Hit::Before;
        }
        if self.holds(PENDING_JSON) || self.holds(MATERIALIZE_JSON) {
            return Hit::Window;
        }
        Hit::After
    }

    /// The land run again after a kill at `hit`: it completes, or answers `E_WS_STALE`; one
    /// whose killed run finished all but its output may find nothing left to land. Its wall
    /// time when it was a whole land.
    fn land_again(&self, hit: Hit) -> Option<Duration> {
        let started = Instant::now();
        let again = inside(&self.dir, &["ws", "land", "-m", "m"]);
        let took = started.elapsed();
        let code = again.envelope()["error"]["code"]
            .as_str()
            .map(str::to_owned);
        match code.as_deref() {
            None => {}
            Some("E_WS_STALE") => return None,
            Some("E_NOTHING_TO_LAND") if hit == Hit::After => return None,
            Some(_) => panic!("{}: {hit:?}: the land again: {again}", self.what),
        }
        let done = again.ok();
        let whole = hit == Hit::Before && done["land"]["status"] == "landed";
        whole.then_some(took)
    }

    /// The update, `ws rebase`.
    fn update(&self) -> Out {
        let updated = inside(&self.dir, &["ws", "rebase"]);
        assert_eq!(updated.exit, 0, "{}: the update: {updated}", self.what);
        updated
    }

    /// W's files: every note in `notes` byte for byte, every other file equal to W's head's
    /// tree, and nothing else but notes.
    fn check_files(&self, notes: &BTreeMap<String, String>) {
        // 1. Every note written before the kill, as written.
        for (path, text) in notes {
            let read = fs::read_to_string(self.dir.join(path));
            assert_eq!(
                read.ok().as_deref(),
                Some(text.as_str()),
                "{}: {path}",
                self.what
            );
        }

        // 2. W's head's tree, file by file, against the disk.
        let info = inside(&self.dir, &["ws", "info"]).ok();
        let head = info["head"].as_str().expect("a head");
        let mut disk = files_of(&self.dir);
        for (path, id) in tree_of(&self.store, head) {
            let bytes = disk.remove(&path);
            let got = bytes.map(|bytes| hash_object(ObjectKind::Blob, &bytes).to_string());
            assert_eq!(
                got.as_deref(),
                Some(id.as_str()),
                "{}: {path} of {head}",
                self.what
            );
        }

        // 3. What is left is notes, the edits the update carried.
        let stray: Vec<&String> = disk
            .keys()
            .filter(|path| !notes.contains_key(*path))
            .collect();
        assert!(
            stray.is_empty(),
            "{}: files off {head}: {stray:?}",
            self.what
        );
    }
}

/// The files below `dir` but its `.swarmws`, by repository path: their bytes.
fn files_of(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(at) = todo.pop() {
        for entry in fs::read_dir(&at).expect("read a directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != SWARMWS_DIR) {
                    todo.push(path);
                }
                continue;
            }
            let rel = path.strip_prefix(dir).expect("below the workspace");
            let rel = rel.to_str().expect("UTF-8 paths").to_owned();
            files.insert(rel, fs::read(&path).expect("read a file"));
        }
    }
    files
}

/// The files of changeset `id`'s tree, by path: their blob ids.
fn tree_of(store: &Store, id: &str) -> BTreeMap<String, String> {
    let listed = store
        .read(&["ls", id, "--depth", "64", "--limit", "1000000"])
        .ok();
    assert_eq!(listed["truncated"], false, "{listed}");
    let entries = listed["entries"].as_array().expect("entries");
    let files = entries.iter().filter(|entry| entry["kind"] != "dir");
    let pair = |entry: &Value| {
        let path = entry["path"].as_str().expect("a path").to_owned();
        (path, entry["id"].as_str().expect("an id").to_owned())
    };
    files.map(pair).collect()
}

/// `kills` SIGKILLs of `ws land -m` on a store of `format`, each followed by the land again,
/// the update and the checks; the kills per [`Hit`].
fn kill_lands(format: StoreFormat, kills: usize) -> Counts {
    let mut sweep = Sweep::new(format);
    let mut counts = Counts::default();

    // 1. The window: one uncut land's wall time here.
    sweep.other_lands(0);
    sweep.chain(0);
    let started = Instant::now();
    inside(&sweep.dir, &["ws", "land", "-m", "m"]).ok();
    sweep.window = started.elapsed();

    let mut killed = 0;
    let mut round = 0;
    while killed < kills {
        round += 1;
        assert!(
            round <= kills * RUNS_PER_KILL,
            "{killed} kills in {round} rounds"
        );
        sweep.what = format!("{format:?} seed {SEED:#x} round {round}");

        // 2. Main moves under W, which commits one file.
        sweep.other_lands(round);
        let from = sweep.main_head();
        let commit = sweep.chain(round);

        // 3. The land, killed at d ~ U[0.3·T, T], the editor writing beside it.
        let window = u64::try_from(sweep.window.as_micros()).expect("a short land");
        let shortest = window * SHORTEST_TENTHS / 10;
        let delay = shortest + sweep.rng.next_u64() % (window - shortest).max(1);
        let delay = Duration::from_micros(delay);
        let editor = Editor::start(&sweep.dir, round);
        let ended = sweep.kill_after(delay);
        let notes = editor.stop();
        sweep.what = format!("{} delay {delay:?}", sweep.what);

        // 4. Where it hit; the land again, which completes or answers E_WS_STALE.
        match ended {
            Ended::Exited(took) => sweep.window = took,
            Ended::Killed => {
                killed += 1;
                let hit = sweep.classify(&from, &commit);
                counts.add(hit);
                if let Some(took) = sweep.land_again(hit) {
                    sweep.window = took;
                }
            }
        }

        // 5. The update; every note on disk, the rest W's head; the store clean; W's commit
        //    landed once.
        sweep.update();
        sweep.check_files(&notes);
        sweep.store.assert_clean(&sweep.what);
        let lands = sweep.lands_of(&from, &commit);
        assert_eq!(lands, 1, "{}: lands of {commit}", sweep.what);
    }

    println!(
        "{format:?}: {killed} kills in {round} rounds: {} before, {} in the window, {} after; \
         last window {:?}",
        counts.before, counts.window, counts.after, sweep.window
    );
    counts
}

per_format! {
    /// The fast variant: [`FAST_KILLS`] kills per format; it prints where they hit.
    fn sigkill_ws_land_keeps_edits(format) {
        kill_lands(format, FAST_KILLS);
    }
}

per_format! {
    /// The slow variant: [`SLOW_KILLS`] kills per format, [`SLOW_IN_WINDOW`] of them at least
    /// in the window.
    #[ignore = "slow, ~1,700 s: 500 kills of ws land per format, each with the land again, \
                the update, ls and verify --deep; estimated from 3.3-3.6 s a round in the fast \
                variant, not yet run whole"]
    fn sigkill_ws_land_500_times(format) {
        let counts = kill_lands(format, SLOW_KILLS);
        assert!(counts.window >= SLOW_IN_WINDOW, "{counts:?}");
    }
}
