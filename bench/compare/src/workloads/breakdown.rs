//! Where a native W1 commit's time goes: W1's transaction on a `MetaStore` whose filesystem
//! is the real one with every call timed (`TimingFs`, through the store's public
//! `create_with_fs`). Each call is filed by what it does to which file; what is not inside a
//! filesystem call (the lock, opening the index, lookups, encoding) is the rest. Means per
//! commit, so that the parts add up to the whole: a kind's time is the time its calls cover,
//! overlaps counted once, since the index sync's five `sync_data`s run at once on five
//! threads and the commit waits for them as for one.
//!
//! Two of `RealFs`'s calls make several system calls, a sync among them: `append_durable`
//! (open, append, `fdatasync`: the journal's two appends) and `atomic_replace` (a temp file
//! written and `fsync`'d, renamed over the file, the directory `fsync`'d). So that each sync is
//! timed on its own, `TimingFs` does `append_durable` step by step, and `atomic_replace` of a
//! ref file, with the same calls in the same order as `RealFs`; every other call (the
//! checkpoint's `atomic_replace` among them) goes to `RealFs` itself.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::{CasResult, ChangesetMeta, FsOps, LinkOutcome, MetaStore, RealFs};

use super::Values;
use crate::data::{chain, changeset};
use crate::engines::{Result, other};

/// When a call started and ended.
type Span = (Instant, Instant);

/// The real filesystem, each call timed and filed by kind.
struct TimingFs {
    root: PathBuf,
    calls: Mutex<BTreeMap<&'static str, Vec<Span>>>,
}

/// How long `spans` cover, overlaps counted once.
fn covered(mut spans: Vec<Span>) -> Duration {
    spans.sort_unstable_by_key(|span| span.0);
    let mut total = Duration::ZERO;
    let mut open: Option<Span> = None;
    for (start, end) in spans {
        match &mut open {
            Some((_, until)) if start <= *until => *until = (*until).max(end),
            _ => {
                if let Some((from, until)) = open.replace((start, end)) {
                    total += until - from;
                }
            }
        }
    }
    if let Some((from, until)) = open {
        total += until - from;
    }
    total
}

/// What a call to `path` is, for the table.
fn kind(op: &str, rel: &str) -> &'static str {
    let file = rel.strip_prefix("meta/").unwrap_or(rel);
    match (op, file) {
        ("fdatasync", "journal.log") => "fdatasync of journal.log (the commit point)",
        ("fdatasync", "journal.off") => "fdatasync of journal.off",
        ("open" | "append", "journal.log" | "journal.off") => {
            "journal appends: open + write, 2 files (no sync)"
        }
        ("temp write", f) if f.starts_with("refs/") => "ref file: temp file created + written",
        ("fsync", f) if f.starts_with("refs/") => "ref file: fsync of the temp file",
        ("rename", f) if f.starts_with("refs/") => "ref file: rename over refs/main",
        ("fsync dir", f) if f.starts_with("refs/") => "ref file: fsync of refs/",
        (_, "journal.ckpt") => "checkpoint replace (every 32 frames)",
        ("sync_data", f) if f.starts_with("sorted") => "compaction (every 1,024 records)",
        ("sync_data", _) => "index sync, 5 files (every 32 frames)",
        ("append", f) if f.starts_with("sorted") => "compaction (every 1,024 records)",
        ("rename_durable" | "remove" | "fsync_dir" | "create_dir_all", f)
            if f.starts_with("sorted") =>
        {
            "compaction (every 1,024 records)"
        }
        ("append", _) => "index appends, 5 files (no sync)",
        ("read", _) => "ref file reads",
        ("file_len", _) => "file length checks",
        _ => "other filesystem calls",
    }
}

impl TimingFs {
    fn time<T>(&self, op: &str, path: &Path, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        let ended = Instant::now();
        let key = kind(op, &self.rel(path));
        let mut calls = self.calls.lock().expect("the call table");
        calls.entry(key).or_default().push((started, ended));
        out
    }

    /// `path` relative to the store root.
    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn take(&self) -> BTreeMap<&'static str, Vec<Span>> {
        std::mem::take(&mut *self.calls.lock().expect("the call table"))
    }
}

/// The filesystem every timed call goes to.
const REAL_FS: RealFs = RealFs::new();

