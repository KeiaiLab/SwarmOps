//! Power-loss crash images: what a store's files can hold after the power fails at any point of a
//! run, built from a recording of the run's filesystem calls (RocksDB's `FaultInjectionTestFS`,
//! FoundationDB's `AsyncFileNonDurable` and SQLite's crash VFS, made deterministic and
//! exhaustive).
//!
//! A process crash ([`swarmops_store::CrashAtFs`]) leaves the kernel's page cache in place: every
//! byte a call wrote reaches the disk in the end. A power loss takes back what was never made
//! durable. [`RecordingFs`] runs the stores' [`FsOps`] calls on real files without a single flush
//! to stable storage, and records each mutating call with its data. [`Timeline`] breaks the calls
//! into steps, a call into more than one where it syncs part-way (`append_durable` writes, then
//! syncs; `atomic_replace` writes its temp file, syncs it, renames it, then syncs the directory),
//! and builds the [`Image`] of what the disk may hold when the power fails after any step:
//!
//! - File data: what the last sync made durable (`sync_data`, or the one inside
//!   `append_durable`, `truncate`, `write_durable_temp` and `atomic_replace`); and of the bytes
//!   written since, as a [`Chooser`] picks ([`Keep`]): none, a prefix up to a 512 B or 4 KiB
//!   boundary, all, the length but zeros, or all but the first 512 B sector, zeros there.
//! - Directory entries (a name created, linked, renamed or removed): the changes a later
//!   `fsync_dir` of their directory covered, and a prefix of the rest in the order they were
//!   made, as the chooser picks: each directory's changes reach the disk in order, and
//!   directories are independent. A rename is a change of its target's directory, all or nothing.
//!
//! [`Seeded`] picks for a seed: 0 the least the disk can hold (every effect not made durable
//! lost), 1 the most (every one kept, what a process crash at that point leaves), and any other
//! seed at random, the same way each time.
//!
//! The CLI's power-loss tests include this file by its path.

#![allow(dead_code)] // Each test binary uses the part it needs.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use swarmops_store::{FsOps, LinkOutcome, RealFs};

/// The sector a power loss tears writes at.
pub const SECTOR: u64 = 512;

/// The page a filesystem writes back at.
pub const PAGE: u64 = 4096;

