//! What gc removes (the M6 design §8.2): leftovers, and nothing else. Temp files past their
//! grace, the staging of a crashed create, derived values of superseded versions and what an
//! upgrade leaves of the format before. No object, changeset, journal, index, view or ref
//! file of the store's format, external id, mirror, workspace or quarantined file is ever one.

use std::fs::{self, Metadata};
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::bytes;

use crate::error::{FileOp, StoreError};
use crate::format::{self, StoreFormat, UPGRADE_POISON};
use crate::fs_ops::{FsMethod, RealFs, StoreFs};
use crate::layout::Layout;

/// The directory of the store root in which a create stages `meta/` (storage spec §3).
const STAGING: &str = "meta.tmp";

/// How old a temp file must be before gc removes it: one a write in progress still holds is
/// younger.
pub const GC_TEMP_GRACE: Duration = Duration::from_hours(24);

/// What kind of leftover a [`Leftover`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum LeftoverKind {
    /// A temp file in `objects/tmp/` older than [`GC_TEMP_GRACE`].
    ObjectTemp,
    /// A temp file in `meta/derived/tmp/` older than [`GC_TEMP_GRACE`].
    DerivedTemp,
    /// `meta.tmp/`, a crashed create's staging, where `meta/` exists.
    MetaStaging,
    /// `meta/derived/<name>/<version>/` of a derivation this build registers, below its
    /// version.
    DerivedVersion,
    /// A file of the format before an upgrade (`meta/journal.*`, `meta/refs/` in a FORMAT 2
    /// store), or of an upgrade that stopped before its commit point.
    UpgradeResidue,
}

impl LeftoverKind {
    /// Its name in the API (`GcCount.kind`): `object_temp`, `derived_temp`, `meta_staging`,
    /// `derived_version` or `upgrade_residue`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ObjectTemp => "object_temp",
            Self::DerivedTemp => "derived_temp",
            Self::MetaStaging => "meta_staging",
            Self::DerivedVersion => "derived_version",
            Self::UpgradeResidue => "upgrade_residue",
        }
    }
}

/// One leftover gc removes: its kind, where it is relative to the store's root, and how many
/// bytes it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Leftover {
    /// What it is.
    pub kind: LeftoverKind,
    /// Its path relative to the store's root (never a host path).
    pub rel: Utf8PathBuf,
    /// The bytes of its files.
    pub bytes: u64,
}

/// Every leftover of the store of `layout`, as of `now`, sorted by kind, then path, except that
/// a poisoned FORMAT 1 checkpoint comes last of all: the rest of an upgrade's residue is removed
/// before it, so that a gc cut short still finds the residue the next time. No derivation is
/// registered: a derived version is never one ([`list_leftovers_of`] names the derivations).
pub fn list_leftovers(layout: &Layout, now: SystemTime) -> Result<Vec<Leftover>, StoreError> {
    list_leftovers_of(layout, now, &[])
}

/// [`list_leftovers`], with the derivations this build registers, each by its name and its
/// version: a version directory below it is a leftover; one of a name not among them is kept.
/// Lists with `std::fs` and reads only `FORMAT` and FORMAT 1's checkpoint, each measured first.
pub fn list_leftovers_of(
    layout: &Layout,
    now: SystemTime,
    derivations: &[(&str, u32)],
) -> Result<Vec<Leftover>, StoreError> {
    let lister = Lister {
        root: layout.root(),
        fs: StoreFs::new(Arc::new(RealFs::new()), layout.root()),
    };
    let mut found = Vec::new();

    // 1. Temp files past their grace.
    lister.temps(
        &layout.objects_tmp(),
        LeftoverKind::ObjectTemp,
        now,
        &mut found,
    )?;
    lister.temps(
        &layout.derived_tmp(),
        LeftoverKind::DerivedTemp,
        now,
        &mut found,
    )?;

    // 2. A crashed create's staging, once there is a `meta/` it did not become.
    let staging = layout.root().join(STAGING);
    if lister.is_dir(&staging)? && lister.is_dir(&layout.meta_dir())? {
        found.push(lister.leftover(LeftoverKind::MetaStaging, &staging)?);
    }

    // 3. Superseded versions of the registered derivations.
    for &(name, version) in derivations {
        lister.versions(&layout.derived_dir().join(name), version, &mut found)?;
    }

    // 4. What an upgrade left of the format it moved from, or of the one it did not reach.
    lister.residue(layout, &mut found)?;

    found.sort_by_key(|leftover| {
        (
            leftover.kind,
            is_poison_slot(leftover),
            leftover.rel.clone(),
        )
    });
    Ok(found)
}

