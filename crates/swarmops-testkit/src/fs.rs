//! [`FsOps`] doubles for the tests, on real files:
//!
//! - [`Watched`]: a filesystem (the real one unless said otherwise) with a [`Watch`] run around
//!   each call, to record calls, fail them (before they run, or after), or hold them;
//!   [`RecordingFs`] records, fails the calls a [`Failing`] names, and runs a [`Hook`] once;
//!   [`FailingFs`] fails the calls a predicate picks.
//! - [`NoSyncFs`]: the real filesystem without a single flush to stable storage, failing where
//!   it fails.
//! - [`Rendezvous`]: tells calls made at once from calls made one after another.
//! - [`HoldThenFail`]: the real filesystem, holding one `append_durable` between its write and
//!   its flush until the test lets it go ([`hold`]: [`Hold`] and [`Held`]), then failing it.
//!
//! Each names a call's method with [`FsMethod`], the store's one list of the 14. The store's
//! tests and benches use them, and so do the repository's, the operations' and the CLI's tests
//! and the journal's fuzz target.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use swarmops_store::{FsMethod, FsOps, LinkOutcome, RealFs};

/// One call of an [`FsOps`] method, as a [`Watched`] filesystem shows it to its watcher.
pub struct Call<'a> {
    /// The method, which compares equal to its name (`call.method == "append"`).
    pub method: FsMethod,
    /// The paths it names: one, or a link's or a rename's source and then its target.
    pub paths: &'a [&'a Path],
}

impl Call<'_> {
    /// The path the call acts on: the last one it names (a link's or a rename's target).
    pub fn path(&self) -> &Path {
        self.paths.last().expect("every call names a path")
    }

    /// `"<method> <file name of its target>"`, as most tests name a call.
    pub fn named(&self) -> String {
        format!("{} {}", self.method, file_name(self.path()))
    }

    /// `"<method> <file name of each path>"`.
    pub fn named_all(&self) -> String {
        let mut call = self.method.as_str().to_owned();
        for path in self.paths {
            call.push(' ');
            call.push_str(&file_name(path));
        }
        call
    }
}

/// The file name of `path`, which must have one.
fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("a path with a file name")
        .to_string_lossy()
        .into_owned()
}

/// What a [`Watched`] filesystem runs around each call.
pub trait Watch: Send + Sync {
    /// Runs before the call. An error fails the call, which then does nothing.
    fn before(&self, call: &Call<'_>) -> io::Result<()>;

    /// Runs after the call, unless [`Watch::before`] failed it.
    fn after(&self, _call: &Call<'_>) {}

    /// Runs after a call that succeeded, after [`Watch::after`]. An error fails the call all
    /// the same, though what it did stays done: a failure after the work, as a sync that fails
    /// after its write.
    fn after_success(&self, _call: &Call<'_>) -> io::Result<()> {
        Ok(())
    }
}

/// A watcher the test holds a handle on too, to read what it counted.
impl<W: Watch + ?Sized> Watch for Arc<W> {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        (**self).before(call)
    }

    fn after(&self, call: &Call<'_>) {
        (**self).after(call);
    }

    fn after_success(&self, call: &Call<'_>) -> io::Result<()> {
        (**self).after_success(call)
    }
}

/// A filesystem, the real one ([`RealFs`]) unless the type says otherwise, its watcher run
/// around each call: `Watched(watcher, RealFs)`, or `Watched(watcher, NoSyncFs)` say, any
/// [`FsOps`] inside. It dereferences to the watcher.
pub struct Watched<W, F = RealFs>(
    /// The watcher.
    pub W,
    /// The filesystem the calls run on.
    pub F,
);

impl<W, F> Deref for Watched<W, F> {
    type Target = W;

