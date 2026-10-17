//! Power loss at any point of a store's life, through the real binary (storage spec §2–§3): the
//! crash images of the store's test support (`power.rs`), opened with `swarmops`.
//!
//! A process crash (`crash.rs`) leaves the kernel's page cache in place, so it cannot show a
//! missing fsync, or writes not yet synced that a power loss tears, reorders or loses. Here one
//! run of a store's life is made in this process on a [`RecordingFs`], deterministically (one
//! thread, signatures at a fixed time): `init`; commits and ref moves (a create, moves, a delete)
//! with a compaction and a checkpoint among them; then commits past the checkpoint, whose index
//! records no sync has made durable. For a power failure after any step of the run, and a seed,
//! the [`Image`] of what the disk may hold is written to a temp dir, and the binary must find in
//! it, as RocksDB's crash tests put it, no hole:
//!
//! - no store before `init` returned: `info` says `E_NOT_A_REPO`, and `init` then makes one;
//! - otherwise a store in the state of a prefix of the transactions issued that holds every one
//!   whose commit point (the sync of its frame in `journal.log`) was passed, and none that had not
//!   begun: `info`, `ref list`, `show` of the last changeset counted and of the next (a seq is
//!   the changeset's place among them) and `log` agree on it; `verify --deep` finds nothing wrong
//!   (the temp files a crash leaves in `objects/tmp/` are no objects); and a new commit lands on
//!   it, after which `verify --deep` is clean again and the history is whole.
//!
//! Images that hold the same files are checked once.

mod support;

#[path = "../../swarmops-store/tests/support/power.rs"]
mod power;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use power::{Call, Chooser, Image, Keep, KeepAllBut, RecordingFs, SECTOR, Seeded, Timeline};
use serde_json::Value;
use swarmops_core::{ChangesetId, EntryKind, RepoPath, Signature};
use swarmops_repo::{CasResult, Change, ChangeOp, CommitRequest, LocalRepo};
use swarmops_store::{FsOps, Index, Layout, MetaLock, MetaStore};

/// How many images are checked at once, each in a temp dir of its own.
const AT_ONCE: usize = 8;

/// Who commits on a store after its power loss.
const AGENT: &str = "test-power/after";

/// One operation of the run.
#[derive(Debug, Clone, Copy)]
enum Op {
    /// A commit of `puts` on commit number `parent` (a root commit for `None`); its frame holds
    /// its changeset.
    Commit {
        parent: Option<usize>,
        puts: &'static [(&'static str, &'static str)],
    },
    /// A ref move by compare-and-swap, from commit number `expected` to `new` (`None`: the ref
    /// does not exist, before or after), recorded as `actor`; its frame holds the move.
    Cas {
        name: &'static str,
        expected: Option<usize>,
        new: Option<usize>,
        actor: &'static str,
    },
    /// A compaction of the index, the writer lock held: no frame.
    Compact,
    /// `MetaStore::flush`: the index synced and the checkpoint moved to the journal's end; no
    /// frame.
    Flush,
}

/// The run after `init`. The actors' lengths place frames where a lost sector takes their
/// header: frame 5's header (bytes 504..512) fills the end of the sector its write starts in,
/// the next sector holding the rest of the frame; frame 10's length field (1020..1024) ends its
/// first sector, its CRC starting the next.
const RUN: &[Op] = &[
    Op::Commit {
        parent: None,
        puts: &[
            ("README.md", "# power\n"),
            ("src/lib.rs", "pub fn f() {}\n"),
        ],
    },
    Op::Cas {
        name: "main",
        expected: None,
        new: Some(0),
        actor: "test-power/worker-1",
    },
    Op::Commit {
        parent: Some(0),
        puts: &[
            ("src/lib.rs", "pub fn f() -> u8 { 1 }\n"),
            ("docs/a.md", "a\n"),
        ],
    },
    Op::Cas {
        name: "main",
        expected: Some(0),
        new: Some(1),
        actor: "test-power/worker-1",
    },
    Op::Compact,
    Op::Commit {
        parent: Some(1),
        puts: &[("docs/b.md", "b\n")],
    },
    Op::Cas {
        name: "release/v1",
        expected: None,
        new: Some(2),
        actor: "test-power/worker-1",
    },
    Op::Flush,
    Op::Commit {
        parent: Some(2),
        puts: &[("src/lib.rs", "pub fn f() -> u8 { 3 }\n")],
    },
    Op::Cas {
        name: "main",
        expected: Some(1),
        new: Some(3),
        actor: "test-power/worker-1",
    },
    Op::Cas {
        name: "release/v1",
        expected: Some(2),
        new: None,
        actor: "test-power/worker-1-again",
    },
    Op::Commit {
        parent: Some(3),
        puts: &[("README.md", "# power, again\n")],
    },
    Op::Cas {
        name: "main",
        expected: Some(3),
        new: Some(4),
        actor: "test-power/worker-1",
    },
];