/// Removes `leftover`, through `fs`, and makes its removal durable: a file is removed and its
/// directory fsynced; a directory has each file under it removed through `fs`, then itself and
/// the directories under it removed with `std::fs` (an [`crate::FsOps`] removes no directory),
/// then its parent fsynced. One gone already, or partly, is removed as far as it is left: a gc
/// cut short, run again, finishes.
pub fn remove(fs: &StoreFs, leftover: &Leftover) -> Result<(), StoreError> {
    let path = fs.root().join(&leftover.rel);
    let Some(found) = lstat(fs, &path)? else {
        return Ok(());
    };

    // 1. The leftover, whole.
    if found.is_dir() {
        remove_tree(fs, &path)?;
    } else {
        remove_file(fs, &path)?;
    }

    // 2. Its removal, durable.
    let parent = path.parent().unwrap_or(fs.root());
    fs.fsync_dir(parent)?;
    Ok(())
}

/// What [`list_leftovers_of`] lists with: the store's root, and a [`StoreFs`] on it that only
/// locates failures.
struct Lister<'a> {
    root: &'a Utf8Path,
    fs: StoreFs,
}

impl Lister<'_> {
    /// Pushes the regular files of `dir` modified more than [`GC_TEMP_GRACE`] before `now`, as
    /// `kind`. A file modified after `now` is kept, as is anything that is no regular file.
    fn temps(
        &self,
        dir: &Utf8Path,
        kind: LeftoverKind,
        now: SystemTime,
        found: &mut Vec<Leftover>,
    ) -> Result<(), StoreError> {
        for (path, meta) in self.entries(dir)? {
            if !meta.is_file() {
                continue;
            }
            let modified = meta
                .modified()
                .map_err(|e| self.fs.locate(FileOp::Stat, &path, e))?;
            let stale = now
                .duration_since(modified)
                .is_ok_and(|age| age > GC_TEMP_GRACE);
            if stale {
                found.push(self.leftover(kind, &path)?);
            }
        }
        Ok(())
    }

    /// Pushes the version directories of derivation directory `dir` that name, in canonical
    /// decimal, a version below `version`.
    fn versions(
        &self,
        dir: &Utf8Path,
        version: u32,
        found: &mut Vec<Leftover>,
    ) -> Result<(), StoreError> {
        for (path, meta) in self.entries(dir)? {
            let Some(named) = path.file_name().and_then(|name| name.parse::<u32>().ok()) else {
                continue;
            };
            // "01" names version 1 only as "1" does: it is no directory a store makes.
            let canonical = path.file_name() == Some(named.to_string().as_str());
            if meta.is_dir() && canonical && named < version {
                found.push(self.leftover(LeftoverKind::DerivedVersion, &path)?);
            }
        }
        Ok(())
    }

    /// Pushes an upgrade's residue: in a FORMAT 2 store whose FORMAT 1 checkpoint is the
    /// poison, FORMAT 1's journal and ref files; in a FORMAT 1 store without it, FORMAT 2's.
    /// The format is the one `FORMAT` names (`layout`'s if there is none), not the one the
    /// store was opened in: a concurrent open may have finished an upgrade since.
    fn residue(&self, layout: &Layout, found: &mut Vec<Leftover>) -> Result<(), StoreError> {
        let format = if lstat(&self.fs, &layout.format_file())?.is_some() {
            format::read_format(layout, &self.fs)?
        } else {
            layout.format()
        };
        let v1 = Layout::with_format(self.root, StoreFormat::V1);
        let poisoned = self.is_poison(&v1.journal_ckpt())?;
        let older = match format {
            StoreFormat::V2 if poisoned => v1,
            StoreFormat::V1 if !poisoned => Layout::with_format(self.root, StoreFormat::V2),
            _ => return Ok(()),
        };
        let files = [
            older.journal_ckpt(),
            older.journal_log(),
            older.journal_off(),
            older.refs_dir(),
        ];
        for path in files {
            if lstat(&self.fs, &path)?.is_some() {
                found.push(self.leftover(LeftoverKind::UpgradeResidue, &path)?);
            }
        }
        Ok(())
    }

    /// Whether the file at `path` is a regular file holding [`UPGRADE_POISON`], measured before
    /// it is read.
    fn is_poison(&self, path: &Utf8Path) -> Result<bool, StoreError> {
        let Some(meta) = lstat(&self.fs, path)? else {
            return Ok(false);
        };
        if !meta.is_file() || meta.len() != bytes::to_u64(UPGRADE_POISON.len()) {
            return Ok(false);
        }
        let held = fs::read(path).map_err(|e| self.fs.locate(FileOp::ReadAt, path, e))?;
        Ok(held == UPGRADE_POISON)
    }

    /// Whether `path` is a directory (not a symlink to one).
    fn is_dir(&self, path: &Utf8Path) -> Result<bool, StoreError> {
        Ok(lstat(&self.fs, path)?.is_some_and(|meta| meta.is_dir()))
    }

    /// The entries of `dir` with UTF-8 names, each with what `lstat` says of it; none for a
    /// missing `dir`. A name that is no UTF-8 is no name the store makes, and is passed over.
    fn entries(&self, dir: &Utf8Path) -> Result<Vec<(Utf8PathBuf, Metadata)>, StoreError> {
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.fs.locate(FileOp::ReadDir, dir, e).into()),
        };
        let mut entries = Vec::new();
        for entry in listing {
            let entry = entry.map_err(|e| self.fs.locate(FileOp::ReadDir, dir, e))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = dir.join(name);
            if let Some(meta) = lstat(&self.fs, &path)? {
                entries.push((path, meta));
            }
        }
        Ok(entries)
    }

    /// The leftover `kind` at `path`, its bytes counted.
    fn leftover(&self, kind: LeftoverKind, path: &Utf8Path) -> Result<Leftover, StoreError> {
        let rel = path
            .strip_prefix(self.root)
            .expect("gc lists under the store's root")
            .to_owned();
        Ok(Leftover {
            kind,
            rel,
            bytes: self.bytes_of(path)?,
        })
    }

    /// The bytes of the regular files at or under `path`, no symlink followed.
    fn bytes_of(&self, path: &Utf8Path) -> Result<u64, StoreError> {
        let Some(meta) = lstat(&self.fs, path)? else {
            return Ok(0);
        };
        if !meta.is_dir() {
            return Ok(if meta.is_file() { meta.len() } else { 0 });
        }
        let mut total = 0u64;
        for (child, _) in self.entries(path)? {
            total = total.saturating_add(self.bytes_of(&child)?);
        }
        Ok(total)
    }
}

