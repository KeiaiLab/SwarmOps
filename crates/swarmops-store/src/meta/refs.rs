//! Refs (storage spec §3): one file per ref, `meta/refs/<name>`, holding
//! `<64 hex digits of the target>\n<version>\n`. The files are a projection of the journal's
//! `RefMove` records: a writer replaces a file atomically (a temp file in its directory,
//! `sync_all`, rename, directory fsync, all in [`FsOps::atomic_replace`]) and removes it when
//! the ref is deleted, then fsyncs the directory.
//!
//! A FORMAT 2 store (the M6 design §2.5) names a ref's file by its name's hash instead,
//! `meta/refs2/<32 hex digits of blake3(b"swarmops:refname:v1\0" ‖ name)>`, and the file holds
//! the name first, `<name>\n<64 hex digits>\n<version>\n`, so that names that clash as paths
//! (`a` and `a/b`) or on a volume that folds case (`Main` and `main`) are files of their own. A
//! read checks the name against the grammar and the file's name against its hash.
//!
//! Ref files are read through [`FsOps::read`] too, the way the object store reads its files,
//! so that tests can step in between a reader's look at the journal and at the refs.
//!
//! A FORMAT 1 name is `main` or `release/<x>`, where `x` is one or more of `[A-Za-z0-9._-]` and
//! does not start with `.`, so that `.`, `..` and the store's own temp files (`.tmp…`) are never
//! refs; at most 255 bytes, what a `RefMove` record holds. FORMAT 2 (the M6 design §2.5) also
//! takes free-form branches, tags and remote-tracking refs; [`ref_kind`] classifies a name by
//! that grammar, and [`check_ref_name_in`] checks a name against a store's format:
//!
//! ```text
//! seg     = [A-Za-z0-9._-]+, not starting with '.', not ending in '.' or ".lock", no ".."
//! path    = seg ("/" seg)*
//! remote  = [a-z0-9][a-z0-9._-]{0,62}, not ending in '.'
//! name    = "main" | "release/" x | "tags/" path | "remote/" remote "/" path
//!         | path, whose first seg is not main, release, tags or remote
//! ```
//!
//! A ref's version counts the writes of its file since the ref was created, from 1. A
//! transaction writes the file of each ref it moves once, with the target its last move
//! leaves, and only if that differs from the file's: so the version grows by one per
//! transaction that changes the target, however many moves the transaction made, and
//! applying a frame again to a file that already shows its target changes nothing. That is
//! what makes replay idempotent.
//!
//! [`FsOps::atomic_replace`]: crate::FsOps::atomic_replace
//! [`FsOps::read`]: crate::FsOps::read

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId};

use crate::error::{FileOp, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops::{FsMethod, StoreFs};
use crate::layout::Layout;

/// The most bytes a ref name may take: its length is one byte of a `RefMove` record.
const MAX_NAME_LEN: usize = 255;

/// The most bytes a ref file holds: 64 hex digits, a line break, a version of at most 20
/// decimal digits (a `u64`), a line break.
const MAX_FILE_LEN: u64 = 64 + 1 + 20 + 1;

/// The most bytes a FORMAT 2 ref file holds: the name (at most [`MAX_NAME_LEN`]) and a line
/// break, then what a FORMAT 1 file holds. 342.
const MAX_FILE_LEN_V2: u64 = MAX_NAME_LEN as u64 + 1 + MAX_FILE_LEN;

/// The most bytes a remote's name may take, as `remote add` takes them.
const MAX_REMOTE_LEN: usize = 63;

/// The suffix a segment of a FORMAT 2 name may not end in: git's lock files end in it.
const LOCK_SUFFIX: &str = ".lock";

/// What a ref name names, by the FORMAT 2 grammar ([`ref_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RefKind {
    /// `main`.
    Main,
    /// `release/<x>`.
    Release,
    /// A free-form branch: a path whose first segment is none of the reserved ones (FORMAT 2).
    Branch,
    /// `tags/<path>`: created and deleted, never moved (FORMAT 2).
    Tag,
    /// `remote/<remote>/<path>`: what a remote's ref names, as a fetch last saw it (FORMAT 2).
    RemoteTracking,
}