    fn deref(&self) -> &W {
        &self.0
    }
}

impl<W: Watch, F: FsOps> Watched<W, F> {
    /// Runs `call`, the call of `method` on `paths`, with the watcher around it.
    fn around<T>(
        &self,
        method: FsMethod,
        paths: &[&Path],
        call: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let call_info = Call { method, paths };
        self.0.before(&call_info)?;
        let out = call();
        self.0.after(&call_info);
        let out = out?;
        self.0.after_success(&call_info)?;
        Ok(out)
    }
}

impl<W: Watch, F: FsOps> FsOps for Watched<W, F> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.around(FsMethod::CreateDirAll, &[p], || self.1.create_dir_all(p))
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.around(FsMethod::WriteDurableTemp, &[dir], || {
            self.1.write_durable_temp(dir, bytes)
        })
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.around(FsMethod::LinkNoclobber, &[from, to], || {
            self.1.link_noclobber(from, to)
        })
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.around(FsMethod::FsyncDir, &[p], || self.1.fsync_dir(p))
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.around(FsMethod::Read, &[p], || self.1.read(p))
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.around(FsMethod::Exists, &[p], || self.1.exists(p))
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.around(FsMethod::Remove, &[p], || self.1.remove(p))
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.around(FsMethod::AppendDurable, &[path], || {
            self.1.append_durable(path, bytes)
        })
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.around(FsMethod::FileLen, &[path], || self.1.file_len(path))
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.around(FsMethod::Truncate, &[path], || self.1.truncate(path, len))
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.around(FsMethod::AtomicReplace, &[path], || {
            self.1.atomic_replace(path, bytes)
        })
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.around(FsMethod::SyncData, &[path], || self.1.sync_data(path))
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.around(FsMethod::Append, &[path], || self.1.append(path, bytes))
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.around(FsMethod::RenameDurable, &[from, to], || {
            self.1.rename_durable(from, to)
        })
    }
}

/// A hook for a [`RecordingFs`]: the call it waits for as the recorder names it, whether it
/// runs right before or right after that call, and what it runs. It runs once.
pub type Hook = (String, When, Box<dyn FnOnce() + Send>);

/// When a [`Hook`] runs, around the call it names.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Right before the call runs.
    Before,
    /// Right after it ran, whether it failed or not.
    After,
}

/// The real filesystem, each call recorded by name (`"<method> <file name>"`, [`Call::named`],
/// unless set otherwise); the calls its [`Failing`] names fail, a [`Hook`] can run once around
/// the call it names, and the `sync_data` calls in flight are counted ([`Rendezvous`]).
pub type RecordingFs = Watched<Recorder>;

/// The watcher of a [`RecordingFs`].
pub struct Recorder {
    /// The calls recorded since the recorder's function last returned them.
    calls: Arc<Mutex<Vec<String>>>,
    /// How it names a call.
    name: Box<dyn Fn(&Call<'_>) -> String + Send + Sync>,
    /// The hook still to run, if any.
    hook: Mutex<Option<Hook>>,
    /// The calls it fails.
    failing: Failing,
    /// Its `sync_data` calls in flight.
    syncs: Rendezvous,
}

impl RecordingFs {
    /// A recorder, and a function that returns the calls made since it last ran.
    pub fn new() -> (RecordingFs, impl Fn() -> Vec<String>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorder = Recorder {
            calls: Arc::clone(&calls),
            name: Box::new(|call: &Call<'_>| call.named()),
            hook: Mutex::new(None),
            failing: Failing::default(),
            syncs: Rendezvous::default(),
        };
        let fs = Watched(recorder, RealFs);
        (fs, move || std::mem::take(&mut *calls.lock().unwrap()))
    }

    /// This recorder, naming each call by the file names of all its paths
    /// ([`Call::named_all`]).
    #[must_use]
    pub fn naming_all_paths(mut self) -> Self {
        self.0.name = Box::new(|call: &Call<'_>| call.named_all());
        self
    }

    /// This recorder, naming each call by its target's path in `root`: `"<method> <path>"`,
    /// the path `.` for `root` itself and `..` for its parent.
    #[must_use]
    pub fn naming_paths_in(mut self, root: PathBuf) -> Self {
        self.0.name = Box::new(move |call: &Call<'_>| {
            let path = call.path();
            let rel = match path.strip_prefix(&root) {
                Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
                Ok(rel) => rel.display().to_string(),
                Err(_) if Some(path) == root.parent() => "..".to_owned(),
                Err(_) => path.display().to_string(),
            };
            format!("{} {rel}", call.method)
        });
        self
    }

    /// This recorder, running `hook` once.
    #[must_use]
    pub fn with_hook(self, hook: Hook) -> Self {
        *self.0.hook.lock().unwrap() = Some(hook);
        self
    }
}

impl Recorder {
    /// What sets the calls this recorder fails.
    pub fn failing(&self) -> Failing {
        self.failing.clone()
    }

    /// What counts this recorder's `sync_data` calls in flight.
    pub fn syncs(&self) -> Rendezvous {
        self.syncs.clone()
    }

