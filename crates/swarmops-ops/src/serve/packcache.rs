//! `serve`'s pack cache (the M17 design §5): what `upload-pack` asks its `pack-objects` for,
//! answered once per identical request, and never with an object no served ref reaches.
//!
//! ```text
//! git <args> on stdin ─▶ 1 the line: git [--shallow-file F] pack-objects --…
//!                        2 the wants: each a served tip on a commit (in process), else git's
//!                          check: each a commit a ref of serve.git reaches, else refused
//!                        3 the key: pack_key(git's identity, args with F's bytes, stdin)
//!                        4 git/serve-cache/<key>.pack, its header checked ─▶ sent
//!                        5 a miss: <key>.lock, then git pack-objects, teed to the client and
//!                          to <key>.tmp-<pid>, synced, renamed into place
//!                        6 the bound: the oldest entries removed until under it
//! ```
//!
//! An entry is a header line, `swarmops-pack 1 <blake3 of the pack> <its length>`, then the
//! pack. The length is written in [`LEN_DIGITS`] digits, so that the header is written in
//! place once the pack is: a pack is written once. The entries are a cache outside the store's
//! format: a damaged one is removed and the pack produced again, a missing one is produced, and
//! a power loss leaves whole entries or ones the header check refuses.

use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use swarmops_api::{ErrorCode, OpError, ServeService};
use swarmops_git::GitOid;
use swarmops_git::mirror::STALE_LOCK_AGE;
use swarmops_git::serve::child::{self, Wants};
use swarmops_git::serve::pack_key::{PackDigest, pack_key};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_git::transport::GitCli;

use super::refresh::{SERVE_LOCK_WAIT, serve_error};
use super::refused;
use crate::error;

/// The most the entries of one store's pack cache hold together: past it, the oldest are
/// removed, and a pack larger than it is sent and not kept.
pub const MAX_PACK_CACHE_BYTES: u64 = 4 << 30;

/// The cache's directory, beside `serve.git` in the store's `git/`.
const CACHE_DIR: &str = "serve-cache";

/// What an entry's, a producer's lock's and a temporary file's names end in, after the key.
const ENTRY_SUFFIX: &str = ".pack";
const LOCK_SUFFIX: &str = ".lock";
const TEMP_INFIX: &str = ".tmp-";

/// An entry's header: this prefix, the pack's digest, a space, its length, a newline.
const HEADER_PREFIX: &[u8] = b"swarmops-pack 1 ";
const DIGEST_HEX: usize = 64;
const LEN_DIGITS: usize = 20;
const HEADER_LEN: usize = HEADER_PREFIX.len() + DIGEST_HEX + 1 + LEN_DIGITS + 1;

/// How much of a pack is read and written at once.
const CHUNK: usize = 64 << 10;

/// Nanoseconds in a second: a modification time's unit in git's identity.
const NANOS_PER_SEC: i128 = 1_000_000_000;

/// How often a follower looks whether the producer of its key let the lock go.
const LOCK_POLL: Duration = Duration::from_millis(10);

/// The line `upload-pack` appends: git, its option naming the shallow file, the command.
const GIT: &str = "git";
const SHALLOW_FILE: &str = "--shallow-file";
const PACK_OBJECTS: &str = "pack-objects";

/// What `upload-pack` feeds `pack-objects` on stdin besides the wants: the shallow commits,
/// then what the client has after this line.
const SHALLOW_LINE: &str = "--shallow ";
const NOT_LINE: &str = "--not";

/// The variable that names the git binary a [`GitCli`] runs, and what a file begins with when
/// it is a script, which runs a git and is none.
const GIT_ENV: &str = "SWARMOPS_GIT";
const SCRIPT_MAGIC: [u8; 2] = *b"#!";

/// The test hook that bounds the cache or skips the reachability check (`bound=<bytes>`,
/// `no-reach-check`), read only in a build with the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
const SERVE_CACHE_ENV: &str = "SWARMOPS_TEST_SERVE_CACHE";

/// Whether the wants are checked against what the served refs reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Checked: a release build's only way.
    Check,
    /// Skipped, by the test hook alone: the self-check of the check.
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    Skip,
}

/// What a lookup of an entry found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// A sound entry, sent.
    Sent,
    /// No entry.
    Missing,
    /// An entry its header refuses, removed.
    Damaged,
}

