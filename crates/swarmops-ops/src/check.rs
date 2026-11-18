//! A tree's checks (M30): `check list`, from the store at a changeset or from a directory the
//! caller names.
//!
//! ```text
//!   store:  rev ─ resolve once ─ show: root tree ─ read_file .swarmops/checks.toml
//!   --dir:  DIR ─ <DIR>/.swarmops/checks.toml (no link followed) ───────┤
//!                                                                       ▼
//!          the file ─▶ file::parse: version 1, [check.<name>] ─▶ sorted, each digested
//!          no file  ─▶ .swarmops/workspaces.json ─▶ file::parse_legacy: check.argv as default
//!          neither  ─▶ no check; `.swarmops` a symbolic link ─▶ not_a_file, in both forms
//! ```
//!
//! Both forms answer [`CheckListResp`]: the store's `vouched: false` at its `tree`, a
//! directory's `vouched: true`, since only the caller stands behind what it holds. A file that
//! does not read is `E_CHECK_FILE` ([`Refusal::into_error`]). Nothing here runs a check or
//! writes anything.

mod digest;
mod file;

use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::MetadataExt as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use camino::Utf8Path;
use serde_json::Value;
use swarmops_api::{
    CheckListReq, CheckListResp, ContentEncoding, ErrorCode, FileKind, OpError, ReadFileReq,
    ResolveRevReq, ShowReq, StoreOps,
};

pub use self::file::{Check, CheckFile, LEGACY_CHECK, ROOT_DIR, Reason, Refusal};
pub use self::file::{parse, parse_legacy};
use crate::error;

/// The check file, at a tree's root.
pub const CHECK_FILE: &str = ".swarmops/checks.toml";

/// The workspaces' settings, whose `check.argv` is the one check of a tree without
/// [`CHECK_FILE`].
pub const LEGACY_FILE: &str = ".swarmops/workspaces.json";

/// The only `version` of [`CHECK_FILE`] this build reads.
pub const CHECK_FILE_VERSION: u32 = 1;

/// The most bytes [`CHECK_FILE`] may hold: 1 MiB.
pub const MAX_CHECK_FILE_BYTES: u64 = 1 << 20;

/// The most checks [`CHECK_FILE`] may hold.
pub const MAX_CHECKS: usize = 1_000;

/// The most bytes of a check's name.
pub const MAX_CHECK_NAME_BYTES: usize = 64;

/// The most strings of a check's `argv`.
pub const MAX_ARGV_ITEMS: usize = 1_024;

/// The most bytes of a check's `argv`, its strings together: 64 KiB.
pub const MAX_ARGV_BYTES: u64 = 64 * 1024;

/// The most seconds a check's `timeout_s` may give: 24 hours.
pub const MAX_CHECK_TIMEOUT_S: u64 = 86_400;

/// The most bytes of [`LEGACY_FILE`] read, as a workspace reads it.
pub(crate) const MAX_LEGACY_BYTES: u64 = 64 * 1024;

/// The directory the check files are in, below a tree's root.
const SWARMOPS_DIR: &str = ".swarmops";

/// `check list`: the checks the tree of `req.rev` describes, read from the store. The revision
/// is resolved once, and both files are read at that changeset.
pub fn list(store: &dyn StoreOps, req: CheckListReq) -> Result<CheckListResp, OpError> {
    // 1. The changeset, once, and its root tree.
    let id = store.resolve_rev(ResolveRevReq::new(req.rev))?.id;
    let mut show = ShowReq::new(id.as_str());
    show.max_changes = 1;
    let tree = store.show(show)?.root;

    // 2. The check file, or else the legacy one; with neither, a `.swarmops` that is a link,
    //    through which neither path is found, is refused as the directory form refuses it.
    let (file, version, checks) = match stored(store, &id, CHECK_FILE, MAX_CHECK_FILE_BYTES)? {
        Some(bytes) => {
            let read = parse(&bytes).map_err(|refusal| refusal.into_error(false))?;
            (Some(CHECK_FILE), Some(read.version), read.checks)
        }
        None => match stored(store, &id, LEGACY_FILE, MAX_LEGACY_BYTES)? {
            Some(bytes) => legacy(&bytes, false)?,
            None if linked_in_store(store, &id)? => {
                return Err(Refusal::linked_dir(CHECK_FILE).into_error(false));
            }
            None => (None, None, Vec::new()),
        },
    };
    let mut resp = answer(false, file, version, &checks);
    resp.tree = Some(tree);
    Ok(resp)
}

/// `check list --dir`: the checks the files below `dir`, a directory of the caller's host,
/// describe. The answer is `vouched`: only the caller stands behind them. Neither
/// `.swarmops` nor a file in it is followed if it is a symbolic link.
pub fn list_dir(dir: &Utf8Path) -> Result<CheckListResp, OpError> {
    // 1. A directory.
    if !std::fs::metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        let mut error = error::error(
            ErrorCode::InvalidArg,
            error::one_line(format!("--dir {dir} names no directory")),
        )
        .with_hint("name the directory whose .swarmops/checks.toml to list: --dir .");
        error.details.insert("reason".to_owned(), "dir".into());
        error.details.insert("dir".to_owned(), dir.as_str().into());
        return Err(error);
    }

    // 2. `.swarmops`, never through a link; then the check file, or else the legacy one.
    let swarmops = dir.join(SWARMOPS_DIR);
    let held = match std::fs::symlink_metadata(&swarmops) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Refusal::linked_dir(CHECK_FILE).into_error(true));
        }
        Ok(meta) => meta.is_dir(),
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(host_failed(SWARMOPS_DIR, &e)),
    };
    let read = |name: &'static str, limit| {
        if held {
            on_host(dir, name, limit)
        } else {
            Ok(None)
        }
    };
    let (file, version, checks) = match read(CHECK_FILE, MAX_CHECK_FILE_BYTES)? {
        Some(bytes) => {
            let read = parse(&bytes).map_err(|refusal| refusal.into_error(true))?;
            (Some(CHECK_FILE), Some(read.version), read.checks)
        }
        None => match read(LEGACY_FILE, MAX_LEGACY_BYTES)? {
            Some(bytes) => legacy(&bytes, true)?,
            None => (None, None, Vec::new()),
        },
    };
    Ok(answer(true, file, version, &checks))
}

