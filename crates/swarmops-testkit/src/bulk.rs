//! The scale fixtures: stores of a million changesets and a million files, built once through
//! the write path and reused (the M21 design §2.10; the M21 plan's task M21-2).
//!
//! A [`Spec`] names a store: how many changesets and files, and whether its object files are
//! written raw or zstd. [`build`] makes it under [`measure::base_dir`] (`SWARMOPS_BENCH_DIR`) in
//! parts and reuses it when the store's `FORMAT` and a fingerprint of what the builder writes
//! match the ones it was built with; [`Builder::next_part`] adds one part, so a build cut short
//! goes on where it stopped. [`pair`] is A1's pair: [`Pair::small`] of 1,000 changesets and files,
//! [`Pair::large`] of 1,000,000 of each. The cases that use the large one are gated by
//! [`scale`] (`SWARMOPS_SCALE=1`) and print `scale-perf: ` lines.
//!
//! ```text
//!   changeset k adds file k (modifies it, from round 2 on); every N/10-th also touches hot.txt
//!
//!   root ── g00 ── d00 ── f000.txt .. f999.txt      (at most 1,000 entries a directory)
//!        │      └─ d01 ..
//!        ├─ g01 ..
//!        └─ hot.txt                                  (touched by 10 changesets)
//! ```

use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::{
    BlobId, ChangesetId, LeafKind, ObjectId, ObjectKind, RepoPath, Signature, Tree, TreeEntry,
    hash_object,
};
use swarmops_repo::{LocalRepo, RawChangeset};
use swarmops_store::{DEFAULT_INIT_FORMAT, FsOps, StoreFormat};

use crate::measure;
use crate::store::real_fs;

/// The variable that turns the scale cases on.
pub const SCALE_ENV: &str = "SWARMOPS_SCALE";

/// The most entries a directory of a fixture holds.
pub const MAX_ENTRIES: usize = 1_000;

/// The changesets of A1's small store, and its files.
pub const SMALL: usize = 1_000;

/// The changesets of A1's large store, and its files.
pub const LARGE: usize = 1_000_000;

/// The variable that sets the large store's size, for a trial of the cases on a store a tenth of
/// the size or less: the figures then name that size, not A1's. Unset, it is [`LARGE`].
pub const LARGE_ENV: &str = "SWARMOPS_SCALE_LARGE";

/// The changesets of the large store this run uses: [`LARGE`], or what [`LARGE_ENV`] says.
pub fn large() -> usize {
    match std::env::var(LARGE_ENV) {
        Ok(value) => value.parse().expect("SWARMOPS_SCALE_LARGE is a count"),
        Err(_) => LARGE,
    }
}

/// The changesets one part of a build records, in one journal frame.
pub const PART: usize = 2_048;

/// The times `hot.txt` is touched, however long the history: A9's path.
pub const HOT_TOUCHES: usize = 10;

/// The path that [`HOT_TOUCHES`] changesets touch.
pub const HOT: &str = "hot.txt";

/// The name of the builder's record of its progress, beside the store.
const STATE: &str = "bulk.state";

/// What the builder writes, versioned: a change to the shape of a fixture changes this, and every
/// fixture built before is rebuilt.
const GENERATOR: &str = "swarmops-bulk 1";

/// The sample ids a fixture keeps, spread over its history, for the cases that look changesets up.
const SAMPLES: usize = 1_000;

/// The ref a fixture's head is on.
pub const MAIN: &str = "main";

/// Whether the scale cases run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// `SWARMOPS_SCALE=1`: run.
    Run,
    /// Anything else: say `not run` and return.
    NotRun,
}

/// Whether `SWARMOPS_SCALE=1`.
pub fn scale() -> Scale {
    match std::env::var(SCALE_ENV) {
        Ok(value) if value == "1" => Scale::Run,
        _ => Scale::NotRun,
    }
}

/// [`scale`] for case `case`: on [`Scale::NotRun`] it prints the one line `scale-perf: <case> not
/// run` that the case then ends on, and touches no fixture.
pub fn gate(case: &str) -> Scale {
    let scale = scale();
    if scale == Scale::NotRun {
        println!("scale-perf: {case} not run");
    }
    scale
}