/// What one transaction of the run records in its frame.
#[derive(Debug, Clone, Copy)]
enum Txn {
    /// A commit's changeset (the commits' frames come in their order).
    Commit,
    /// A move of ref `name` to commit number `new`, or its deletion.
    Cas {
        name: &'static str,
        new: Option<usize>,
    },
}

/// One frame of the run, in journal order.
#[derive(Debug, Clone, Copy)]
struct Frame {
    what: Txn,
    /// The step that writes the frame to `journal.log`; the step after it syncs the frame, which
    /// is its transaction's commit point.
    write: usize,
}

/// The recorded run.
struct Recorded {
    /// The temp dir it ran in, its store at `<base>/store`: the calls' paths start with it.
    base: PathBuf,
    timeline: Timeline,
    /// The id of each commit, in hex, by number.
    commits: Vec<String>,
    /// The first parent of each commit, by number.
    parents: Vec<Option<usize>>,
    frames: Vec<Frame>,
    /// How many steps came before `init` returned.
    init: usize,
    /// The calls, as recorded.
    calls: Vec<Call>,
}

/// A fixed signature, so that a commit makes the same changeset in every run.
fn signature() -> Signature {
    Signature {
        name: b"test-power/worker-1".to_vec(),
        email: b"worker-1@test-power.swarmops.local".to_vec(),
        secs: 1_700_000_000,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// Runs `init`, then [`RUN`], on a [`RecordingFs`] in a temp dir, on a one-thread pool: objects
/// are written, and directories synced, one at a time, in the same order every run.
fn record() -> Recorded {
    let dir = tempfile::tempdir().expect("make a temp dir");
    let base = dir.path().canonicalize().expect("the temp dir's path");
    let root = Utf8PathBuf::from_path_buf(base.join("store")).expect("UTF-8 temp paths");
    let fs = Arc::new(RecordingFs::new());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("a one-thread pool");
    let (commits, parents) = pool.install(|| run(&root, &fs));
    let calls = fs.calls();
    let timeline = Timeline::new(&base, &calls);
    let log = base.join("store/meta/journal.log");
    let mut whats = RUN.iter().filter_map(|op| match *op {
        Op::Commit { .. } => Some(None),
        Op::Cas { name, new, .. } => Some(Some((name, new))),
        Op::Compact | Op::Flush => None,
    });
    let mut frames = Vec::new();
    for (at, call) in calls.iter().enumerate() {
        if let Call::AppendDurable { path, bytes } = call
            && *path == log
            && !bytes.is_empty()
        {
            let what = match whats.next().expect("a frame per commit and ref move") {
                None => Txn::Commit,
                Some((name, new)) => Txn::Cas { name, new },
            };
            let write = timeline.steps_of(at).start;
            frames.push(Frame { what, write });
        }
    }
    assert!(whats.next().is_none(), "a frame per commit and ref move");
    Recorded {
        base,
        init: timeline.mark("init"),
        timeline,
        commits: commits.iter().map(ToString::to_string).collect(),
        parents,
        frames,
        calls,
    }
}

/// The run itself; the commits' ids and first parents.
fn run(root: &Utf8Path, fs: &Arc<RecordingFs>) -> (Vec<ChangesetId>, Vec<Option<usize>>) {
    let shared: Arc<dyn FsOps> = Arc::clone(fs) as Arc<dyn FsOps>;
    let repo = LocalRepo::create_with_fs(root, "power", Arc::clone(&shared)).expect("init");
    fs.mark("init");
    let (mut commits, mut parents) = (Vec::new(), Vec::new());
    for (n, op) in RUN.iter().enumerate() {
        match *op {
            Op::Commit { parent, puts } => {
                let changes = puts
                    .iter()
                    .map(|(path, content)| Change {
                        path: RepoPath::new(*path).expect("a repo path"),
                        op: ChangeOp::Put {
                            kind: EntryKind::File,
                            content: content.as_bytes().to_vec(),
                        },
                    })
                    .collect();
                let request = CommitRequest {
                    parents: parent.map(|p| commits[p]).into_iter().collect(),
                    changes,
                    author: signature(),
                    committer: signature(),
                    message: format!("commit {}", commits.len()).into_bytes(),
                    metadata: Vec::new(),
                    allow_empty: false,
                };
                let outcome = repo.commit(request).expect("commit");
                assert!(outcome.warnings.is_empty(), "{outcome:?}");
                commits.push(outcome.id);
                parents.push(parent);
            }
            Op::Cas {
                name,
                expected,
                new,
                actor,
            } => {
                let expected = expected.map(|e| commits[e]);
                let outcome = repo
                    .cas_ref(name, expected, new.map(|t| commits[t]), actor)
                    .expect("cas_ref");
                assert!(matches!(outcome.result, CasResult::Applied { .. }));
                assert!(outcome.warnings.is_empty(), "{outcome:?}");
            }
            Op::Compact => {
                let layout = Layout::new(root);
                let _lock = MetaLock::acquire(&layout, Duration::from_secs(10)).expect("lock");
                let mut index = Index::open(&layout, Arc::clone(&shared)).expect("open");
                index.compact().expect("compact");
            }
            Op::Flush => {
                let mut meta = MetaStore::open_with_fs(root, Arc::clone(&shared)).expect("open");
                meta.set_parallelism(1);
                meta.flush().expect("flush");
            }
        }
        fs.mark(&format!("op {n}"));
    }
    (commits, parents)
}

/// The state of a store: its changesets and its refs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    /// How many changesets it holds: the first so many commits of the run.
    changesets: usize,
    /// Each ref's target (in hex) and version.
    refs: BTreeMap<String, (String, u64)>,
}

impl Recorded {
    /// The state after the first `frames` frames.
    fn state_after(&self, frames: usize) -> State {
        let mut state = State {
            changesets: 0,
            refs: BTreeMap::new(),
        };
        for frame in &self.frames[..frames] {
            match frame.what {
                Txn::Commit => state.changesets += 1,
                Txn::Cas { name, new: None } => {
                    state.refs.remove(name);
                }
                Txn::Cas {
                    name,
                    new: Some(new),
                } => {
                    let target = self.commits[new].clone();
                    let version = match state.refs.get(name) {
                        Some((old, version)) if *old == target => *version,
                        Some((_, version)) => version + 1,
                        None => 1,
                    };
                    state.refs.insert(name.to_owned(), (target, version));
                }
            }
        }
        state
    }

    /// The first-parent history from the commit whose id is `id` (in hex), newest first.
    fn history(&self, id: &str) -> Vec<String> {
        let mut at = self.commits.iter().position(|commit| commit == id);
        let mut history = Vec::new();
        while let Some(n) = at {
            history.push(self.commits[n].clone());
            at = self.parents[n];
        }
        history
    }

    /// How many frames the first `cut` steps synced: committed transactions, which a power loss
    /// must not take back.
    fn durable(&self, cut: usize) -> usize {
        self.frames
            .iter()
            .filter(|frame| frame.write + 1 < cut)
            .count()
    }

    /// How many frames the first `cut` steps began to write: no later one can be there.
    fn begun(&self, cut: usize) -> usize {
        self.frames.iter().filter(|frame| frame.write < cut).count()
    }

    /// How many filesystem calls the run made that change something (marks aside).
    fn calls(&self) -> usize {
        (self.calls.iter())
            .filter(|call| !matches!(call, Call::Mark(_)))
            .count()
    }

    /// What step `cut` is (the last one before the power failed), for messages.
    fn after(&self, cut: usize) -> String {
        match cut.checked_sub(1) {
            Some(last) => format!("after step {last}, {}", self.timeline.step(last).label),
            None => "before the first step".to_owned(),
        }
    }
}

/// What the binary found in an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// No store: `info` says `E_NOT_A_REPO`, and `init` then makes one.
    NoStore,
    /// A store in the state of the first so many frames.
    Prefix(usize),
}

