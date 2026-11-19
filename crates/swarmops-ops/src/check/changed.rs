//! The changed paths a plan reads (M30-4): from the store's diff, or from a list the caller
//! gives, matched as they come and never kept.
//!
//! ```text
//!   the store:  diff base → rev, a page at a time ─┐
//!   a list:     entries ended by \n, or by NUL ────┼─▶ Changed::path ─▶ see(path)
//!   --changed:  one path each ─────────────────────┘        │
//!                                                           ▼
//!        not a repository path, over 4,096 bytes ─▶ path_unreadable   ┐
//!        (a line: quoted, or a control character)                     │  the plan is widened:
//!        more than 100,000 entries, a list past 16 MiB ─▶ paths_truncated   every check runs
//!        no path given at all ─▶ paths_unknown                        ┘
//! ```
//!
//! A [`Changed`] is what reading them came to: how many paths were read, and the first cause,
//! if any, for which they cannot be trusted to be all of them. A plan that holds a cause runs
//! every check: an unknown, cut or unreadable list never plans a check as not needed.

use std::fs::File;
use std::io::Read;

use camino::Utf8Path;
use swarmops_api::{CheckPlanWidened, Diff, DiffFormat, DiffReq, ErrorCode, OpError, StoreOps};
use swarmops_core::RepoPath;

use super::{
    CHECK_FILE, LEGACY_FILE, MAX_PLAN_LIST_BYTES, MAX_PLAN_PATH_BYTES, MAX_PLAN_PATHS, SWARMOPS_DIR,
};
use crate::error;

/// The paths whose change is a change of a tree's checks: the check file, the legacy file,
/// and the directory both are in (changed itself when it becomes, or stops being, a file or a
/// link).
pub const CHECK_FILE_PATHS: [&str; 3] = [CHECK_FILE, LEGACY_FILE, SWARMOPS_DIR];

/// The files of a page of the store's diff a plan reads at once.
pub(crate) const PLAN_DIFF_PAGE: usize = 1_000;

/// What ends an entry of a list of changed paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Sep {
    /// A line feed: one path a line. A path a tool would quote cannot be told from its quoted
    /// form, so a line that starts with `"` or holds a control character is unreadable.
    Line,
    /// A NUL byte: any repository path reads.
    Nul,
}

/// What reading a change's paths came to: how many were read, whether the check file is among
/// them, and the first cause, if any, that widens the plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changed {
    /// The changed paths read: each a repository path within its bound.
    count: u64,
    /// The entries taken, readable or not: what [`MAX_PLAN_PATHS`] bounds.
    entries: u64,
    /// Whether one of [`CHECK_FILE_PATHS`] was read.
    check_file: bool,
    /// The first cause that holds, in the causes' own order.
    cause: Option<CheckPlanWidened>,
}

impl Changed {
    /// A change of no path: nothing read yet, nothing widened.
    pub fn none() -> Changed {
        Changed::default()
    }

    /// A change whose paths are not known: `paths_unknown`.
    pub fn unknown() -> Changed {
        let mut changed = Changed::default();
        changed.widen(CheckPlanWidened::PathsUnknown);
        changed
    }

    /// The changed paths read.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Whether the check file, the legacy file or `.swarmops` itself was among them.
    pub fn check_file(&self) -> bool {
        self.check_file
    }

    /// Why the plan is widened: the first cause that holds, in the causes' own order. None
    /// when every path was read.
    pub fn cause(&self) -> Option<CheckPlanWidened> {
        self.cause
    }

    /// Widens the plan for `cause`; the first cause in the causes' own order stays the one
    /// named, whatever order they were found in.
    pub fn widen(&mut self, cause: CheckPlanWidened) {
        self.cause = Some(self.cause.map_or(cause, |held| held.min(cause)));
    }

    /// Reads one changed path: `see` is given it when it is a repository path of at most
    /// [`MAX_PLAN_PATH_BYTES`]; any other is `path_unreadable`. `false` once more than
    /// [`MAX_PLAN_PATHS`] entries came (`paths_truncated`): nothing more is read.
    pub fn path(&mut self, path: &str, see: &mut dyn FnMut(&str)) -> bool {
        self.entry(path.as_bytes(), Sep::Nul, see)
    }

    /// Reads the list `bytes`, its entries ended by `sep`: a last entry without its end is
    /// taken, an empty entry passed over. A list past [`MAX_PLAN_LIST_BYTES`] is read to that
    /// bound, its last partial entry left out, and is `paths_truncated`.
    pub fn list(&mut self, bytes: &[u8], sep: Sep, see: &mut dyn FnMut(&str)) {
        // 1. At most the bound: what is past it is not read, and says so.
        let limit = usize::try_from(MAX_PLAN_LIST_BYTES).unwrap_or(usize::MAX);
        let cut = bytes.len() > limit;
        let bytes = &bytes[..bytes.len().min(limit)];

        // 2. Each entry, to the last end; then what follows it, unless the list was cut there.
        let end = match sep {
            Sep::Line => b'\n',
            Sep::Nul => 0,
        };
        let mut rest = bytes;
        while let Some(at) = rest.iter().position(|&b| b == end) {
            if !self.entry(&rest[..at], sep, see) {
                return;
            }
            rest = &rest[at + 1..];
        }
        if cut {
            self.widen(CheckPlanWidened::PathsTruncated);
            return;
        }
        self.entry(rest, sep, see);
    }