/// Prints one figure line of case `case`: `scale-perf: <case>: <text>`.
pub fn say(case: &str, text: &str) {
    println!("scale-perf: {case}: {text}");
}

/// How the object files of a fixture are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Objects {
    /// Raw: no zstd, so that millions of trees are built fast (the store's `test-hooks` seam; a
    /// build without it writes zstd all the same). Ids and reads are the same.
    Raw,
    /// As the store writes them, zstd where it shrinks the payload.
    Zstd,
}

/// What a fixture holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// How many changesets, in one chain.
    pub changesets: usize,
    /// How many files the tree ends with: changeset `k` puts file `k % files`.
    pub files: usize,
    /// How the object files are written.
    pub objects: Objects,
}

impl Spec {
    /// A fixture of `changesets` and `files`, its objects raw.
    pub fn new(changesets: usize, files: usize) -> Spec {
        Spec {
            changesets,
            files,
            objects: Objects::Raw,
        }
    }

    /// This spec with its object files written as `objects`.
    #[must_use]
    pub fn with_objects(self, objects: Objects) -> Spec {
        Spec { objects, ..self }
    }

    /// The directory name of this spec under the bench directory.
    fn dir_name(&self) -> String {
        let objects = match self.objects {
            Objects::Raw => "raw",
            Objects::Zstd => "zstd",
        };
        format!("bulk-{}x{}-{objects}", self.changesets, self.files)
    }

    /// What a fixture of this spec is built from: the generator, the store's format and the
    /// shape. A fixture is reused when its stored one is equal.
    fn fingerprint(&self, format: StoreFormat) -> String {
        let text = format!(
            "{GENERATOR}\nformat {}\nchangesets {}\nfiles {}\nmax-entries {MAX_ENTRIES}\n\
             hot {HOT_TOUCHES}\nobjects {:?}\n",
            format.version(),
            self.changesets,
            self.files,
            self.objects,
        );
        hash_object(ObjectKind::Blob, text.as_bytes()).to_hex()
    }
}

/// A built fixture.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// The store's path.
    pub store: Utf8PathBuf,
    /// What it holds.
    pub spec: Spec,
    /// The last changeset, on `main`.
    pub head: ChangesetId,
    /// Changesets spread over its history, each with its index (the first is 0): 1,000 at
    /// most, for the lookups.
    pub samples: Vec<(usize, ChangesetId)>,
    /// Whether this call built anything: [`Built::Now`], or [`Built::Before`] for one found
    /// whole.
    pub built: Built,
}

/// Whether a call of [`build`] made the fixture or found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Built {
    /// This call wrote at least one part.
    Now,
    /// Found whole, with a matching fingerprint: nothing was written.
    Before,
}

impl Fixture {
    /// Opens the store for reading (and whatever else the test does to it).
    pub fn open(&self) -> LocalRepo {
        LocalRepo::open(&self.store).expect("open the fixture's store")
    }

    /// The path of file `k` of the tree.
    pub fn file_path(k: usize) -> String {
        file_path(k)
    }
}

/// A's pair: a small and a large store.
#[derive(Debug, Clone)]
pub struct Pair {
    /// 1,000 changesets, 1,000 files.
    pub small: Fixture,
    /// 1,000,000 changesets, 1,000,000 files.
    pub large: Fixture,
}

/// A1's pair, built or found under `SWARMOPS_BENCH_DIR`; the large one takes hours the first time
/// (`fixture_next_part` goes on a part at a time).
pub fn pair() -> Pair {
    Pair {
        small: build(Spec::new(SMALL, SMALL)),
        large: build(Spec::new(large(), large())),
    }
}

/// `spec`, built or found under `SWARMOPS_BENCH_DIR`, to the end.
pub fn build(spec: Spec) -> Fixture {
    build_in(&measure::base_dir(), spec, real_fs())
}

/// [`build`] under `base`, every filesystem change of the stores going through `fs`.
pub fn build_in(base: &Path, spec: Spec, fs: Arc<dyn FsOps>) -> Fixture {
    let mut builder = Builder::open(base, spec, fs);
    while builder.next_part() == Part::Written {}
    builder.finish()
}