/// One run of the binary, its envelope parsed.
struct Asked {
    exit: i32,
    envelope: Value,
    run: support::Run,
}

impl Asked {
    /// The result of a run that must have succeeded.
    fn ok(&self) -> Result<&Value, String> {
        if self.exit == 0 {
            Ok(&self.envelope["result"])
        } else {
            Err(format!("it failed: {}", self.run))
        }
    }

    /// The error code of a run that failed.
    fn code(&self) -> Option<&str> {
        self.envelope["error"]["code"].as_str()
    }
}

/// Runs the binary with `args`.
fn ask(args: &[&str]) -> Result<Asked, String> {
    let run = support::run(args, &[]);
    let Some(exit) = run.status.code() else {
        return Err(format!("killed by a signal: {run}"));
    };
    let envelope = serde_json::from_str(run.stdout.trim())
        .map_err(|e| format!("stdout is no JSON envelope ({e}): {run}"))?;
    Ok(Asked {
        exit,
        envelope,
        run,
    })
}

/// What `verify --deep` finds in the store at `store`.
fn problems(store: &str) -> Result<Vec<String>, String> {
    let asked = ask(&["--repo", store, "verify", "--deep"])?;
    let report = match (asked.exit, asked.code()) {
        (0, _) => &asked.envelope["result"],
        (1, Some("E_STORE_CORRUPT")) => &asked.envelope["error"]["details"],
        _ => return Err(format!("verify --deep: {}", asked.run)),
    };
    let problems = report["problems"]
        .as_array()
        .ok_or_else(|| format!("verify --deep reported no problem list: {}", asked.run))?;
    Ok(problems
        .iter()
        .map(|problem| problem.as_str().unwrap_or("?").to_owned())
        .collect())
}

