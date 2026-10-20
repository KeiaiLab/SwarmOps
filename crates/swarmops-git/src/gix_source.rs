//! [`GixSource`], the [`GitSource`] of a repository on disk, read through gix: with
//! [`gix_sink`](crate::gix_sink), the crate's contact with gix (umbrella decision 3), and what
//! computes the SHA-1 behind [`git_hash`](crate::git_hash). Replacing gix, by git2 0.21.0 for
//! instance, replaces these two modules alone.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{GitOid, GitSource, ObjectFormat, RawKind, SourceError, Unsupported};

/// A [`GitSource`] over a git repository on disk, read through gix (`=0.89.0`, its `sha1`
/// feature alone).
///
/// It opens the repository as gix's `open_opts` finds it, with `gix::open::Options::isolated()`:
/// no global or system configuration, no `GIT_*` variable. It refuses, before any read, a
/// repository whose objects are named by another hash than SHA-1 (`extensions.objectFormat`),
/// one whose refs are kept otherwise than as files (`extensions.refStorage`: gix-ref 0.69 reads no
/// reftable, and would see no refs), a shallow clone, one where a file gix reads in the git
/// directory is a FIFO, a socket or a device ([`Unsupported::SpecialFile`]), and one where a file
/// gix reads whole there, a ref among them, is longer than 16 MiB
/// ([`Unsupported::FileTooLong`]). Replacement objects never apply, and gix's revision syntax is
/// not built in (its `revision` feature is off). Every object [`read`] returns is checked against
/// its id.
///
/// [`read`]: GitSource::read
pub struct GixSource {
    repo: gix::Repository,
}

impl fmt::Debug for GixSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GixSource")
            .field("git_dir", &self.repo.git_dir())
            .finish()
    }
}

impl GixSource {
    /// Opens the repository at `path`, a work tree or a git directory (a bare `repo.git`), as
    /// gix's `open_opts` finds it: `<path>/.git`, or else `path` itself.
    ///
    /// [`SourceError::NotARepo`] when gix opens no repository there,
    /// [`SourceError::Unsupported`] for one it does not read as it is, a special file where it
    /// reads one among them.
    pub fn open(path: &Path) -> Result<GixSource, SourceError> {
        Ok(GixSource {
            repo: open_repository(path)?,
        })
    }

    /// The object the ref `name` points at, through symbolic refs, tags not peeled.
    fn follow(&self, name: &str) -> Result<GitOid, SourceError> {
        let mut reference = match self.repo.try_find_reference(name) {
            Ok(Some(reference)) => reference,
            // No ref has the name, or it is no ref name at all (`main~3`, `a..b`).
            Ok(None) => return Err(not_found(name)),
            Err(error) if error.is_validation() => return Err(not_found(name)),
            Err(error) => return Err(io(&error)),
        };
        match reference.follow_to_object() {
            Ok(id) => Ok(from_gix(&id.detach())),
            // A symbolic ref to a ref that does not exist, as the `HEAD` of an unborn branch.
            Err(error) if error.is_not_found() => Err(not_found(name)),
            Err(error) => Err(io(&error)),
        }
    }

    /// The commit `oid` peels to, through annotated tags: each tag read, its hash checked, and the
    /// object of its first line followed. The walk ends: a tag's id hashes the bytes that name the
    /// next object, so tags cannot name each other in a cycle.
    fn peel(&self, name: &str, mut oid: GitOid) -> Result<GitOid, SourceError> {
        loop {
            match self.header(oid)?.0 {
                RawKind::Commit => return Ok(oid),
                RawKind::Tag => {
                    let (_, tag) = self.read(oid)?;
                    oid = tag_target(&tag).ok_or_else(|| SourceError::Corrupt {
                        oid,
                        reason: "an annotated tag whose first line names no object".to_owned(),
                    })?;
                }
                kind => {
                    return Err(SourceError::NotACommit {
                        name: name.to_owned(),
                        kind,
                    });
                }
            }
        }
    }
}

impl GitSource for GixSource {
    fn object_format(&self) -> ObjectFormat {
        // `open` refuses every other format.
        ObjectFormat::Sha1
    }

