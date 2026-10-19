//! Power loss at any point of a store's life, through the real binary (storage spec §2–§3): the
//! crash images of the test kit (`swarmops_testkit::power`), opened with `swarmops`.
//!
//! A process crash (`crash.rs`) leaves the kernel's page cache in place, so it cannot show a
//! missing fsync, or writes not yet synced that a power loss tears, reorders or loses. Here one
//! run of a store's life is made in this process on an [`ImageRecordingFs`], deterministically
//! (one thread, signatures at a fixed time): `init`; commits and ref moves (a create, moves, a
//! delete) with a compaction and a checkpoint among them; then commits past the checkpoint, whose
//! index records no sync has made durable, the last of them naming by id a blob that a writer of
//! its own linked and did not flush, so that only the commit makes its name durable. The slow variant's run ends with a transaction of
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
//! The run ends by recording two external ids (`meta/external_ids/`) of commits whose frames are
//! durable ([`Op::ExternalIds`]): in every image each is whole and names its commit, or is
//! absent, and it is absent from no image of a power failure after its put returned.
//!
//! Then three changesets whose fields the caller gives are recorded in one frame through
//! `Repo::commit_raw_batch` ([`Op::Batch`]): a second root changeset, its child, and a merge of
//! the child with an earlier commit, their trees and blobs put first and flushed with the
//! changesets' objects before the frame. The frame is one transaction: every image holds all
//! three changesets or none of them.
//!
//! Images that hold the same files are checked once.
//!
//! One more run, of its own (`committed_read`), checks that a committed snapshot
//! (`MetaStore::read_committed`, storage spec §3) holds nothing a power loss after it can take
//! back, where a lock-free snapshot may; its images are read with the library, as the snapshot
//! was taken.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use swarmops_core::bytes::to_u64;
use swarmops_core::{Changeset, ChangesetId, LeafKind, ObjectKind, RepoPath, Signature, Tree};
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
    /// A commit on commit number `parent` of one change at `path` that names by id the blob of
    /// `content` (`ChangeOp::PutBlob`): the blob is put first through an object store of its own,
    /// which does not flush it, as a writer that linked it and crashed before its flush (or one
    /// still running) leaves it, so that only the commit's retain of the blob makes its name
    /// durable before the frame names it. Its frame holds its changeset.
    CommitBlob {
        parent: Option<usize>,
        path: &'static str,
        content: &'static str,
    },
    /// External ids of system `git` recorded with `Repo::record_external_ids`, after the frames
    /// of the commits they name: each a key (20 bytes in hex, as a git oid is) and the commit
    /// number its entry names. No frame: each entry's file, and the directories it makes,
    /// durable before the put returns.
    ExternalIds {
        entries: &'static [(&'static str, usize)],
    },
    /// [`BATCHED`] changesets with caller-given fields in one frame, through
    /// `Repo::commit_raw_batch`: a second root changeset, a child of it, and a merge of that child
    /// with commit number `merge`. Their trees and blobs are put first (`Repo::put_objects`), and
    /// made durable by the batch's flush, with the changesets' objects, before its frame.
    Batch { merge: usize },
}

/// How many changesets [`Op::Batch`] records in its one frame.
const BATCHED: usize = 3;

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
/// Frame 14 holds a changeset and main's move to it ([`Op::CommitAdvance`]); frame 15, a
/// changeset that names by id a blob whose put no flush followed ([`Op::CommitBlob`]); after it,
/// [`Op::ExternalIds`] records external ids of commits 0 and 4, in two fan-out directories; and
/// frame 16, the last, holds the three changesets of [`Op::Batch`], whose merge has commit 4 as
/// its second parent.
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
    Op::CommitBlob {
        parent: Some(6),
        path: "blob.bin",
        content: "a blob named by id\n",
    },
    Op::ExternalIds {
        entries: &[
            ("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567", 0),
            ("fedcba9876543210fedcba9876543210fedcba98", 4),
        ],
    },
    Op::Batch { merge: 4 },
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
    /// [`Op::Batch`]'s [`BATCHED`] changesets, in one frame.
    Batch,
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
    /// The blobs commits named by id ([`Op::CommitBlob`]): the commit's number and the blob.
    named: Vec<(usize, swarmops_core::ObjectId)>,
    frames: Vec<Frame>,
    /// The step that writes each `Compacted` frame, which is no transaction's, so not one of
    /// `frames`.
    compacted: Vec<usize>,
    /// How many steps came before `init` returned.
    init: usize,
    /// The external ids [`Op::ExternalIds`] recorded.
    external: Vec<External>,
    /// The calls, as recorded.
    calls: Vec<RecordedCall>,
}