/// `Ok` if `verify --deep` finds nothing wrong in the store at `store`; `when` says when.
fn clean(store: &str, when: &str) -> Result<(), String> {
    match problems(store)? {
        problems if problems.is_empty() => Ok(()),
        problems => Err(format!("verify --deep {when}: {problems:#?}")),
    }
}

/// The ids `log` lists from `rev`, newest first.
fn log(store: &str, rev: &str) -> Result<Vec<String>, String> {
    let asked = ask(&["--repo", store, "log", rev])?;
    let entries = asked.ok()?["entries"]
        .as_array()
        .ok_or_else(|| format!("log {rev}: no entries: {}", asked.run))?;
    Ok(entries
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or("?").to_owned())
        .collect())
}

/// What the binary shows of a store.
#[derive(Debug)]
struct Seen {
    state: State,
    /// What `show` gives as the seq of the last commit `info` counts and of the one after it,
    /// by number: `None` for `E_REV_NOT_FOUND`.
    shown: Vec<(usize, Option<u64>)>,
    /// What `log` lists from the last commit `info` counts: every commit there is, since the
    /// run's history is one line.
    history: Vec<String>,
}

impl Seen {
    /// Whether the store is in the state of the first `frames` frames of `rec`.
    fn is_after(&self, rec: &Recorded, frames: usize) -> bool {
        let state = rec.state_after(frames);
        let count = state.changesets;
        let shown = (self.shown.iter()).all(|&(n, seq)| seq == (n < count).then(|| to_u64(n)));
        let history = match count.checked_sub(1) {
            Some(last) => self.history == rec.history(&rec.commits[last]),
            None => self.history.is_empty(),
        };
        state == self.state && shown && history
    }
}