    /// Runs the hook if it waits for `call` at `when`.
    fn run_hook(&self, call: &str, when: When) {
        let hook = {
            let mut hook = self.hook.lock().unwrap();
            if hook
                .as_ref()
                .is_some_and(|(at, at_when, _)| at == call && *at_when == when)
            {
                hook.take()
            } else {
                None
            }
        };
        if let Some((_, _, run)) = hook {
            run();
        }
    }
}

impl Watch for Recorder {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        let named = (self.name)(call);
        self.calls.lock().unwrap().push(named.clone());
        self.failing.check(&named)?;
        self.run_hook(&named, When::Before);
        if call.method == FsMethod::SyncData {
            self.syncs.arrive();
        }
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if call.method == FsMethod::SyncData {
            self.syncs.depart();
        }
        self.run_hook(&(self.name)(call), When::After);
    }
}

/// Which calls a [`RecordingFs`] fails, while set: each call as it records it. Clones share the
/// setting.
#[derive(Clone, Default)]
pub struct Failing(Arc<Mutex<Vec<&'static str>>>);

impl Failing {
    /// Fails `call`, and no other.
    pub fn set(&self, call: &'static str) {
        *self.0.lock().unwrap() = vec![call];
    }

    /// Fails `call` too.
    pub fn also(&self, call: &'static str) {
        self.0.lock().unwrap().push(call);
    }

    /// Fails no call.
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// An injected error if `call` is a failing call; the call then does nothing.
    fn check(&self, call: &str) -> io::Result<()> {
        if self.0.lock().unwrap().contains(&call) {
            Err(io::Error::other(format!("injected: {call}")))
        } else {
            Ok(())
        }
    }
}

/// The real filesystem, except that the calls its predicate picks, given the method (which
/// compares equal to its name) and the path it acts on ([`Call::path`]), fail with an injected
/// error. `Watched(FailWhen::new(..), fs)` fails them on any other filesystem `fs`.
pub type FailingFs = Watched<FailWhen>;

/// Which calls a [`FailingFs`] fails: given the method and the path it acts on.
type Predicate = dyn Fn(FsMethod, &Path) -> bool + Send + Sync;

/// The watcher of a [`FailingFs`]: fails the calls its predicate picks, before they run.
pub struct FailWhen(Box<Predicate>);

impl FailWhen {
    /// Fails the calls `fail` picks, given the method and the path the call acts on.
    pub fn new(fail: impl Fn(FsMethod, &Path) -> bool + Send + Sync + 'static) -> Self {
        FailWhen(Box::new(fail))
    }
}

impl FailingFs {
    /// The real filesystem, failing the calls `fail` picks, given the method and the path the
    /// call acts on.
    pub fn new(fail: impl Fn(FsMethod, &Path) -> bool + Send + Sync + 'static) -> Self {
        Watched(FailWhen::new(fail), RealFs)
    }
}

impl Watch for FailWhen {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if (self.0)(call.method, call.path()) {
            Err(io::Error::other(format!(
                "injected failure: {} {}",
                call.method,
                call.path().display()
            )))
        } else {
            Ok(())
        }
    }
}

/// [`RealFs`] without a single flush to stable storage: every effect on the files' contents and
/// names is `RealFs`'s, and only its `sync_all` and `sync_data` are left out. Where `RealFs` opens
/// a path to sync it (`sync_data`, `fsync_dir`, the directory of an `atomic_replace` and of a
/// `rename_durable`), it opens it all the same, so that a missing path fails as there
/// (`NotFound`). For the tests, benches and fuzz targets whose subject is what the files hold and
/// what readers see, which a flush left out does not change, and that make too many writes to pay
/// for their flushes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSyncFs;

impl FsOps for NoSyncFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        RealFs.create_dir_all(p)
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        let (_file, path) = temp.keep().map_err(|e| e.error)?;
        Ok(path)
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        RealFs.link_noclobber(from, to)
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        File::open(p).map(drop)
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        RealFs.remove(p)
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OpenOptions::new().write(true).open(path)?.set_len(len)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = parent_dir(path);
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        // On failure the temp file is removed when the returned `NamedTempFile` drops.
        temp.persist(path).map_err(|e| e.error)?;
        self.fsync_dir(dir)
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        File::open(path).map(drop)
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)?;
        self.fsync_dir(parent_dir(to))
    }
}

/// The directory that holds `path`, as `RealFs` takes it: `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// A rendezvous for the doubles, to tell calls made at once from calls made one after another:
/// it counts the calls in flight and the most that were at once, and while armed it holds each
/// call until that many are in flight together (or until [`Rendezvous::PATIENCE`] has passed).
/// Calls made one after another then never overlap, and calls made at once all do. Clones share
/// the count.
#[derive(Clone, Default)]
pub struct Rendezvous(Arc<(Mutex<InFlight>, Condvar)>);

