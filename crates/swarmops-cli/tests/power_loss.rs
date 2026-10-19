//! Power loss at any point of a store's life, through the real binary (storage spec §2–§3): the
//! crash images of the test kit (`swarmops_testkit::power`), opened with `swarmops`.
//!
//! A process crash (`crash.rs`) leaves the kernel's page cache in place, so it cannot show a
//! missing fsync, or writes not yet synced that a power loss tears, reorders or loses. Here one
//! run of a store's life is made in this process on an [`ImageRecordingFs`], deterministically
//! (one thread, signatures at a fixed time): `init`; commits and ref moves (a create, moves, a
//! delete) with a compaction and a checkpoint among them; then commits past the checkpoint, whose
//! index records no sync has made durable. The slow variant's run ends with a transaction of
//! more than 1,024 changesets, which its writer compacts after, as every writer does past that
//! many records: an index sync, the compaction, a `Compacted` frame, the checkpoint. For a power failure after any step of the run, and a
//! seed, the [`Image`] of what the disk may hold is written to a temp dir, and the binary must
//! find in it, as RocksDB's crash tests put it, no hole:
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use swarmops_core::bytes::to_u64;
use swarmops_core::{Changeset, ChangesetId, EntryKind, ObjectKind, RepoPath, Signature, Tree};
use swarmops_repo::{CasResult, Change, ChangeOp, CommitRequest, LocalRepo};
use swarmops_store::{
    ChangesetMeta, FsObjectStore, FsOps, Index, Journal, Layout, MetaLock, MetaStore, ObjectStore,
    RealFs, Record,
};
use swarmops_testkit::power::{
    Chooser, Image, ImageRecordingFs, Keep, KeepAllBut, PAGE, RecordedCall, SECTOR, Seeded,
    Timeline,
};

use crate::support;

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
    /// [`BULK`] root changesets of the empty tree in one transaction of their own, their objects
    /// put and flushed first (between the marks `bulk objects` and `bulk objects flushed`): its
    /// frame takes the index's tail past [`Index::COMPACT_THRESHOLD`], so its writer syncs the
    /// index, compacts, appends a `Compacted` frame (no transaction's) and moves the checkpoint.
    Bulk,
    /// A commit of `puts` on commit number `parent` that advances ref `name` from it (a commit
    /// with `advance`): its frame holds its changeset and the ref's move to it, together.
    CommitAdvance {
        parent: Option<usize>,
        puts: &'static [(&'static str, &'static str)],
        name: &'static str,
    },
}

/// How many changesets [`Op::Bulk`] records in its one frame: more than the index's compaction
/// threshold, whatever tail the run left before.
const BULK: u64 = Index::COMPACT_THRESHOLD + 1;

/// The run after `init`. The actors' lengths place frames where a lost sector takes their
/// header: frame 5's header (bytes 504..512) fills the end of the sector its write starts in,
/// the next sector holding the rest of the frame; frame 10's length field (1020..1024) ends its
/// first sector, its CRC starting the next; and frame 13's length field (1535..1539) straddles a
/// sector boundary, its low byte alone in the first sector, its payload over 255 bytes, so that a
/// lost first sector leaves it a length that is not 0 but wrong (the long actors of frames 12 and
/// 13 place it).
/// Frame 14, the last, holds a changeset and main's move to it ([`Op::CommitAdvance`]).
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
    Op::Commit {
        parent: Some(4),
        puts: &[("docs/c.md", "c\n")],
    },
    Op::Cas {
        name: "main",
        expected: Some(4),
        new: Some(5),
        actor: concat!(
            "test-power/worker-1-whose-name-is-long-enough-to-place-the-next-frame-",
            "where-its-length-field-straddles-a-sector-boundary-",
            "01234567890123456789012345678901234567890123456789012345678901234567890123456",
        ),
    },
    Op::Cas {
        name: "release/v2",
        expected: None,
        new: Some(5),
        actor: concat!(
            "test-power/worker-1-whose-name-is-long-enough-for-a-payload-over-255-bytes-",
            "so-that-the-length-keeps-a-byte-that-is-not-zero-when-its-low-byte-is-lost-",
            "01234567890123456789012345678901234567890123456789",
        ),
    },
    Op::CommitAdvance {
        parent: Some(5),
        puts: &[("src/lib.rs", "pub fn f() -> u8 { 6 }\n")],
        name: "main",
    },
];