/// What one call of [`Builder::next_part`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// It recorded one more part.
    Written,
    /// The fixture is whole; nothing was written.
    Complete,
}

/// The state of a fixture's tree while it is built: every directory's entries.
struct TreeState {
    /// The files' directories, one per [`MAX_ENTRIES`] files.
    leaves: Vec<Vec<TreeEntry>>,
    /// The groups' entries: one per [`MAX_ENTRIES`] leaves.
    groups: Vec<Vec<TreeEntry>>,
    /// The hot file's blob.
    hot: Option<BlobId>,
}

/// What a part of a build gathers before it writes anything.
#[derive(Default)]
struct Gathered {
    /// The blobs and trees to put.
    objects: Vec<(ObjectKind, Vec<u8>)>,
    /// The changesets, in order, each with its root.
    raws: Vec<RawChangeset>,
}

/// A fixture in the making: a part at a time.
pub struct Builder {
    spec: Spec,
    dir: PathBuf,
    fingerprint: String,
    format: StoreFormat,
    fs: Arc<dyn FsOps>,
    /// How many changesets are recorded.
    done: usize,
    head: Option<ChangesetId>,
    samples: Vec<(usize, ChangesetId)>,
    repo: Option<LocalRepo>,
    tree: TreeState,
    /// Whether any part was written by this builder.
    wrote: bool,
    /// The changesets of a part.
    part: usize,
    /// Whether `main` is on the head: the record says so once it is.
    main_set: bool,
    /// The lock on the fixture's directory name, held while this builder lives: two cases that
    /// need one fixture, in two processes, build it one after the other, not at once.
    _lock: fs::File,
}

impl Builder {
    /// The builder of `spec` under `base`: it reads what the fixture there holds. A fixture whose
    /// fingerprint differs from what this builder writes is removed and made again. Touches
    /// nothing when the fixture is whole.
    pub fn open(base: &Path, spec: Spec, fs: Arc<dyn FsOps>) -> Builder {
        assert!(
            spec.files > 0 && spec.changesets >= spec.files,
            "a fixture has files, and a changeset for each: {spec:?}"
        );
        let format = DEFAULT_INIT_FORMAT;
        let dir = base.join(spec.dir_name());
        let lock_path = base.join(format!("{}.lock", spec.dir_name()));
        let lock = fs::File::create(&lock_path).expect("create the fixture's lock file");
        lock.lock().expect("lock the fixture");
        let fingerprint = spec.fingerprint(format);
        let mut builder = Builder {
            spec,
            dir,
            fingerprint,
            format,
            fs,
            done: 0,
            head: None,
            samples: Vec::new(),
            repo: None,
            tree: TreeState::new(spec.files),
            wrote: false,
            part: PART,
            main_set: false,
            _lock: lock,
        };
        builder.resume();
        builder
    }

    /// Reads the progress record, if it is this builder's, and rebuilds the tree's state from it;
    /// else clears what is there.
    fn resume(&mut self) {
        let Some(state) = read_state(&self.dir) else {
            self.clear();
            return;
        };
        if state.fingerprint != self.fingerprint {
            self.clear();
            return;
        }
        self.done = state.done;
        self.head = state.head;
        self.samples = state.samples;
        self.main_set = state.main_set;
        for k in 0..self.done {
            self.tree.apply(&self.spec, k);
        }
        self.tree.rebuild_groups();
    }