/// What a [`Rendezvous`] counts.
#[derive(Default)]
struct InFlight {
    /// How many calls each call waits to be in flight with it; 0 waits for none.
    want: usize,
    /// The calls in flight now.
    now: usize,
    /// The most calls that were in flight at once since it was last armed.
    most: usize,
}

impl Rendezvous {
    /// How long a call waits for the others.
    pub const PATIENCE: Duration = Duration::from_secs(5);

    /// From now on, each call waits until `want` are in flight together; 0 waits for none.
    /// Starts the count of the most at once afresh.
    pub fn arm(&self, want: usize) {
        let mut flight = self.0.0.lock().unwrap();
        (flight.want, flight.most) = (want, 0);
    }

    /// The most calls that were in flight at once since [`Rendezvous::arm`] (or ever).
    pub fn most(&self) -> usize {
        self.0.0.lock().unwrap().most
    }

    /// A call is in flight from now on: counts it, then waits for the others.
    pub fn arrive(&self) {
        let (lock, changed) = &*self.0;
        let mut flight = lock.lock().unwrap();
        flight.now += 1;
        flight.most = flight.most.max(flight.now);
        changed.notify_all();
        let deadline = Instant::now() + Self::PATIENCE;
        // Once `most` reaches `want` every waiter goes on, even after some have finished.
        while flight.most < flight.want {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            flight = changed.wait_timeout(flight, left).unwrap().0;
        }
    }

    /// A call that [`Rendezvous::arrive`] counted is done.
    pub fn depart(&self) {
        self.0.0.lock().unwrap().now -= 1;
    }
}

/// One call of a double held part-way until the test lets it go, the double's end: [`Hold::hold`]
/// tells the test that the call is held, and waits. [`hold`] makes it together with the test's
/// end, [`Held`]. Unlike a [`Rendezvous`], which lets calls go once enough are in flight, it hands
/// the call over in order: held first, then let go.
pub struct Hold {
    /// Tells the test that the call is held.
    held: mpsc::Sender<()>,
    /// Never receives anything: it ends when the test drops its sender, letting the call go.
    release: mpsc::Receiver<()>,
}

/// The test's end of a [`Hold`]: [`Held::wait`] waits until the call is held, and
/// [`Held::release`], or dropping it, lets the call go on.
pub struct Held {
    /// Says that the call is held.
    held: mpsc::Receiver<()>,
    /// Dropped to let the call go.
    _release: mpsc::Sender<()>,
}

/// A hold of one call: the double's end, and the test's.
pub fn hold() -> (Hold, Held) {
    let (tell, told) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let double = Hold {
        held: tell,
        release: released,
    };
    let test = Held {
        held: told,
        _release: release,
    };
    (double, test)
}

impl Hold {
    /// How long either end waits for the other at most: longer than a loaded host delays a
    /// thread, and short enough that a test that fails while a call is held ends rather than
    /// hangs.
    pub const PATIENCE: Duration = Duration::from_secs(60);

    /// Tells the test that the call is held, then waits until the test lets it go (or has
    /// dropped its end already), or until [`Hold::PATIENCE`] has passed.
    pub fn hold(self) {
        // A test that dropped its end lets the call go at once.
        let _ = self.held.send(());
        let _ = self.release.recv_timeout(Self::PATIENCE);
    }
}

impl Held {
    /// Waits until the call is held. Panics if it is not within [`Hold::PATIENCE`], or if the
    /// double dropped its end without holding the call (the call failed before it got there).
    pub fn wait(&self) {
        if let Err(e) = self.held.recv_timeout(Hold::PATIENCE) {
            panic!("the call to hold was never held: {e}");
        }
    }

    /// Lets the held call go on, as dropping `self` does.
    pub fn release(self) {
        drop(self);
    }
}

/// Linux's `EIO`: what an `fdatasync` reports when the writeback it waits for fails.
const EIO: i32 = 5;