/// What the binary shows of the store at `store`, whose `info` gave `info`.
fn seen(rec: &Recorded, store: &str, info: &Value) -> Result<Seen, String> {
    let changesets = info["changesets"]
        .as_u64()
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| format!("info without changesets: {info}"))?;
    let listed = ask(&["--repo", store, "ref", "list"])?;
    let mut refs = BTreeMap::new();
    for entry in listed.ok()?["refs"].as_array().into_iter().flatten() {
        let name = entry["name"].as_str().unwrap_or("?").to_owned();
        let target = entry["target"].as_str().unwrap_or("?").to_owned();
        refs.insert(name, (target, entry["version"].as_u64().unwrap_or(0)));
    }
    let mut shown = Vec::new();
    for n in changesets.saturating_sub(1)..=changesets {
        let Some(id) = rec.commits.get(n) else {
            continue;
        };
        let asked = ask(&["--repo", store, "show", id])?;
        let seq = match (asked.exit, asked.code()) {
            (0, _) => Some(asked.envelope["result"]["seq"].as_u64().ok_or("no seq")?),
            (_, Some("E_REV_NOT_FOUND")) => None,
            _ => return Err(format!("show {id}: {}", asked.run)),
        };
        shown.push((n, seq));
    }
    let last = changesets
        .checked_sub(1)
        .and_then(|last| rec.commits.get(last));
    let history = match last {
        Some(last) => log(store, last)?,
        None => Vec::new(),
    };
    Ok(Seen {
        state: State { changesets, refs },
        shown,
        history,
    })
}

/// Writes `image` to a fresh temp dir and finds out what the binary makes of it.
fn found_in(rec: &Recorded, image: &Image) -> Result<Found, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("a temp dir: {e}"))?;
    image
        .materialize(dir.path())
        .map_err(|e| format!("writing the image: {e}"))?;
    let path = dir.path().join("store");
    let store = path.to_str().expect("UTF-8 temp paths");
    let info = ask(&["--repo", store, "info"])?;
    let formatted = image.file(Path::new("store/FORMAT")).is_some();
    match (info.exit, info.code()) {
        (_, Some("E_NOT_A_REPO")) if !formatted => no_store(store),
        (0, _) if formatted => a_store(rec, store, info.ok()?, dir.path()),
        _ => Err(format!("info, with FORMAT {formatted}: {}", info.run)),
    }
}

/// [`found_in`] a directory without a store, whose `info` said `E_NOT_A_REPO`: `init` makes it.
fn no_store(store: &str) -> Result<Found, String> {
    let init = ask(&["init", store])?;
    let made = init.ok()?;
    if made["changesets"] != 0 || made["refs"] != 0 {
        return Err(format!("init made a store that is not empty: {}", init.run));
    }
    clean(store, "after init")?;
    Ok(Found::NoStore)
}

/// [`found_in`] a store, whose `info` gave `info`; `tmp` takes a change list.
fn a_store(rec: &Recorded, store: &str, info: &Value, tmp: &Path) -> Result<Found, String> {
    let seen = seen(rec, store, info)?;
    let frames = (0..=rec.frames.len())
        .find(|&frames| seen.is_after(rec, frames))
        .ok_or_else(|| format!("the state of no prefix of the run: {seen:#?}"))?;
    clean(store, "after the power loss")?;

    // A commit lands on it, and leaves it clean, its history whole.
    let changes = tmp.join("after.json");
    let list = r#"[{"path": "after.txt", "op": "put", "content": {"inline": "after\n"}}]"#;
    fs::write(&changes, list).map_err(|e| format!("a change list: {e}"))?;
    let changes = changes.to_str().expect("UTF-8 temp paths");
    let args = ["--repo", store, "--as", AGENT, "commit", "-m", "after"];
    let commit = ask(&[&args[..], &["--changes", changes]].concat())?;
    let result = commit.ok()?;
    if result.get("warnings").is_some() {
        return Err(format!("the next commit warned: {}", commit.run));
    }
    let new = result["changeset"].as_str().ok_or("no changeset")?;
    clean(store, "after the next commit")?;
    let state = rec.state_after(frames);
    let mut history = vec![new.to_owned()];
    if let Some((main, _)) = state.refs.get("main") {
        history.extend(rec.history(main));
    }
    match log(store, new)? {
        logged if logged == history => Ok(Found::Prefix(frames)),
        logged => Err(format!(
            "log of the next commit: {logged:?}, not {history:?}"
        )),
    }
}

/// One distinct image a sweep checked: the cuts and seeds whose images hold its files, and
/// what the binary found in it.
struct Checked<'a> {
    uses: &'a [(usize, u64)],
    found: Result<Found, String>,
}

/// What a sweep found.
struct Swept {
    /// How many distinct images it checked.
    images: usize,
    /// How many of them held each prefix of the frames, and how many no store.
    found: BTreeMap<Option<usize>, usize>,
}

