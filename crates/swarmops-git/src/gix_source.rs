//! [`GixSource`], the [`GitSource`] of a repository on disk, read through gix: the crate's one
//! contact with gix (umbrella decision 3), which also computes the SHA-1 behind
//! [`git_hash`](crate::git_hash). Replacing gix, by git2 0.21.0 for instance, replaces this module
//! alone.

use std::fmt;
use std::path::Path;

use crate::{GitOid, GitSource, ObjectFormat, RawKind, SourceError, Unsupported};

/// A [`GitSource`] over a git repository on disk, read through gix (`=0.89.0`, its `sha1`
/// feature alone).
///
/// It opens the repository as gix's `open_opts` finds it, with `gix::open::Options::isolated()`:
/// no global or system configuration, no `GIT_*` variable. It refuses, before any read, a
/// repository whose objects are named by another hash than SHA-1 (`extensions.objectFormat`),
/// one whose refs are kept otherwise than as files (`extensions.refStorage`: gix-ref 0.69 reads no
/// reftable, and would see no refs), and a shallow clone. Replacement objects never apply, and
/// gix's revision syntax is not built in (its `revision` feature is off). Every object [`read`]
/// returns is checked against its id.
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
    /// [`SourceError::Unsupported`] for one it does not read as it is.
    pub fn open(path: &Path) -> Result<GixSource, SourceError> {
        // gix cannot open a SHA-256 repository without its `sha256` feature, and says so without
        // the format's name: the repository's own configuration is read first, to name it.
        if let Some(config) = own_config(path) {
            refuse_extensions(&config)?;
        }
        let mut repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|error| {
            SourceError::NotARepo {
                reason: reason(&error),
            }
        })?;
        // Replacement objects never apply. gix 0.89 reads `core.useReplaceRefs` the other way
        // round (`false` is what loads `refs/replace/`), so the object handle is told to ignore
        // them whatever the configuration says; `read`'s hash check would see one regardless.
        repo.objects.ignore_replacements = true;
        refuse_extensions(repo.config_snapshot().plumbing())?;
        if repo.is_shallow().map_err(|error| io(&error))? {
            return Err(SourceError::Unsupported(Unsupported::Shallow));
        }
        Ok(GixSource { repo })
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
    match object_sha1(kind, &bytes) {
        Some(hashed) if hashed == oid => Ok((kind, bytes)),
        Some(hashed) => Err(SourceError::Corrupt {
            oid,
            reason: format!("its bytes, a {kind}, hash to {hashed}"),
        }),
        None => Err(SourceError::Corrupt {
            oid,
            reason: "git's collision detection flags its bytes as a SHA-1 collision attack"
                .to_owned(),
        }),
    }
}

/// The repository's own configuration file, read as git reads its repository format (no
/// includes), where gix's `open_opts` finds the repository: `<path>/.git` or else `path` itself,
/// through a `.git` file and a `commondir` file. `None` where gix finds no repository or the
/// file does not read, which the open then reports.
fn own_config(path: &Path) -> Option<gix::config::File> {
    let (dot_git, kind) = [path.join(gix::discover::DOT_GIT_DIR), path.to_owned()]
        .into_iter()
        .find_map(|candidate| {
            let kind = gix::discover::is_git(&candidate).ok()?;
            Some((candidate, kind))
        })?;
    let cwd = std::env::current_dir().ok()?;
    let (git_dir, _) = gix::discover::repository::Path::from_dot_git_dir(dot_git, kind, &cwd)?
        .into_repository_and_work_tree_directories();
    let common_dir = match gix::discover::path::from_plain_file(&git_dir.join("commondir")) {
        Some(Ok(common_dir)) => git_dir.join(common_dir),
        _ => git_dir,
    };
    gix::config::File::from_path_no_includes(common_dir.join("config"), gix::config::Source::Local)
        .ok()
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
fn reason(error: &gix::Error) -> String {
    let mut messages: Vec<String> = Vec::new();
    for error in error.iter_errors() {
        let message = error.to_string();
        if !messages.contains(&message) {
            messages.push(message);
        }
    }
    messages.join(": ")
}

fn to_gix(oid: GitOid) -> gix::ObjectId {
    gix::ObjectId::from(oid.0)
}

fn from_gix(id: &gix::ObjectId) -> GitOid {
    GitOid(
        id.as_bytes()
            .try_into()
            .expect("a SHA-1 build of gix names objects by 20 bytes"),
    )
}

fn raw_kind(kind: gix::objs::Kind) -> RawKind {
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