/// An external id the run recorded ([`Op::ExternalIds`]).
#[derive(Debug, Clone)]
struct External {
    /// Its key.
    key: Vec<u8>,
    /// The number of the commit its entry names.
    commit: usize,
    /// How many steps came before its put returned.
    returned: usize,
}

/// The bytes the hex digits `hex` spell.
fn key_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex digits"))
        .collect()
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
    let Ran {
        commits,
        parents,
        named,
    } = pool.install(|| run(&root, &fs, ops));
    let calls = fs.calls();
    let timeline = Timeline::new(&base, &calls);
    let log = base.join("store/meta/journal.log");
    // Each frame's transaction, in order; `None` for the `Compacted` frame that follows a bulk
    // transaction, which is no transaction's.
    let mut whats = ops.iter().flat_map(|op| match *op {
        Op::Commit { .. } | Op::CommitBlob { .. } => vec![Some(Txn::Commit)],
        Op::Cas { name, new, .. } => vec![Some(Txn::Cas { name, new })],
        Op::Bulk => vec![Some(Txn::Bulk), None],
        Op::CommitAdvance { name, .. } => vec![Some(Txn::CommitAdvance { name })],
        Op::Batch { .. } => vec![Some(Txn::Batch)],
        Op::Compact | Op::Flush | Op::ExternalIds { .. } => Vec::new(),
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
    let mut external = Vec::new();
    for (n, op) in ops.iter().enumerate() {
        if let Op::ExternalIds { entries } = *op {
            let returned = timeline.mark(&format!("op {n}"));
            external.extend(entries.iter().map(|&(key, commit)| External {
                key: key_bytes(key),
                commit,
                returned,
            }));
        }
    }
    Recorded {
        base,
        init: timeline.mark("init"),
        external,
        timeline,
        commits: commits.iter().map(ToString::to_string).collect(),
        parents,
        named,
        frames,
        compacted,
        calls,
    }
}

/// What [`run`] made.
struct Ran {
    /// The changesets' ids, in their order.
    commits: Vec<ChangesetId>,
    /// Their first parents, by number.
    parents: Vec<Option<usize>>,
    /// The blobs commits named by id: the commit's number and the blob.
    named: Vec<(usize, swarmops_core::ObjectId)>,
}

/// The run itself, `init` then `ops`.
fn run(root: &Utf8Path, fs: &Arc<ImageRecordingFs>, ops: &[Op]) -> Ran {
    let shared: Arc<dyn FsOps> = Arc::clone(fs) as Arc<dyn FsOps>;
    let repo = LocalRepo::create_with_fs(root, "power", Arc::clone(&shared)).expect("init");
    fs.mark("init");
    let (mut commits, mut parents, mut named) = (Vec::new(), Vec::new(), Vec::new());
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
                            kind: LeafKind::File,
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
            Op::CommitBlob {
                parent,
                path,
                content,
            } => {
                let on = parent.map(|p| commits[p]);
                let (id, blob) =
                    commit_blob(root, &shared, &repo, on, path, content, commits.len());
                named.push((commits.len(), blob));
                commits.push(id);
                parents.push(parent);
            }
            Op::ExternalIds { entries } => record_external_ids(&repo, &commits, entries),
            Op::Batch { merge } => {
                let (first, merged) = (commits.len(), commits[merge]);
                commits.extend(raw_batch(&repo, merged));
                parents.extend([None, Some(first), Some(first + 1)]);
            }
        }
        fs.mark(&format!("op {n}"));
    }
    Ran {
        commits,
        parents,
        named,
    }
}