/// One mutating [`FsOps`] call as [`RecordingFs`] ran it, with the data it wrote; or a mark.
#[derive(Debug, Clone)]
pub enum Call {
    /// `create_dir_all`: the directories it made, outermost first (none: not recorded).
    CreateDirAll(Vec<PathBuf>),
    /// `write_durable_temp`: the file it made and what it wrote there.
    WriteDurableTemp {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    /// `link_noclobber`: whether `to` was made (it did not exist), then `from` removed.
    LinkNoclobber {
        from: PathBuf,
        to: PathBuf,
        placed: bool,
    },
    FsyncDir(PathBuf),
    Remove(PathBuf),
    AppendDurable {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    Truncate {
        path: PathBuf,
        len: u64,
    },
    AtomicReplace {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    SyncData(PathBuf),
    Append {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    RenameDurable {
        from: PathBuf,
        to: PathBuf,
    },
    /// Not a call: a point between calls that the test marked, an operation returning, say.
    Mark(String),
}

/// The real filesystem without a flush to stable storage, each mutating call recorded with its
/// data ([`Call`]); reads go to [`RealFs`] unrecorded. Calls from several threads are run and
/// recorded one at a time, so the record is the order they took effect in.
#[derive(Debug, Default)]
pub struct RecordingFs {
    calls: Mutex<Vec<Call>>,
}

impl RecordingFs {
    pub fn new() -> RecordingFs {
        RecordingFs::default()
    }

    /// Marks this point of the run as `label`.
    pub fn mark(&self, label: &str) {
        self.log().push(Call::Mark(label.to_owned()));
    }

    /// The calls (and marks) so far.
    pub fn calls(&self) -> Vec<Call> {
        self.log().clone()
    }

    fn log(&self) -> std::sync::MutexGuard<'_, Vec<Call>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `call` with the record locked, and records what it returns to record.
    fn run<T>(&self, call: impl FnOnce() -> io::Result<(T, Option<Call>)>) -> io::Result<T> {
        let mut log = self.log();
        let (out, recorded) = call()?;
        log.extend(recorded);
        Ok(out)
    }
}

impl FsOps for RecordingFs {
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.run(|| {
            let mut made: Vec<PathBuf> = p
                .ancestors()
                .take_while(|dir| !dir.exists())
                .map(Path::to_path_buf)
                .collect();
            made.reverse();
            fs::create_dir_all(p)?;
            Ok(((), (!made.is_empty()).then_some(Call::CreateDirAll(made))))
        })
    }

    fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        self.run(|| {
            let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
            temp.write_all(bytes)?;
            let (_file, path) = temp.keep().map_err(|e| e.error)?;
            let call = Call::WriteDurableTemp {
                path: path.clone(),
                bytes: bytes.to_vec(),
            };
            Ok((path, Some(call)))
        })
    }

    fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
        self.run(|| {
            let placed = match fs::hard_link(from, to) {
                Ok(()) => true,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
                Err(e) => return Err(e),
            };
            fs::remove_file(from)?;
            let outcome = if placed {
                LinkOutcome::Placed
            } else {
                LinkOutcome::AlreadyExists
            };
            let call = Call::LinkNoclobber {
                from: from.to_owned(),
                to: to.to_owned(),
                placed,
            };
            Ok((outcome, Some(call)))
        })
    }

    fn fsync_dir(&self, p: &Path) -> io::Result<()> {
        self.run(|| {
            File::open(p)?;
            Ok(((), Some(Call::FsyncDir(p.to_owned()))))
        })
    }

    fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
        RealFs.read(p)
    }

    fn exists(&self, p: &Path) -> io::Result<bool> {
        RealFs.exists(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.run(|| {
            fs::remove_file(p)?;
            Ok(((), Some(Call::Remove(p.to_owned()))))
        })
    }

    fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.run(|| {
            let offset = append_to(path, bytes)?;
            let call = Call::AppendDurable {
                path: path.to_owned(),
                bytes: bytes.to_vec(),
            };
            Ok((offset, Some(call)))
        })
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        RealFs.file_len(path)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.run(|| {
            OpenOptions::new().write(true).open(path)?.set_len(len)?;
            let call = Call::Truncate {
                path: path.to_owned(),
                len,
            };
            Ok(((), Some(call)))
        })
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.run(|| {
            let dir = path.parent().expect("a file in a directory");
            let mut temp = tempfile::Builder::new().tempfile_in(dir)?;
            temp.write_all(bytes)?;
            temp.persist(path).map_err(|e| e.error)?;
            let call = Call::AtomicReplace {
                path: path.to_owned(),
                bytes: bytes.to_vec(),
            };
            Ok(((), Some(call)))
        })
    }

    fn sync_data(&self, path: &Path) -> io::Result<()> {
        self.run(|| {
            File::open(path)?;
            Ok(((), Some(Call::SyncData(path.to_owned()))))
        })
    }

    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        self.run(|| {
            let offset = append_to(path, bytes)?;
            let call = Call::Append {
                path: path.to_owned(),
                bytes: bytes.to_vec(),
            };
            Ok((offset, Some(call)))
        })
    }

    fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.run(|| {
            fs::rename(from, to)?;
            let call = Call::RenameDurable {
                from: from.to_owned(),
                to: to.to_owned(),
            };
            Ok(((), Some(call)))
        })
    }
}

/// Appends `bytes` to `path` (created if missing), no flush; the length before.
fn append_to(path: &Path, bytes: &[u8]) -> io::Result<u64> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    let offset = file.metadata()?.len();
    file.write_all(bytes)?;
    Ok(offset)
}

/// What a directory entry names: a directory, or a file (an inode, which hard links share).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    Dir(usize),
    File(usize),
}