impl RefKind {
    /// Its name in the API: `main`, `release`, `branch`, `tag` or `remote_tracking`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Release => "release",
            Self::Branch => "branch",
            Self::Tag => "tag",
            Self::RemoteTracking => "remote_tracking",
        }
    }

    /// The first format version that takes names of this kind.
    pub fn needs(self) -> StoreFormat {
        match self {
            Self::Main | Self::Release => StoreFormat::V1,
            Self::Branch | Self::Tag | Self::RemoteTracking => StoreFormat::V2,
        }
    }
}

/// What `name` names by the FORMAT 2 grammar (see [the module documentation](self)); `None`
/// for a name no format takes. Examples: `main` is [`RefKind::Main`], `release/1.0`
/// [`RefKind::Release`], `feature/x` [`RefKind::Branch`], `tags/v1` [`RefKind::Tag`],
/// `remote/origin/main` [`RefKind::RemoteTracking`]; `tags` alone and `main/x` are `None`.
pub fn ref_kind(name: &str) -> Option<RefKind> {
    if name.len() > MAX_NAME_LEN {
        return None;
    }
    if name == "main" {
        return Some(RefKind::Main);
    }

    // 1. The reserved first segments, each with its own rule for the rest.
    if let Some(x) = name.strip_prefix("release/") {
        return is_release_name(x).then_some(RefKind::Release);
    }
    if let Some(path) = name.strip_prefix("tags/") {
        return is_path(path).then_some(RefKind::Tag);
    }
    if let Some(rest) = name.strip_prefix("remote/") {
        let (remote, path) = rest.split_once('/')?;
        let tracked = check_remote_segment(remote).is_ok() && is_path(path);
        return tracked.then_some(RefKind::RemoteTracking);
    }

    // 2. Any other path is a branch, unless it starts with a reserved segment on its own.
    let first = name.split('/').next().unwrap_or_default();
    let reserved = matches!(first, "main" | "release" | "tags" | "remote");
    (!reserved && is_path(name)).then_some(RefKind::Branch)
}

/// Refuses a remote's name that is not `[a-z0-9][a-z0-9._-]{0,62}` or ends in `.`, with
/// [`StoreError::RefName`]: the segment after `remote/` in a remote-tracking ref, and the
/// grammar of a remote's name in `remote add`, one rule for both.
pub fn check_remote_segment(remote: &str) -> Result<(), StoreError> {
    let starts = remote
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest = remote
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    if starts && rest && remote.len() <= MAX_REMOTE_LEN && !remote.ends_with('.') {
        Ok(())
    } else {
        Err(StoreError::RefName(remote.to_owned()))
    }
}

/// Refuses a name a store of `format` does not take. FORMAT 1 takes what [`check_ref_name`]
/// takes; a name only a later format takes is [`StoreError::FormatRequired`], one no format
/// takes [`StoreError::RefName`].
pub fn check_ref_name_in(format: StoreFormat, name: &str) -> Result<(), StoreError> {
    match format {
        StoreFormat::V1 => {
            if check_ref_name(name).is_ok() {
                return Ok(());
            }
            match ref_kind(name) {
                Some(kind) => Err(StoreError::FormatRequired {
                    name: name.to_owned(),
                    needs: kind.needs().version(),
                    store: format.version(),
                }),
                None => Err(StoreError::RefName(name.to_owned())),
            }
        }
        StoreFormat::V2 => match ref_kind(name) {
            Some(_) => Ok(()),
            None => Err(StoreError::RefName(name.to_owned())),
        },
    }
}

/// Refuses a move of ref `name` (a name `format` takes) from `old` to `new` that its kind
/// does not allow: a tag is created and deleted, never moved ([`StoreError::TagImmutable`]).
/// FORMAT 1 has no tags, so every move is allowed there.
pub fn check_move(
    format: StoreFormat,
    name: &str,
    old: Option<ChangesetId>,
    new: Option<ChangesetId>,
) -> Result<(), StoreError> {
    if format == StoreFormat::V1 {
        return Ok(());
    }

    // A tag's move from one target to another; creating and deleting it are moves too.
    let moves_tag = ref_kind(name) == Some(RefKind::Tag) && old.is_some() && new.is_some();
    if moves_tag {
        return Err(StoreError::TagImmutable(name.to_owned()));
    }
    Ok(())
}