/// [`Op::CommitBlob`] on the store at `root`, whose repository is `repo`, its calls through
/// `shared`: the blob of `content` linked through an object store of its own, which never flushes
/// it, then commit number `number`, on `parent`, of `path` naming that blob by id. The
/// changeset's id, and the blob's.
fn commit_blob(
    root: &Utf8Path,
    shared: &Arc<dyn FsOps>,
    repo: &LocalRepo,
    parent: Option<ChangesetId>,
    path: &str,
    content: &str,
    number: usize,
) -> (ChangesetId, swarmops_core::ObjectId) {
    let writer = FsObjectStore::open(Layout::new(root), Box::new(Arc::clone(shared)));
    let id = writer
        .put(ObjectKind::Blob, content.as_bytes())
        .expect("put the blob");
    drop(writer);
    let request = CommitRequest {
        parents: parent.into_iter().collect(),
        changes: vec![Change {
            path: RepoPath::new(path).expect("a repo path"),
            op: ChangeOp::PutBlob {
                kind: LeafKind::File,
                id: swarmops_core::BlobId(id),
            },
        }],
        author: signature(),
        committer: signature(),
        message: format!("commit {number}").into_bytes(),
        metadata: Vec::new(),
        allow_empty: false,
        advance: None,
    };
    let outcome = repo.commit(request).expect("commit");
    assert!(outcome.warnings.is_empty(), "{outcome:?}");
    assert_eq!(outcome.blobs_written, 0, "the blob was put before");
    // None of the commit's own objects (its root tree and its changeset) lies in the blob's
    // fan-out directory, so only the blob's retain has the commit fsync it.
    assert!(
        id.0[0] != outcome.root.0.0[0] && id.0[0] != outcome.id.0.0[0],
        "the blob {id} shares a fan-out directory with the commit's {outcome:?}"
    );
    (outcome.id, id)
}