/// Whether `leftover` is FORMAT 1's checkpoint, which an upgrade's residue removes last.
fn is_poison_slot(leftover: &Leftover) -> bool {
    leftover.kind == LeftoverKind::UpgradeResidue && leftover.rel == "meta/journal.ckpt"
}

/// What `lstat` says of `path`; `None` if nothing is there.
fn lstat(fs: &StoreFs, path: &Utf8Path) -> Result<Option<Metadata>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(fs.locate(FileOp::Stat, path, e).into()),
    }
}

/// Removes the file (or symlink, or other non-directory) at `path` through `fs`; one gone
/// already is removed.
fn remove_file(fs: &StoreFs, path: &Utf8Path) -> Result<(), StoreError> {
    match fs.ops().remove(path.as_std_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(fs.locate(FileOp::Fs(FsMethod::Remove), path, e).into()),
    }
}

/// Removes the directory at `path` and all it holds: its files through `fs`, the directories
/// with `std::fs`, deepest first.
fn remove_tree(fs: &StoreFs, path: &Utf8Path) -> Result<(), StoreError> {
    let listing = match fs::read_dir(path) {
        Ok(listing) => listing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(fs.locate(FileOp::ReadDir, path, e).into()),
    };
    for entry in listing {
        let entry = entry.map_err(|e| fs.locate(FileOp::ReadDir, path, e))?;
        let Ok(child) = Utf8PathBuf::from_path_buf(entry.path()) else {
            // A name that is no UTF-8: removed as it is named.
            fs::remove_file(entry.path()).map_err(|e| fs.locate(FileOp::RemoveDirAll, path, e))?;
            continue;
        };
        match lstat(fs, &child)? {
            Some(meta) if meta.is_dir() => remove_tree(fs, &child)?,
            Some(_) => remove_file(fs, &child)?,
            None => {}
        }
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(fs.locate(FileOp::RemoveDirAll, path, e).into()),
    }
}