/// What a file and its version gave: the file's name, its version, its checks.
type Listed = (Option<&'static str>, Option<u32>, Vec<Check>);

/// The legacy file's `bytes`, read: its one check, or none.
fn legacy(bytes: &[u8], vouched: bool) -> Result<Listed, OpError> {
    let check = parse_legacy(bytes).map_err(|refusal| refusal.into_error(vouched))?;
    Ok(match check {
        Some(check) => (Some(LEGACY_FILE), None, vec![check]),
        None => (None, None, Vec::new()),
    })
}

/// The answer: `checks` from `file` at `version`, each with its digest.
fn answer(
    vouched: bool,
    file: Option<&str>,
    version: Option<u32>,
    checks: &[Check],
) -> CheckListResp {
    let mut resp = CheckListResp::new(vouched, checks.iter().map(Check::out).collect());
    resp.file = file.map(str::to_owned);
    resp.version = version;
    resp
}

/// Whether `.swarmops` at changeset `id` is a symbolic link: a path below it reads as missing
/// then, since the store follows no link.
fn linked_in_store(store: &dyn StoreOps, id: &str) -> Result<bool, OpError> {
    let mut req = ReadFileReq::new(id, SWARMOPS_DIR);
    req.max_bytes = 1;
    match store.read_file(req) {
        Ok(read) => Ok(read.kind == FileKind::Symlink),
        Err(e)
            if e.code == ErrorCode::PathNotFound.as_str()
                || e.code == ErrorCode::IsDir.as_str() =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// The bytes of `path` at changeset `id`, at most `limit` of them; none when the tree holds no
/// such path. A link or a directory there is `not_a_file`, a file over `limit` `too_large`.
fn stored(
    store: &dyn StoreOps,
    id: &str,
    path: &'static str,
    limit: u64,
) -> Result<Option<Vec<u8>>, OpError> {
    // 1. At most the limit, so that a larger file is never read whole.
    let mut req = ReadFileReq::new(id, path);
    req.max_bytes = limit;
    let read = match store.read_file(req) {
        Ok(read) => read,
        Err(e) if e.code == ErrorCode::PathNotFound.as_str() => return Ok(None),
        Err(e) if e.code == ErrorCode::IsDir.as_str() => {
            return Err(Refusal::not_a_file(path).into_error(false));
        }
        Err(e) => return Err(e),
    };

    // 2. A file of the limit or fewer bytes, as its bytes.
    if read.kind == FileKind::Symlink {
        return Err(Refusal::not_a_file(path).into_error(false));
    }
    if read.size > limit || read.truncated {
        return Err(Refusal::too_large(path, read.size, limit).into_error(false));
    }
    match read.encoding {
        ContentEncoding::Base64 => STANDARD.decode(read.content).map(Some).map_err(|e| {
            error::error(
                ErrorCode::Internal,
                format!("{path} came back as base64 that does not decode: {e}"),
            )
        }),
        _ => Ok(Some(read.content.into_bytes())),
    }
}

/// The bytes of `<dir>/<name>`, at most `limit` of them, read on a handle of its own; none when
/// nothing is there. A link, a directory or anything but a file there is `not_a_file`, a file
/// over `limit` `too_large`.
fn on_host(dir: &Utf8Path, name: &'static str, limit: u64) -> Result<Option<Vec<u8>>, OpError> {
    let path = dir.join(name);
    let failed = |e: &std::io::Error| host_failed(name, e);

    // 1. A file, not followed if it is a link: a FIFO would block the open.
    let seen = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Err(Refusal::not_a_file(name).into_error(true)),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed(&e)),
    };

    // 2. The same file opened: a link put there since is refused, not followed.
    let file = std::fs::File::open(&path).map_err(|e| failed(&e))?;
    let opened = file.metadata().map_err(|e| failed(&e))?;
    if !opened.is_file() || (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
        return Err(Refusal::not_a_file(name).into_error(true));
    }

    // 3. At most one byte past the limit, which tells a file over it.
    let mut bytes = Vec::new();
    (file.take(limit.saturating_add(1)))
        .read_to_end(&mut bytes)
        .map_err(|e| failed(&e))?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > limit {
        let size = opened.len().max(read);
        return Err(Refusal::too_large(name, size, limit).into_error(true));
    }
    Ok(Some(bytes))
}

/// `E_IO` for `name`, below the directory the caller named, that could not be read: the
/// message names it relative to that directory, never by the host's whole path.
fn host_failed(name: &str, e: &std::io::Error) -> OpError {
    let mut error = error::error(
        ErrorCode::Io,
        error::one_line(format!("{name} cannot be read: {}", e.kind())),
    );
    error.details.insert("path".to_owned(), Value::from(name));
    error
}

#[cfg(test)]
mod tests;