/// [`Op::ExternalIds`] on `repo`: each of `entries`, a key in hex digits and the number of the
/// commit in `commits` it names, recorded in the external id index as system `git`'s; each is
/// new, so the put writes every one.
fn record_external_ids(repo: &LocalRepo, commits: &[ChangesetId], entries: &[(&str, usize)]) {
    let entries: Vec<(Vec<u8>, swarmops_store::ExternalEntry)> = entries
        .iter()
        .map(|&(key, commit)| {
            let entry = swarmops_store::ExternalEntry {
                changeset: commits[commit],
                faithful: true,
            };
            (key_bytes(key), entry)
        })
        .collect();
    let put = repo
        .record_external_ids(swarmops_store::SYSTEM_GIT, &entries)
        .expect("record_external_ids");
    assert_eq!(put.written, to_u64(entries.len()), "{put:?}");
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

/// [`Op::Batch`] on `repo`: the trees and blobs of three changesets put, then the changesets,
/// whose fields this gives, recorded in one frame by `Repo::commit_raw_batch`: a second root
/// changeset, a child of it, and a merge of that child with `merged`. Their ids, in the order
/// the frame records them.
fn raw_batch(repo: &LocalRepo, merged: ChangesetId) -> Vec<ChangesetId> {
    use swarmops_core::{BlobId, TreeEntry, TreeId, hash_object};
    use swarmops_repo::RawChangeset;

    // Each changeset's files, all in the directory `raw`: the root's, then what the child and the
    // merge add to their first parent's.
    let files: [&[(&str, &str)]; BATCHED] = [
        &[("root.txt", "a second root\n")],
        &[
            ("child.txt", "its child\n"),
            ("root.txt", "a second root\n"),
        ],
        &[
            ("child.txt", "its child\n"),
            ("merge.txt", "a merge\n"),
            ("root.txt", "a second root\n"),
        ],
    ];
    let mut objects = BTreeMap::new();
    let roots: Vec<TreeId> = files
        .iter()
        .map(|files| {
            let entries = files
                .iter()
                .map(|&(name, content)| {
                    let blob = BlobId(hash_object(ObjectKind::Blob, content.as_bytes()));
                    objects.insert(blob.0, (ObjectKind::Blob, content.as_bytes().to_vec()));
                    TreeEntry::leaf(name, LeafKind::File, blob)
                })
                .collect();
            let dir = Tree::new(entries).expect("a tree");
            let root = Tree::new(vec![TreeEntry::dir("raw", dir.id())]).expect("a tree");
            for tree in [&dir, &root] {
                objects.insert(tree.id().0, (ObjectKind::Tree, tree.encode()));
            }
            root.id()
        })
        .collect();
    repo.put_objects(objects.into_values().collect())
        .expect("put the trees and blobs");

    // The batch's later changesets name its earlier ones as parents, by the ids a batch gives
    // them: learned first from a batch that is dropped uncommitted, which writes nothing.
    let mut scratch = repo.raw_batch();
    let (mut raws, mut ids) = (Vec::new(), Vec::new());
    for (n, root) in roots.into_iter().enumerate() {
        let parents = match n {
            0 => Vec::new(),
            1 => vec![ids[0]],
            _ => vec![ids[1], merged],
        };
        let raw = RawChangeset {
            parents,
            root,
            author: signature(),
            committer: signature(),
            message: format!("raw {n}").into_bytes(),
            extra_headers: Vec::new(),
            metadata: Vec::new(),
        };
        ids.push(scratch.push(raw.clone()).expect("a raw changeset"));
        raws.push(raw);
    }
    drop(scratch);
    let outcome = repo.commit_raw_batch(raws).expect("commit_raw_batch");
    assert!(outcome.warnings.is_empty(), "{outcome:?}");
    assert!(outcome.frame_seq.is_some(), "{outcome:?}");
    let made: Vec<ChangesetId> = outcome.outcomes.iter().map(|made| made.id).collect();
    assert_eq!(made, ids);
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
                Txn::Batch => state.changesets += BATCHED,
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
    /// What `log` lists from the last commit `info` counts: its first-parent history, every
    /// commit there is until [`Op::Batch`] makes a second root.
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

/// `Ok` if the external ids the run recorded are in the store at `store`, in state `state`, as
/// the image of a power failure after the first `cut` steps may hold them: each whole and naming
/// its commit, which the store holds; or absent, but only if the power failed before its put
/// returned.
fn external_ids_as_imaged(
    rec: &Recorded,
    store: &str,
    state: &State,
    cut: usize,
) -> Result<(), String> {
    let layout = Layout::new(Utf8PathBuf::from(store));
    let index = swarmops_store::ExternalIds::new(layout, Arc::new(RealFs));
    for External {
        key,
        commit,
        returned,
    } in &rec.external
    {
        let named = &rec.commits[*commit];
        match index.get(swarmops_store::SYSTEM_GIT, key) {
            Ok(Some(entry))
                if entry.changeset.to_string() == *named && *commit < state.changesets => {}
            Ok(None) if cut < *returned => {}
            found => {
                return Err(format!(
                    "the external id of commit {commit} ({named}), whose put returned after step \
                     {returned}: {found:?}, in a store of {} changesets",
                    state.changesets
                ));
            }
        }
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
    // A commit that names a blob by id relies on a blob it did not write: wherever its frame
    // is, the blob is too (and `verify --deep` below hashes it).
    let held = rec.state_after(frames).changesets;
    let layout = Layout::new(Utf8PathBuf::from(store));
    for &(n, id) in rec.named.iter().filter(|&&(n, _)| n < held) {
        if !layout.object_path(id).is_file() {
            return Err(format!("commit {n} names blob {id}, which the image lacks"));
        }
    }
    clean(store, "after the power loss")?;
    external_ids_as_imaged(rec, store, &seen.state, cut)?;
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
#[ignore = "slow: thousands of images through the binary; just check-slow runs it"]
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
/// synced (`ObjectStore::flush`); a commit flushes its objects, and the fan-out directories of the
/// blobs it names by id, which it retains, before it appends its frame. So
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

    // Each frame that records changesets (a commit's, the batch's), the step after which it is
    // durable, and the step that writes it.
    let commits: Vec<(usize, usize, usize)> = (rec.frames.iter().enumerate())
        .filter(|(_, frame)| {
            matches!(
                frame.what,
                Txn::Commit | Txn::CommitAdvance { .. } | Txn::Batch
            )
        })
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

/// The committed snapshot ([`MetaStore::read_committed`], storage spec §3 "읽기") under power
/// loss: a recorded run of its own, its images read with the library, as the snapshot was taken.
mod committed_read {
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use camino::Utf8PathBuf;
    use swarmops_core::{ChangesetId, LeafKind, RepoPath};
    use swarmops_repo::{Advance, Change, ChangeOp, CommitRequest, LocalRepo};
    use swarmops_store::{FsOps, LinkOutcome, MetaRead, MetaStore, StoreError, TailRepair};
    use swarmops_testkit::fs::{Hold, NoSyncFs, hold};
    use swarmops_testkit::power::{Image, ImageRecordingFs, RecordedCall, Seeded, Timeline};

    use super::signature;

    /// The recorder, holding, once armed, the next `append_durable` of the store's `journal.log`
    /// between its write and its sync, as a writer is between its frame's write and its
    /// `fdatasync`: the call appends without a sync ([`FsOps::append`]), is held ([`Hold`]),
    /// then syncs the file ([`FsOps::sync_data`]) and succeeds. The recorder records the two
    /// calls, whose steps are those of the one `append_durable`, its write and then its sync,
    /// with what the test marks while the call is held between them.
    struct Gated {
        /// What every call runs on.
        fs: Arc<ImageRecordingFs>,
        /// The store's `journal.log`.
        log: PathBuf,
        /// The hold, once armed and until the call takes it.
        hold: Mutex<Option<Hold>>,
    }

    impl FsOps for Gated {
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            self.fs.create_dir_all(p)
        }

        fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
            self.fs.write_durable_temp(dir, bytes)
        }

        fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
            self.fs.link_noclobber(from, to)
        }

        fn fsync_dir(&self, p: &Path) -> io::Result<()> {
            self.fs.fsync_dir(p)
        }

        fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
            self.fs.read(p)
        }

        fn exists(&self, p: &Path) -> io::Result<bool> {
            self.fs.exists(p)
        }

        fn remove(&self, p: &Path) -> io::Result<()> {
            self.fs.remove(p)
        }

        fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            let hold = if path == self.log {
                self.hold.lock().unwrap().take()
            } else {
                None
            };
            let Some(hold) = hold else {
                return self.fs.append_durable(path, bytes);
            };
            let at = self.fs.append(path, bytes)?;
            hold.hold();
            self.fs.sync_data(path)?;
            Ok(at)
        }

        fn file_len(&self, path: &Path) -> io::Result<u64> {
            self.fs.file_len(path)
        }

        fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
            self.fs.truncate(path, len)
        }

        fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.fs.atomic_replace(path, bytes)
        }

        fn sync_data(&self, path: &Path) -> io::Result<()> {
            self.fs.sync_data(path)
        }

        fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            self.fs.append(path, bytes)
        }

        fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.fs.rename_durable(from, to)
        }
    }

    /// What a snapshot shows of the run.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Shown {
        /// How many changesets it counts.
        changesets: u64,
        /// Which of the run's commits it holds, in their order.
        holds: Vec<ChangesetId>,
        /// Each ref, by name, and its target.
        refs: Vec<(String, ChangesetId)>,
    }

    impl Shown {
        /// Whether it holds every changeset and every ref target `snapshot` shows.
        fn holds_all_of(&self, snapshot: &Shown) -> bool {
            snapshot.holds.iter().all(|id| self.holds.contains(id))
                && snapshot.refs.iter().all(|moved| self.refs.contains(moved))
        }
    }

    /// What `read` shows of the run's commits `commits`.
    fn shown(read: &MetaRead<'_>, commits: &[ChangesetId]) -> Result<Shown, StoreError> {
        let mut holds = Vec::new();
        for &id in commits {
            if read.changeset(id)?.is_some() {
                holds.push(id);
            }
        }
        let refs = read.list_refs()?;
        Ok(Shown {
            changesets: read.count_changesets()?,
            holds,
            refs: refs
                .into_iter()
                .map(|(name, at)| (name, at.target))
                .collect(),
        })
    }

    /// What readers of the store in `image` find of the run's commits `commits`: a lock-free
    /// snapshot, and then a committed snapshot, which repairs the journal's tail first as a
    /// writer would, without a flush ([`NoSyncFs`]: what they see is the subject, and no power
    /// fails after it).
    fn shown_in(image: &Image, commits: &[ChangesetId]) -> Result<(Shown, Shown), String> {
        let dir = tempfile::tempdir().map_err(|e| format!("a temp dir: {e}"))?;
        image
            .materialize(dir.path())
            .map_err(|e| format!("writing the image: {e}"))?;
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8 paths");
        let store =
            MetaStore::open_with_fs(&root, Arc::new(NoSyncFs)).map_err(|e| format!("open: {e}"))?;
        let read = store.read().map_err(|e| format!("a snapshot: {e}"))?;
        let plain = shown(&read, commits).map_err(|e| format!("a snapshot: {e}"))?;
        drop(read);
        let (read, _) =
            (store.read_committed()).map_err(|e| format!("a committed snapshot: {e}"))?;
        let committed = shown(&read, commits).map_err(|e| format!("a committed snapshot: {e}"))?;
        Ok((plain, committed))
    }

    /// Commit `n` of the run: the file `<n>.txt` put on commit `parent` (a root for `None`), and
    /// main advanced to it in the same transaction.
    fn one_file(n: usize, parent: Option<ChangesetId>) -> CommitRequest {
        CommitRequest {
            parents: parent.into_iter().collect(),
            changes: vec![Change {
                path: RepoPath::new(format!("{n}.txt")).expect("a repo path"),
                op: ChangeOp::Put {
                    kind: LeafKind::File,
                    content: format!("{n}\n").into_bytes(),
                },
            }],
            author: signature(),
            committer: signature(),
            message: format!("commit {n}").into_bytes(),
            metadata: Vec::new(),
            allow_empty: false,
            advance: Some(Advance {
                name: "main".to_owned(),
                actor: "test-power/worker-1".to_owned(),
            }),
        }
    }

    /// The recorded run of [`read_committed_snapshot_survives_power_loss`].
    struct Run {
        timeline: Timeline,
        /// How many calls it made, marks aside.
        calls: usize,
        /// The step that syncs the third commit's frame.
        synced: usize,
        /// The three commits, in order.
        commits: [ChangesetId; 3],
        /// What a lock-free snapshot taken at the mark shows.
        local: Shown,
        /// What the committed snapshot started at the mark shows, and what its tail check
        /// repaired.
        committed: (Shown, TailRepair),
    }

    /// Records the run of [`read_committed_snapshot_survives_power_loss`]: `init`, two one-file
    /// commits, then the third, its writer held at its frame's sync ([`Gated`]) on a thread of
    /// its own while the run marks `committed-read`, takes a lock-free snapshot and starts a
    /// committed one on another thread, which marks `committed-read returned` once it has its
    /// snapshot. The writer is let go once that thread is waiting, 300 ms on; it must not have
    /// its snapshot by then.
    fn record_run() -> Run {
        let dir = tempfile::tempdir().expect("make a temp dir");
        let base = dir.path().canonicalize().expect("the temp dir's path");
        let root = Utf8PathBuf::from_path_buf(base.join("store")).expect("UTF-8 temp paths");
        let log = base.join("store/meta/journal.log");
        let fs = Arc::new(ImageRecordingFs::new());
        let gated = Arc::new(Gated {
            fs: Arc::clone(&fs),
            log: log.clone(),
            hold: Mutex::new(None),
        });
        // One thread: the commits' objects are written, and their directories synced, in the
        // same order every run.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("a one-thread pool");
        let shared = Arc::clone(&gated) as Arc<dyn FsOps>;
        let repo = pool
            .install(|| LocalRepo::create_with_fs(&root, "power", shared))
            .expect("init");
        fs.mark("init");
        let commit = |n, parent| {
            pool.install(|| repo.commit(one_file(n, parent)))
                .expect("commit")
                .id
        };
        let first = commit(0, None);
        let second = commit(1, Some(first));

        let (gate, held) = hold();
        *gated.hold.lock().unwrap() = Some(gate);
        let readers = Arc::clone(&fs) as Arc<dyn FsOps>;
        let open = || MetaStore::open_with_fs(&root, Arc::clone(&readers)).expect("open");
        let recorder = &*fs;
        let (third, local, committed) = thread::scope(|scope| {
            let writer = scope.spawn(|| commit(2, Some(second)));
            held.wait();
            recorder.mark("committed-read");
            // The frame is whole in journal.log and not synced: a lock-free snapshot holds it.
            let local_store = open();
            let local_read = local_store.read().expect("a snapshot");

            let (started, starting) = mpsc::channel();
            let (returned, has_returned) = mpsc::channel();
            let (tell, told) = mpsc::channel();
            let reader = scope.spawn(move || {
                let store = open().with_lock_timeout(Duration::from_secs(60));
                started.send(()).expect("the test waits");
                let (read, repair) = store.read_committed().expect("a committed snapshot");
                recorder.mark("committed-read returned");
                returned.send(()).expect("the test waits");
                let third = told.recv_timeout(Duration::from_secs(60));
                let third = third.expect("the third commit's id");
                let shows = shown(&read, &[first, second, third]).expect("what it shows");
                (shows, repair)
            });
            starting
                .recv_timeout(Duration::from_secs(60))
                .expect("the reader started");
            thread::sleep(Duration::from_millis(300));
            assert!(
                has_returned.try_recv().is_err(),
                "a committed snapshot returned while the writer held the lock"
            );
            held.release();
            let third = writer.join().expect("the writer");
            tell.send(third).expect("the reader waits");
            let committed = reader.join().expect("the reader");
            let local = shown(&local_read, &[first, second, third]).expect("what it shows");
            (third, local, committed)
        });

        let calls = fs.calls();
        let timeline = Timeline::new(&base, &calls);
        let at_mark = (calls.iter())
            .position(|call| matches!(call, RecordedCall::Mark(label) if label == "committed-read"))
            .expect("the mark");
        let sync = (at_mark..calls.len())
            .find(|&n| matches!(&calls[n], RecordedCall::SyncData(path) if *path == log))
            .expect("the frame's sync");
        Run {
            synced: timeline.steps_of(sync).start,
            calls: (calls.iter())
                .filter(|call| !matches!(call, RecordedCall::Mark(_)))
                .count(),
            timeline,
            commits: [first, second, third],
            local,
            committed,
        }
    }

    /// Checks the image of every cut of the run after `mark`, for seeds 0 and 2 to 9
    /// ([`Seeded`]): what it lacks of what `snapshot` shows ([`lacks`]), for each image that lacks
    /// something. Each distinct image is checked once, built again from its first cut and seed,
    /// `AT_ONCE` at a time. Returns how many distinct images there were, and the failures.
    fn images_after(
        timeline: &Timeline,
        mark: usize,
        commits: &[ChangesetId],
        snapshot: &Shown,
    ) -> (usize, Vec<String>) {
        let mut distinct = HashMap::new();
        for cut in mark + 1..=timeline.steps() {
            for seed in [0, 2, 3, 4, 5, 6, 7, 8, 9] {
                let image = timeline.image(cut, &mut Seeded::new(seed, cut));
                distinct.entry(image.digest()).or_insert((cut, seed));
            }
        }
        let mut distinct: Vec<(usize, u64)> = distinct.into_values().collect();
        distinct.sort_unstable();
        let mut failures = Vec::new();
        for batch in distinct.chunks(super::AT_ONCE) {
            thread::scope(|scope| {
                let checks: Vec<_> = (batch.iter())
                    .map(|&(cut, seed)| {
                        scope.spawn(move || {
                            let image = timeline.image(cut, &mut Seeded::new(seed, cut));
                            lacks(&image, commits, snapshot).map(|why| {
                                let lost = image.describe();
                                format!("cut {cut}, seed {seed}: {why}\nlost:\n{lost}")
                            })
                        })
                    })
                    .collect();
                for check in checks {
                    let failed = check
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    failures.extend(failed);
                }
            });
        }
        (distinct.len(), failures)
    }

    /// What the store in `image` lacks of what `snapshot` shows, to a lock-free snapshot or to a
    /// committed one ([`shown_in`]); `None` if it holds all of it.
    fn lacks(image: &Image, commits: &[ChangesetId], snapshot: &Shown) -> Option<String> {
        match shown_in(image, commits) {
            Ok((plain, _)) if !plain.holds_all_of(snapshot) => {
                Some(format!("a snapshot shows {plain:?}"))
            }
            Ok((_, committed)) if !committed.holds_all_of(snapshot) => {
                Some(format!("a committed snapshot shows {committed:?}"))
            }
            Ok(_) => None,
            Err(why) => Some(why),
        }
    }

    /// A committed snapshot survives a power loss, where a lock-free one may not (design-L107
    /// §5, test 3). The run ([`record_run`]): `init`, two one-file commits that each advance main,
    /// and a third whose writer is held after its frame's bytes are written and before they are
    /// synced ([`Gated`]), where the run marks `committed-read`. There a lock-free snapshot holds
    /// the third frame, and the image of a power failure right then that keeps nothing unsynced
    /// (seed 0) lacks it. A committed snapshot, started there on a thread of its own (a `flock`
    /// is per open file, so the writer's thread could take it too), waits until the writer lets
    /// the lock go, so that it returns after the frame's sync; and every image of a power
    /// failure after the mark, keeping nothing unsynced or what seeds 2 to 9 pick, holds every
    /// changeset and every ref target that snapshot returned, to a lock-free snapshot and to a
    /// committed one alike. The default gate checks every cut after the mark: the run is short.
    #[test]
    fn read_committed_snapshot_survives_power_loss() {
        let run = record_run();
        let [first, second, third] = run.commits;
        let after = Shown {
            changesets: 3,
            holds: vec![first, second, third],
            refs: vec![("main".to_owned(), third)],
        };
        assert_eq!(run.local, after, "a lock-free snapshot at the mark");
        assert_eq!(
            run.committed,
            (after.clone(), TailRepair::default()),
            "the committed snapshot: the writer had listed its frame"
        );
        let mark = run.timeline.mark("committed-read");
        assert_eq!(
            run.synced, mark,
            "the frame's sync is the step right after the mark"
        );
        let returned = run.timeline.mark("committed-read returned");
        assert!(
            returned > run.synced,
            "the committed snapshot returned after {returned} steps, before the frame's sync \
             (step {}): it did not wait for the writer",
            run.synced
        );

        // A power failure at the mark, keeping nothing unsynced, loses the third frame.
        let lost = run.timeline.image(mark, &mut Seeded::new(0, mark));
        let before = Shown {
            changesets: 2,
            holds: vec![first, second],
            refs: vec![("main".to_owned(), second)],
        };
        let found = shown_in(&lost, &run.commits).expect("the image at the mark");
        assert_eq!(found, (before.clone(), before), "{}", lost.describe());

        // Every power failure after the mark keeps what the committed snapshot returned.
        let (images, failures) = images_after(&run.timeline, mark, &run.commits, &after);
        assert!(
            failures.is_empty(),
            "{} of {images} distinct images lack what the committed snapshot returned \
             ({after:?}); the first ones:\n\n{}",
            failures.len(),
            failures[..failures.len().min(5)].join("\n\n")
        );
        println!(
            "committed read: {} calls in {} steps, the mark after {mark}, the snapshot returned \
             after {returned}; {images} distinct images after the mark",
            run.calls,
            run.timeline.steps()
        );
    }
}