    /// Removes the fixture's directory.
    fn clear(&mut self) {
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove {}: {e}", self.dir.display()),
        }
    }

    /// Parts of `changesets` changesets from now on: a test's small parts.
    #[must_use]
    pub fn with_part(mut self, changesets: usize) -> Builder {
        self.part = changesets.max(1);
        self
    }

    /// How many changesets are recorded.
    pub fn done(&self) -> usize {
        self.done
    }

    /// The store's path.
    fn store_path(&self) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(self.dir.join("store")).expect("a UTF-8 bench path")
    }

    /// The store, created on first use and opened after.
    fn repo(&mut self) -> &LocalRepo {
        if self.repo.is_none() {
            let store = self.store_path();
            fs::create_dir_all(&self.dir).expect("create the fixture directory");
            let exists = store.join("FORMAT").exists();
            let seam = self.spec.objects == Objects::Raw;
            raw_seam(seam);
            let repo = if exists {
                LocalRepo::open_with_fs(&store, Arc::clone(&self.fs))
            } else {
                LocalRepo::create_with_fs_as(&store, "bulk", self.format, Arc::clone(&self.fs))
            };
            self.repo = Some(repo.expect("open the fixture's store"));
            raw_seam(false);
        }
        self.repo.as_ref().expect("opened above")
    }

    /// Records the next [`PART`] changesets (fewer at the end), then the progress record.
    pub fn next_part(&mut self) -> Part {
        if self.done >= self.spec.changesets {
            return Part::Complete;
        }
        let from = self.done;
        let to = (from + self.part).min(self.spec.changesets);

        // 1. The part's objects and changesets, made in memory from the tree's state.
        let mut gathered = Gathered::default();
        for k in from..to {
            let raw = self.tree.step(&self.spec, k, &mut gathered.objects);
            gathered.raws.push(raw);
        }

        // 2. Objects first (the raw batch names the roots), then the changesets in one frame,
        //    each on the one before it.
        let mut parent = self.head;
        let repo = self.repo();
        repo.put_objects(gathered.objects).expect("put the objects");
        let mut batch = repo.raw_batch();
        let mut made = Vec::new();
        for mut raw in gathered.raws {
            raw.parents = parent.into_iter().collect();
            let id = batch.push(raw).expect("push a changeset");
            made.push(id);
            parent = Some(id);
        }
        batch.commit().expect("record the part");

        // 3. What the next part and a later run need.
        for (offset, id) in made.iter().enumerate() {
            let k = from + offset;
            if k.is_multiple_of(self.sample_every()) {
                self.samples.push((k, *id));
            }
        }
        self.head = parent;
        self.done = to;
        self.wrote = true;
        self.write_state();
        Part::Written
    }

    /// Every how many changesets one is kept as a sample.
    fn sample_every(&self) -> usize {
        (self.spec.changesets / SAMPLES).max(1)
    }

    /// Writes the progress record, replacing the last one whole.
    fn write_state(&self) {
        let mut text = format!("fingerprint {}\ndone {}\n", self.fingerprint, self.done);
        if let Some(head) = self.head {
            let _ = writeln!(text, "head {}", head.0.to_hex());
        }
        if self.main_set {
            text.push_str("main 1\n");
        }
        for (k, id) in &self.samples {
            let _ = writeln!(text, "sample {k} {}", id.0.to_hex());
        }
        let temp = self.dir.join(format!("{STATE}.new"));
        let mut file = fs::File::create(&temp).expect("create the progress record");
        file.write_all(text.as_bytes()).expect("write it");
        file.sync_all().expect("sync it");
        fs::rename(&temp, self.dir.join(STATE)).expect("replace the record");
    }

    /// Puts `main` on the head and flushes the metadata, once the fixture is whole; the fixture.
    ///
    /// # Panics
    ///
    /// If the fixture is not whole ([`Part::Complete`] not yet returned).
    pub fn finish(mut self) -> Fixture {
        assert!(
            self.done == self.spec.changesets,
            "finish: {} of {} changesets are recorded",
            self.done,
            self.spec.changesets
        );
        let head = self.head.expect("a whole fixture has a head");
        if !self.main_set {
            let repo = self.repo();
            repo.cas_ref(MAIN, None, Some(head), "bulk")
                .expect("move main to the head");
            repo.flush_metadata().expect("flush the metadata");
            self.main_set = true;
            self.write_state();
        }
        Fixture {
            store: self.store_path(),
            spec: self.spec,
            head,
            samples: self.samples.clone(),
            built: if self.wrote {
                Built::Now
            } else {
                Built::Before
            },
        }
    }
}

/// Switches the store's raw-object seam, where the build has it.
fn raw_seam(on: bool) {
    #[cfg(feature = "test-hooks")]
    swarmops_store::raw_writes_by_default(on);
    #[cfg(not(feature = "test-hooks"))]
    let _ = on;
}