/// Whether a produced pack is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Written to the cache as it is sent: this run holds its key's lock.
    Cached,
    /// Sent alone: another producer held the key's lock past the wait.
    Uncached,
}

/// The cache's settings for this run: its bound and the reachability check.
#[derive(Debug, Clone, Copy)]
struct Settings {
    /// The most the entries hold together.
    bound: u64,
    /// Whether the wants are checked.
    reach: Reach,
}

impl Settings {
    /// A release build's: [`MAX_PACK_CACHE_BYTES`], checked.
    #[cfg(not(feature = "test-hooks"))]
    fn read() -> Settings {
        Settings {
            bound: MAX_PACK_CACHE_BYTES,
            reach: Reach::Check,
        }
    }

    /// A test build's: a release build's, changed by `SWARMOPS_TEST_SERVE_CACHE`.
    #[cfg(feature = "test-hooks")]
    fn read() -> Settings {
        let mut settings = Settings {
            bound: MAX_PACK_CACHE_BYTES,
            reach: Reach::Check,
        };
        let value = std::env::var(SERVE_CACHE_ENV).unwrap_or_default();
        if value == "no-reach-check" {
            settings.reach = Reach::Skip;
        }
        if let Some(bound) = value.strip_prefix("bound=").and_then(|n| n.parse().ok()) {
            settings.bound = bound;
        }
        settings
    }
}

/// Answers `pack-objects` for `upload-pack` on `repo`: `args` the line git appended to the
/// hook, `stdin` what git fed it; the pack goes to `out`. An error comes before any byte of it.
pub(crate) fn send(
    repo: &ServeRepo,
    args: &[OsString],
    stdin: &[u8],
    out: &mut dyn Write,
) -> Result<(), OpError> {
    let settings = Settings::read();

    // 1-2. The line git appended, then the wants, each served.
    let line = pack_line(args)?;
    let git =
        GitCli::find().map_err(|e| error::transport_error(e, "serve.git", error::Phase::Fetch))?;
    let wants = wants(stdin)?;
    if settings.reach == Reach::Check && !served(&git, repo, &wants)? {
        return Err(not_served());
    }

    // 3. The key, of everything the pack depends on.
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let identity = git_identity(&git, &env)?;
    let key = request_key(&identity, line, stdin)?;

    // 4. The entry, when it is there and sound; without a cache directory, the pack alone.
    let Ok(dir) = cache_dir(repo) else {
        return produce(&git, repo, line, stdin, out, Keep::Uncached, Path::new(""));
    };
    let entry = dir.join(format!("{key}{ENTRY_SUFFIX}"));
    if look_up(&entry, out).map_err(|e| cache_failed(&e))? == Found::Sent {
        return Ok(());
    }

    // 5. A miss: this run produces it under the key's lock, or sends it alone past the wait;
    //    a producer that let the lock go may have left the entry. A lock that cannot be taken
    //    is one held.
    let lock = lock_key(&dir, &key).ok().flatten();
    if lock.is_some() && look_up(&entry, out).map_err(|e| cache_failed(&e))? == Found::Sent {
        return Ok(());
    }
    let keep = if lock.is_some() {
        Keep::Cached
    } else {
        Keep::Uncached
    };
    let temp = dir.join(format!("{key}{TEMP_INFIX}{}", std::process::id()));
    produce(&git, repo, line, stdin, out, keep, &temp)?;
    if keep == Keep::Uncached {
        return Ok(());
    }

    // 6. Kept within the bound. The pack is sent: a cache that fails to keep it costs the next
    //    identical request a miss, never this one.
    let _ = keep_entry(&temp, &entry, settings.bound);
    let _ = evict(&dir, settings.bound);
    drop(lock);
    Ok(())
}

/// Whether each of `wants` may be sent: the served tips, read in process, decide what they
/// know; git's two children what they do not. Nothing else is served.
fn served(git: &GitCli, repo: &ServeRepo, wants: &[GitOid]) -> Result<bool, OpError> {
    let checked = match child::check_tips(repo, wants).map_err(serve_error)? {
        Wants::Served => return Ok(true),
        Wants::Unknown => child::check_wants(git, repo, wants).map_err(serve_error)?,
        _ => return Ok(false),
    };
    Ok(checked == Wants::Served)
}

