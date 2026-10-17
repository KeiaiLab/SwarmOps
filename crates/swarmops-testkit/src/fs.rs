//! [`FsOps`] doubles for the tests, on real files:
//!
//! - [`Watched`]: the real filesystem with a [`Watch`] run around each call, to record calls,
//!   fail them (before they run, or after), or hold them; [`RecordingFs`] records, fails the
//!   calls a [`Failing`] names, and runs a [`Hook`] once; [`FailingFs`] fails the calls a
//!   predicate picks.
//! - [`FastFs`]: the real filesystem without a single flush to stable storage.
//! - [`Rendezvous`]: tells calls made at once from calls made one after another.
//!
//! The store's tests use them, and so do the repository's, the operations' and the CLI's.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use swarmops_store::{FsOps, LinkOutcome, RealFs};

/// One call of an [`FsOps`] method, as a [`Watched`] filesystem shows it to its watcher.
pub struct Call<'a> {
    /// The method's name, `"append"` say.
    pub method: &'static str,
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
        let mut call = self.method.to_owned();
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

/// The real filesystem ([`RealFs`]), its watcher run around each call. It dereferences to the
/// watcher.
pub struct Watched<W>(
    /// The watcher.
    pub W,
);

impl<W> Deref for Watched<W> {
    type Target = W;

    fn deref(&self) -> &W {
        &self.0
    }
}

impl<W: Watch> Watched<W> {
    /// Runs `call`, the call of `method` on `paths`, with the watcher around it.
    fn around<T>(
        &self,
        method: &'static str,
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

impl<W: Watch> FsOps for Watched<W> {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.around("create_dir_all", &[p], || RealFs.create_dir_all(p))
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.around("write_durable_temp", &[dir], || {
            RealFs.write_durable_temp(dir, bytes)
        })
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.around("link_noclobber", &[from, to], || {
            RealFs.link_noclobber(from, to)
        })
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.around("fsync_dir", &[p], || RealFs.fsync_dir(p))
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        self.around("read", &[p], || RealFs.read(p))
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        self.around("exists", &[p], || RealFs.exists(p))
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.around("remove", &[p], || RealFs.remove(p))
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.around("append_durable", &[path], || {
            RealFs.append_durable(path, bytes)
        })
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.around("file_len", &[path], || RealFs.file_len(path))
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.around("truncate", &[path], || RealFs.truncate(path, len))
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.around("atomic_replace", &[path], || {
            RealFs.atomic_replace(path, bytes)
        })
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.around("sync_data", &[path], || RealFs.sync_data(path))
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.around("append", &[path], || RealFs.append(path, bytes))
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.around("rename_durable", &[from, to], || {
            RealFs.rename_durable(from, to)
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
        let fs = Watched(Recorder {
            calls: Arc::clone(&calls),
            name: Box::new(|call: &Call<'_>| call.named()),
            hook: Mutex::new(None),
            failing: Failing::default(),
            syncs: Rendezvous::default(),
        });
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
        if call.method == "sync_data" {
            self.syncs.arrive();
        }
        Ok(())
    }

    fn after(&self, call: &Call<'_>) {
        if call.method == "sync_data" {
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

/// The real filesystem, except that the calls its predicate picks, given the method's name and
/// the path it acts on ([`Call::path`]), fail with an injected error.
pub type FailingFs = Watched<FailWhen>;

/// Which calls a [`FailingFs`] fails: given the method's name and the path it acts on.
type Predicate = dyn Fn(&str, &Path) -> bool + Send + Sync;

/// The watcher of a [`FailingFs`].
pub struct FailWhen(Box<Predicate>);

impl FailingFs {
    /// The real filesystem, failing the calls `fail` picks, given the method's name and the
    /// path the call acts on.
    pub fn new(fail: impl Fn(&str, &Path) -> bool + Send + Sync + 'static) -> Self {
        Watched(FailWhen(Box::new(fail)))
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

/// [`RealFs`] without a single flush to stable storage: for tests whose subject is what the
/// files hold and what readers see, which a skipped fsync does not change, and that make too
/// many writes to pay for their fsyncs.
pub struct FastFs;

impl FsOps for FastFs {
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

    fn fsync_dir(&self, _p: &Path) -> io::Result<()> {
        Ok(())
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
        let dir = path.parent().expect("a file in a directory");
        let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
        temp.write_all(bytes)?;
        temp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    fn sync_data(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        RealFs.append(path, bytes)
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
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