/// One change to a directory: what a power loss may take back unless a later `fsync_dir` of the
/// directory covered it.
#[derive(Debug, Clone)]
enum Entry {
    /// `name` in `dir` names `node` now: a new directory, a new file, or a link to a file.
    Add {
        dir: usize,
        name: OsString,
        node: Node,
    },
    /// `name` in `dir` names nothing now.
    Remove { dir: usize, name: OsString },
    /// `from_name` in `from_dir` moved to `to_name` in `to_dir`, replacing what was there.
    Rename {
        from_dir: usize,
        from_name: OsString,
        to_dir: usize,
        to_name: OsString,
    },
}

impl Entry {
    /// The directory whose change it is: a rename's target directory.
    fn dir(&self) -> usize {
        match self {
            Entry::Add { dir, .. } | Entry::Remove { dir, .. } => *dir,
            Entry::Rename { to_dir, .. } => *to_dir,
        }
    }
}

/// What a step does.
#[derive(Debug, Clone)]
enum Effect {
    Entry(Entry),
    /// `fsync_dir`: every change made to the directory so far is durable.
    DirSync(usize),
    /// Bytes written to file `inode` at `at`: not durable yet.
    Write {
        inode: usize,
        at: u64,
        bytes: Vec<u8>,
    },
    /// File `inode` cut (or grown) to `len`: not durable yet.
    SetLen {
        inode: usize,
        len: u64,
    },
    /// File `inode`'s data, as written so far, is durable.
    DataSync(usize),
}

/// One step of a run: the power may fail after it.
#[derive(Debug, Clone)]
pub struct Step {
    /// The call it is part of, its paths relative to the base: `append_durable
    /// store/meta/journal.log (write)`, say.
    pub label: String,
    effects: Vec<Effect>,
}

/// A run's calls, as steps the power may fail after; see the module documentation.
#[derive(Debug, Clone)]
pub struct Timeline {
    steps: Vec<Step>,
    /// The steps of each call, by its index in the calls (a mark has none).
    call_steps: Vec<Range<usize>>,
    /// Each mark, with how many steps came before it.
    marks: Vec<(String, usize)>,
    dirs: usize,
    inodes: usize,
}

/// The directory tree as the calls leave it, while a [`Timeline`] is built.
struct Builder<'a> {
    base: &'a Path,
    /// The entries of each directory, by id; 0 is the base.
    tree: Vec<BTreeMap<OsString, Node>>,
    /// The length of each file, by inode.
    lens: Vec<u64>,
    temps: u64,
    timeline: Timeline,
}

impl Timeline {
    /// The timeline of `calls`, which [`RecordingFs`] recorded on paths under `base` (`base`
    /// existed already, and the calls made everything under it).
    pub fn new(base: &Path, calls: &[Call]) -> Timeline {
        let mut builder = Builder {
            base,
            tree: vec![BTreeMap::new()],
            lens: Vec::new(),
            temps: 0,
            timeline: Timeline {
                steps: Vec::new(),
                call_steps: Vec::new(),
                marks: Vec::new(),
                dirs: 1,
                inodes: 0,
            },
        };
        for call in calls {
            let first = builder.timeline.steps.len();
            builder.call(call);
            let steps = first..builder.timeline.steps.len();
            builder.timeline.call_steps.push(steps);
        }
        builder.timeline.dirs = builder.tree.len();
        builder.timeline.inodes = builder.lens.len();
        builder.timeline
    }

    /// How many steps there are: the power may fail after `0..=steps()` of them.
    pub fn steps(&self) -> usize {
        self.steps.len()
    }

    /// Step `step` (from 0).
    pub fn step(&self, step: usize) -> &Step {
        &self.steps[step]
    }

    /// The steps of call `call` (its index among the recorded calls, marks included).
    pub fn steps_of(&self, call: usize) -> Range<usize> {
        self.call_steps[call].clone()
    }

    /// How many steps came before mark `label`; panics if there is no such mark.
    pub fn mark(&self, label: &str) -> usize {
        self.marks
            .iter()
            .find(|(mark, _)| mark == label)
            .map_or_else(|| panic!("no mark {label:?}"), |&(_, steps)| steps)
    }