/// The identity of the git binary `git` runs, which the key names in place of its version: its
/// canonical path, length, modification time in nanoseconds, device and inode. A binary
/// replaced or moved has another. A script, as `SWARMOPS_GIT` may name, is no git but runs
/// one: the git that `PATH` of `env` finds is named after it, when it is another file, so
/// that an upgrade of that git changes the key though the script stays. No git is run.
fn git_identity(git: &GitCli, env: &[(OsString, OsString)]) -> Result<String, OpError> {
    // 1. The binary named.
    let unread =
        |e: io::Error| OpError::new(ErrorCode::Io, format!("git: its binary: {}", e.kind()));
    let path = fs::canonicalize(git.binary()).map_err(unread)?;
    let named = file_identity(&path).map_err(unread)?;
    if !is_script(&path) {
        return Ok(named);
    }

    // 2. Beneath a script: the git PATH finds, when there is one and it is not the script.
    let beneath = path_git(env)
        .and_then(|found| fs::canonicalize(found).ok())
        .filter(|found| *found != path)
        .and_then(|found| file_identity(&found).ok());
    Ok(match beneath {
        Some(beneath) => format!("{named}\n{beneath}"),
        None => named,
    })
}

/// The identity of the file at the canonical `path`: its device, inode, length, modification
/// time in nanoseconds and path.
fn file_identity(path: &Path) -> io::Result<String> {
    let meta = fs::metadata(path)?;
    let mtime_ns = i128::from(meta.mtime()) * NANOS_PER_SEC + i128::from(meta.mtime_nsec());
    Ok(format!(
        "{} {} {} {mtime_ns} {}",
        meta.dev(),
        meta.ino(),
        meta.len(),
        path.to_string_lossy()
    ))
}

/// Whether the file at `path` begins as a script does; one that cannot be read is none.
fn is_script(path: &Path) -> bool {
    let mut magic = [0; SCRIPT_MAGIC.len()];
    let read = File::open(path).and_then(|mut file| file.read_exact(&mut magic));
    read.is_ok() && magic == SCRIPT_MAGIC
}

/// The git `PATH` of `env` finds, whatever [`GIT_ENV`] names.
fn path_git(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    let unnamed = env.iter().filter(|(key, _)| key != GIT_ENV).cloned();
    let git = GitCli::find_with_env(unnamed).ok()?;
    Some(git.binary().to_path_buf())
}

/// The line after `git` in `args`, which must be `git [--shallow-file <file>] pack-objects`
/// and options: anything else did not come from `upload-pack`.
fn pack_line(args: &[OsString]) -> Result<&[OsString], OpError> {
    let not_pack = || {
        refused(
            ErrorCode::InvalidArg,
            "not_from_git",
            "serve --hook pack-objects runs as git's pack-objects hook: git [--shallow-file \
             <file>] pack-objects <options>",
        )
    };
    let Some((first, line)) = args.split_first() else {
        return Err(not_pack());
    };
    let command = match line {
        [option, _, rest @ ..] if option == SHALLOW_FILE => rest,
        rest => rest,
    };
    let Some((name, options)) = command.split_first() else {
        return Err(not_pack());
    };
    let optional = options
        .iter()
        .all(|option| option.as_bytes().starts_with(b"--"));
    if first != GIT || name != PACK_OBJECTS || !optional {
        return Err(not_pack());
    }
    Ok(line)
}

/// The objects `stdin` wants: its lines before `--not`, the shallow ones aside.
fn wants(stdin: &[u8]) -> Result<Vec<GitOid>, OpError> {
    let text = String::from_utf8_lossy(stdin);
    let mut wants = Vec::new();
    for line in text.lines() {
        if line == NOT_LINE {
            break;
        }
        if line.is_empty() || line.starts_with(SHALLOW_LINE) {
            continue;
        }
        let oid = line.parse::<GitOid>().map_err(|_| not_served())?;
        wants.push(oid);
    }
    Ok(wants)
}

/// What the request `args` (the line git appended) with `stdin` serves (the M17 design §6): a
/// clone when it excludes nothing (no `^<oid>` line, no have after `--not`, no shallow file),
/// else a fetch.
pub(crate) fn service_of(args: &[OsString], stdin: &[u8]) -> ServeService {
    // 1. A shallow client: git names its shallow file, empty for a client that is not.
    let shallow = args
        .windows(2)
        .any(|pair| pair[0] == SHALLOW_FILE && !pair[1].is_empty());

    // 2. An excluded commit, or a have after `--not`.
    let text = String::from_utf8_lossy(stdin);
    let excluded = text.lines().any(|line| line.starts_with('^'));
    let mut after_not = text.lines().skip_while(|line| *line != NOT_LINE).skip(1);
    let haves = after_not.any(|line| !line.is_empty());
    if shallow || excluded || haves {
        return ServeService::Fetch;
    }
    ServeService::Clone
}

