//! [`CatFileSource`], a [`GitSource`] whose objects come from one long-lived
//! `git cat-file --batch-command` child, with git's pack maps bounded: what a push's hooks read
//! the push's objects through (the M17 closure design §3.5).
//!
//! ```text
//!   CatFileSource
//!     ├── header / read ──▶ stdin  "info <oid>\n" | "contents <oid>\n"
//!     │                 ◀── stdout "<oid> <kind> <size>\n" [<size> bytes "\n"] | "<oid> missing\n"
//!     │                     git cat-file --batch-command, its pack windows at most
//!     │                     CAT_FILE_WINDOW each and CAT_FILE_MAPPED in all
//!     └── resolve / object_format, and read of a loose object ──▶ a GixSource
//! ```
//!
//! gix maps each pack file whole, so a reader of every object of a 2 GiB pack keeps 2 GiB of
//! file pages resident; git unmaps its windows past `core.packedGitLimit`. A loose object is a
//! file gix reads, not maps, and git 2.43's `cat-file` streams a loose blob shorter than its
//! header says, then waits for the next request: so gix reads the loose ones.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use crate::gix_source::not_the_object;
use crate::transport::GitCli;
use crate::{GitOid, GitSource, GixSource, ObjectFormat, RawKind, SourceError};

/// The most of a pack git maps at once for [`CatFileSource`], one window
/// (`core.packedGitWindowSize`).
pub const CAT_FILE_WINDOW: u64 = 32 << 20;

/// The most of its packs git keeps mapped for [`CatFileSource`] in all
/// (`core.packedGitLimit`): past it, git unmaps the least recently used windows.
pub const CAT_FILE_MAPPED: u64 = 64 << 20;

/// The most of inflated delta bases git keeps for [`CatFileSource`]
/// (`core.deltaBaseCacheLimit`).
const DELTA_BASE_CACHE: u64 = 16 << 20;

/// The most of one answer line read: `<40 hex> <kind> <size>\n` is far shorter.
const LINE_MAX: u64 = 256;

/// The bytes of git's stderr kept: its end.
const STDERR_KEPT: usize = 4 << 10;

/// The variables that point git at a push's quarantine, and a repository's object directory.
const OBJECT_DIRECTORY: &str = "GIT_OBJECT_DIRECTORY";
const ALTERNATE_OBJECT_DIRECTORIES: &str = "GIT_ALTERNATE_OBJECT_DIRECTORIES";
const OBJECTS: &str = "objects";

/// The variable that keeps git from fetching an object a partial clone lacks (git 2.45 and
/// newer; an older git finds no transport allowed).
const NO_LAZY_FETCH: &str = "GIT_NO_LAZY_FETCH";

/// What `cat-file` answers for an object the repository lacks.
const MISSING: &str = "missing";

/// A [`GitSource`] over a bare git repository on disk whose objects are read by one
/// `git cat-file --batch-command` child (git 2.36 or newer), started when it opens and ended when
/// it drops, so that the pack file pages it keeps resident stay near [`CAT_FILE_MAPPED`]
/// whatever the pack's length.
///
/// [`header`](GitSource::header) and [`read`](GitSource::read) ask the child (`info`,
/// `contents`), but for the read of a loose object, and every object read is checked against its
/// id; replacement objects never apply
/// (`core.useReplaceRefs=false`). [`resolve`](GitSource::resolve) and
/// [`object_format`](GitSource::object_format) go to a [`GixSource`] of the same repository,
/// which also refuses, when it opens, what a [`GixSource`] refuses.
///
/// A child that ends, or answers out of step, fails the request as [`SourceError::Corrupt`] with
/// the end of git's stderr, and every later one as [`SourceError::Io`]. In a partial clone, an
/// object the repository lacks ends a git older than 2.45 (it tries the promisor remote, which
/// no transport may reach) rather than being [`SourceError::Missing`]; `serve.git` is never one.
#[derive(Debug)]
#[non_exhaustive]
pub struct CatFileSource {
    /// The refs, the few objects a peel reads, and the loose objects.
    refs: GixSource,
    /// The object directories, the quarantine first: where a loose object's file is.
    objects: Vec<PathBuf>,
    /// The child, one request at a time.
    batch: Mutex<Batch>,
}