    /// The bytes step `step` writes, and where: `(at, len)` for each write.
    pub fn writes(&self, step: usize) -> Vec<(u64, u64)> {
        self.steps[step]
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Write { at, bytes, .. } => Some((*at, to_u64(bytes.len()))),
                _ => None,
            })
            .collect()
    }

    /// What the disk may hold once the power fails after the first `cut` steps, as `chooser`
    /// picks among what was not durable then.
    pub fn image(&self, cut: usize, chooser: &mut dyn Chooser) -> Image {
        let mut data = vec![Data::default(); self.inodes];
        let mut logs: Vec<Vec<(usize, &Entry)>> = vec![Vec::new(); self.dirs];
        let mut synced = vec![0; self.dirs];
        let mut order = 0;
        for step in &self.steps[..cut] {
            for effect in &step.effects {
                match effect {
                    Effect::Entry(entry) => {
                        logs[entry.dir()].push((order, entry));
                        order += 1;
                    }
                    Effect::DirSync(dir) => synced[*dir] = logs[*dir].len(),
                    Effect::Write { inode, at, bytes } => data[*inode].write(*at, bytes),
                    Effect::SetLen { inode, len } => data[*inode].set_len(*len),
                    Effect::DataSync(inode) => data[*inode].sync(),
                }
            }
        }
        let mut choices = Vec::new();
        // The entries that reach the disk: each directory's covered ones and a prefix of the
        // rest, applied in the order they were made, to the base as it was at the start.
        let paths = self.dir_paths(cut);
        let mut kept: Vec<(usize, &Entry)> = Vec::new();
        for (dir, log) in logs.iter().enumerate() {
            let uncovered = log.len() - synced[dir];
            let keep = if uncovered == 0 {
                0
            } else {
                chooser.entries(&paths[dir], uncovered).min(uncovered)
            };
            if keep < uncovered {
                choices.push(format!(
                    "{}/: {keep} of the {uncovered} changes no fsync covered",
                    paths[dir].display()
                ));
            }
            kept.extend(&log[..synced[dir] + keep]);
        }
        kept.sort_by_key(|&(order, _)| order);
        let mut tree: Vec<BTreeMap<OsString, Node>> = vec![BTreeMap::new(); self.dirs];
        for (_, entry) in kept {
            apply(&mut tree, entry);
        }
        let mut image = Image::default();
        // One pick per file, however many names it has.
        let mut picked: BTreeMap<usize, Keep> = BTreeMap::new();
        walk(&tree, 0, Path::new(""), &mut |path, node| match node {
            Node::Dir(_) => image.dirs.push(path.to_owned()),
            Node::File(inode) => {
                let file = &data[inode];
                let bytes = if file.current == file.durable {
                    file.current.clone()
                } else {
                    let keep = *picked.entry(inode).or_insert_with(|| {
                        let keep = chooser.data(path);
                        if keep != Keep::All {
                            choices.push(format!("{}: {keep:?}", path.display()));
                        }
                        keep
                    });
                    file.content(keep)
                };
                image.files.push((path.to_owned(), bytes));
            }
        });
        image.choices = choices;
        image
    }

    /// The path of each directory, relative to the base, as the steps before `cut` left it
    /// (where it was last named, for one no longer named).
    fn dir_paths(&self, cut: usize) -> Vec<PathBuf> {
        let mut tree: Vec<BTreeMap<OsString, Node>> = vec![BTreeMap::new(); self.dirs];
        let mut paths = vec![PathBuf::new(); self.dirs];
        for step in &self.steps[..cut] {
            for effect in &step.effects {
                if let Effect::Entry(entry) = effect {
                    apply(&mut tree, entry);
                }
            }
        }
        walk(&tree, 0, Path::new(""), &mut |path, node| {
            if let Node::Dir(dir) = node {
                path.clone_into(&mut paths[dir]);
            }
        });
        paths
    }
}