/// The key of the request `line` with `stdin`, the shallow file's bytes in place of its path
/// (none when git names none, as it does for a client that is not shallow).
fn request_key(identity: &str, line: &[OsString], stdin: &[u8]) -> Result<String, OpError> {
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(line.len());
    let mut shallow_next = false;
    for arg in line {
        let bytes = arg.as_bytes();
        if !shallow_next {
            shallow_next = arg == SHALLOW_FILE;
            parts.push(bytes.to_vec());
            continue;
        }
        shallow_next = false;
        let file = match bytes {
            [] => Vec::new(),
            path => fs::read(Path::new(std::ffi::OsStr::from_bytes(path)))
                .map_err(|e| cache_failed(&e))?,
        };
        parts.push(file);
    }
    let borrowed: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Ok(pack_key(identity, &borrowed, stdin))
}

/// The cache's directory, made when it is missing.
fn cache_dir(repo: &ServeRepo) -> io::Result<PathBuf> {
    let git = repo.git_dir().parent().unwrap_or(repo.git_dir());
    let dir = git.as_std_path().join(CACHE_DIR);
    match fs::create_dir(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(dir),
    }
}

/// Sends the entry at `path` to `out` when its header holds the digest and the length of the
/// pack after it, checked before the first byte is sent, and marks it used; a damaged one is
/// removed.
fn look_up(path: &Path, out: &mut dyn Write) -> io::Result<Found> {
    // 1. The entry and its header.
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Found::Missing),
        Err(e) => return Err(e),
    };
    let mut header = [0; HEADER_LEN];
    let named = match file.read_exact(&mut header) {
        Ok(()) => parse_header(&header),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
        Err(e) => return Err(e),
    };

    // 2. The pack after it, against the header.
    let read = PackDigest::of(&mut file)?;
    if named != Some(read) {
        remove(path)?;
        return Ok(Found::Damaged);
    }

    // 3. Sent, and marked used: the bound removes the least recently used first.
    file.seek(SeekFrom::Start(header.len() as u64))?;
    io::copy(&mut file, out)?;
    let _ = file.set_modified(SystemTime::now());
    Ok(Found::Sent)
}

/// The digest and length a header names, when it is one.
fn parse_header(header: &[u8; HEADER_LEN]) -> Option<(String, u64)> {
    let rest = header.strip_prefix(HEADER_PREFIX)?;
    let (digest, rest) = rest.split_at(DIGEST_HEX);
    let (space, rest) = rest.split_at(1);
    let (len, newline) = rest.split_at(LEN_DIGITS);
    let hex = digest
        .iter()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let digits = len.iter().all(u8::is_ascii_digit);
    if !(hex && digits && space == b" " && newline == b"\n") {
        return None;
    }
    let len = std::str::from_utf8(len).ok()?.parse().ok()?;
    Some((String::from_utf8(digest.to_vec()).ok()?, len))
}

/// The header of a pack of digest `digest` and length `len`.
fn header(digest: &str, len: u64) -> Vec<u8> {
    let mut header = HEADER_PREFIX.to_vec();
    header.extend_from_slice(format!("{digest} {len:0LEN_DIGITS$}\n").as_bytes());
    header
}

/// The lock of `key`'s producer in `dir`, waited for at most [`SERVE_LOCK_WAIT`]: none past it.
fn lock_key(dir: &Path, key: &str) -> io::Result<Option<File>> {
    let path = dir.join(format!("{key}{LOCK_SUFFIX}"));
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
        if started.elapsed() >= SERVE_LOCK_WAIT {
            return Ok(None);
        }
        thread::sleep(LOCK_POLL);
    }
}