    fn resolve(&self, name: &str) -> Result<GitOid, SourceError> {
        // 40 hex digits name an object before any ref, as git takes them; an id the repository
        // lacks names nothing.
        if let Some(oid) = GitOid::from_hex(name.as_bytes()) {
            return self.peel(name, oid).map_err(|error| match error {
                SourceError::Missing { oid: missing } if missing == oid => not_found(name),
                error => error,
            });
        }
        let target = self.follow(name)?;
        self.peel(name, target)
    }

    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError> {
        let header = self
            .repo
            .try_find_header(to_gix(oid))
            .map_err(|error| damaged(oid, &error))?
            .ok_or(SourceError::Missing { oid })?;
        Ok((raw_kind(header.kind()), header.size()))
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError> {
        let object = self
            .repo
            .try_find_object(to_gix(oid))
            .map_err(|error| damaged(oid, &error))?
            .ok_or(SourceError::Missing { oid })?
            .detach();
        checked(oid, raw_kind(object.kind), object.data)
    }
}

/// The repository at `path`, a work tree or a git directory, as gix's `open_opts` finds it
/// (`<path>/.git`, or else `path` itself), with `gix::open::Options::isolated()`: no global or
/// system configuration and no `GIT_*` variable, and replacement objects never applied. It
/// refuses, before any read, a repository where a file gix reads in the git directory is a FIFO,
/// a socket or a device, or one it reads whole longer than 16 MiB ([`GitDir`]), one whose objects
/// are named by another hash than SHA-1, one whose refs are kept otherwise than as files, and a
/// shallow clone. [`GixSource::open`] and [`GixSink::open`](crate::GixSink::open) both open
/// through it, so that the export writes only repositories the import reads.
pub(crate) fn open_repository(path: &Path) -> Result<gix::Repository, SourceError> {
    if let Some(git_dir) = GitDir::find(path)? {
        git_dir.check()?;
        // gix cannot open a SHA-256 repository without its `sha256` feature, and says so without
        // the format's name: the repository's own configuration is read first, to name it.
        if let Some(config) = git_dir.own_config()? {
            refuse_extensions(&config)?;
        }
    }
    let mut repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|error| {
        SourceError::NotARepo {
            reason: reason(&error),
        }
    })?;
    // Replacement objects never apply. gix 0.89 reads `core.useReplaceRefs` the other way round
    // (`false` is what loads `refs/replace/`), so the object handle is told to ignore them
    // whatever the configuration says; `read`'s hash check would see one regardless.
    repo.objects.ignore_replacements = true;
    refuse_extensions(repo.config_snapshot().plumbing())?;
    if repo.is_shallow().map_err(|error| io(&error))? {
        return Err(SourceError::Unsupported(Unsupported::Shallow));
    }
    Ok(repo)
}