/// Applies `entry` to `tree`. A rename of a name that is not there (one whose own entry a power
/// loss took back) is skipped.
fn apply(tree: &mut [BTreeMap<OsString, Node>], entry: &Entry) {
    match entry {
        Entry::Add { dir, name, node } => {
            tree[*dir].insert(name.clone(), *node);
        }
        Entry::Remove { dir, name } => {
            tree[*dir].remove(name);
        }
        Entry::Rename {
            from_dir,
            from_name,
            to_dir,
            to_name,
        } => {
            if let Some(node) = tree[*from_dir].remove(from_name) {
                tree[*to_dir].insert(to_name.clone(), node);
            }
        }
    }
}

/// Calls `visit` with the path (relative to the base) and node of everything under directory
/// `dir`, which is at `path`, parents before children, in name order.
fn walk(
    tree: &[BTreeMap<OsString, Node>],
    dir: usize,
    path: &Path,
    visit: &mut dyn FnMut(&Path, Node),
) {
    for (name, &node) in &tree[dir] {
        let child = path.join(name);
        visit(&child, node);
        if let Node::Dir(sub) = node {
            walk(tree, sub, &child, visit);
        }
    }
}

impl Builder<'_> {
    fn call(&mut self, call: &Call) {
        match call {
            Call::CreateDirAll(made) => {
                for dir in made {
                    let (parent, name) = self.place(dir);
                    let new = self.tree.len();
                    self.tree.push(BTreeMap::new());
                    let add = self.add(parent, name, Node::Dir(new));
                    self.step(format!("create_dir_all {}", self.rel(dir)), vec![add]);
                }
            }
            Call::WriteDurableTemp { path, bytes } => {
                let inode = self.new_file("write_durable_temp", path, bytes);
                self.step(
                    format!("write_durable_temp {} (sync)", self.rel(path)),
                    vec![Effect::DataSync(inode)],
                );
            }
            Call::LinkNoclobber { from, to, placed } => {
                if *placed {
                    let inode = self.file(from);
                    let (dir, name) = self.place(to);
                    let add = self.add(dir, name, Node::File(inode));
                    let label = format!("link_noclobber {} (link)", self.rel(to));
                    self.step(label, vec![add]);
                }
                let remove = self.remove(from);
                let label = format!("link_noclobber {} (remove the temp)", self.rel(from));
                self.step(label, vec![remove]);
            }
            Call::FsyncDir(path) => {
                let dir = self.dir(path);
                self.step(
                    format!("fsync_dir {}", self.rel(path)),
                    vec![Effect::DirSync(dir)],
                );
            }
            Call::Remove(path) => {
                let remove = self.remove(path);
                self.step(format!("remove {}", self.rel(path)), vec![remove]);
            }
            Call::AppendDurable { path, bytes } => {
                let inode = self.append(path, bytes, "append_durable");
                let label = format!("append_durable {} (sync)", self.rel(path));
                self.step(label, vec![Effect::DataSync(inode)]);
            }
            Call::Truncate { path, len } => {
                let inode = self.file(path);
                self.lens[inode] = *len;
                let rel = self.rel(path);
                self.step(
                    format!("truncate {rel} (cut)"),
                    vec![Effect::SetLen { inode, len: *len }],
                );
                self.step(
                    format!("truncate {rel} (sync)"),
                    vec![Effect::DataSync(inode)],
                );
            }
            Call::AtomicReplace { path, bytes } => self.atomic_replace(path, bytes),
            Call::SyncData(path) => {
                let inode = self.file(path);
                self.step(
                    format!("sync_data {}", self.rel(path)),
                    vec![Effect::DataSync(inode)],
                );
            }
            Call::Append { path, bytes } => {
                self.append(path, bytes, "append");
            }
            Call::RenameDurable { from, to } => {
                let rename = self.rename(from, to);
                let rel = self.rel(to);
                self.step(format!("rename_durable {rel} (rename)"), vec![rename]);
                let dir = self.place(to).0;
                self.step(
                    format!("rename_durable {rel} (sync the directory)"),
                    vec![Effect::DirSync(dir)],
                );
            }
            Call::Mark(label) => {
                let at = self.timeline.steps.len();
                self.timeline.marks.push((label.clone(), at));
            }
        }
    }

    /// `atomic_replace`: a temp file written, synced, renamed over `path`, the directory synced.
    fn atomic_replace(&mut self, path: &Path, bytes: &[u8]) {
        self.temps += 1;
        let temp = path.with_file_name(format!(".tmp-power-{}", self.temps));
        let inode = self.new_file("atomic_replace", &temp, bytes);
        let rel = self.rel(path);
        self.step(
            format!("atomic_replace {rel} (sync the temp file)"),
            vec![Effect::DataSync(inode)],
        );
        let rename = self.rename(&temp, path);
        self.step(format!("atomic_replace {rel} (rename)"), vec![rename]);
        let dir = self.place(path).0;
        self.step(
            format!("atomic_replace {rel} (sync the directory)"),
            vec![Effect::DirSync(dir)],
        );
    }

    /// A new file at `path` holding `bytes`, made by `call` in one step; its inode.
    fn new_file(&mut self, call: &str, path: &Path, bytes: &[u8]) -> usize {
        let inode = self.lens.len();
        self.lens.push(to_u64(bytes.len()));
        let (dir, name) = self.place(path);
        let add = self.add(dir, name, Node::File(inode));
        let write = Effect::Write {
            inode,
            at: 0,
            bytes: bytes.to_vec(),
        };
        let label = format!("{call} {} (write)", self.rel(path));
        self.step(label, vec![add, write]);
        inode
    }

    /// `bytes` appended to `path`, made if missing, in one step; its inode.
    fn append(&mut self, path: &Path, bytes: &[u8], call: &str) -> usize {
        let mut effects = Vec::new();
        let inode = match self.find(path) {
            Some(Node::File(inode)) => inode,
            Some(Node::Dir(_)) => panic!("{} is a directory", path.display()),
            None => {
                let inode = self.lens.len();
                self.lens.push(0);
                let (dir, name) = self.place(path);
                effects.push(self.add(dir, name, Node::File(inode)));
                inode
            }
        };
        let at = self.lens[inode];
        self.lens[inode] += to_u64(bytes.len());
        effects.push(Effect::Write {
            inode,
            at,
            bytes: bytes.to_vec(),
        });
        self.step(format!("{call} {} (write)", self.rel(path)), effects);
        inode
    }

    fn step(&mut self, label: String, effects: Vec<Effect>) {
        self.timeline.steps.push(Step { label, effects });
    }

    /// Adds `name` to `dir` in the tree; the effect that does so.
    fn add(&mut self, dir: usize, name: OsString, node: Node) -> Effect {
        self.tree[dir].insert(name.clone(), node);
        Effect::Entry(Entry::Add { dir, name, node })
    }

    fn remove(&mut self, path: &Path) -> Effect {
        let (dir, name) = self.place(path);
        self.tree[dir]
            .remove(&name)
            .unwrap_or_else(|| panic!("{} is not there to remove", path.display()));
        Effect::Entry(Entry::Remove { dir, name })
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Effect {
        let (from_dir, from_name) = self.place(from);
        let (to_dir, to_name) = self.place(to);
        let node = self.tree[from_dir]
            .remove(&from_name)
            .unwrap_or_else(|| panic!("{} is not there to rename", from.display()));
        self.tree[to_dir].insert(to_name.clone(), node);
        Effect::Entry(Entry::Rename {
            from_dir,
            from_name,
            to_dir,
            to_name,
        })
    }

    /// The directory that holds `path`, and its name there.
    fn place(&self, path: &Path) -> (usize, OsString) {
        let name = path.file_name().expect("a path with a name").to_owned();
        let parent = path.parent().expect("a path with a parent");
        (self.dir(parent), name)
    }

    fn dir(&self, path: &Path) -> usize {
        match self.find(path) {
            Some(Node::Dir(dir)) => dir,
            other => panic!("{} is no directory: {other:?}", path.display()),
        }
    }

    fn file(&self, path: &Path) -> usize {
        match self.find(path) {
            Some(Node::File(inode)) => inode,
            other => panic!("{} is no file: {other:?}", path.display()),
        }
    }

    /// What `path` names in the tree now.
    fn find(&self, path: &Path) -> Option<Node> {
        let rel = path
            .strip_prefix(self.base)
            .unwrap_or_else(|_| panic!("{} lies outside the base", path.display()));
        let mut node = Node::Dir(0);
        for component in rel.components() {
            let Component::Normal(name) = component else {
                panic!("{}: an unexpected component", path.display());
            };
            let Node::Dir(dir) = node else {
                return None;
            };
            node = *self.tree[dir].get(name)?;
        }
        Some(node)
    }

    fn rel(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.base).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            rel.display().to_string()
        }
    }
}