/// [`RealFs`], or any filesystem `F`, with the first `append_durable` of one file held between
/// its write and its flush, and then failed: the call appends its bytes without a flush
/// ([`FsOps::append`]), is held there ([`Hold`]) until the test lets it go ([`Held`]), and fails
/// with `EIO`, as an `fdatasync` whose writeback failed does, its bytes left in the file (the page
/// cache) for the caller to cut back. On a store's `journal.log`: a writer between its frame's
/// write and its sync, whose sync then fails, so that it cuts the frame back off the journal under
/// the lock (storage spec §2 rule 3 (a)). Every other call, and every later `append_durable` of
/// the file, goes to `F` as it is.
pub struct HoldThenFail<F = RealFs> {
    /// The file whose first `append_durable` is held, by the path the caller names it with.
    path: PathBuf,
    /// The hold, until that call takes it.
    hold: Mutex<Option<Hold>>,
    /// What every call runs on.
    fs: F,
}

impl<F: FsOps> HoldThenFail<F> {
    /// `fs`, with the first `append_durable` of `path` held and failed (a store names its files
    /// by its root as it was given: `Layout::journal_log`, say), and the test's end of the hold.
    pub fn new(path: impl Into<PathBuf>, fs: F) -> (HoldThenFail<F>, Held) {
        let (double, test) = hold();
        let fs = HoldThenFail {
            path: path.into(),
            hold: Mutex::new(Some(double)),
            fs,
        };
        (fs, test)
    }
}

impl<F: FsOps> FsOps for HoldThenFail<F> {
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
        let hold = if path == self.path {
            self.hold.lock().unwrap().take()
        } else {
            None
        };
        let Some(hold) = hold else {
            return self.fs.append_durable(path, bytes);
        };
        self.fs.append(path, bytes)?;
        hold.hold();
        Err(io::Error::from_raw_os_error(EIO))
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

/// Copies directory `from` to `to`, files and subdirectories, without syncing anything.
pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("make a directory");
    for entry in fs::read_dir(from).expect("read a directory") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("its type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy a file");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::Path;

    use swarmops_store::{FsMethod, FsOps, LinkOutcome, RealFs};

    use super::*;