/// Runs git's `pack-objects` with `line` and `stdin` on `repo`, its pack sent to `out` and,
/// when `keep` says so, written to `temp` after a header to fill in: what git would send.
fn produce(
    git: &GitCli,
    repo: &ServeRepo,
    line: &[OsString],
    stdin: &[u8],
    out: &mut dyn Write,
    keep: Keep,
    temp: &Path,
) -> Result<(), OpError> {
    // 1. git, its stdin written beside the reading of its pack.
    let mut packer = child::pack_objects(git, repo, line).map_err(serve_error)?;
    let input = stdin.to_vec();
    let mut packer_in = packer.stdin.take();
    let writer = thread::spawn(move || {
        if let Some(stdin) = packer_in.as_mut() {
            // A git that exits early closes its stdin: its exit says why.
            let _ = stdin.write_all(&input);
        }
    });

    // 2. The pack, teed: a temporary entry that cannot be written is given up, not the client.
    let mut file = match keep {
        Keep::Cached => start_temp(temp).ok(),
        Keep::Uncached => None,
    };
    let teed = tee(packer.stdout.take(), out, &mut file);
    let status = packer.wait();
    let _ = writer.join();

    // 3. A pack whole and git content, its entry finished when it was written; else nothing
    //    kept.
    let failed = match (teed, status) {
        (Ok(digest), Ok(status)) if status.success() => {
            let finished = file.is_some_and(|file| finish_temp(file, &digest).is_ok());
            if !finished {
                let _ = remove(temp);
            }
            return Ok(());
        }
        (Err(e), _) | (_, Err(e)) => cache_failed(&e),
        (Ok(_), Ok(status)) => OpError::new(
            ErrorCode::GitRead,
            format!("serve.git: git pack-objects failed ({status})"),
        ),
    };
    let _ = packer.kill();
    drop(file);
    let _ = remove(temp);
    Err(failed)
}

/// A temporary entry at `path`, its header's place reserved.
fn start_temp(path: &Path) -> io::Result<File> {
    let mut file = File::create(path)?;
    file.write_all(&[b' '; HEADER_LEN])?;
    Ok(file)
}

/// Copies `pack` to `out` and to `file`, a chunk at a time; the digest of what it copied. A
/// write to `file` that fails drops it, and the copy to `out` goes on.
fn tee(
    pack: Option<std::process::ChildStdout>,
    out: &mut dyn Write,
    file: &mut Option<File>,
) -> io::Result<PackDigest> {
    let mut digest = PackDigest::new();
    let Some(mut pack) = pack else {
        return Err(io::Error::other("no pack from git"));
    };
    let mut chunk = vec![0; CHUNK];
    loop {
        let read = match pack.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        out.write_all(&chunk[..read])?;
        let unwritten = (file.as_mut()).is_some_and(|file| file.write_all(&chunk[..read]).is_err());
        if unwritten {
            *file = None;
        }
        digest.update(&chunk[..read]);
    }
    out.flush()?;
    Ok(digest)
}

/// Fills in the header of the temporary entry `file` with `digest`, then syncs it.
fn finish_temp(mut file: File, digest: &PackDigest) -> io::Result<()> {
    let (hex, len) = digest.finish();
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header(&hex, len))?;
    file.sync_data()
}

/// Renames the synced temporary entry `temp` into place as `entry`, unless it is larger than
/// `bound`, when it is removed: sent, not kept.
fn keep_entry(temp: &Path, entry: &Path, bound: u64) -> io::Result<()> {
    if fs::metadata(temp)?.len() > bound {
        return remove(temp);
    }
    fs::rename(temp, entry)
}

/// Removes the entries of `dir`, the least recently used first, until they hold at most
/// `bound` together; then a dead producer's temporary file, and the lock of a key that has no
/// entry and no producer.
fn evict(dir: &Path, bound: u64) -> io::Result<()> {
    // 1. The entries, by when they were last used, and what they hold.
    let mut entries = Vec::new();
    let mut others = Vec::new();
    for listed in fs::read_dir(dir)? {
        let listed = listed?;
        let Ok(meta) = listed.metadata() else {
            continue;
        };
        let name = listed.file_name().to_string_lossy().into_owned();
        if name.ends_with(ENTRY_SUFFIX) {
            entries.push((meta.modified()?, meta.len(), listed.path()));
        } else {
            others.push((name, meta.modified()?, listed.path()));
        }
    }
    entries.sort();
    let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();

    // 2. The oldest removed until under the bound.
    for (_, len, path) in &entries {
        if total <= bound {
            break;
        }
        remove(path)?;
        total -= len;
    }

    // 3. What a producer left: a temporary file not written for the mirror's stale age, a
    //    lock no producer holds whose entry is gone.
    let now = SystemTime::now();
    for (name, modified, path) in others {
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if name.contains(TEMP_INFIX) && age >= STALE_LOCK_AGE {
            remove(&path)?;
        }
        let Some(key) = name.strip_suffix(LOCK_SUFFIX) else {
            continue;
        };
        if dir.join(format!("{key}{ENTRY_SUFFIX}")).exists() {
            continue;
        }
        let Ok(file) = File::options().write(true).open(&path) else {
            continue;
        };
        if file.try_lock().is_ok() {
            remove(&path)?;
        }
    }
    Ok(())
}