/// The progress record, read back.
struct State {
    main_set: bool,
    fingerprint: String,
    done: usize,
    head: Option<ChangesetId>,
    samples: Vec<(usize, ChangesetId)>,
}

/// The progress record of the fixture in `dir`, or `None` when there is none or it is torn.
fn read_state(dir: &Path) -> Option<State> {
    let text = fs::read_to_string(dir.join(STATE)).ok()?;
    let mut state = State {
        main_set: false,
        fingerprint: String::new(),
        done: 0,
        head: None,
        samples: Vec::new(),
    };
    for line in text.lines() {
        let mut words = line.split(' ');
        match words.next()? {
            "fingerprint" => words.next()?.clone_into(&mut state.fingerprint),
            "main" => state.main_set = true,
            "done" => state.done = words.next()?.parse().ok()?,
            "head" => state.head = Some(changeset(words.next()?)?),
            "sample" => {
                let k = words.next()?.parse().ok()?;
                state.samples.push((k, changeset(words.next()?)?));
            }
            _ => return None,
        }
    }
    Some(state)
}

/// The changeset id spelt `hex`.
fn changeset(hex: &str) -> Option<ChangesetId> {
    Some(ChangesetId::new(ObjectId::from_hex(hex).ok()?))
}

/// File `k`: `g<g>/d<d>/f<f>.txt`, at most [`MAX_ENTRIES`] to a directory.
fn file_path(k: usize) -> String {
    let (group, leaf, file) = place(k);
    format!("g{group:03}/d{leaf:03}/f{file:03}.txt")
}

/// File `k`'s group, directory in the group, and place in the directory.
fn place(k: usize) -> (usize, usize, usize) {
    let leaf = k / MAX_ENTRIES;
    (leaf / MAX_ENTRIES, leaf % MAX_ENTRIES, k % MAX_ENTRIES)
}

/// File `k`'s text in round `round`: lines of prose, so that zstd has something to shrink, and
/// every file's bytes differ.
fn content(k: usize, round: usize) -> Vec<u8> {
    let mut text = String::new();
    for line in 0..8 {
        let _ = writeln!(
            text,
            "file {k} round {round} line {line}: the quick brown fox jumps over the lazy dog"
        );
    }
    text.into_bytes()
}

/// The text of file `k` as its first changeset puts it: what a read at a revision must return.
pub fn file_text(k: usize) -> Vec<u8> {
    content(k, 0)
}

/// The text of `hot.txt` as changeset `k` leaves it.
fn hot_content(k: usize) -> Vec<u8> {
    format!("hot as of changeset {k}\n").into_bytes()
}

/// Whether changeset `k` touches [`HOT`]: every `changesets / HOT_TOUCHES`-th, from the first.
fn touches_hot(spec: &Spec, k: usize) -> bool {
    let every = (spec.changesets / HOT_TOUCHES).max(1);
    k.is_multiple_of(every) && k / every < HOT_TOUCHES
}

/// A fixed signature for changeset `k`: the same one each build.
fn signature(k: usize) -> Signature {
    let secs = 1_760_000_000 + i64::try_from(k).expect("a changeset index fits");
    Signature::new(
        b"bulk/agent".to_vec(),
        b"agent@bulk.swarmops.local".to_vec(),
        secs,
        0,
        false,
    )
    .expect("a zero offset is valid")
}

/// The blob id of `payload`.
fn blob_id(payload: &[u8]) -> BlobId {
    BlobId::new(hash_object(ObjectKind::Blob, payload))
}

impl TreeState {
    fn new(files: usize) -> TreeState {
        let leaves = files.div_ceil(MAX_ENTRIES);
        let groups = leaves.div_ceil(MAX_ENTRIES);
        TreeState {
            leaves: vec![Vec::new(); leaves],
            groups: vec![Vec::new(); groups],
            hot: None,
        }
    }

    /// The groups' entries from the directories', after a replay that made none.
    fn rebuild_groups(&mut self) {
        for (index, entries) in self.leaves.iter().enumerate() {
            if entries.is_empty() {
                continue;
            }
            let id = Tree::new(entries.clone()).expect("a valid directory").id();
            let name = format!("d{:03}", index % MAX_ENTRIES);
            self.groups[index / MAX_ENTRIES].push(TreeEntry::dir(name, id));
        }
    }