/// What one transaction of the run records in its frame.
#[derive(Debug, Clone, Copy)]
enum Txn {
    /// A commit's changeset (the commits' frames come in their order).
    Commit,
    /// [`Op::Bulk`]'s [`BULK`] changesets, in one frame.
    Bulk,
    /// A move of ref `name` to commit number `new`, or its deletion.
    Cas {
        name: &'static str,
        new: Option<usize>,
    },
    /// [`Op::CommitAdvance`]'s changeset and the move of ref `name` to it, in one frame.
    CommitAdvance { name: &'static str },
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
    /// The step that writes each `Compacted` frame, which is no transaction's, so not one of
    /// `frames`.
    compacted: Vec<usize>,
    /// How many steps came before `init` returned.
    init: usize,
    /// The calls, as recorded.
    calls: Vec<RecordedCall>,
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

/// Runs `init`, then [`RUN`], on an [`ImageRecordingFs`] in a temp dir, on a one-thread pool:
/// objects are written, and directories synced, one at a time, in the same order every run.
fn record() -> Recorded {
    record_ops(RUN)
}

/// [`record`], with the run `ops` after `init`.
///
/// The recorded order of the calls, so the steps and the images, is the same in every run
/// because three settings keep the run to one thread: the one-thread rayon pool it runs in,
/// where `put_batch` writes the objects and `flush` syncs the directories one at a time;
/// `set_parallelism(1)` on the stores [`Op::Flush`] and [`Op::Bulk`] open, which sync the index
/// files one at a time; and the compaction handle of [`Op::Compact`], whose `sync_threads` is 1
/// unless set. `LocalRepo`'s own metadata store keeps the default parallelism (5), which is
/// deterministic only while no transaction of it syncs the index: none reaches
/// `CHECKPOINT_EVERY` (32 frames past the checkpoint) or the compaction threshold, so a `RUN`
/// that grows past 32 frames since [`Op::Flush`] would record its index syncs in an order that
/// varies between runs (the oracle stays valid; the images change), and [`Op::Bulk`] passes the
/// threshold on a store of its own.
fn record_ops(ops: &[Op]) -> Recorded {
    let dir = tempfile::tempdir().expect("make a temp dir");
    let base = dir.path().canonicalize().expect("the temp dir's path");
    let root = Utf8PathBuf::from_path_buf(base.join("store")).expect("UTF-8 temp paths");
    let fs = Arc::new(ImageRecordingFs::new());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("a one-thread pool");
    let (commits, parents) = pool.install(|| run(&root, &fs, ops));
    let calls = fs.calls();
    let timeline = Timeline::new(&base, &calls);
    let log = base.join("store/meta/journal.log");
    // Each frame's transaction, in order; `None` for the `Compacted` frame that follows a bulk
    // transaction, which is no transaction's.
    let mut whats = ops.iter().flat_map(|op| match *op {
        Op::Commit { .. } => vec![Some(Txn::Commit)],
        Op::Cas { name, new, .. } => vec![Some(Txn::Cas { name, new })],
        Op::Bulk => vec![Some(Txn::Bulk), None],
        Op::CommitAdvance { name, .. } => vec![Some(Txn::CommitAdvance { name })],
        Op::Compact | Op::Flush => Vec::new(),
    });
    let (mut frames, mut compacted) = (Vec::new(), Vec::new());
    for (at, call) in calls.iter().enumerate() {
        if let RecordedCall::AppendDurable { path, bytes } = call
            && *path == log
            && !bytes.is_empty()
        {
            let what = whats.next().expect("a frame per commit and ref move");
            if let Some(what) = what {
                let write = timeline.steps_of(at).start;
                frames.push(Frame { what, write });
            } else {
                compacted.push(timeline.steps_of(at).start);
            }
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
        compacted,
        calls,
    }
}

/// The run itself, `init` then `ops`; the changesets' ids and first parents, in their order.
fn run(
    root: &Utf8Path,
    fs: &Arc<ImageRecordingFs>,
    ops: &[Op],
) -> (Vec<ChangesetId>, Vec<Option<usize>>) {
    let shared: Arc<dyn FsOps> = Arc::clone(fs) as Arc<dyn FsOps>;
    let repo = LocalRepo::create_with_fs(root, "power", Arc::clone(&shared)).expect("init");
    fs.mark("init");
    let (mut commits, mut parents) = (Vec::new(), Vec::new());
    for (n, op) in ops.iter().enumerate() {
        match *op {
            Op::Commit { parent, puts } | Op::CommitAdvance { parent, puts, .. } => {
                let advance = match *op {
                    Op::CommitAdvance { name, .. } => Some(swarmops_repo::Advance {
                        name: name.to_owned(),
                        actor: "test-power/worker-1".to_owned(),
                    }),
                    _ => None,
                };
                let advancing = advance.is_some();
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
                    advance,
                };
                let outcome = repo.commit(request).expect("commit");
                assert!(outcome.warnings.is_empty(), "{outcome:?}");
                let moved = outcome.advanced.as_ref().map(|moved| moved.already_applied);
                assert_eq!(moved, advancing.then_some(false), "{outcome:?}");
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
            Op::Bulk => {
                let ids = bulk(root, &shared, fs);
                parents.extend(ids.iter().map(|_| None));
                commits.extend(ids);
            }
        }
        fs.mark(&format!("op {n}"));
    }
    (commits, parents)
}

/// [`Op::Bulk`] on the store at `root`, its calls through `shared` (the recorder `fs`, which
/// marks where its objects are written); the changesets' ids, in the order it records them.
fn bulk(root: &Utf8Path, shared: &Arc<dyn FsOps>, fs: &ImageRecordingFs) -> Vec<ChangesetId> {
    let tree = Tree::empty_root();
    let changesets: Vec<Changeset> = (0..BULK)
        .map(|n| Changeset {
            parents: Vec::new(),
            root: tree.id(),
            delta: Vec::new(),
            author: signature(),
            committer: signature(),
            message: format!("bulk {n}").into_bytes(),
            extra_headers: Vec::new(),
            metadata: Vec::new(),
        })
        .collect();
    let mut objects = vec![(ObjectKind::Tree, tree.encode())];
    for changeset in &changesets {
        objects.push((ObjectKind::Changeset, changeset.encode().expect("encode")));
    }
    fs.mark("bulk objects");
    let store = FsObjectStore::open(Layout::new(root), Box::new(Arc::clone(shared)));
    store.put_batch(objects).expect("put the objects");
    store.flush().expect("flush the objects");
    fs.mark("bulk objects flushed");
    let mut meta = MetaStore::open_with_fs(root, Arc::clone(shared)).expect("open");
    meta.set_parallelism(1);
    let mut write = meta.write().expect("write");
    let mut ids = Vec::new();
    for changeset in &changesets {
        let id = changeset.id().expect("the id");
        let delta_hash = changeset.delta_hash().expect("the delta hash");
        let record = ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash,
            parents: Vec::new(),
        };
        write.insert_changeset(record).expect("insert");
        ids.push(id);
    }
    let info = write.commit().expect("commit");
    assert!(
        info.checkpointed,
        "it passed the compaction threshold: {info:?}"
    );
    ids
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
                Txn::Bulk => state.changesets += usize::try_from(BULK).unwrap(),
                Txn::Cas { name, new: None } => {
                    state.refs.remove(name);
                }
                Txn::Cas {
                    name,
                    new: Some(new),
                } => self.move_ref(&mut state, name, new),
                Txn::CommitAdvance { name } => {
                    let new = state.changesets;
                    state.changesets += 1;
                    self.move_ref(&mut state, name, new);
                }
            }
        }
        state
    }

    /// `state` with ref `name` moved to commit number `new`: its version grows by one when its
    /// target changes, and is 1 for a new ref.
    fn move_ref(&self, state: &mut State, name: &str, new: usize) {
        let target = self.commits[new].clone();
        let version = match state.refs.get(name) {
            Some((old, version)) if *old == target => *version,
            Some((_, version)) => version + 1,
            None => 1,
        };
        state.refs.insert(name.to_owned(), (target, version));
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

    /// How many frames of `journal.log` the first `cut` steps began to write, the `Compacted`
    /// ones too: an image holds no other whole frame.
    fn frames_begun(&self, cut: usize) -> usize {
        self.begun(cut) + self.compacted.iter().filter(|&&write| write < cut).count()
    }

    /// How many filesystem calls the run made that change something (marks aside).
    fn calls(&self) -> usize {
        (self.calls.iter())
            .filter(|call| !matches!(call, RecordedCall::Mark(_)))
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

/// Writes `image`, of a power failure after the first `cut` steps, to a fresh temp dir and finds
/// out what the binary makes of it.
fn found_in(rec: &Recorded, image: &Image, cut: usize) -> Result<Found, String> {
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
        (0, _) if formatted => a_store(rec, store, info.ok()?, dir.path(), cut),
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

/// The journal of the store at `store`, on the real filesystem.
fn journal_of(store: &str) -> Result<Journal, String> {
    let layout = Layout::new(Utf8PathBuf::from(store));
    Journal::open(&layout, Arc::new(RealFs)).map_err(|e| format!("open the journal: {e}"))
}

/// Where the frame that records changeset `id` (in hex) starts in the journal of the store at
/// `store`.
fn frame_of(store: &str, id: &str) -> Result<u64, String> {
    let frames = journal_of(store)?
        .tail_frames_from(0)
        .map_err(|e| format!("read the journal: {e}"))?;
    let records_it = |records: &[Record]| {
        records.iter().any(|record| {
            matches!(record, Record::Changeset { id: recorded, .. } if recorded.to_string() == id)
        })
    };
    frames
        .iter()
        .find(|frame| records_it(&frame.records))
        .map(|frame| frame.offset)
        .ok_or_else(|| format!("no frame records {id}"))
}

/// The journal an image holds, before the first commit on it repairs it.
#[derive(Debug, Clone, Copy)]
struct Imaged {
    /// The length of its `journal.log`.
    log_len: u64,
    /// The whole frames in it, as a reader reads them: the listed ones, and the whole ones after
    /// them that `journal.off` does not list, up to a torn tail.
    whole: u64,
    /// How many frames its `journal.off` lists, as readers count them (zeroed entries at its end
    /// dropped).
    entries: u64,
}

/// `Ok` if `recovered`, the `meta.recovered` of the first commit on the image of a power failure
/// after the first `cut` steps, says what that commit's write repaired of `imaged`: where the
/// commit's frame landed (`landed`) is where the journal ended after the repair, so a cut is
/// reported exactly when that is short of the image's `journal.log` end, from there to that end;
/// and the frames listed are the whole ones `journal.off` did not list, no more than the run had
/// begun to write, a `Compacted` frame counted too, past those it listed.
fn recovered_as_imaged(
    rec: &Recorded,
    cut: usize,
    recovered: &Value,
    imaged: Imaged,
    landed: u64,
) -> Result<(), String> {
    let Imaged {
        log_len,
        whole,
        entries,
    } = imaged;
    let log_cut = (landed < log_len).then(|| json!({"at": landed, "bytes": log_len - landed}));
    if recovered.get("log_cut") != log_cut.as_ref() {
        return Err(format!(
            "the next commit's frame landed at {landed} of a journal.log {log_len} bytes long, \
             and it reported {recovered}"
        ));
    }
    let listed = recovered["frames_listed"].as_u64().unwrap_or(0);
    let begun = to_u64(rec.frames_begun(cut));
    if Some(listed) != whole.checked_sub(entries) || Some(listed) > begun.checked_sub(entries) {
        return Err(format!(
            "the next commit listed {listed} frames, the image holding {whole} whole of which \
             {entries} listed, of the {begun} the run began: {recovered}"
        ));
    }
    Ok(())
}

/// [`found_in`] a store, whose `info` gave `info`, in the image of a power failure after the
/// first `cut` steps; `tmp` takes a change list.
fn a_store(
    rec: &Recorded,
    store: &str,
    info: &Value,
    tmp: &Path,
    cut: usize,
) -> Result<Found, String> {
    let seen = seen(rec, store, info)?;
    let frames = (0..=rec.frames.len())
        .find(|&frames| seen.is_after(rec, frames))
        .ok_or_else(|| format!("the state of no prefix of the run: {seen:#?}"))?;
    clean(store, "after the power loss")?;
    // The journal as the image holds it, which the next commit's write repairs.
    let journal = journal_of(store)?;
    let whole = journal
        .tail_frames_from(0)
        .map_err(|e| format!("read the journal: {e}"))?;
    let imaged = Imaged {
        log_len: journal.len(),
        whole: to_u64(whole.len()),
        entries: journal
            .frame_count()
            .map_err(|e| format!("count the listed frames: {e}"))?,
    };

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
    let recovered = &commit.envelope["meta"]["recovered"];
    recovered_as_imaged(rec, cut, recovered, imaged, frame_of(store, new)?)
        .map_err(|why| format!("{why}: {}", commit.run))?;
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
                        found_in(rec, &image, cut)
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
/// seeds: nothing kept, everything kept, and eight at random; on [`RUN`] and then
/// [`Op::Bulk`], so that the images hold the compaction a writer makes past
/// [`Index::COMPACT_THRESHOLD`] records: the index synced, the views written and renamed into
/// place, the `Compacted` frame and the checkpoint after it, every cut of it checked. Two long
/// runs of the bulk transaction repeat, call after call, what [`RUN`]'s cuts check at every step:
/// its objects' writes (two calls for each of its 1,026 objects, as for every commit's objects)
/// and the materialization of its records past the checkpoint (an unsynced append to each of
/// the five index files, for each record, as every commit's materialization makes). Of their
/// cuts it checks the first and last eight and every 64th.
#[test]
#[ignore = "slow, ~280 s: images of 830 cuts x 10 seeds through the binary; just check-slow runs it"]
fn power_loss_images_recover_a_prefix_with_every_acknowledged_commit_at_every_cut() {
    let ops: Vec<Op> = RUN.iter().copied().chain([Op::Bulk]).collect();
    let rec = record_ops(&ops);
    let objects = rec.timeline.mark("bulk objects")..rec.timeline.mark("bulk objects flushed");
    // From the bulk frame's listing (its write and sync, then its entry's) to the index sync
    // that starts the compaction.
    let listed = rec.frames.last().expect("the bulk frame").write + 4;
    let synced = (listed..rec.timeline.steps())
        .find(|&step| rec.timeline.step(step).label.starts_with("sync_data"))
        .expect("the bulk transaction syncs the index");
    let sampled = |cut: usize| {
        [&objects, &(listed..synced)].into_iter().all(|run| {
            !(run.start + 8..run.end.saturating_sub(8)).contains(&cut)
                || (cut - run.start).is_multiple_of(64)
        })
    };
    let cuts: Vec<usize> = (0..=rec.timeline.steps())
        .filter(|&cut| sampled(cut))
        .collect();
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
        match found_in(&rec, &image, cut) {
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
/// sector, where only its length does, its CRC across the sector boundary, and where its length
/// straddles the boundary: its low byte lost, the length reads as one that is not 0 but wrong,
/// and the frame fails its CRC, or runs past the end of the file, instead.
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
        // The frame's end past the first sector the write touches.
        if at + len <= boundary {
            continue;
        }
        let image = image_losing(&rec, frame.write + 1, &[(log, Keep::FirstSectorLost)]);
        let bytes = image.file(Path::new(log)).expect("journal.log");
        let start = usize::try_from(at).unwrap();
        // The bytes of its length field in that sector: all four, or the low ones of a length
        // that straddles the boundary, the rest of it kept.
        let lost = usize::try_from(boundary - at).unwrap().min(4);
        assert!(
            bytes[start..start + lost].iter().all(|&byte| byte == 0),
            "frame {n}: the first {lost} bytes of its length lost"
        );
        // A length that straddles the boundary keeps a byte that is not 0 (its payload is over
        // 255 bytes), so it reads as a length that is wrong, not as 0.
        assert!(
            lost == 4 || bytes[start + lost..start + 4].iter().any(|&byte| byte != 0),
            "frame {n}: the rest of its length is 0"
        );
        assert!(
            bytes[start..].iter().any(|&byte| byte != 0),
            "frame {n}: a later sector kept"
        );
        torn.push((n, boundary - at));
        match found_in(&rec, &image, frame.write + 1) {
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
    assert!(
        torn.iter().any(|&(_, lost)| lost < 4),
        "a frame whose length straddles the boundary, its low bytes lost: {torn:?}"
    );
}

/// A frame written to `journal.log` and not yet synced may reach the disk in part: a power loss
/// keeps the bytes before a 512 B boundary the writeback had reached and loses the rest. A frame
/// that straddles a boundary is then cut there, its header whole or only its length kept, and none
/// of its payload: a torn tail like any other, so the store opens and holds the frames before it.
#[test]
fn a_frame_kept_up_to_a_sector_boundary_is_a_torn_tail() {
    let rec = record();
    let log = "store/meta/journal.log";
    let keep = Keep::Prefix {
        granule: SECTOR,
        pick: 0,
    };
    let mut kept = Vec::new();
    let mut failures = Vec::new();
    for (n, frame) in rec.frames.iter().enumerate() {
        let [(at, len)] = rec.timeline.writes(frame.write)[..] else {
            panic!("frame {n}: one write");
        };
        let boundary = (at / SECTOR + 1) * SECTOR;
        if at + len <= boundary {
            continue;
        }
        let image = image_losing(&rec, frame.write + 1, &[(log, keep)]);
        let bytes = image.file(Path::new(log)).expect("journal.log");
        assert_eq!(
            to_u64(bytes.len()),
            boundary,
            "frame {n}: kept up to the boundary"
        );
        kept.push((n, boundary - at));
        match found_in(&rec, &image, frame.write + 1) {
            Ok(Found::Prefix(frames)) if frames == n => {}
            other => failures.push(format!(
                "frame {n} at {at}..{}, kept up to {boundary}: {other:?}",
                at + len
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert!(
        kept.iter().any(|&(_, bytes)| bytes >= 8),
        "a frame whose whole header is kept, its payload lost: {kept:?}"
    );
    assert!(
        kept.iter().any(|&(_, bytes)| (4..8).contains(&bytes)),
        "a frame whose length is kept, its CRC lost: {kept:?}"
    );
}

/// The sweeps rely on [`Seeded`] for the shapes of loss no other test pins: a seed of 2 or more
/// picks each [`Keep`] at random (the prefix up to a sector and up to a page boundary among
/// them), and, of a directory's changes no fsync covered, keeps some but not all.
#[test]
fn a_random_seed_picks_every_shape_of_loss() {
    let shapes: BTreeSet<Keep> = (0..200)
        .map(|cut| match Seeded::new(2, cut).data(Path::new("file")) {
            Keep::Prefix { granule, .. } => Keep::Prefix { granule, pick: 0 },
            other => other,
        })
        .collect();
    let prefix = |granule| Keep::Prefix { granule, pick: 0 };
    assert_eq!(
        shapes.into_iter().collect::<Vec<_>>(),
        [
            Keep::Drop,
            prefix(SECTOR),
            prefix(PAGE),
            Keep::All,
            Keep::ZeroFilled,
            Keep::FirstSectorLost
        ]
    );
    assert!(
        (0..200).any(|cut| (1..3).contains(&Seeded::new(2, cut).entries(Path::new("dir"), 3))),
        "some of a directory's changes are kept, not none and not all"
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
            RecordedCall::WriteDurableTemp { path, bytes } => {
                temps.insert(path, bytes);
            }
            RecordedCall::LinkNoclobber {
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
        .filter(|(_, frame)| matches!(frame.what, Txn::Commit | Txn::CommitAdvance { .. }))
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
        let found = found_in(&rec, &least(durable), durable);
        assert_eq!(found, Ok(Found::Prefix(n + 1)), "frame {n} just durable");
    }
}