/// One file's data once the power fails: what the disk holds for sure, and what was written.
#[derive(Debug, Clone, Default)]
struct Data {
    durable: Vec<u8>,
    current: Vec<u8>,
}

impl Data {
    fn write(&mut self, at: u64, bytes: &[u8]) {
        let at = to_usize(at);
        if self.current.len() < at + bytes.len() {
            self.current.resize(at + bytes.len(), 0);
        }
        self.current[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn set_len(&mut self, len: u64) {
        self.current.resize(to_usize(len), 0);
    }

    fn sync(&mut self) {
        self.durable.clone_from(&self.current);
    }

    /// What the file holds after the power loss, `keep` picking among the bytes written since
    /// the last sync: the files only grow between syncs (they are appended to; a cut syncs at
    /// once), unless a cut is what was in progress.
    fn content(&self, keep: Keep) -> Vec<u8> {
        let (durable, current) = (&self.durable, &self.current);
        let from = durable.len();
        if current.len() <= from {
            // A cut that did not reach the disk, or did.
            return if keep == Keep::Drop {
                durable.clone()
            } else {
                current.clone()
            };
        }
        assert_eq!(
            &current[..from],
            &durable[..],
            "a file only grows between syncs"
        );
        let to = current.len();
        let zeros_to = |end: usize| {
            let mut bytes = current.clone();
            bytes[from..end].fill(0);
            bytes
        };
        match keep {
            Keep::Drop => durable.clone(),
            Keep::All => current.clone(),
            Keep::Prefix { granule, pick } => {
                let granule = to_usize(granule);
                // The boundaries past the synced length and before the end.
                let bounds: Vec<usize> = (from / granule + 1..=(to - 1) / granule)
                    .map(|n| n * granule)
                    .collect();
                match bounds.get(to_usize(pick) % bounds.len().max(1)) {
                    Some(&bound) => current[..bound].to_vec(),
                    None => durable.clone(),
                }
            }
            Keep::ZeroFilled => zeros_to(to),
            Keep::FirstSectorLost => {
                let sector = to_usize(SECTOR);
                zeros_to(((from / sector + 1) * sector).min(to))
            }
        }
    }
}

/// What reaches the disk of the bytes written to a file since its last sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Keep {
    /// None of them: the file is as the last sync left it.
    Drop,
    /// Those up to a boundary of `granule` bytes ([`SECTOR`] or [`PAGE`]) past the synced
    /// length, the `pick`-th of them (modulo how many there are); none if there is no such
    /// boundary before the end.
    Prefix { granule: u64, pick: u64 },
    /// All of them.
    All,
    /// The length, but zeros for the bytes.
    ZeroFilled,
    /// All of them but those in the first 512 B sector they touch, zeros there.
    FirstSectorLost,
}

/// Picks what a power loss keeps of what was not durable: see [`Timeline::image`].
pub trait Chooser {
    /// How many of the `uncovered` changes (`1..`) made to directory `dir` (relative to the
    /// base) after its last `fsync_dir` reach the disk, from the first: `0..=uncovered`.
    fn entries(&mut self, dir: &Path, uncovered: usize) -> usize;