/// A [`CatFileSource`]'s child and its pipes.
#[derive(Debug)]
struct Batch {
    /// The child, waited for when it ends.
    child: Child,
    /// Its requests; `None` once it ended.
    stdin: Option<ChildStdin>,
    /// Its answers.
    stdout: BufReader<ChildStdout>,
    /// What drains its stderr, keeping the end; `None` once joined.
    stderr: Option<JoinHandle<Vec<u8>>>,
}

/// What `cat-file` answered about an object.
enum Answer {
    /// It has the object, of this kind and length.
    Found(RawKind, u64),
    /// It lacks it.
    Missing,
}

impl CatFileSource {
    /// Opens the bare repository `git_dir`, its objects read by a `git cat-file` child of the
    /// user's git ([`GitCli::find`]).
    ///
    /// [`SourceError::NotARepo`] or [`SourceError::Unsupported`] as [`GixSource::open`] refuses
    /// it; [`SourceError::Io`] when git cannot be found or started.
    pub fn open(git_dir: &Path) -> Result<CatFileSource, SourceError> {
        let refs = GixSource::open(git_dir)?;
        CatFileSource::start(refs, git_dir, None)
    }

    /// Opens the bare repository `git_dir` as [`CatFileSource::open`] does, its objects looked
    /// up in `quarantine` first: the object directory git's receive-pack keeps a push's objects
    /// in until its `pre-receive` hook accepts them (`GIT_QUARANTINE_PATH`), given to the child
    /// as its object directory, with `git_dir`'s own as its alternate. Nothing is written into
    /// it.
    pub fn open_quarantine(
        git_dir: &Path,
        quarantine: &Path,
    ) -> Result<CatFileSource, SourceError> {
        let refs = GixSource::open_quarantine(git_dir, quarantine)?;
        CatFileSource::start(refs, git_dir, Some(quarantine))
    }

    /// The source of `refs`, its child started on `git_dir`, `quarantine` first when given.
    fn start(
        refs: GixSource,
        git_dir: &Path,
        quarantine: Option<&Path>,
    ) -> Result<CatFileSource, SourceError> {
        // 1. git, its maps bounded and replacements off, in the command's own configuration.
        // `find` fails only with `GitMissing`.
        let git = GitCli::find().map_err(|_| io_error(&"no git to run"))?;
        let (window, mapped, bases) = (
            CAT_FILE_WINDOW.to_string(),
            CAT_FILE_MAPPED.to_string(),
            DELTA_BASE_CACHE.to_string(),
        );
        let config = [
            ("core.packedGitWindowSize", OsStr::new(&window)),
            ("core.packedGitLimit", OsStr::new(&mapped)),
            ("core.deltaBaseCacheLimit", OsStr::new(&bases)),
            ("core.useReplaceRefs", OsStr::new("false")),
        ];
        let mut command = git.serve_command(git_dir, &config);
        command
            .args(["cat-file", "--batch-command"])
            .env(NO_LAZY_FETCH, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 2. The quarantine as the object directory, the repository's objects its alternate.
        let own = git_dir.join(OBJECTS);
        if let Some(quarantine) = quarantine {
            command
                .env(OBJECT_DIRECTORY, quarantine)
                .env(ALTERNATE_OBJECT_DIRECTORIES, &own);
        }
        let objects = quarantine.map(Path::to_path_buf).into_iter().chain([own]);

        // 3. The child, its stderr drained beside it so that it never blocks on a full pipe.
        let mut child = command.spawn().map_err(|error| io_error(&error))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(io_error(&"git cat-file's pipes"));
        };
        let batch = Batch {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr: Some(thread::spawn(move || drain(stderr))),
        };
        Ok(CatFileSource {
            refs,
            objects: objects.collect(),
            batch: Mutex::new(batch),
        })
    }