/// The SHA-1 git gives an object of `kind` whose bytes are `bytes` (the hash of
/// `"<kind> <len>\0"` and the bytes), by gix's SHA-1, which runs git's collision detection:
/// `None` when that detection flags the bytes as a SHA-1 collision attack, an object git refuses
/// to read. [`git_hash`](crate::git_hash) is this.
pub(crate) fn object_sha1(kind: RawKind, bytes: &[u8]) -> Option<GitOid> {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(format!("{kind} {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    Some(from_gix(&hasher.try_finalize().ok()?))
}

/// `bytes`, read as the object `oid` of `kind`, when they hash to `oid`;
/// [`SourceError::Corrupt`] otherwise.
fn checked(oid: GitOid, kind: RawKind, bytes: Vec<u8>) -> Result<(RawKind, Vec<u8>), SourceError> {
    match not_the_object(oid, kind, &bytes) {
        None => Ok((kind, bytes)),
        Some(reason) => Err(SourceError::Corrupt { oid, reason }),
    }
}

/// Why `bytes`, read as the object `oid` of `kind`, are not that object: they hash to another
/// id, or git's collision detection flags them. `None` when they hash to `oid`.
pub(crate) fn not_the_object(oid: GitOid, kind: RawKind, bytes: &[u8]) -> Option<String> {
    match object_sha1(kind, bytes) {
        Some(hashed) if hashed == oid => None,
        Some(hashed) => Some(format!("its bytes, a {kind}, hash to {hashed}")),
        None => {
            Some("git's collision detection flags its bytes as a SHA-1 collision attack".to_owned())
        }
    }
}

/// Where gix's `open_opts` finds a repository's files: its git directory, and the common
/// directory that a linked work tree shares with its main one (the git directory itself
/// otherwise).
///
/// Opening a FIFO blocks until something opens it for writing, and a device's read can block or
/// never end, and gix opens and reads the files of a git directory without looking at what they
/// are (gix-config's `from_path_no_includes` and the repository's open, gix-discover's
/// `is_git`, gix-ref's loose refs and `packed-refs`, gix-odb's alternates), as git itself does.
/// An extracted archive can hold such a file where git keeps one. So every file that the open,
/// and the source's lookups after it, read here is looked at first, by its metadata, which
/// follows symlinks and opens nothing ([`GitDir::find`], [`GitDir::check`]), and one that is no
/// regular file and no directory is refused ([`Unsupported::SpecialFile`]). Anything else is left
/// to gix, which fails on it rather than blocks: a file that is missing or cannot be looked at, a
/// directory where a file should be. gix reads a loose object only when its metadata says it is a
/// regular file, and a pack only when its directory entry says so. The files gix reads whole, the
/// refs among them, are bounded too ([`MAX_WHOLE_LEN`]). The repository's files are taken as they
/// are when it opens: one swapped for a FIFO later is not seen.
struct GitDir {
    /// The git directory: `HEAD`, the other pseudo-refs, `config.worktree` and `commondir`.
    git: PathBuf,
    /// The common directory: `config`, `packed-refs`, `refs/`, `objects/` and `worktrees/`.
    common: PathBuf,
}

impl GitDir {
    /// The repository at `path` as gix's `open_opts` finds it: `<path>/.git`, or else `path`
    /// itself (`path` alone when its last component is `.git`), the first that
    /// `gix::discover::is_git` takes, with the files that call reads in each looked at before
    /// ([`GitDir::check_candidate`]). `None` where gix finds no repository, which its open then
    /// reports.
    fn find(path: &Path) -> Result<Option<GitDir>, SourceError> {
        let candidates = if path.ends_with(gix::discover::DOT_GIT_DIR) {
            vec![path.to_owned()]
        } else {
            vec![path.join(gix::discover::DOT_GIT_DIR), path.to_owned()]
        };
        for candidate in candidates {
            GitDir::check_candidate(&candidate)?;
            let Ok(kind) = gix::discover::is_git(&candidate) else {
                continue;
            };
            let Ok(cwd) = std::env::current_dir() else {
                return Ok(None);
            };
            let Some(found) =
                gix::discover::repository::Path::from_dot_git_dir(candidate, kind, &cwd)
            else {
                return Ok(None);
            };
            let (git, _) = found.into_repository_and_work_tree_directories();
            let mut git_dir = GitDir {
                common: git.clone(),
                git,
            };
            // A linked work tree's `commondir` names the common directory; gix reads at most
            // 64 KiB of it.
            let commondir = git_dir.git.join("commondir");
            if git_dir.present(&commondir)?
                && let Some(Ok(common)) = gix::discover::path::from_plain_file(&commondir)
            {
                git_dir.common = git_dir.git.join(common);
            }
            return Ok(Some(git_dir));
        }
        Ok(None)
    }

    /// Refuses a special file among those `gix::discover::is_git` reads to see whether
    /// `candidate` holds a git directory: `HEAD`, `commondir` and `gitdir` of the directory it
    /// names, itself or the one its `.git` file names (gix reads that file only when it is a
    /// regular file, and at most 64 KiB of it).
    fn check_candidate(candidate: &Path) -> Result<(), SourceError> {
        let Ok(metadata) = fs::metadata(candidate) else {
            return Ok(());
        };
        let dir = if metadata.is_file() {
            match gix::discover::path::from_gitdir_file(candidate) {
                Ok(dir) => dir,
                Err(_) => return Ok(()),
            }
        } else {
            candidate.to_owned()
        };
        let found = GitDir {
            git: dir.clone(),
            common: dir,
        };
        // gix reads nothing else where there is no `HEAD`, and `gitdir` only beside a `commondir`.
        let head = found.present_whole(&found.git.join("HEAD"))?;
        if head && found.present(&found.git.join("commondir"))? {
            found.present(&found.git.join("gitdir"))?;
        }
        Ok(())
    }

    /// Refuses a special file among the rest of what gix reads as it opens the repository, and
    /// the source's lookups after it: `config` and `config.worktree` (which gix reads under
    /// `extensions.worktreeConfig`), each read whole, `packed-refs`, the loose refs
    /// ([`GitDir::check_refs`]) and the alternates ([`GitDir::check_alternates`]).
    fn check(&self) -> Result<(), SourceError> {
        self.present_whole(&self.common.join("config"))?;
        self.present_whole(&self.git.join("config.worktree"))?;
        self.present(&self.common.join("packed-refs"))?;
        self.check_refs()?;
        self.check_alternates()
    }

    /// Refuses a special file among the loose refs a lookup can read, directly or through a
    /// symbolic ref: the pseudo-refs at the top of the git directory, of the common one and of
    /// each linked work tree's (`worktrees/<id>/`) — `HEAD`, `FETCH_HEAD` and every other name
    /// of capitals and `_`, which gix reads there — and every file under their `refs/`, through
    /// symlinked directories too, each directory once; and one longer than [`MAX_WHOLE_LEN`],
    /// since gix reads a ref whole.
    fn check_refs(&self) -> Result<(), SourceError> {
        let mut tops = vec![self.git.clone(), self.common.clone()];
        if let Ok(linked) = fs::read_dir(self.common.join("worktrees")) {
            tops.extend(linked.flatten().map(|entry| entry.path()));
        }
        let mut seen = HashSet::new();
        let mut dirs = Vec::new();
        for top in tops {
            let Ok(real) = fs::canonicalize(&top) else {
                continue;
            };
            if !seen.insert(real) {
                continue;
            }
            if let Ok(entries) = fs::read_dir(&top) {
                for entry in entries.flatten() {
                    if is_pseudo_ref(&entry.file_name()) {
                        self.present_whole(&entry.path())?;
                    }
                }
            }
            dirs.push(top.join("refs"));
        }
        while let Some(dir) = dirs.pop() {
            let Ok(real) = fs::canonicalize(&dir) else {
                continue;
            };
            if !seen.insert(real) {
                continue;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                if metadata.is_dir() {
                    dirs.push(path);
                } else {
                    self.refuse_special_or_long(&path, &metadata)?;
                }
            }
        }
        Ok(())
    }

    /// Refuses a special `objects/info/alternates` of the object directory, and of each object
    /// directory it names in turn, as gix reads them when it opens the repository
    /// (`gix::odb::alternate::resolve`, which takes a relative path from the repository's own
    /// `objects`), and one longer than [`MAX_WHOLE_LEN`], since gix reads each whole: each is
    /// read here as [`GitDir::read_whole`] reads it, to follow it.
    fn check_alternates(&self) -> Result<(), SourceError> {
        let objects = self.common.join("objects");
        let mut dirs = vec![objects.clone()];
        let mut seen = HashSet::new();
        while let Some(dir) = dirs.pop() {
            let Ok(real) = fs::canonicalize(&dir) else {
                continue;
            };
            if !seen.insert(real) {
                continue;
            }
            let alternates = dir.join("info").join("alternates");
            let Some(listed) = self.read_whole(&alternates)? else {
                continue;
            };
            // A file gix's open fails on is left to it.
            let Ok(listed) = gix::odb::alternate::parse(&listed) else {
                continue;
            };
            dirs.extend(listed.into_iter().map(|alternate| objects.join(alternate)));
        }
        Ok(())
    }

    /// The repository's own configuration, its common directory's `config`, read as git reads
    /// its repository format (no includes), as [`GitDir::read_whole`] reads it. `None` where it is
    /// not there, does not read or does not parse, which the open then reports.
    fn own_config(&self) -> Result<Option<gix::config::File>, SourceError> {
        let Some(bytes) = self.read_whole(&self.common.join("config"))? else {
            return Ok(None);
        };
        let local = gix::config::file::Metadata::from(gix::config::Source::Local);
        let options = gix::config::file::init::Options::default();
        Ok(gix::config::File::from_bytes_no_includes(&bytes, local, options).ok())
    }

    /// Whether the file at `path` is there, refusing it when it is a special file. One whose
    /// metadata cannot be read is as good as absent: gix's open of it fails rather than blocks.
    fn present(&self, path: &Path) -> Result<bool, SourceError> {
        let Ok(metadata) = fs::metadata(path) else {
            return Ok(false);
        };
        self.refuse_special(path, &metadata)?;
        Ok(true)
    }

    /// [`GitDir::present`], for a file gix reads whole: a regular file longer than
    /// [`MAX_WHOLE_LEN`] is refused too.
    fn present_whole(&self, path: &Path) -> Result<bool, SourceError> {
        let Ok(metadata) = fs::metadata(path) else {
            return Ok(false);
        };
        self.refuse_special_or_long(path, &metadata)?;
        Ok(true)
    }

    /// The bytes of the file at `path`, which gix reads whole, once
    /// [`GitDir::present_whole`] has looked at it, and no more than [`MAX_WHOLE_LEN`] of them
    /// (one that grew since is refused). `None` where it is not there or does not read, which
    /// gix's open then reports.
    fn read_whole(&self, path: &Path) -> Result<Option<Vec<u8>>, SourceError> {
        if !self.present_whole(path)? {
            return Ok(None);
        }
        let Ok(file) = fs::File::open(path) else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        if file
            .take(MAX_WHOLE_LEN + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Ok(None);
        }
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if len > MAX_WHOLE_LEN {
            return Err(self.too_long(path, len));
        }
        Ok(Some(bytes))
    }

    /// [`GitDir::refuse_special`], for a file gix reads whole: a regular file longer than
    /// [`MAX_WHOLE_LEN`] is refused too.
    fn refuse_special_or_long(
        &self,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<(), SourceError> {
        self.refuse_special(path, metadata)?;
        if metadata.is_file() && metadata.len() > MAX_WHOLE_LEN {
            return Err(self.too_long(path, metadata.len()));
        }
        Ok(())
    }

    /// Refuses the file at `path`, whose metadata (symlinks followed) is `metadata`, when it is
    /// neither a regular file nor a directory: a FIFO, a socket or a device.
    fn refuse_special(&self, path: &Path, metadata: &fs::Metadata) -> Result<(), SourceError> {
        if metadata.is_file() || metadata.is_dir() {
            return Ok(());
        }
        let file = self.named(path);
        Err(SourceError::Unsupported(Unsupported::SpecialFile(file)))
    }

    /// The refusal of the file at `path`, `len` bytes long, past [`MAX_WHOLE_LEN`].
    fn too_long(&self, path: &Path, len: u64) -> SourceError {
        SourceError::Unsupported(Unsupported::FileTooLong {
            file: self.named(path),
            len,
            max: MAX_WHOLE_LEN,
        })
    }

    /// The file at `path` as a refusal names it: by its path in the common directory or in the
    /// git directory, or by its own path outside them (an alternate object directory elsewhere).
    fn named(&self, path: &Path) -> String {
        let file = [&self.common, &self.git]
            .into_iter()
            .find_map(|root| path.strip_prefix(root).ok())
            .unwrap_or(path);
        file.display().to_string()
    }
}

/// The longest file of the git directory that the source lets gix read into memory whole, `HEAD`
/// and every other ref file (a loose ref, a pseudo-ref), `config`, `config.worktree` and
/// `objects/info/alternates`: 16 MiB, far past any that git writes. A longer one, which a sparse
/// file can claim at no cost in disk, is refused unread ([`Unsupported::FileTooLong`]).
const MAX_WHOLE_LEN: u64 = 16 << 20;

/// Whether `name`, at the top of a git directory, is a pseudo-ref's (`HEAD`, `FETCH_HEAD`,
/// `ORIG_HEAD`, …): capitals and `_` alone, as gix-ref reads a ref of such a name there.
fn is_pseudo_ref(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
    })
}

/// Refuses a configuration whose objects are named by another hash than SHA-1, or whose refs are
/// kept otherwise than as files (`extensions.refStorage = files` is the default, written out).
fn refuse_extensions(config: &gix::config::File) -> Result<(), SourceError> {
    if let Some(format) = config.string("extensions.objectFormat")
        && !format.eq_ignore_ascii_case(b"sha1")
    {
        let format = format.to_string();
        return Err(SourceError::Unsupported(Unsupported::ObjectFormat(format)));
    }
    if let Some(storage) = config.string("extensions.refStorage")
        && !storage.eq_ignore_ascii_case(b"files")
    {
        let storage = storage.to_string();
        return Err(SourceError::Unsupported(Unsupported::RefStorage(storage)));
    }
    Ok(())
}

/// The object an annotated tag's bytes name on their first line, `object <id>`.
fn tag_target(tag: &[u8]) -> Option<GitOid> {
    let first_line = tag.split(|&byte| byte == b'\n').next()?;
    GitOid::from_hex(first_line.strip_prefix(b"object ")?)
}

fn not_found(name: &str) -> SourceError {
    SourceError::RefNotFound {
        name: name.to_owned(),
    }
}

/// A failure to read the object `oid`: [`SourceError::Corrupt`] when gix classes it as damaged
/// data, [`SourceError::Io`] otherwise.
fn damaged(oid: GitOid, error: &gix::Error) -> SourceError {
    if error.is_corrupted() {
        SourceError::Corrupt {
            oid,
            reason: reason(error),
        }
    } else {
        SourceError::Io {
            reason: format!("reading object {oid}: {}", reason(error)),
        }
    }
}

fn io(error: &gix::Error) -> SourceError {
    SourceError::Io {
        reason: reason(error),
    }
}

/// gix's error and every error under it, joined by `: `, each message once.
pub(crate) fn reason(error: &gix::Error) -> String {
    let mut messages: Vec<String> = Vec::new();
    for error in error.iter_errors() {
        let message = error.to_string();
        if !messages.contains(&message) {
            messages.push(message);
        }
    }
    messages.join(": ")
}

/// `oid` as gix names it.
pub(crate) fn to_gix(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from(oid.0)
}

/// gix's object id `id` as a [`GitOid`].
pub(crate) fn from_gix(id: &gix::ObjectId) -> GitOid {
    GitOid(
        id.as_bytes()
            .try_into()
            .expect("a SHA-1 build of gix names objects by 20 bytes"),
    )
}

/// gix's object kind `kind` as a [`RawKind`].
pub(crate) fn raw_kind(kind: gix::objs::Kind) -> RawKind {
    match kind {
        gix::objs::Kind::Commit => RawKind::Commit,
        gix::objs::Kind::Tree => RawKind::Tree,
        gix::objs::Kind::Blob => RawKind::Blob,
        gix::objs::Kind::Tag => RawKind::Tag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_that_do_not_hash_to_their_oid_are_corrupt() {
        let bytes = b"what the id names\n".to_vec();
        let oid = crate::git_hash(RawKind::Blob, &bytes);
        assert_eq!(
            checked(oid, RawKind::Blob, bytes.clone()),
            Ok((RawKind::Blob, bytes.clone()))
        );
        // Other bytes under the id, and the same bytes under another kind, hash otherwise.
        for (kind, other) in [
            (RawKind::Blob, b"what another id names\n".to_vec()),
            (RawKind::Commit, bytes),
        ] {
            assert!(
                matches!(
                    checked(oid, kind, other),
                    Err(SourceError::Corrupt { oid: named, .. }) if named == oid
                ),
                "{kind}"
            );
        }
    }
}