    /// What reaches the disk of the bytes written to `file` (relative to the base) since its
    /// last sync.
    fn data(&mut self, file: &Path) -> Keep;
}

/// The [`Chooser`] of a seed: 0 keeps nothing that was not durable, 1 keeps everything, and any
/// other seed picks at random, the same way for the same seed and cut.
#[derive(Debug, Clone)]
pub struct Seeded {
    seed: u64,
    rng: SplitMix64,
}

impl Seeded {
    pub fn new(seed: u64, cut: usize) -> Seeded {
        Seeded {
            seed,
            rng: SplitMix64(seed ^ to_u64(cut).wrapping_mul(0xa076_1d64_78bd_642f)),
        }
    }
}

impl Chooser for Seeded {
    fn entries(&mut self, _dir: &Path, uncovered: usize) -> usize {
        match self.seed {
            0 => 0,
            1 => uncovered,
            _ => to_usize(self.rng.below(to_u64(uncovered) + 1)),
        }
    }

    fn data(&mut self, _file: &Path) -> Keep {
        match self.seed {
            0 => Keep::Drop,
            1 => Keep::All,
            _ => match self.rng.below(6) {
                0 => Keep::Drop,
                1 => Keep::Prefix {
                    granule: SECTOR,
                    pick: self.rng.next(),
                },
                2 => Keep::Prefix {
                    granule: PAGE,
                    pick: self.rng.next(),
                },
                3 => Keep::All,
                4 => Keep::ZeroFilled,
                _ => Keep::FirstSectorLost,
            },
        }
    }
}

/// A [`Chooser`] that keeps every directory change, and of the files' data what `keep` says for
/// each file (relative to the base), everything when it says `None`.
pub struct KeepAllBut<F: FnMut(&Path) -> Option<Keep>>(pub F);

impl<F: FnMut(&Path) -> Option<Keep>> Chooser for KeepAllBut<F> {
    fn entries(&mut self, _dir: &Path, uncovered: usize) -> usize {
        uncovered
    }