    /// Puts changeset `k`'s change into the entries, without making any object: the replay of a
    /// resumed build, and the first half of [`TreeState::step`]. Returns the change's blob payload.
    fn apply(&mut self, spec: &Spec, k: usize) -> Vec<u8> {
        let file = k % spec.files;
        let round = k / spec.files;
        let payload = content(file, round);
        let (group, leaf_in_group, place_in_leaf) = place(file);
        let leaf = group * MAX_ENTRIES + leaf_in_group;
        let name = format!("f{place_in_leaf:03}.txt");
        let entry = TreeEntry::leaf(name, LeafKind::File, blob_id(&payload));
        let entries = &mut self.leaves[leaf];
        match entries.get_mut(place_in_leaf) {
            Some(slot) => *slot = entry,
            None => entries.push(entry),
        }
        if touches_hot(spec, k) {
            self.hot = Some(blob_id(&hot_content(k)));
        }
        payload
    }

    /// Makes changeset `k`: applies its change, puts the blobs and the trees that changed into
    /// `objects`, and returns the changeset with no parent: the caller names it, once the one
    /// before has an id.
    fn step(
        &mut self,
        spec: &Spec,
        k: usize,
        objects: &mut Vec<(ObjectKind, Vec<u8>)>,
    ) -> RawChangeset {
        let payload = self.apply(spec, k);
        objects.push((ObjectKind::Blob, payload));
        if touches_hot(spec, k) {
            objects.push((ObjectKind::Blob, hot_content(k)));
        }

        // The changed directory, its group, and the root, bottom up.
        let file = k % spec.files;
        let (group, leaf_in_group, _) = place(file);
        let leaf = group * MAX_ENTRIES + leaf_in_group;
        let leaf_tree = Tree::new(self.leaves[leaf].clone()).expect("a valid directory");
        let leaf_id = leaf_tree.id();
        objects.push((ObjectKind::Tree, leaf_tree.encode()));

        let name = format!("d{leaf_in_group:03}");
        let entry = TreeEntry::dir(name, leaf_id);
        let entries = &mut self.groups[group];
        match entries.get_mut(leaf_in_group) {
            Some(slot) => *slot = entry,
            None => entries.push(entry),
        }
        let group_tree = Tree::new(self.groups[group].clone()).expect("a valid group");
        let group_id = group_tree.id();
        objects.push((ObjectKind::Tree, group_tree.encode()));

        let mut root = Vec::new();
        for (index, entries) in self.groups.iter().enumerate() {
            if entries.is_empty() {
                continue;
            }
            let id = if index == group {
                group_id
            } else {
                Tree::new(entries.clone()).expect("a valid group").id()
            };
            root.push(TreeEntry::dir(format!("g{index:03}"), id));
        }
        if let Some(hot) = self.hot {
            root.push(TreeEntry::leaf(HOT, LeafKind::File, hot));
        }
        root.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let root_tree = Tree::new(root).expect("a valid root");
        let root_id = root_tree.id();
        objects.push((ObjectKind::Tree, root_tree.encode()));

        RawChangeset::new(
            Vec::new(),
            root_id,
            signature(k),
            signature(k),
            format!("bulk {k}").into_bytes(),
            Vec::new(),
            Vec::new(),
        )
    }
}