impl FsOps for TimingFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.time("create_dir_all", p, || REAL_FS.create_dir_all(p))
    }
    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.time("write_durable_temp", dir, || {
            REAL_FS.write_durable_temp(dir, bytes)
        })
    }
    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.time("link_noclobber", to, || REAL_FS.link_noclobber(from, to))
    }
    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.time("fsync_dir", p, || REAL_FS.fsync_dir(p))
    }
    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.time("read", p, || REAL_FS.read(p))
    }
    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.time("exists", p, || REAL_FS.exists(p))
    }
    fn remove(&self, p: &Path) -> io::Result<()> {
        self.time("remove", p, || REAL_FS.remove(p))
    }
    /// `RealFs::append_durable`, step by step.
    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let mut file = self.time("open", path, || {
            OpenOptions::new().append(true).create(true).open(path)
        })?;
        let offset = file.metadata()?.len();
        self.time("append", path, || file.write_all(bytes))?;
        self.time("fdatasync", path, || file.sync_data())?;
        Ok(offset)
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.time("file_len", path, || REAL_FS.file_len(path))
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.time("truncate", path, || REAL_FS.truncate(path, len))
    }
    /// `RealFs::atomic_replace`, step by step for a ref file.
    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if !self.rel(path).starts_with("meta/refs/") {
            return self.time("atomic_replace", path, || {
                REAL_FS.atomic_replace(path, bytes)
            });
        }
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let temp = self.time("temp write", path, || {
            let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
            temp.write_all(bytes)?;
            Ok::<_, io::Error>(temp)
        })?;
        self.time("fsync", path, || temp.as_file().sync_all())?;
        self.time("rename", path, || {
            temp.persist(path).map(drop).map_err(|e| e.error)
        })?;
        self.time("fsync dir", path, || File::open(dir)?.sync_all())
    }
    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.time("sync_data", path, || REAL_FS.sync_data(path))
    }
    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.time("append", path, || REAL_FS.append(path, bytes))
    }
    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.time("rename_durable", to, || REAL_FS.rename_durable(from, to))
    }
}

fn cid(id: [u8; 32]) -> ChangesetId {
    ChangesetId::new(ObjectId::new(id))
}

/// W1's transaction, `txns` timed after `warmup`; the values are means per commit: `commit_ms`,
/// and for each kind of call `<kind> ms` and `<kind> calls`, and the `rest ms`.
pub fn run(base: &Path, warmup: u64, txns: u64) -> Result<Values> {
    let dir = tempfile::Builder::new()
        .prefix("breakdown-native-")
        .tempdir_in(base)?;
    let root = dir.path().canonicalize()?;
    let fs = Arc::new(TimingFs {
        root: root.clone(),
        calls: Mutex::default(),
    });
    let utf8 = Utf8Path::from_path(&root).ok_or_else(|| other("a non-UTF-8 dir"))?;
    let store = MetaStore::create_with_fs(utf8, Arc::clone(&fs) as Arc<dyn FsOps>)?;
    let mut main = None;
    let mut commit = |n: u64| -> Result<()> {
        let cs = changeset(chain::W1, n, None);
        let mut write = store.write()?;
        let parents = cs.parents.iter().copied().map(cid).collect();
        write.insert_changeset(ChangesetMeta::new(cid(cs.id), 0, 0, cs.delta_hash, parents))?;
        match write.cas_ref("main", main.map(cid), Some(cid(cs.id)), "w1")? {
            CasResult::Applied { .. } => {}
            CasResult::Mismatch { .. } => return Err(other("main moved")),
            _ => return Err(other("a CAS result this harness does not know")),
        }
        write.commit()?;
        main = Some(cs.id);
        Ok(())
    };
    for n in 0..warmup {
        commit(n)?;
    }
    fs.take();
    let started = Instant::now();
    for n in warmup..warmup + txns {
        commit(n)?;
    }
    let total = started.elapsed();
    let calls = fs.take();
    let per = txns as f64;
    let commit_ms = total.as_secs_f64() * 1e3 / per;
    let mut values = Values::from([("commit_ms".into(), commit_ms)]);
    let mut inside = 0.0;
    for (kind, spans) in calls {
        let count = spans.len();
        let ms = covered(spans).as_secs_f64() * 1e3 / per;
        inside += ms;
        values.insert(format!("{kind} ms"), ms);
        values.insert(format!("{kind} calls"), count as f64 / per);
    }
    values.insert(
        "rest: lock, index open and lookups, encoding (outside filesystem calls) ms".into(),
        commit_ms - inside,
    );
    Ok(values)
}