    fn data(&mut self, file: &Path) -> Keep {
        (self.0)(file).unwrap_or(Keep::All)
    }
}

/// SplitMix64: a small, fast, seedable generator, enough to pick crash images.
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n`, `n > 0`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the disk holds after a power loss: directories and files, by path relative to the base.
#[derive(Debug, Clone, Default)]
pub struct Image {
    /// Parents before children.
    dirs: Vec<PathBuf>,
    files: Vec<(PathBuf, Vec<u8>)>,
    /// What the chooser took back, one line each.
    choices: Vec<String>,
}

impl Image {
    /// A digest of the directories and files: two images with the same one hold the same.
    pub fn digest(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.dirs.hash(&mut hasher);
        self.files.hash(&mut hasher);
        hasher.finish()
    }

    /// The content of file `path`, if the image holds it.
    pub fn file(&self, path: &Path) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(file, _)| file == path)
            .map(|(_, bytes)| bytes.as_slice())
    }

    /// The files under `dir` (relative to the base), with their contents.
    pub fn files_under<'a>(&'a self, dir: &'a Path) -> impl Iterator<Item = (&'a Path, &'a [u8])> {
        self.files
            .iter()
            .filter(move |(file, _)| file.starts_with(dir))
            .map(|(file, bytes)| (file.as_path(), bytes.as_slice()))
    }

    /// What the power loss took back, one line each: every directory change that no fsync
    /// covered and was lost, and every file whose unsynced bytes did not all stay.
    pub fn describe(&self) -> String {
        if self.choices.is_empty() {
            return "nothing was lost".to_owned();
        }
        let mut text = String::new();
        for choice in &self.choices {
            let _ = writeln!(text, "  {choice}");
        }
        text
    }

    /// Writes the image under `base`, which must be an empty directory.
    pub fn materialize(&self, base: &Path) -> io::Result<()> {
        for dir in &self.dirs {
            fs::create_dir(base.join(dir))?;
        }
        for (file, bytes) in &self.files {
            fs::write(base.join(file), bytes)?;
        }
        Ok(())
    }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("usize fits in u64")
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("a length in memory fits in usize")
}