    /// What reading the list `bytes` alone comes to ([`Changed::list`]). The fuzz target's
    /// entry.
    pub fn from_list(bytes: &[u8], sep: Sep, see: &mut dyn FnMut(&str)) -> Changed {
        let mut changed = Changed::default();
        changed.list(bytes, sep, see);
        changed
    }

    /// Reads the files that differ from changeset `from` to changeset `to`, both full ids, in
    /// path order: those of `first`, a first page already read, if any, then every later page
    /// of the store's diff, 1,000 names at a time (`PLAN_DIFF_PAGE`), to its end or to the
    /// bound. A changed path is every file of the diff, whatever its status.
    pub fn store(
        &mut self,
        store: &dyn StoreOps,
        from: &str,
        to: &str,
        first: Option<Diff>,
        see: &mut dyn FnMut(&str),
    ) -> Result<(), OpError> {
        let page_after = |cursor: Option<String>| {
            let mut req = DiffReq::new(from, to);
            req.format = DiffFormat::Names;
            req.limit = PLAN_DIFF_PAGE;
            req.cursor = cursor;
            store.diff(req)
        };
        let mut page = match first {
            Some(page) => page,
            None => page_after(None)?,
        };
        loop {
            for file in &page.files {
                if !self.path(&file.path, see) {
                    return Ok(());
                }
            }
            let Some(cursor) = page.cursor.filter(|_| page.truncated) else {
                return Ok(());
            };
            page = page_after(Some(cursor))?;
        }
    }

    /// What reading the store's diff from `from` to `to` alone comes to ([`Changed::store`]).
    pub fn from_store(
        store: &dyn StoreOps,
        from: &str,
        to: &str,
        first: Option<Diff>,
        see: &mut dyn FnMut(&str),
    ) -> Result<Changed, OpError> {
        let mut changed = Changed::default();
        changed.store(store, from, to, first, see)?;
        Ok(changed)
    }

    /// One entry as it came: passed over when empty, counted against the bound, then read as a
    /// path or found unreadable. `false` once the bound is passed.
    fn entry(&mut self, entry: &[u8], sep: Sep, see: &mut dyn FnMut(&str)) -> bool {
        if entry.is_empty() {
            return true;
        }

        // 1. Within the bound, or the plan is cut here.
        if self.entries == MAX_PLAN_PATHS {
            self.widen(CheckPlanWidened::PathsTruncated);
            return false;
        }
        self.entries += 1;

        // 2. A repository path, as it is; in a list of lines, not one a tool wrote quoted.
        let Some(path) = readable(entry, sep) else {
            self.widen(CheckPlanWidened::PathUnreadable);
            return true;
        };
        self.count += 1;
        self.check_file |= CHECK_FILE_PATHS.contains(&path);
        see(path);
        true
    }
}

/// `entry` as a changed path: UTF-8, at most [`MAX_PLAN_PATH_BYTES`], a repository path (not
/// absolute, no empty, `.` or `..` component, no NUL); and, in a list of lines, not starting
/// with `"` and holding no control character, since a tool that writes one name a line quotes
/// such names, and a quoted name read as a path would match nothing.
fn readable(entry: &[u8], sep: Sep) -> Option<&str> {
    if entry.len() > MAX_PLAN_PATH_BYTES {
        return None;
    }
    let path = std::str::from_utf8(entry).ok()?;
    let quoted = || path.starts_with('"') || path.bytes().any(|b| b < 0x20 || b == 0x7f);
    if sep == Sep::Line && quoted() {
        return None;
    }
    RepoPath::new(path).ok()?;
    Some(path)
}

/// The bytes of the caller's list of changed paths at `path`, at most one past
/// [`MAX_PLAN_LIST_BYTES`] (so that more than the bound tells a list over it). The file is
/// opened as the caller named it: a link is followed and a FIFO read, since the caller vouches
/// for it. One that does not open or read is `E_IO`, `details.path` as given and
/// `details.vouched: true`: an error, never a plan.
pub fn read_from(path: &Utf8Path) -> Result<Vec<u8>, OpError> {
    let read = File::open(path).and_then(read_bounded);
    read.map_err(|e| unread(path.as_str(), &e))
}

/// The bytes of `list`, at most one past [`MAX_PLAN_LIST_BYTES`]: how a list is read from a
/// file or from standard input.
pub fn read_bounded(list: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    list.take(MAX_PLAN_LIST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// `E_IO` for the list of changed paths named `name`, which failed with `e`.
pub fn unread(name: &str, e: &std::io::Error) -> OpError {
    let mut error = error::error(
        ErrorCode::Io,
        format!(
            "the list of changed paths {name:?} cannot be read: {}",
            e.kind()
        ),
    )
    .with_hint("name a file of changed paths, one a line, or NUL-ended with --nul; - is stdin");
    error.details.insert("path".to_owned(), name.into());
    error.details.insert("vouched".to_owned(), true.into());
    error
}

#[cfg(test)]
mod tests;