/// The FORMAT 1 rule for `x` in `release/<x>`: one or more of `[A-Za-z0-9._-]`, not starting
/// with `.`.
fn is_release_name(x: &str) -> bool {
    !x.is_empty()
        && !x.starts_with('.')
        && x.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `path` is one or more segments joined by `/`, each one of [`is_segment`].
fn is_path(path: &str) -> bool {
    path.split('/').all(is_segment)
}

/// A segment: one or more of `[A-Za-z0-9._-]`, not starting with `.`, not ending in `.` or
/// `.lock`, not holding `..`.
fn is_segment(seg: &str) -> bool {
    is_release_name(seg)
        && !seg.ends_with('.')
        && !seg.ends_with(LOCK_SUFFIX)
        && !seg.contains("..")
}

/// Where a ref points, as its file (or the journal past the checkpoint) says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefState {
    /// The changeset the ref names.
    pub target: ChangesetId,
    /// How many transactions have changed the target since the ref was created, from 1.
    pub version: u64,
}

impl RefState {
    /// A ref naming `target` at `version`.
    ///
    /// Invariant: `version` is at least 1. Nothing checks it here, and nothing needs to: no
    /// store call takes a state, and a ref file whose version is 0 reads as `InvalidData`. A
    /// state built here is a value to compare with what the store returns.
    pub fn new(target: ChangesetId, version: u64) -> RefState {
        RefState { target, version }
    }
}

/// One ref file, as [`crate::MetaStore::ref_files`] reads it.
#[derive(Debug)]
#[non_exhaustive]
pub struct RefFile {
    /// The ref's name.
    pub name: String,
    /// The state the file holds, or the error reading it gave (`InvalidData` for a file that
    /// is not a ref file).
    pub state: Result<RefState, StoreError>,
}

/// Refuses, with [`StoreError::RefName`], a name that is not `main` or `release/<x>` (see [the
/// module documentation](self)), or that is longer than 255 bytes. Every ref operation checks
/// its name so first; a caller that must refuse a bad name before it writes anything else, as
/// a commit that advances a ref does before it puts its objects, calls it itself.
pub fn check_ref_name(name: &str) -> Result<(), StoreError> {
    let valid = name == "main" || name.strip_prefix("release/").is_some_and(is_release_name);
    if valid && name.len() <= MAX_NAME_LEN {
        Ok(())
    } else {
        Err(StoreError::RefName(name.to_owned()))
    }
}

/// The state of ref `name` (a valid name) as its file says; `None` if it has no file.
pub(crate) fn read(
    layout: &Layout,
    fs: &StoreFs,
    name: &str,
) -> Result<Option<RefState>, StoreError> {
    let file = read_file(layout.format(), fs, &path(layout, name))?;
    Ok(file.map(|(_, state)| state))
}

/// The ref files of a store, by name, with the format they were read under: what a reader's
/// overlay applies the frames past the checkpoint to, by that format's grammar.
#[derive(Debug, Clone)]
pub(crate) struct RefFiles {
    /// The store's format.
    pub(crate) format: StoreFormat,
    /// Every ref that has a file, by name.
    pub(crate) states: BTreeMap<String, RefState>,
}

/// Every ref that has a file, by name. Entries of `meta/refs/` that are not refs (temp
/// files, anything this version does not name) are skipped.
pub(crate) fn read_all(layout: &Layout, fs: &StoreFs) -> Result<RefFiles, StoreError> {
    let mut refs = BTreeMap::new();
    if layout.format() == StoreFormat::V2 {
        for file in read_each(layout, fs)? {
            refs.insert(file.name, file.state?);
        }
        return Ok(RefFiles {
            format: layout.format(),
            states: refs,
        });
    }
    if let Some(state) = read(layout, fs, "main")? {
        refs.insert("main".to_owned(), state);
    }
    for name in release_names(layout, fs)? {
        // A file removed since the listing is a ref deleted meanwhile.
        if let Some(state) = read(layout, fs, &name)? {
            refs.insert(name, state);
        }
    }
    Ok(RefFiles {
        format: layout.format(),
        states: refs,
    })
}

/// Every ref that has a file, by name, each with its state or the error reading its file
/// gave: [`read_all`], except that a damaged file does not fail the rest. The names are the
/// ones [`read_all`] reads, and a file removed since the listing is skipped the same way.
pub(crate) fn read_each(layout: &Layout, fs: &StoreFs) -> Result<Vec<RefFile>, StoreError> {
    if layout.format() == StoreFormat::V2 {
        return read_each_v2(layout, fs);
    }
    let mut names = vec!["main".to_owned()];
    names.extend(release_names(layout, fs)?);
    let mut refs = Vec::with_capacity(names.len());
    for name in names {
        let state = match read(layout, fs, &name) {
            Ok(Some(state)) => Ok(state),
            Ok(None) => continue,
            Err(e) => Err(e),
        };
        refs.push(RefFile { name, state });
    }
    Ok(refs)
}

