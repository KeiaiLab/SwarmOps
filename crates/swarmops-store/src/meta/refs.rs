//! Refs (storage spec §3): one file per ref, `meta/refs/<name>`, holding
//! `<64 hex digits of the target>\n<version>\n`. The files are a projection of the journal's
//! `RefMove` records: a writer replaces a file atomically (a temp file in its directory,
//! `sync_all`, rename, directory fsync, all in [`FsOps::atomic_replace`]) and removes it when
//! the ref is deleted, then fsyncs the directory.
//!
//! Ref files are read through [`FsOps::read`] too, the way the object store reads its files,
//! so that tests can step in between a reader's look at the journal and at the refs.
//!
//! A name is `main` or `release/<x>`, where `x` is one or more of `[A-Za-z0-9._-]` and does
//! not start with `.`, so that `.`, `..` and the store's own temp files (`.tmp…`) are never
//! refs; at most 255 bytes, what a `RefMove` record holds.
//!
//! A ref's version counts the writes of its file since the ref was created, from 1. A
//! transaction writes the file of each ref it moves once, with the target its last move
//! leaves, and only if that differs from the file's: so the version grows by one per
//! transaction that changes the target, however many moves the transaction made, and
//! applying a frame again to a file that already shows its target changes nothing. That is
//! what makes replay idempotent.

use std::collections::BTreeMap;
use std::io;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId};

use crate::error::StoreError;
use crate::fs_ops::FsOps;
use crate::layout::Layout;

/// The most bytes a ref name may take: its length is one byte of a `RefMove` record.
const MAX_NAME_LEN: usize = 255;

/// The most bytes a ref file holds: 64 hex digits, a line break, a version of at most 20
/// decimal digits (a `u64`), a line break.
const MAX_FILE_LEN: u64 = 64 + 1 + 20 + 1;

/// Where a ref points, as its file (or the journal past the checkpoint) says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefState {
    /// The changeset the ref names.
    pub target: ChangesetId,
    /// How many transactions have changed the target since the ref was created, from 1.
    pub version: u64,
}

/// One ref file, as [`crate::MetaStore::ref_files`] reads it.
#[derive(Debug)]
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
    let release = |x: &str| {
        !x.is_empty()
            && !x.starts_with('.')
            && x.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let valid = name == "main" || name.strip_prefix("release/").is_some_and(release);
    if valid && name.len() <= MAX_NAME_LEN {
        Ok(())
    } else {
        Err(StoreError::RefName(name.to_owned()))
    }
}

/// The state of ref `name` (a valid name) as its file says; `None` if it has no file.
pub(crate) fn read(
    layout: &Layout,
    fs: &dyn FsOps,
    name: &str,
) -> Result<Option<RefState>, StoreError> {
    read_file(fs, &path(layout, name), name)
}

/// Every ref that has a file, by name. Entries of `meta/refs/` that are not refs (temp
/// files, anything this version does not name) are skipped.
pub(crate) fn read_all(
    layout: &Layout,
    fs: &dyn FsOps,
) -> Result<BTreeMap<String, RefState>, StoreError> {
    let mut refs = BTreeMap::new();
    if let Some(state) = read(layout, fs, "main")? {
        refs.insert("main".to_owned(), state);
    }
    for name in release_names(layout)? {
        // A file removed since the listing is a ref deleted meanwhile.
        if let Some(state) = read(layout, fs, &name)? {
            refs.insert(name, state);
        }
    }
    Ok(refs)
}

/// Every ref that has a file, by name, each with its state or the error reading its file
/// gave: [`read_all`], except that a damaged file does not fail the rest. The names are the
/// ones [`read_all`] reads, and a file removed since the listing is skipped the same way.
pub(crate) fn read_each(layout: &Layout, fs: &dyn FsOps) -> Result<Vec<RefFile>, StoreError> {
    let mut names = vec!["main".to_owned()];
    names.extend(release_names(layout)?);
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

/// The names of the `release/<x>` refs that `meta/refs/release/` holds a file for, sorted.
/// Entries that are not refs (temp files, anything this version does not name) are skipped.
fn release_names(layout: &Layout) -> Result<Vec<String>, StoreError> {
    let entries = match std::fs::read_dir(layout.meta_path("refs/release")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
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
    fs: &dyn FsOps,
    name: &str,
    target: Option<ChangesetId>,
) -> Result<bool, StoreError> {
    let path = path(layout, name);
    let current = read_file(fs, &path, name)?;
    match moved(current, target) {
        // It says that already: see above for why that is enough (rule 5).
        next if next == current => return Ok(false),
        Some(state) => {
            let refs = layout.meta_path("refs");
            make_dir(fs, &refs, &layout.meta_dir())?;
            if let Some(dir) = path.parent()
                && dir != refs
            {
                make_dir(fs, dir, &refs)?;
            }
            let bytes = format!("{}\n{}\n", state.target, state.version);
            fs.atomic_replace(path.as_std_path(), bytes.as_bytes())?;
        }
        None => {
            fs.remove(path.as_std_path())?;
            let dir = path.parent().expect("a ref file lies in meta/refs/");
            fs.fsync_dir(dir.as_std_path())?;
        }
    }
    Ok(true)
}

/// The file of ref `name`.
fn path(layout: &Layout, name: &str) -> Utf8PathBuf {
    layout.meta_path(&format!("refs/{name}"))
}

/// Creates `dir` if it is missing, and makes its entry in `parent` durable.
fn make_dir(fs: &dyn FsOps, dir: &Utf8Path, parent: &Utf8Path) -> Result<(), StoreError> {
    if !dir.is_dir() {
        fs.create_dir_all(dir.as_std_path())?;
        fs.fsync_dir(parent.as_std_path())?;
    }
    Ok(())
}

/// The state the file of ref `name`, at `path`, holds; `None` if there is no file. Anything
/// but `<64 hex digits>\n<version>\n`, the version a decimal of at least 1, is
/// `InvalidData`, its message naming the file by its place in the store, `meta/refs/<name>`
/// (never the store's root, a host path). A file longer than [`MAX_FILE_LEN`] is refused by its
/// length, measured before it is read (through `std::fs`, no [`FsOps`] call), so that a crafted
/// file of huge length is never read into memory; a missing file reaches the read, which
/// reports it.
fn read_file(fs: &dyn FsOps, path: &Utf8Path, name: &str) -> Result<Option<RefState>, StoreError> {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > MAX_FILE_LEN
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "meta/refs/{name} is {} bytes long, not a ref file",
                meta.len()
            ),
        )
        .into());
    }
    let Some(bytes) = fs.read(path.as_std_path())? else {
        return Ok(None);
    };
    let parsed = std::str::from_utf8(&bytes).ok().and_then(|text| {
        let (hex, rest) = text.split_once('\n')?;
        let version = rest.strip_suffix('\n')?;
        if !version.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(RefState {
            target: ChangesetId(ObjectId::from_hex(hex).ok()?),
            version: version.parse().ok().filter(|&version| version > 0)?,
        })
    });
    parsed.map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("meta/refs/{name} is not a ref file: \"<64 hex digits>\\n<version>\\n\""),
        )
        .into()
    })
}