    /// What a call gave, its value as two filesystems can share it: an error's kind, or the
    /// value (a temp file's path only as made).
    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Done,
        Read(Option<Vec<u8>>),
        Exists(bool),
        Len(u64),
        Linked(LinkOutcome),
        TempMade,
        Failed(io::ErrorKind),
    }

    /// Calls `method` of `fs` on `path`, with `to` as a link's or a rename's target and
    /// `b"bytes"` for what it writes.
    fn call(fs: &dyn FsOps, method: FsMethod, path: &Path, to: &Path) -> Outcome {
        fn of<T>(result: io::Result<T>, ok: impl FnOnce(T) -> Outcome) -> Outcome {
            result.map_or_else(|e| Outcome::Failed(e.kind()), ok)
        }
        let bytes = b"bytes";
        match method {
            FsMethod::CreateDirAll => of(fs.create_dir_all(path), |()| Outcome::Done),
            FsMethod::WriteDurableTemp => {
                of(fs.write_durable_temp(path, bytes), |_| Outcome::TempMade)
            }
            FsMethod::LinkNoclobber => of(fs.link_noclobber(path, to), Outcome::Linked),
            FsMethod::FsyncDir => of(fs.fsync_dir(path), |()| Outcome::Done),
            FsMethod::Read => of(fs.read(path), Outcome::Read),
            FsMethod::Exists => of(fs.exists(path), Outcome::Exists),
            FsMethod::Remove => of(fs.remove(path), |()| Outcome::Done),
            FsMethod::AppendDurable => of(fs.append_durable(path, bytes), Outcome::Len),
            FsMethod::FileLen => of(fs.file_len(path), Outcome::Len),
            FsMethod::Truncate => of(fs.truncate(path, 1), |()| Outcome::Done),
            FsMethod::AtomicReplace => of(fs.atomic_replace(path, bytes), |()| Outcome::Done),
            FsMethod::SyncData => of(fs.sync_data(path), |()| Outcome::Done),
            FsMethod::Append => of(fs.append(path, bytes), Outcome::Len),
            FsMethod::RenameDurable => of(fs.rename_durable(path, to), |()| Outcome::Done),
        }
    }

    /// Every method of the double gives what `RealFs` gives where a path is missing (in a
    /// directory that exists, or in one that does not) and where a link's or a rename's target
    /// lies in a missing directory: the same error kind, or the same value. A sync of a path
    /// that does not exist fails, as it does on the real filesystem.
    #[test]
    fn no_sync_fs_fails_where_real_fs_fails() {
        let cases = [
            ("missing", "to"),
            ("no-dir/missing", "to"),
            ("file", "no-dir/to"),
        ];
        for method in FsMethod::ALL {
            for (path, to) in cases {
                let outcomes = [&RealFs as &dyn FsOps, &NoSyncFs].map(|fs| {
                    let dir = tempfile::tempdir().unwrap();
                    fs::write(dir.path().join("file"), b"a file").unwrap();
                    call(fs, method, &dir.path().join(path), &dir.path().join(to))
                });
                assert_eq!(outcomes[0], outcomes[1], "{method} {path} {to}");
            }
        }
    }

    /// A script of every method on `fs` in `dir`, two links of the same name and a temp file
    /// left behind among them: what each call gave.
    fn script(fs: &dyn FsOps, dir: &Path) -> Vec<Outcome> {
        let (sub, log, ckpt) = (dir.join("sub"), dir.join("log"), dir.join("ckpt"));
        let object = sub.join("object");
        let mut outcomes = vec![call(fs, FsMethod::CreateDirAll, &sub.join("deeper"), dir)];
        for _ in 0..2 {
            let temp = fs.write_durable_temp(&sub, b"object").unwrap();
            outcomes.push(call(fs, FsMethod::LinkNoclobber, &temp, &object));
        }
        outcomes.extend([
            call(fs, FsMethod::WriteDurableTemp, dir, dir),
            call(fs, FsMethod::FsyncDir, &sub, dir),
            call(fs, FsMethod::Read, &object, dir),
            call(fs, FsMethod::Exists, &object, dir),
            call(fs, FsMethod::AppendDurable, &log, dir),
            call(fs, FsMethod::Append, &log, dir),
            call(fs, FsMethod::FileLen, &log, dir),
            call(fs, FsMethod::Truncate, &log, dir),
            call(fs, FsMethod::SyncData, &log, dir),
            call(fs, FsMethod::AtomicReplace, &ckpt, dir),
            call(fs, FsMethod::AtomicReplace, &ckpt, dir),
            call(fs, FsMethod::RenameDurable, &object, &dir.join("renamed")),
            call(fs, FsMethod::Append, &dir.join("gone"), dir),
            call(fs, FsMethod::Remove, &dir.join("gone"), dir),
        ]);
        outcomes
    }

    /// The tree under `dir`: each path in it, a temp file's name normalized, with the file's
    /// content (`None` for a directory).
    fn tree(dir: &Path) -> Vec<(String, Option<Vec<u8>>)> {
        let mut tree = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let name = path.file_name().unwrap().to_string_lossy();
                let rel = if name.starts_with(".tmp") {
                    rel.replace(&*name, ".tmp*")
                } else {
                    rel
                };
                if path.is_dir() {
                    tree.push((rel, None));
                    stack.push(path);
                } else {
                    tree.push((rel, Some(fs::read(&path).unwrap())));
                }
            }
        }
        tree.sort();
        tree
    }

    /// The same calls of all 14 methods leave the same files, with the same contents, on the
    /// double as on the real filesystem, and give the same results.
    #[test]
    fn no_sync_fs_leaves_what_real_fs_leaves() {
        let (real, no_sync) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ran = script(&RealFs, real.path());
        assert_eq!(script(&NoSyncFs, no_sync.path()), ran);
        assert!(ran.contains(&Outcome::Linked(LinkOutcome::AlreadyExists)));
        let left = tree(real.path());
        assert_eq!(tree(no_sync.path()), left);
        assert!(left.iter().any(|(path, _)| path == ".tmp*"), "{left:?}");
    }

    /// A watched double runs over any filesystem: a [`FailWhen`] over [`NoSyncFs`] fails the
    /// call it picks, before it runs, and the others run on the double, which fails a sync of a
    /// missing path as the real filesystem does.
    #[test]
    fn failing_fs_over_any_inner_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        let failing: Watched<FailWhen, NoSyncFs> = Watched(
            FailWhen::new(|method, path| method == "sync_data" && path.ends_with("f")),
            NoSyncFs,
        );
        let fs: Arc<dyn FsOps> = Arc::new(failing);
        assert_eq!(fs.append_durable(&file, b"ab").unwrap(), 0);
        let failed = fs.sync_data(&file).unwrap_err();
        assert_eq!(
            failed.to_string(),
            format!("injected failure: sync_data {}", file.display())
        );
        assert_eq!(fs.append(&file, b"cd").unwrap(), 2);
        assert_eq!(fs::read(&file).unwrap(), b"abcd");
        let missing = fs.fsync_dir(&dir.path().join("missing")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound, "{missing}");
    }
}