    /// Whether an object directory holds `oid` as a loose file, `<2 hex>/<38 hex>`.
    fn loose(&self, oid: GitOid) -> bool {
        let hex = oid.to_string();
        let (fan, rest) = hex.split_at(2);
        (self.objects.iter()).any(|dir| dir.join(fan).join(rest).is_file())
    }
}

impl Batch {
    /// Asks the child `verb` about `oid`, and reads the answer's line: with `contents`, the
    /// object's bytes follow, which the caller reads.
    fn ask(&mut self, verb: &str, oid: GitOid) -> Result<Answer, SourceError> {
        // 1. The request: a child that ended takes none.
        let request = format!("{verb} {oid}\n");
        let stdin = self.stdin.as_mut().ok_or_else(|| ended(None))?;
        let sent = stdin
            .write_all(request.as_bytes())
            .and_then(|()| stdin.flush());
        sent.map_err(|error| ended(Some(&error)))?;

        // 2. The answer's line: none means git could not read the object and ended.
        let mut line = Vec::new();
        let read = (&mut self.stdout)
            .take(LINE_MAX)
            .read_until(b'\n', &mut line);
        read.map_err(|error| ended(Some(&error)))?;
        if line.pop() != Some(b'\n') {
            return Err(self.fail(oid, "no answer"));
        }

        // 3. An answer out of step ends the child: what follows would be misread.
        answer(oid, &line).inspect_err(|_| {
            self.end();
        })
    }

    /// The child ended, or is ended now, on a request about `oid`: `what` went wrong.
    fn fail(&mut self, oid: GitOid, what: &str) -> SourceError {
        let reason = format!("git cat-file gave {what}: {}", self.end());
        SourceError::Corrupt { oid, reason }
    }

    /// Ends the child (its stdin closed) and waits for it: how it exited and the end of its
    /// stderr, as text.
    fn end(&mut self) -> String {
        drop(self.stdin.take());
        let status = match self.child.wait() {
            Ok(status) => status.to_string(),
            Err(error) => error.to_string(),
        };
        let tail = (self.stderr.take())
            .and_then(|drain| drain.join().ok())
            .unwrap_or_default();
        let tail = String::from_utf8_lossy(&tail);
        format!("{status}: {}", tail.trim_end())
    }
}

impl GitSource for CatFileSource {
    fn object_format(&self) -> ObjectFormat {
        self.refs.object_format()
    }

    fn resolve(&self, name: &str) -> Result<GitOid, SourceError> {
        self.refs.resolve(name)
    }