/// The repo path of file `k`.
pub fn repo_path(k: usize) -> RepoPath {
    RepoPath::new(file_path(k)).expect("a valid path")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::RecordingFs;

    /// The changesets on the first-parent chain from `fixture`'s head.
    fn chain_len(fixture: &Fixture) -> usize {
        let repo = fixture.open();
        let read = repo.snapshot().expect("snapshot");
        read.first_parents(fixture.head).count()
    }

    #[test]
    fn bulk_builds_the_small_pair_and_reuses_it() {
        let base = tempfile::tempdir().unwrap();
        let spec = Spec::new(SMALL, SMALL);

        // 1. The first call builds: a chain of 1,000, `main` on its head.
        let first = build_in(base.path(), spec, real_fs());
        assert_eq!(first.built, Built::Now);
        assert_eq!(chain_len(&first), SMALL);
        let repo = first.open();
        assert_eq!(repo.resolve(MAIN).unwrap(), first.head);
        let (_, sample) = first.samples[1];
        assert!(repo.changeset_meta(sample).unwrap().is_some());

        // 2. The second finds it whole and writes nothing: no FsOps call at all.
        let (fs, calls) = RecordingFs::new();
        let second = build_in(base.path(), spec, Arc::new(fs));
        assert_eq!(second.built, Built::Before);
        assert_eq!(second.head, first.head);
        assert_eq!(
            calls(),
            Vec::<String>::new(),
            "a reused fixture makes no call"
        );
    }

    #[test]
    fn a_changed_fingerprint_rebuilds() {
        let base = tempfile::tempdir().unwrap();
        let spec = Spec::new(200, 200);
        let first = build_in(base.path(), spec, real_fs());

        // A fixture built by another generator, or of another FORMAT, carries another
        // fingerprint.
        let state = base.path().join(spec.dir_name()).join(STATE);
        let text = fs::read_to_string(&state).unwrap();
        let old = format!("fingerprint {}", spec.fingerprint(DEFAULT_INIT_FORMAT));
        let other = text.replace(&old, "fingerprint 0000");
        assert_ne!(other, text, "the record names its fingerprint");
        fs::write(&state, other).unwrap();

        let again = build_in(base.path(), spec, real_fs());
        assert_eq!(again.built, Built::Now, "rebuilt, not reused");
        assert_eq!(again.head, first.head, "the same fixture again");
        assert_eq!(chain_len(&again), 200);
    }

    #[test]
    fn a_build_goes_on_where_it_stopped() {
        let base = tempfile::tempdir().unwrap();
        let spec = Spec::new(300, 300);

        // One part, then the builder is dropped as a cut run would be.
        let mut builder = Builder::open(base.path(), spec, real_fs()).with_part(100);
        assert_eq!(builder.next_part(), Part::Written);
        assert_eq!(builder.done(), 100);
        drop(builder);

        let resumed = build_in(base.path(), spec, real_fs());
        let other = tempfile::tempdir().unwrap();
        let whole = build_in(other.path(), spec, real_fs());
        assert_eq!(resumed.built, Built::Now);
        assert_eq!(
            resumed.head, whole.head,
            "the resumed build is the whole one"
        );
        assert_eq!(chain_len(&resumed), 300);
    }

    #[test]
    fn main_is_set_for_a_store_whose_parts_were_all_written_before() {
        let base = tempfile::tempdir().unwrap();
        let spec = Spec::new(200, 200);

        // The last part is written, the builder never finishes (`fixture_next_part` ends so).
        let mut builder = Builder::open(base.path(), spec, real_fs());
        while builder.next_part() == Part::Written {}
        drop(builder);

        // A later build finds every part there, and still puts `main` on the head.
        let fixture = build_in(base.path(), spec, real_fs());
        let repo = fixture.open();
        assert_eq!(repo.resolve(MAIN).unwrap(), fixture.head);

        // Once: the next call writes nothing.
        let (fs, calls) = RecordingFs::new();
        build_in(base.path(), spec, Arc::new(fs));
        assert_eq!(calls(), Vec::<String>::new());
    }

    #[test]
    fn a_directory_holds_at_most_1000_entries() {
        let base = tempfile::tempdir().unwrap();
        let fixture = build_in(base.path(), Spec::new(SMALL, SMALL), real_fs());
        let repo = fixture.open();
        let read = repo.snapshot().unwrap();
        let root = read
            .tree(
                read.changeset(fixture.head)
                    .unwrap()
                    .unwrap()
                    .changeset
                    .root,
            )
            .unwrap();
        assert!(root.entries().len() <= MAX_ENTRIES);
        let (_, _, content) = read
            .read_blob_at(fixture.head, &repo_path(7))
            .unwrap()
            .expect("file 7 is there");
        assert_eq!(content, super::content(7, 0));
    }
}