/// Removes the file `path`; one already gone is no error.
fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The refusal of a want that is no commit a served ref reaches: nothing is sent.
fn not_served() -> OpError {
    refused(
        ErrorCode::PolicyDenied,
        "not_served",
        "a wanted object is no commit a served ref is or reaches: fetch a served ref",
    )
}

/// The error of the cache's directory or an entry that failed.
fn cache_failed(e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("git/{CACHE_DIR}: {}", e.kind()))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use swarmops_api::ErrorCode;
    use swarmops_git::serve::pack_key::pack_key;
    use swarmops_git::serve::repo::ServeRepo;
    use swarmops_git::transport::GitCli;

    use super::{
        Found, GIT_ENV, HEADER_LEN, Keep, PACK_OBJECTS, PackDigest, SHALLOW_FILE, evict,
        git_identity, header, keep_entry, look_up, pack_line, parse_header, produce, request_key,
        service_of, wants,
    };

    /// What a stand-in answers `git version` with: a release the transport runs.
    const STAND_IN_VERSION: &str = "git version 2.43.0";

    /// A stand-in for git at `dir/git`, a shell script of `body`; the [`GitCli`] that runs it.
    /// It answers `git version` first, as every child is checked by it before it starts.
    fn stand_in(dir: &Path, body: &str) -> GitCli {
        let program = dir.join("git");
        let version = format!("[ \"$1\" = version ] && {{ echo '{STAND_IN_VERSION}'; exit 0; }}");
        fs::write(&program, format!("#!/bin/sh\n{version}\n{body}\n")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let env = [(OsString::from(GIT_ENV), program.into_os_string())];
        GitCli::find_with_env(env).unwrap()
    }

    /// The line `upload-pack` appends is taken, with or without the shallow file; anything else
    /// is refused `not_from_git`.
    #[test]
    fn only_git_s_line_is_taken() {
        let line = |args: &[&str]| -> Vec<OsString> { args.iter().map(OsString::from).collect() };
        let taken = [
            line(&["git", "pack-objects", "--revs", "--stdout"]),
            line(&[
                "git",
                "--shallow-file",
                "",
                "pack-objects",
                "--revs",
                "--shallow",
            ]),
        ];
        for args in taken {
            assert_eq!(pack_line(&args).unwrap(), &args[1..]);
        }
        let refused = [
            line(&[]),
            line(&["git"]),
            line(&["sh", "pack-objects"]),
            line(&["git", "upload-pack", "--revs"]),
            line(&["git", "pack-objects", "--revs", "out"]),
            line(&["git", "-c", "x=y", "pack-objects"]),
        ];
        for args in refused {
            let e = pack_line(&args).unwrap_err();
            assert_eq!(e.details["reason"], "not_from_git", "{args:?}");
        }
    }

    /// The wants are the lines before `--not`, the shallow ones aside; a line that is no
    /// object id is refused.
    #[test]
    fn wants_are_before_not() {
        let a = "1".repeat(40);
        let b = "2".repeat(40);
        let stdin = format!("--shallow {a}\n{a}\n{b}\n--not\n{b}\n\n");
        let wanted: Vec<String> = wants(stdin.as_bytes())
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(wanted, [a, b]);
        assert!(wants(b"HEAD\n--not\n").is_err());
    }

    /// A header names a digest and a length in fixed widths, and only such a line is one.
    #[test]
    fn a_header_round_trips() {
        let digest = "ab".repeat(32);
        let written = header(&digest, 1234);
        assert_eq!(written.len(), HEADER_LEN);
        let fixed: [u8; HEADER_LEN] = written.clone().try_into().unwrap();
        assert_eq!(parse_header(&fixed), Some((digest, 1234)));
        let mut broken = fixed;
        broken[HEADER_LEN - 1] = b' ';
        assert_eq!(parse_header(&broken), None);
    }

    /// A sound entry is sent; one whose pack no longer matches its header is removed, not
    /// sent; none is missing.
    #[test]
    fn only_a_sound_entry_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.pack");
        let pack = b"PACK and the rest".to_vec();
        let (digest, len) = PackDigest::of(&mut pack.as_slice()).unwrap();
        fs::write(&path, [header(&digest, len), pack.clone()].concat()).unwrap();

        let mut out = Vec::new();
        assert_eq!(look_up(&path, &mut out).unwrap(), Found::Sent);
        assert_eq!(out, pack);

        let mut damaged = fs::read(&path).unwrap();
        *damaged.last_mut().unwrap() ^= 1;
        fs::write(&path, damaged).unwrap();
        let mut out = Vec::new();
        assert_eq!(look_up(&path, &mut out).unwrap(), Found::Damaged);
        assert!(out.is_empty() && !path.exists());
        assert_eq!(look_up(&path, &mut out).unwrap(), Found::Missing);
    }

    /// The bound removes the least recently used entries first, until the rest fit.
    #[test]
    fn the_oldest_entries_go_first() {
        let dir = tempfile::tempdir().unwrap();
        let at = |secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        for (name, used) in [("old.pack", 1), ("mid.pack", 2), ("new.pack", 3)] {
            let path = dir.path().join(name);
            fs::write(&path, [0; 10]).unwrap();
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(at(used)).unwrap();
        }
        evict(dir.path(), 25).unwrap();
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["mid.pack", "new.pack"]);
    }

    /// A request that excludes nothing is a clone; a have, an excluded commit or a shallow file
    /// makes it a fetch.
    #[test]
    fn a_clone_excludes_nothing() {
        let args = |shallow: &str| -> Vec<OsString> {
            ["git", "--shallow-file", shallow, "pack-objects", "--stdout"]
                .iter()
                .map(OsString::from)
                .collect()
        };
        let want = "0123456789012345678901234567890123456789";
        let have = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let clone = swarmops_api::ServeService::Clone;
        let fetch = swarmops_api::ServeService::Fetch;
        for (shallow, stdin, service) in [
            ("", format!("{want}\n--not\n\n"), clone),
            ("", format!("{want}\n"), clone),
            ("", format!("{want}\n--not\n{have}\n\n"), fetch),
            ("", format!("{want}\n^{have}\n"), fetch),
            ("shallow", format!("{want}\n--not\n\n"), fetch),
        ] {
            assert_eq!(
                service_of(&args(shallow), stdin.as_bytes()),
                service,
                "{stdin:?}"
            );
        }
    }

    /// A `pack-objects` that fails after it sent bytes leaves nothing to keep: the client got
    /// what was sent, the request fails, and the temporary entry is gone. One that succeeds
    /// leaves a sound entry, so the failure is what keeps nothing.
    #[test]
    fn a_failed_pack_objects_after_streaming_caches_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = ServeRepo::open(camino::Utf8Path::from_path(root.path()).unwrap()).unwrap();
        let line = [OsString::from(PACK_OBJECTS), OsString::from("--stdout")];
        let sent = b"PACK, then git's exit";
        let run = |exit: &str, temp: &Path| {
            let bin = tempfile::tempdir().unwrap();
            let git = stand_in(
                bin.path(),
                &format!("printf \"PACK, then git's exit\"\n{exit}"),
            );
            let mut out = Vec::new();
            let produced = produce(&git, &repo, &line, b"", &mut out, Keep::Cached, temp);
            assert_eq!(out, sent, "{exit}");
            produced
        };

        // 1. The control: git content, the temporary entry whole under its header.
        let kept = root.path().join("kept.tmp-1");
        run("exit 0", &kept).unwrap();
        let mut out = Vec::new();
        assert_eq!(look_up(&kept, &mut out).unwrap(), Found::Sent);
        assert_eq!(out, sent);

        // 2. git fails after the same bytes: an error, and no temporary entry.
        let dropped = root.path().join("dropped.tmp-1");
        let failed = run("exit 1", &dropped).unwrap_err();
        assert_eq!(failed.code, ErrorCode::GitRead.as_str(), "{failed:?}");
        assert!(!dropped.exists(), "a failed pack was kept");
    }

    /// A pack larger than the bound is sent and not kept: its temporary entry is removed, and
    /// no entry takes its place. One of exactly the bound is kept.
    #[test]
    fn a_pack_over_the_bound_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let (temp, entry) = (dir.path().join("k.tmp-1"), dir.path().join("k.pack"));
        let pack = [0; 10];

        fs::write(&temp, pack).unwrap();
        keep_entry(&temp, &entry, 9).unwrap();
        assert!(!temp.exists() && !entry.exists(), "kept past the bound");

        fs::write(&temp, pack).unwrap();
        keep_entry(&temp, &entry, 10).unwrap();
        assert!(!temp.exists());
        assert_eq!(fs::read(&entry).unwrap(), pack);
    }

    /// The key of a request holds the bytes of the shallow file, not its path, and the
    /// identity of the git binary, which stands where its version did: two requests that
    /// differ in either never share a pack, and a shallow file read again elsewhere does.
    #[test]
    fn the_key_holds_the_shallow_file_and_the_git_version() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, bytes: &str| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        let line = |shallow: &Path| -> Vec<OsString> {
            [
                OsStr::new(SHALLOW_FILE),
                shallow.as_os_str(),
                OsStr::new(PACK_OBJECTS),
            ]
            .into_iter()
            .map(OsString::from)
            .collect()
        };
        let key =
            |identity: &str, shallow: &Path| request_key(identity, &line(shallow), b"w\n").unwrap();
        let parts = |shallow: &'static [u8]| -> [&[u8]; 3] {
            [SHALLOW_FILE.as_bytes(), shallow, PACK_OBJECTS.as_bytes()]
        };

        // 1. The shallow file: its bytes, whatever its path; none for a client that is not
        //    shallow; one that cannot be read fails the request.
        let (one, same, other) = (
            file("one", "a\n"),
            file("same", "a\n"),
            file("other", "b\n"),
        );
        assert_eq!(key("git", &one), key("git", &same));
        assert_ne!(key("git", &one), key("git", &other));
        assert_eq!(key("git", &one), pack_key("git", &parts(b"a\n"), b"w\n"));
        assert_eq!(
            key("git", Path::new("")),
            pack_key("git", &parts(b""), b"w\n")
        );
        let unread = request_key("git", &line(&dir.path().join("gone")), b"w\n").unwrap_err();
        assert_eq!(unread.code, ErrorCode::Io.as_str(), "{unread:?}");

        // 2. The git binary: another identity, another key.
        assert_ne!(key("git", &one), key("another git", &one));

        // 3. Its identity: another for a binary elsewhere, and for one replaced where it was.
        let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let git = stand_in(first.path(), "exit 0");
        let before = git_identity(&git, &[]).unwrap();
        assert_eq!(before, git_identity(&git, &[]).unwrap());
        let elsewhere = stand_in(second.path(), "exit 0");
        assert_ne!(before, git_identity(&elsewhere, &[]).unwrap());
        let replaced = stand_in(first.path(), "exit 0 # replaced");
        assert_ne!(before, git_identity(&replaced, &[]).unwrap());
    }

    /// A script named in `SWARMOPS_GIT` is not the git that packs: the key also names the git
    /// `PATH` finds beneath it, so that one replaced where it was has another identity though
    /// the script stays as it is. Without a git on `PATH`, the script alone is named.
    #[test]
    fn a_wrapper_s_key_names_the_git_beneath() {
        // 1. A wrapper, and beneath it the git PATH finds.
        let (wrapper_dir, path_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let wrapper = stand_in(wrapper_dir.path(), "exec git \"$@\"");
        let path = [(OsString::from("PATH"), path_dir.path().into())];
        let alone = git_identity(&wrapper, &path).unwrap();
        assert_eq!(alone, git_identity(&wrapper, &[]).unwrap());

        // 2. The git beneath, then replaced where it was: another identity each time.
        stand_in(path_dir.path(), "exit 0");
        let beneath = git_identity(&wrapper, &path).unwrap();
        assert_ne!(alone, beneath, "the git beneath is not named");
        assert_eq!(beneath, git_identity(&wrapper, &path).unwrap());
        stand_in(path_dir.path(), "exit 0 # upgraded");
        let upgraded = git_identity(&wrapper, &path).unwrap();
        assert_ne!(beneath, upgraded, "an upgrade beneath kept the identity");

        // 3. A wrapper that is the git PATH finds names itself once.
        let own = [(OsString::from("PATH"), wrapper_dir.path().into())];
        assert_eq!(alone, git_identity(&wrapper, &own).unwrap());
    }
}