/// [`read_each`] of a FORMAT 2 store: every file of `refs2/` named by 32 lowercase hex digits,
/// sorted by the name it holds. A file that does not read is listed under its file name, the
/// only name it has.
fn read_each_v2(layout: &Layout, fs: &StoreFs) -> Result<Vec<RefFile>, StoreError> {
    let dir = layout.refs_dir();
    let mut refs = Vec::new();
    for stem in file_stems(fs, &dir)? {
        // A file removed since the listing is a ref deleted meanwhile.
        let file = match read_file(StoreFormat::V2, fs, &dir.join(&stem)) {
            Ok(Some((name, state))) => RefFile {
                name: name.unwrap_or(stem),
                state: Ok(state),
            },
            Ok(None) => continue,
            Err(e) => RefFile {
                name: stem,
                state: Err(e),
            },
        };
        refs.push(file);
    }
    refs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(refs)
}

/// The entries of `dir` that name a FORMAT 2 ref file: 32 lowercase hex digits. Temp files and
/// anything else are skipped. A failure to list the directory is located by `fs`.
fn file_stems(fs: &StoreFs, dir: &Utf8Path) -> Result<Vec<String>, StoreError> {
    let listing_failed = |e| fs.locate(FileOp::ReadDir, dir, e);
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(listing_failed(e).into()),
    };
    let mut stems = Vec::new();
    for entry in entries {
        let entry = entry.map_err(listing_failed)?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue; // not UTF-8, so not a ref file
        };
        if is_stem(&file_name) {
            stems.push(file_name);
        }
    }
    Ok(stems)
}