    fn header(&self, oid: GitOid) -> Result<(RawKind, u64), SourceError> {
        let mut batch = self.batch.lock().unwrap_or_else(PoisonError::into_inner);
        match batch.ask("info", oid)? {
            Answer::Found(kind, size) => Ok((kind, size)),
            Answer::Missing => Err(SourceError::Missing { oid }),
        }
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SourceError> {
        // 1. A loose object through gix, which reads its file whole and checks it.
        if self.loose(oid) {
            return self.refs.read(oid);
        }

        // 2. The object's line.
        let mut batch = self.batch.lock().unwrap_or_else(PoisonError::into_inner);
        let (kind, size) = match batch.ask("contents", oid)? {
            Answer::Found(kind, size) => (kind, size),
            Answer::Missing => return Err(SourceError::Missing { oid }),
        };

        // 3. Its bytes and the newline after them; fewer mean git ended on a damaged object.
        let mut bytes = Vec::new();
        let read = (&mut batch.stdout).take(size).read_to_end(&mut bytes);
        read.map_err(|error| ended(Some(&error)))?;
        let mut newline = [0_u8; 1];
        let whole = u64::try_from(bytes.len()) == Ok(size)
            && batch.stdout.read_exact(&mut newline).is_ok()
            && newline == *b"\n";
        if !whole {
            return Err(batch.fail(oid, &format!("{} of its {size} bytes", bytes.len())));
        }

        // 4. The bytes are the object its id names.
        match not_the_object(oid, kind, &bytes) {
            None => Ok((kind, bytes)),
            Some(reason) => Err(SourceError::Corrupt { oid, reason }),
        }
    }
}

impl Drop for CatFileSource {
    /// Closes the child's stdin, which ends it, and waits for it.
    fn drop(&mut self) {
        let batch = self.batch.get_mut().unwrap_or_else(PoisonError::into_inner);
        batch.end();
    }
}

/// The answer `line` (without its newline) gives about `oid`: `<oid> <kind> <size>` or
/// `<oid> missing`.
fn answer(oid: GitOid, line: &[u8]) -> Result<Answer, SourceError> {
    let unexpected = || SourceError::Io {
        reason: format!(
            "git cat-file answered {:?} about {oid}",
            String::from_utf8_lossy(line)
        ),
    };

    // 1. The id it answers about: the one asked, or the pipe is out of step.
    let text = std::str::from_utf8(line).map_err(|_| unexpected())?;
    let mut words = text.split(' ');
    if words.next().and_then(|hex| hex.parse().ok()) != Some(oid) {
        return Err(unexpected());
    }

    // 2. Missing, or the kind and the length.
    let (kind, size) = match (words.next(), words.next(), words.next()) {
        (Some(MISSING), None, None) => return Ok(Answer::Missing),
        (Some(kind), Some(size), None) => (kind, size),
        _ => return Err(unexpected()),
    };
    let size = size.parse().map_err(|_| unexpected())?;
    let kind = match kind {
        "commit" => RawKind::Commit,
        "tree" => RawKind::Tree,
        "blob" => RawKind::Blob,
        "tag" => RawKind::Tag,
        other => {
            return Err(SourceError::Corrupt {
                oid,
                reason: format!("an object of no kind git writes: {other:?}"),
            });
        }
    };
    Ok(Answer::Found(kind, size))
}

/// Reads `stderr` to its end, keeping its last [`STDERR_KEPT`] bytes.
fn drain(mut stderr: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let n = match stderr.read(&mut chunk) {
            Ok(0) | Err(_) => return kept,
            Ok(n) => n,
        };
        kept.extend_from_slice(&chunk[..n]);
        let over = kept.len().saturating_sub(STDERR_KEPT);
        kept.drain(..over);
    }
}

/// The child took no request, or its pipe failed: `error`, if one was given.
fn ended(error: Option<&io::Error>) -> SourceError {
    let reason = match error {
        Some(error) => format!("git cat-file ended: {error}"),
        None => "git cat-file ended".to_owned(),
    };
    SourceError::Io { reason }
}

/// `error` as [`SourceError::Io`].
fn io_error(error: &dyn fmt::Display) -> SourceError {
    SourceError::Io {
        reason: format!("starting git cat-file: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Answer, answer};
    use crate::{GitOid, RawKind, SourceError};

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    fn oid() -> GitOid {
        OID.parse().unwrap()
    }

    /// `cat-file`'s answer lines: an object's kind and length, a missing object, and lines out
    /// of step or of no kind git writes.
    #[test]
    fn answers_parse() {
        let found = answer(oid(), format!("{OID} blob 12").as_bytes());
        assert!(matches!(found, Ok(Answer::Found(RawKind::Blob, 12))));
        let missing = answer(oid(), format!("{OID} missing").as_bytes());
        assert!(matches!(missing, Ok(Answer::Missing)));

        let other = "1123456789abcdef0123456789abcdef01234567 blob 12";
        for line in [other, &format!("{OID} blob"), &format!("{OID} blob x")] {
            let got = answer(oid(), line.as_bytes());
            assert!(matches!(got, Err(SourceError::Io { .. })), "{line}");
        }
        let odd = answer(oid(), format!("{OID} note 3").as_bytes());
        assert!(matches!(odd, Err(SourceError::Corrupt { .. })));
    }
}