/// Checks the image of every cut of `cuts` for every seed of `seeds` ([`Seeded`]): the binary
/// must find no store before `init` returned, and otherwise a prefix of the run that holds every
/// frame the cut synced and none it had not begun. Each distinct image is checked once, built
/// again from its first cut and seed (images are cheap to build, not to keep).
fn sweep(rec: &Recorded, cuts: &[usize], seeds: &[u64]) -> Swept {
    let mut by_digest: HashMap<u64, Vec<(usize, u64)>> = HashMap::new();
    for &cut in cuts {
        for &seed in seeds {
            let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
            by_digest
                .entry(image.digest())
                .or_default()
                .push((cut, seed));
        }
    }
    let mut distinct: Vec<Vec<(usize, u64)>> = by_digest.into_values().collect();
    distinct.sort_unstable();
    let mut checked: Vec<Checked<'_>> = Vec::new();
    for batch in distinct.chunks(AT_ONCE) {
        thread::scope(|scope| {
            let runs: Vec<_> = batch
                .iter()
                .map(|uses| {
                    scope.spawn(move || {
                        let (cut, seed) = uses[0];
                        let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                        found_in(rec, &image)
                    })
                })
                .collect();
            for (uses, run) in batch.iter().zip(runs) {
                let found = run
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                checked.push(Checked { uses, found });
            }
        });
    }
    let mut failures = Vec::new();
    let mut found = BTreeMap::new();
    for Checked {
        uses,
        found: result,
    } in &checked
    {
        for &(cut, seed) in *uses {
            if let Err(why) = allowed(rec, cut, result) {
                let image = rec.timeline.image(cut, &mut Seeded::new(seed, cut));
                let after = rec.after(cut);
                failures.push(format!(
                    "cut {cut} ({after}), seed {seed}: {why}\nlost:\n{}",
                    image.describe()
                ));
            }
        }
        if let Ok(result) = result {
            let prefix = match result {
                Found::NoStore => None,
                Found::Prefix(frames) => Some(*frames),
            };
            *found.entry(prefix).or_default() += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cuts and seeds failed, over {} distinct images; the first ones:\n\n{}",
        failures.len(),
        cuts.len() * seeds.len(),
        checked.len(),
        failures[..failures.len().min(5)].join("\n\n")
    );
    Swept {
        images: checked.len(),
        found,
    }
}

/// `Ok` if `found` is what the image of a power failure after the first `cut` steps may hold.
fn allowed(rec: &Recorded, cut: usize, found: &Result<Found, String>) -> Result<(), String> {
    let (durable, begun) = (rec.durable(cut), rec.begun(cut));
    match found {
        Err(why) => Err(why.clone()),
        Ok(Found::NoStore) if cut < rec.init => Ok(()),
        Ok(Found::NoStore) => Err("no store, though init had returned".to_owned()),
        Ok(Found::Prefix(frames)) if (durable..=begun).contains(frames) => Ok(()),
        Ok(Found::Prefix(frames)) => Err(format!(
            "the first {frames} frames, not {durable} to {begun}: a committed transaction lost, \
             or one never begun found"
        )),
    }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("usize fits in u64")
}

/// The image of a power failure after `cut` steps that keeps every directory change and every
/// file's unsynced bytes, but loses those of the files `lose` names as it says.
fn image_losing(rec: &Recorded, cut: usize, lose: &[(&str, Keep)]) -> Image {
    let mut chooser = KeepAllBut(|file: &Path| {
        lose.iter()
            .find(|(path, _)| file == Path::new(path))
            .map(|&(_, keep)| keep)
    });
    rec.timeline.image(cut, &mut chooser as &mut dyn Chooser)
}

/// The power fails after any step of a store's life (`init`, commits, ref moves, a compaction,
/// a checkpoint, commits past it), and the disk keeps nothing that was not durable (seed 0);
/// or, at every twentieth cut, what two seeds pick at random of it. The binary finds a prefix of
/// the run that holds every committed transaction, or no store before `init` returned: see the
/// module documentation. The slow variant checks every cut with more seeds.
#[test]
fn power_loss_images_recover_a_prefix_with_every_acknowledged_commit() {
    let rec = record();
    let cuts: Vec<usize> = (0..=rec.timeline.steps()).collect();
    let swept = sweep(&rec, &cuts, &[0]);
    let sampled: Vec<usize> = cuts.iter().copied().filter(|cut| cut % 20 == 0).collect();
    let random = sweep(&rec, &sampled, &[2, 3]);
    println!(
        "power loss: {} calls in {} steps, {} frames; {} distinct images with nothing kept, \
         {} at random",
        rec.calls(),
        rec.timeline.steps(),
        rec.frames.len(),
        swept.images,
        random.images
    );
    let all: Vec<Option<usize>> = (0..=rec.frames.len()).map(Some).collect();
    for frames in &all {
        assert!(
            swept.found.contains_key(frames),
            "no image held the first {frames:?} frames: {:?}",
            swept.found
        );
    }
    assert!(swept.found.contains_key(&None), "no image without a store");
}

/// [`power_loss_images_recover_a_prefix_with_every_acknowledged_commit`] at every cut, for ten
/// seeds: nothing kept, everything kept, and eight at random.
#[test]
#[ignore = "slow: thousands of images through the binary; just check-slow runs it"]
fn power_loss_images_recover_a_prefix_with_every_acknowledged_commit_at_every_cut() {
    let rec = record();
    let cuts: Vec<usize> = (0..=rec.timeline.steps()).collect();
    let seeds: Vec<u64> = (0..10).collect();
    let swept = sweep(&rec, &cuts, &seeds);
    println!(
        "power loss: {} calls in {} steps; {} distinct images, by the frames they held: {:?}",
        rec.calls(),
        rec.timeline.steps(),
        swept.images,
        swept.found
    );
    for frames in 0..=rec.frames.len() {
        assert!(swept.found.contains_key(&Some(frames)), "{:?}", swept.found);
    }
}

/// The index files past the checkpoint (`idx/*.col`, `parents.log`) are appended to without a
/// sync: a power loss can keep a file's length and lose its bytes, zeros in their place. Records
/// so zero-filled hold nothing the journal vouches for: readers must not count them (the frames
/// past the checkpoint give the changesets), `verify --deep` must not take them for damage, and
/// the next writer must replay the journal's records in their place, whichever file lost its
/// bytes, or all five did.
#[test]
fn zero_filled_index_tail_after_power_loss_is_replayed_not_counted() {
    let rec = record();
    let cut = rec.timeline.steps();
    let index = [
        "store/meta/parents.log",
        "store/meta/idx/par.col",
        "store/meta/idx/gen.col",
        "store/meta/idx/dh.col",
        "store/meta/idx/ids.col",
    ];
    let mut failures = Vec::new();
    for lost in index.iter().map(|file| vec![*file]).chain([index.to_vec()]) {
        let lose: Vec<(&str, Keep)> = lost.iter().map(|file| (*file, Keep::ZeroFilled)).collect();
        let image = image_losing(&rec, cut, &lose);
        for file in &lost {
            let bytes = image.file(Path::new(file)).expect("an index file");
            assert!(
                bytes.ends_with(&[0; 32]) || bytes.ends_with(&[0; 8]),
                "{file}: the records past the checkpoint are zeros now"
            );
        }
        match found_in(&rec, &image) {
            Ok(Found::Prefix(frames)) if frames == rec.frames.len() => {}
            other => failures.push(format!("{lost:?} zero-filled: {other:?}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// A frame is written to `journal.log` and then synced: a power loss in between may persist a
/// later sector of it and lose the first, its header there. Its length then reads 0, which no
/// writer writes, followed by bytes that are not zero. The frame never reached its commit point
/// (its sync would have made its header durable), so it is a torn tail, cut like any other: the
/// store opens and holds the frames before it. This holds where the whole header lies in the lost
/// sector, and where only its length does, its CRC across the sector boundary.
#[test]
fn a_lost_header_sector_is_a_torn_tail() {
    let rec = record();
    let log = "store/meta/journal.log";
    let mut torn = Vec::new();
    let mut failures = Vec::new();
    for (n, frame) in rec.frames.iter().enumerate() {
        let [(at, len)] = rec.timeline.writes(frame.write)[..] else {
            panic!("frame {n}: one write");
        };
        let boundary = (at / SECTOR + 1) * SECTOR;
        // The length field in the first sector the write touches, the frame's end past it.
        if boundary < at + 4 || at + len <= boundary {
            continue;
        }
        let image = image_losing(&rec, frame.write + 1, &[(log, Keep::FirstSectorLost)]);
        let bytes = image.file(Path::new(log)).expect("journal.log");
        let start = usize::try_from(at).unwrap();
        assert_eq!(
            &bytes[start..start + 4],
            &[0; 4],
            "frame {n}: its length lost"
        );
        assert!(
            bytes[start..].iter().any(|&byte| byte != 0),
            "frame {n}: a later sector kept"
        );
        torn.push((n, boundary - at));
        match found_in(&rec, &image) {
            Ok(Found::Prefix(frames)) if frames == n => {}
            other => failures.push(format!(
                "frame {n} at {at}..{}, its first {} bytes lost: {other:?}",
                at + len,
                boundary - at
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert!(
        torn.iter().any(|&(_, lost)| lost >= 8),
        "a frame whose whole header lies in a lost sector: {torn:?}"
    );
    assert!(
        torn.iter().any(|&(_, lost)| (4..8).contains(&lost)),
        "a frame whose length lies in a lost sector, its CRC across the boundary: {torn:?}"
    );
}

/// An object is linked into its fan-out directory and is durable only once that directory is
/// synced (`ObjectStore::flush`); a commit flushes its objects before it appends its frame. So
/// once a commit's frame is durable, every object linked before it survives a power loss at any
/// later point, whole, even when the disk keeps nothing else that was not durable; and the store
/// verifies clean then. The images see what a process crash cannot: an object linked, its
/// directory not yet synced, that a power loss takes back.
#[test]
fn objects_referenced_by_a_durable_frame_survive_power_loss() {
    let rec = record();
    let objects = rec.base.join("store/objects");
    let mut temps: HashMap<&Path, &[u8]> = HashMap::new();
    // Each object file the run linked, the step that did, and its content.
    let mut links: Vec<(PathBuf, usize, &[u8])> = Vec::new();
    for (at, call) in rec.calls.iter().enumerate() {
        match call {
            Call::WriteDurableTemp { path, bytes } => {
                temps.insert(path, bytes);
            }
            Call::LinkNoclobber {
                from,
                to,
                placed: true,
            } if to.parent().and_then(Path::parent) == Some(&objects) => {
                let rel = to.strip_prefix(&rec.base).expect("under the base");
                let linked = rec.timeline.steps_of(at).start;
                links.push((rel.to_owned(), linked, temps[from.as_path()]));
            }
            _ => {}
        }
    }
    assert!(links.len() > 10, "the run links its objects: {links:?}");

    let least = |cut: usize| rec.timeline.image(cut, &mut Seeded::new(0, cut));
    // Linked, but its directory not synced: a power loss takes the object back.
    let window = links
        .iter()
        .any(|(path, linked, _)| least(linked + 1).file(path).is_none());
    assert!(
        window,
        "some object is linked before its directory is synced"
    );

    // Each commit's frame, the step after which it is durable, and the step that writes it.
    let commits: Vec<(usize, usize, usize)> = (rec.frames.iter().enumerate())
        .filter(|(_, frame)| matches!(frame.what, Txn::Commit))
        .map(|(n, frame)| (n, frame.write + 2, frame.write))
        .collect();
    let first = commits.first().expect("the run commits").1;
    for cut in first..=rec.timeline.steps() {
        let image = least(cut);
        // The objects linked before a frame durable by now.
        let written = (commits.iter().rev())
            .find(|&&(_, durable, _)| durable <= cut)
            .map_or(0, |&(_, _, write)| write);
        for (path, _, bytes) in links.iter().filter(|(_, linked, _)| *linked < written) {
            assert_eq!(
                image.file(path),
                Some(*bytes),
                "{}: {} is not whole, though a frame written after it is durable",
                rec.after(cut),
                path.display()
            );
        }
    }
    for &(n, durable, _) in &commits {
        let found = found_in(&rec, &least(durable));
        assert_eq!(found, Ok(Found::Prefix(n + 1)), "frame {n} just durable");
    }
}
