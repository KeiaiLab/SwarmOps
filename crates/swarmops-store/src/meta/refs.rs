//! Refs (storage spec §3): one file per ref, `meta/refs/<name>`, holding
//! `<64 hex digits of the target>\n<version>\n`. The files are a projection of the journal's
//! `RefMove` records: a writer replaces a file atomically (a temp file in its directory,
//! `sync_all`, rename, directory fsync, all in [`FsOps::atomic_replace`]) and removes it when
//! the ref is deleted, then fsyncs the directory.
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

use std::collections::BTreeMap;
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
    read_file(fs, &path(layout, name))
}

/// Every ref that has a file, by name. Entries of `meta/refs/` that are not refs (temp
/// files, anything this version does not name) are skipped.
pub(crate) fn read_all(
    layout: &Layout,
    fs: &StoreFs,
) -> Result<BTreeMap<String, RefState>, StoreError> {
    let mut refs = BTreeMap::new();
    if let Some(state) = read(layout, fs, "main")? {
        refs.insert("main".to_owned(), state);
    }
    for name in release_names(layout, fs)? {
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
pub(crate) fn read_each(layout: &Layout, fs: &StoreFs) -> Result<Vec<RefFile>, StoreError> {
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
    let current = read_file(fs, &path)?;
    match moved(current, target) {
        // It says that already: see above for why that is enough (rule 5).
        next if next == current => return Ok(false),
        Some(state) => {
            let refs = layout.refs_dir();
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
    layout.refs_dir().join(name)
}

/// Creates `dir` if it is missing, and makes its entry in `parent` durable.
fn make_dir(fs: &StoreFs, dir: &Utf8Path, parent: &Utf8Path) -> Result<(), StoreError> {
    if !dir.is_dir() {
        fs.create_dir_all(dir.as_std_path())?;
        fs.fsync_dir(parent.as_std_path())?;
    }
    Ok(())
}

/// The state the ref file at `path` holds; `None` if there is no file. Anything but
/// `<64 hex digits>\n<version>\n`, the version a decimal of at least 1, is `InvalidData`, a
/// failed read of the file, which `fs` names by its place in the store, `meta/refs/<name>`
/// (never by the store's root, a host path). A file longer than [`MAX_FILE_LEN`] is refused by
/// its length, measured before it is read (through `std::fs`, no [`crate::FsOps`] call), so
/// that a crafted file of huge length is never read into memory; a missing file reaches the
/// read, which reports it.
fn read_file(fs: &StoreFs, path: &Utf8Path) -> Result<Option<RefState>, StoreError> {
    let refused = |why: String| {
        let e = io::Error::new(io::ErrorKind::InvalidData, why);
        StoreError::from(fs.locate(FileOp::Fs(FsMethod::Read), path, e))
    };
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > MAX_FILE_LEN
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
        let (hex, rest) = text.split_once('\n')?;
        let version = rest.strip_suffix('\n')?;
        if !version.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(RefState {
            target: ChangesetId::new(ObjectId::from_hex(hex).ok()?),
            version: version.parse().ok().filter(|&version| version > 0)?,
        })
    });
    parsed
        .map(Some)
        .ok_or_else(|| refused(r#"not a ref file: "<64 hex digits>\n<version>\n""#.to_owned()))
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
}