/// Whether `file_name` is 32 lowercase hex digits, as [`file_stem`] gives.
fn is_stem(file_name: &str) -> bool {
    file_name.len() == STEM_LEN
        && (file_name.bytes()).all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// The names of the `release/<x>` refs that `meta/refs/release/` holds a file for, sorted.
/// Entries that are not refs (temp files, anything this version does not name) are skipped. A
/// failure to list the directory is located by `fs`.
fn release_names(layout: &Layout, fs: &StoreFs) -> Result<Vec<String>, StoreError> {
    let dir = layout.refs_dir().join("release");
    let listing_failed = |e| fs.locate(FileOp::ReadDir, &dir, e);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(listing_failed(e).into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(listing_failed)?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue; // not UTF-8, so not a ref
        };
        let name = format!("release/{file_name}");
        if check_ref_name(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// The state a ref in state `current` has once a transaction leaves it at `target`: gone
/// for `None`; unchanged if it names `target` already; otherwise `target` at the next
/// version (1 for a ref that did not exist).
pub(crate) fn moved(current: Option<RefState>, target: Option<ChangesetId>) -> Option<RefState> {
    let target = target?;
    match current {
        Some(state) if state.target == target => Some(state),
        _ => Some(RefState {
            target,
            version: current.map_or(0, |state| state.version).saturating_add(1),
        }),
    }
}

/// Makes the file of ref `name` (a valid name) say what [`moved`] gives for `target`:
/// replaces it, removes it, or leaves it alone if it says that already. Leaving it alone relies
/// on storage spec §2 rule 5: the file may say it because of a replace whose directory fsync
/// failed, and on the filesystems the store supports a failed directory fsync is fail-stop, so
/// every later sync fails too and nothing that relies on the file (a checkpoint past its frame)
/// is written after it. Returns whether it replaced or removed the file.
pub(crate) fn write(
    layout: &Layout,
    fs: &StoreFs,
    name: &str,
    target: Option<ChangesetId>,
) -> Result<bool, StoreError> {
    let path = path(layout, name);
    let current = read_file(layout.format(), fs, &path)?.map(|(_, state)| state);
    match moved(current, target) {
        // It says that already: see above for why that is enough (rule 5).
        next if next == current => return Ok(false),
        Some(state) => put(layout, fs, name, &path, state)?,
        None => remove(fs, &path)?,
    }
    Ok(true)
}

/// Makes the ref files hold `states` and nothing else, whatever they hold now: the file of each
/// ref `states` names written unless it reads as that state already (a damaged, missing or stale
/// one is), and every other ref file removed. Entries that are no ref files (temp files) are left
/// alone. What a rebuild of the ref files from the journal writes ([`crate::meta::fsck`]); the
/// caller holds [`crate::MetaLock`], and `states` holds valid names. Returns how many files it
/// wrote or removed.
pub(crate) fn rebuild(
    layout: &Layout,
    fs: &StoreFs,
    states: &BTreeMap<String, RefState>,
) -> Result<u64, StoreError> {
    let mut changed = 0;

    // 1. The refs `states` names, each file written unless it holds its state.
    let mut kept = BTreeSet::new();
    for (name, &state) in states {
        let path = path(layout, name);
        let holds = read_file(layout.format(), fs, &path).ok().flatten();
        if holds.map(|(_, held)| held) != Some(state) {
            put(layout, fs, name, &path, state)?;
            changed += 1;
        }
        kept.insert(path);
    }

    // 2. Every other ref file: a ref no move leaves, or a file under a name it does not hold.
    let listed: Vec<Utf8PathBuf> = match layout.format() {
        StoreFormat::V1 => {
            let mut names = vec!["main".to_owned()];
            names.extend(release_names(layout, fs)?);
            names.iter().map(|name| path(layout, name)).collect()
        }
        StoreFormat::V2 => {
            let dir = layout.refs_dir();
            let stems = file_stems(fs, &dir)?;
            stems.iter().map(|stem| dir.join(stem)).collect()
        }
    };
    for path in listed {
        if kept.contains(&path) || !std::fs::exists(&path).unwrap_or(true) {
            continue;
        }
        remove(fs, &path)?;
        changed += 1;
    }
    Ok(changed)
}

/// Replaces the file of ref `name` at `path` with one holding `state`, making its directories
/// first if they are missing.
fn put(
    layout: &Layout,
    fs: &StoreFs,
    name: &str,
    path: &Utf8Path,
    state: RefState,
) -> Result<(), StoreError> {
    let refs = layout.refs_dir();
    make_dir(fs, &refs, &layout.meta_dir())?;
    if let Some(dir) = path.parent()
        && dir != refs
    {
        make_dir(fs, dir, &refs)?;
    }
    // FORMAT 2 files hold their name: their own is its hash.
    let bytes = match layout.format() {
        StoreFormat::V1 => format!("{}\n{}\n", state.target, state.version),
        StoreFormat::V2 => format!("{name}\n{}\n{}\n", state.target, state.version),
    };
    fs.atomic_replace(path.as_std_path(), bytes.as_bytes())?;
    Ok(())
}

/// Removes the ref file at `path`, then makes its removal durable.
fn remove(fs: &StoreFs, path: &Utf8Path) -> Result<(), StoreError> {
    fs.remove(path.as_std_path())?;
    let dir = path.parent().expect("a ref file lies in meta/refs/");
    fs.fsync_dir(dir.as_std_path())?;
    Ok(())
}

/// The file of ref `name`: `refs/<name>`, or in FORMAT 2 `refs2/<file_stem(name)>`.
fn path(layout: &Layout, name: &str) -> Utf8PathBuf {
    match layout.format() {
        StoreFormat::V1 => layout.refs_dir().join(name),
        StoreFormat::V2 => layout.refs_dir().join(file_stem(name)),
    }
}

/// The domain the FORMAT 2 file name of a ref is hashed under (the M6 design §2.5): a disk
/// contract of FORMAT 2, not the object hash contract.
const NAME_HASH_DOMAIN: &[u8] = b"swarmops:refname:v1\0";

/// How many hex digits of the name's hash name a FORMAT 2 ref file: 16 bytes.
const STEM_LEN: usize = 32;

/// The FORMAT 2 file name of ref `name`: `hex(blake3(NAME_HASH_DOMAIN ‖ name)[0..16])`, 32
/// lowercase hex digits.
fn file_stem(name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(NAME_HASH_DOMAIN);
    hasher.update(name.as_bytes());
    let hex = hasher.finalize().to_hex();
    hex[..STEM_LEN].to_owned()
}

/// Creates `dir` if it is missing, and makes its entry in `parent` durable.
fn make_dir(fs: &StoreFs, dir: &Utf8Path, parent: &Utf8Path) -> Result<(), StoreError> {
    if !dir.is_dir() {
        fs.create_dir_all(dir.as_std_path())?;
        fs.fsync_dir(parent.as_std_path())?;
    }
    Ok(())
}

/// The state the ref file at `path` holds, with the name it holds (FORMAT 2's files hold one;
/// FORMAT 1's are named by it, so `None`); `None` if there is no file. Anything but
/// `<64 hex digits>\n<version>\n`, the version a decimal of at least 1, is `InvalidData`, a
/// failed read of the file, which `fs` names by its place in the store, `meta/refs/<name>`
/// (never by the store's root, a host path). A FORMAT 2 file holds `<name>\n` first, a name of
/// the FORMAT 2 grammar whose [`file_stem`] is the file's name, or it is `InvalidData` too. A
/// file longer than [`MAX_FILE_LEN`] ([`MAX_FILE_LEN_V2`]) is refused by its length, measured
/// before it is read (through `std::fs`, no [`crate::FsOps`] call), so that a crafted file of
/// huge length is never read into memory; a missing file reaches the read, which reports it.
fn read_file(
    format: StoreFormat,
    fs: &StoreFs,
    path: &Utf8Path,
) -> Result<Option<(Option<String>, RefState)>, StoreError> {
    let refused = |why: String| {
        let e = io::Error::new(io::ErrorKind::InvalidData, why);
        StoreError::from(fs.locate(FileOp::Fs(FsMethod::Read), path, e))
    };
    let (max_len, layout) = match format {
        StoreFormat::V1 => (MAX_FILE_LEN, r#""<64 hex digits>\n<version>\n""#),
        StoreFormat::V2 => (MAX_FILE_LEN_V2, r#""<name>\n<64 hex digits>\n<version>\n""#),
    };
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > max_len
    {
        return Err(refused(format!(
            "is {} bytes long, not a ref file",
            meta.len()
        )));
    }
    let Some(bytes) = fs.read(path.as_std_path())? else {
        return Ok(None);
    };
    let parsed = std::str::from_utf8(&bytes).ok().and_then(|text| {
        // 1. FORMAT 2: the name, which the file's own name must be the hash of.
        let (name, text) = match format {
            StoreFormat::V1 => (None, text),
            StoreFormat::V2 => {
                let (name, rest) = text.split_once('\n')?;
                let named = ref_kind(name).is_some() && path.file_name() == Some(&file_stem(name));
                (Some(named.then(|| name.to_owned())?), rest)
            }
        };

        // 2. The target and the version.
        let (hex, rest) = text.split_once('\n')?;
        let version = rest.strip_suffix('\n')?;
        if !version.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let state = RefState {
            target: ChangesetId::new(ObjectId::from_hex(hex).ok()?),
            version: version.parse().ok().filter(|&version| version > 0)?,
        };
        Some((name, state))
    });
    parsed
        .map(Some)
        .ok_or_else(|| refused(format!("not a ref file: {layout}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each reserved first segment has its own rule, any other path is a branch, and a name
    /// no rule takes is `None`.
    #[test]
    fn ref_kind_classifies_every_prefix() {
        let cases: [(&str, Option<RefKind>); 22] = [
            ("main", Some(RefKind::Main)),
            ("release/x", Some(RefKind::Release)),
            ("release/1.0.0", Some(RefKind::Release)),
            ("tags/x", Some(RefKind::Tag)),
            ("tags/v1/rc.1", Some(RefKind::Tag)),
            ("remote/r/x", Some(RefKind::RemoteTracking)),
            ("remote/origin/feature/a", Some(RefKind::RemoteTracking)),
            ("feature/x", Some(RefKind::Branch)),
            ("x", Some(RefKind::Branch)),
            ("mainline", Some(RefKind::Branch)),
            ("tags", None),
            ("release", None),
            ("remote/r", None),
            ("remote/Origin/x", None),
            ("main/x", None),
            ("release/a/b", None),
            ("release/.x", None),
            ("feature/x.lock", None),
            ("feature/.x", None),
            ("a..b", None),
            ("a//b", None),
            ("", None),
        ];
        for (name, kind) in cases {
            assert_eq!(ref_kind(name), kind, "{name:?}");
        }
        let long = format!("feature/{}", "x".repeat(MAX_NAME_LEN - "feature/".len()));
        assert_eq!(ref_kind(&long), Some(RefKind::Branch));
        assert_eq!(ref_kind(&format!("{long}x")), None);
    }

    /// FORMAT 1 takes its own names; a name only FORMAT 2 takes is `FormatRequired`, naming
    /// format 2 and the store's 1; a name neither takes is `RefName`.
    #[test]
    fn v1_checks_names_by_format() {
        for name in ["main", "release/x"] {
            check_ref_name_in(StoreFormat::V1, name).unwrap();
        }
        for name in ["feature/x", "tags/v1", "remote/origin/main"] {
            let Err(StoreError::FormatRequired {
                name: refused,
                needs,
                store,
            }) = check_ref_name_in(StoreFormat::V1, name)
            else {
                panic!("{name}: not FormatRequired");
            };
            assert_eq!((refused.as_str(), needs, store), (name, 2, 1));
        }
        for name in ["tags", "main/x", ".x"] {
            assert!(
                matches!(
                    check_ref_name_in(StoreFormat::V1, name),
                    Err(StoreError::RefName(_))
                ),
                "{name}"
            );
        }
        check_move(StoreFormat::V1, "main", None, None).unwrap();
    }

    /// A remote's name: lowercase letters and digits first, then `.`, `_` and `-` too, at
    /// most 63 bytes, not ending in `.`.
    #[test]
    fn remote_segments_follow_the_remote_grammar() {
        for good in ["origin", "up-stream", "a", "0.x_y", &"a".repeat(63)] {
            check_remote_segment(good).unwrap();
        }
        for bad in ["", "Origin", "-x", ".x", "x.", "a/b", &"a".repeat(64)] {
            assert!(check_remote_segment(bad).is_err(), "{bad:?}");
        }
    }

    /// A FORMAT 2 store in a temp dir: its layout and its `fs`, `meta/` made.
    fn v2_store() -> (tempfile::TempDir, Layout, StoreFs) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let layout = Layout::with_format(root, StoreFormat::V2);
        std::fs::create_dir_all(layout.meta_dir()).unwrap();
        let fs = StoreFs::new(std::sync::Arc::new(crate::RealFs), root);
        (dir, layout, fs)
    }

    /// A changeset id made of `byte`.
    fn cs(byte: u8) -> ChangesetId {
        ChangesetId::new(ObjectId::new([byte; 32]))
    }

    /// Whether `result` is the `InvalidData` failure of a file that is no ref file.
    fn is_invalid<T: std::fmt::Debug>(result: &Result<T, StoreError>) -> bool {
        matches!(result, Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::InvalidData)
    }

    /// A FORMAT 2 ref file is `refs2/<hex(H[0..16])>` holding the name, the target and the
    /// version; it moves, versions and goes away as a FORMAT 1 file does.
    #[test]
    fn v2_ref_files_round_trip() {
        let (_dir, layout, fs) = v2_store();
        assert!(write(&layout, &fs, "feature/x", Some(cs(1))).unwrap());

        // 1. The file: named by the name's hash, holding the name first.
        let file = layout.refs_dir().join(file_stem("feature/x"));
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, format!("feature/x\n{}\n1\n", cs(1)));
        assert_eq!(
            read(&layout, &fs, "feature/x").unwrap(),
            Some(RefState::new(cs(1), 1))
        );

        // 2. A move bumps the version, a repeat changes nothing, a delete removes the file.
        assert!(write(&layout, &fs, "feature/x", Some(cs(2))).unwrap());
        assert!(!write(&layout, &fs, "feature/x", Some(cs(2))).unwrap());
        let all = read_all(&layout, &fs).unwrap();
        assert_eq!(all.states.get("feature/x"), Some(&RefState::new(cs(2), 2)));
        assert!(write(&layout, &fs, "feature/x", None).unwrap());
        assert_eq!(read(&layout, &fs, "feature/x").unwrap(), None);
        assert!(!file.exists());
    }

    /// The file name recipe is FORMAT 2's disk contract: pinned.
    #[test]
    fn v2_file_names_are_the_name_hash() {
        assert_eq!(file_stem("main"), PINNED_MAIN_STEM);
        assert_eq!(file_stem("main").len(), 32);
        assert_ne!(file_stem("main"), file_stem("Main"));
    }

    /// `blake3(b"swarmops:refname:v1\0" ‖ b"main")[0..16]`, in lowercase hex.
    const PINNED_MAIN_STEM: &str = "72e8984deb3122ae48ce1eb128fae1d6";

    /// A file whose name is not the hash of the name it holds is no ref file: reading it is
    /// `InvalidData`, and the listing reports it under its file name.
    #[test]
    fn v2_misnamed_file_is_invalid_data() {
        let (_dir, layout, fs) = v2_store();
        write(&layout, &fs, "feature/x", Some(cs(1))).unwrap();
        let refs = layout.refs_dir();
        let stem = file_stem("feature/y");
        std::fs::rename(refs.join(file_stem("feature/x")), refs.join(&stem)).unwrap();

        assert!(is_invalid(&read(&layout, &fs, "feature/y")));
        assert!(is_invalid(&read_all(&layout, &fs)));
        let each = read_each(&layout, &fs).unwrap();
        assert_eq!(each.len(), 1);
        assert_eq!(each[0].name, stem);
        assert!(is_invalid(&each[0].state));

        // A file named by the hash of the name it holds, but a name no FORMAT 2 store takes.
        let bad = "main/x";
        let file = refs.join(file_stem(bad));
        std::fs::write(&file, format!("{bad}\n{}\n1\n", cs(1))).unwrap();
        let each = read_each(&layout, &fs).unwrap();
        assert!(each.iter().all(|file| is_invalid(&file.state)), "{each:?}");
    }

    /// The longest ref file, 342 bytes, reads; one byte more is refused by its length, even
    /// though it would parse (a version with a leading zero).
    #[test]
    fn v2_file_length_is_checked_before_reading() {
        let (_dir, layout, fs) = v2_store();
        let name = format!("feature/{}", "x".repeat(MAX_NAME_LEN - "feature/".len()));
        let file = layout.refs_dir().join(file_stem(&name));
        std::fs::create_dir_all(layout.refs_dir()).unwrap();

        let longest = format!("{name}\n{}\n{}\n", cs(1), u64::MAX);
        assert_eq!(longest.len(), 342);
        std::fs::write(&file, &longest).unwrap();
        assert_eq!(
            read(&layout, &fs, &name).unwrap(),
            Some(RefState::new(cs(1), u64::MAX))
        );

        std::fs::write(&file, format!("{name}\n{}\n0{}\n", cs(1), u64::MAX)).unwrap();
        let refused = read(&layout, &fs, &name);
        assert!(is_invalid(&refused), "{refused:?}");
        assert!(
            format!("{refused:?}").contains("343 bytes long"),
            "{refused:?}"
        );
    }

    /// Entries of `refs2/` that are not 32 lowercase hex digits (temp files, anything else)
    /// are passed over by the listings.
    #[test]
    fn v2_listing_skips_what_is_no_ref_file() {
        let (_dir, layout, fs) = v2_store();
        write(&layout, &fs, "main", Some(cs(1))).unwrap();
        let refs = layout.refs_dir();
        let upper = file_stem("feature/x").to_ascii_uppercase();
        let short = &file_stem("feature/y")[..31];
        let long = format!("{}0", file_stem("feature/z"));
        for junk in [".tmp1234", upper.as_str(), short, long.as_str(), "main"] {
            std::fs::write(refs.join(junk), "junk").unwrap();
        }

        let all = read_all(&layout, &fs).unwrap();
        assert_eq!(all.states.keys().collect::<Vec<_>>(), ["main"]);
        let each = read_each(&layout, &fs).unwrap();
        assert_eq!(each.len(), 1);
        assert_eq!(each[0].name, "main");
    }

    /// Names that would clash as paths (`a` and `a/b`) or on a volume that folds case (`Main`
    /// and `main`) are files of their own.
    #[test]
    fn v2_names_clashing_as_paths_coexist() {
        let (_dir, layout, fs) = v2_store();
        let names = ["a", "a/b", "Main", "main"];
        for (byte, name) in (1..).zip(names) {
            write(&layout, &fs, name, Some(cs(byte))).unwrap();
        }
        let all = read_all(&layout, &fs).unwrap();
        for (byte, name) in (1..).zip(names) {
            assert_eq!(
                all.states.get(name),
                Some(&RefState::new(cs(byte), 1)),
                "{name}"
            );
        }
        let listed: Vec<String> = read_each(&layout, &fs)
            .unwrap()
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert_eq!(listed, ["Main", "a", "a/b", "main"]);
    }
}
