//! What a check reported, taken in once it has ended: its report file (M31-4) and the JUnit and
//! SARIF reports it declares (M31-5), read into findings within the run's bound.
//!
//! ```text
//!   before the check starts: each declared report looked at (Before::look), nothing read:
//!     absent, or a regular file's (dev, ino, size, mtime, ctime); and the wall clock
//!   the check ended, its process group gone (passed, failed, timed_out; never cancelled)
//!     ├─ <run>/checks/<check>/report.jsonl (SWARMOPS_CHECK_REPORT), if there:
//!     │    open_bounded: a regular file, no link followed, no FIFO waited on, at most 64 MiB
//!     │    ─▶ findings::read_bounded
//!     └─ each declared report, <cwd>/<path>, in declared order:
//!          cwd opened by its path, then each component openat(O_NOFOLLOW | O_DIRECTORY), the
//!          last O_NOFOLLOW | O_NONBLOCK: a link at any component, or anything but a regular
//!          file ─▶ refused not_a_file, nothing behind it read; not there ─▶ missing
//!          its five values unchanged, or its mtime more than 1 s before the start ─▶ stale
//!          else at most 64 MiB ─▶ junit::read_keeping | sarif::read_keeping
//!   Intake: the findings, each through the run's Budget; every finding counted; a
//!           CheckReportReadOut per report
//! ```
//!
//! A report that cannot be read is a state of that report, never the run's error: the check's
//! outcome stays the answer. Nothing here writes, the tree checked is never touched, and no store
//! is opened.

use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use swarmops_api::{
    CheckFindingOut, CheckReportFault, CheckReportFormat, CheckReportOut, CheckReportReadOut,
    CheckReportState,
};

use super::findings::{Budget, read_bounded};
use super::{MAX_REPORT_BYTES, Opened, junit, open_bounded, sarif};
use crate::error::one_line;

/// How much earlier than the check's start a declared report's time may be and still be this
/// run's: a file system's clock may be coarser than the wall clock's.
pub const REPORT_FRESH_SLACK: Duration = Duration::from_secs(1);

/// How the check's directory is opened: as its path names it, links followed, as the check ran
/// there.
const CWD_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// How a directory below it is opened: never through a link, never blocking.
const DIR_FLAGS: OFlags = CWD_FLAGS.union(OFlags::NOFOLLOW).union(OFlags::NONBLOCK);

/// How a report is opened: never through a link, never blocking on a FIFO.
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// What one check's reports gave.
#[derive(Debug, Default)]
pub(crate) struct Intake {
    /// The findings the run returns, in order.
    pub(crate) findings: Vec<CheckFindingOut>,
    /// Every finding read, returned or not.
    pub(crate) total: u64,
    /// Each report, as it was read: the report file first, when there is one.
    pub(crate) reports: Vec<CheckReportReadOut>,
}

impl Intake {
    /// Takes in the `found` findings of report number `index`, of which `findings` were kept,
    /// each through `budget`; those not kept are counted as not returned.
    fn add(
        &mut self,
        index: usize,
        findings: Vec<CheckFindingOut>,
        found: u64,
        budget: &mut Budget,
    ) {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let kept = u64::try_from(findings.len()).unwrap_or(u64::MAX);
        self.total += found;
        for mut finding in findings {
            finding.report = index;
            if budget.take(&finding) {
                self.findings.push(finding);
            }
        }
        budget.skip(found.saturating_sub(kept));
    }
}

/// Takes in what a check that ended reported in its report file at `report`, through the run's
/// `budget`. A check that wrote no report file has no report.
pub(crate) fn take_in(report: &Utf8Path, budget: &mut Budget) -> Intake {
    let mut intake = Intake::default();
    let refused = |fault, message: String| {
        let mut out = CheckReportReadOut::new(
            CheckReportFormat::Findings,
            report.as_str(),
            CheckReportState::Refused,
        );
        out.fault = Some(fault);
        out.message = Some(one_line(message));
        out
    };
    match open_bounded(report, MAX_REPORT_BYTES) {
        Ok(Opened::Missing) => {}
        Ok(Opened::NotAFile) => intake.reports.push(refused(
            CheckReportFault::NotAFile,
            "not a regular file: a symbolic link, a directory, a FIFO or another kind of entry, \
             not read"
                .to_owned(),
        )),
        Err(e) => intake.reports.push(refused(
            CheckReportFault::Io,
            format!("it cannot be read: {e}"),
        )),
        Ok(Opened::File { bytes, .. }) => {
            let limit = usize::try_from(MAX_REPORT_BYTES).unwrap_or(usize::MAX);
            let read = read_bounded(&bytes, limit, budget.room());
            drop(bytes);
            let mut out = CheckReportReadOut::new(
                CheckReportFormat::Findings,
                report.as_str(),
                CheckReportState::Read,
            );
            out.findings = Some(read.found);
            out.invalid_lines = read.invalid;
            out.cut = read.cut;
            out.message = match (read.first_invalid, read.cut) {
                (first, false) => first,
                (Some(first), true) => Some(format!("{first}; {}", cut_words())),
                (None, true) => Some(cut_words()),
            };
            let index = intake.reports.len();
            intake.reports.push(out);
            intake.add(index, read.findings, read.found, budget);
        }
    }
    intake
}

/// What identifies a declared report's file and its last change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// Its device and inode.
    id: (u64, u64),
    /// Its size.
    size: u64,
    /// Its modification time: seconds and nanoseconds since the epoch.
    mtime: (i64, i64),
    /// Its status change time.
    ctime: (i64, i64),
}

impl Stamp {
    /// The stamp of `meta`.
    fn of(meta: &std::fs::Metadata) -> Stamp {
        Stamp {
            id: (meta.dev(), meta.ino()),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// Whether its modification time is more than [`REPORT_FRESH_SLACK`] before `started`.
    fn older_than(&self, started: SystemTime) -> bool {
        let Some(edge) = started.checked_sub(REPORT_FRESH_SLACK) else {
            return false;
        };
        let edge = edge.duration_since(UNIX_EPOCH).unwrap_or_default();
        let (secs, nanos) = self.mtime;
        let edge_secs = i64::try_from(edge.as_secs()).unwrap_or(i64::MAX);
        let edge_nanos = i64::from(edge.subsec_nanos());
        (secs, nanos) < (edge_secs, edge_nanos)
    }
}

/// A check's declared reports, as they were just before it started, and when that was.
#[derive(Debug, Clone)]
pub(crate) struct Before {
    /// Each report's stamp, in declared order: none when it was not a regular file there.
    seen: Vec<Option<Stamp>>,
    /// The wall clock then.
    started: SystemTime,
}

impl Before {
    /// Looks at each of `reports` below the check's directory `cwd`, as [`take_in`] will after
    /// the check, reading none: so that a report left from before is never taken as this run's.
    pub(crate) fn look(cwd: &Utf8Path, reports: &[CheckReportOut]) -> Before {
        let seen = (reports.iter())
            .map(|report| match find(cwd, &report.path) {
                Ok(Found::File(fd)) => std::fs::File::from(fd)
                    .metadata()
                    .ok()
                    .map(|meta| Stamp::of(&meta)),
                _ => None,
            })
            .collect();
        Before {
            seen,
            started: SystemTime::now(),
        }
    }
}

/// What is at a declared report's path.
enum Found {
    /// Nothing.
    Missing,
    /// A symbolic link at one of its components, or something other than a regular file.
    NotAFile,
    /// A regular file, opened.
    File(OwnedFd),
}

/// What is at `path` (a repository path) below `cwd`: `cwd` opened as it is, then each
/// component, never through a link and never blocking; the file opened only once it is seen to
/// be a regular file, and then only if the opened one is it.
fn find(cwd: &Utf8Path, path: &str) -> io::Result<Found> {
    // 1. The directories, each by a handle of the one above it.
    let mut dir = rustix::fs::open(cwd.as_std_path(), CWD_FLAGS, Mode::empty())?;
    let names: Vec<&str> = path.split('/').collect();
    let Some((last, parents)) = names.split_last() else {
        return Ok(Found::Missing);
    };
    for name in parents {
        dir = match rustix::fs::openat(&dir, *name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Found::Missing),
            Err(e) if is_link(e) => return Ok(Found::NotAFile),
            Err(Errno::NOTDIR) => return Ok(not_a_dir(&dir, name)),
            Err(e) => return Err(e.into()),
        };
    }

    // 2. The file: a regular one, seen without following, then opened as it is.
    let seen = match rustix::fs::statat(&dir, *last, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(Found::Missing),
        Err(e) => return Err(e.into()),
    };
    if FileType::from_raw_mode(seen.st_mode) != FileType::RegularFile {
        return Ok(Found::NotAFile);
    }
    let fd = match rustix::fs::openat(&dir, *last, FILE_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(Found::Missing),
        Err(e) if is_link(e) => return Ok(Found::NotAFile),
        Err(e) => return Err(e.into()),
    };
    let opened = rustix::fs::fstat(&fd)?;
    let same = opened.st_dev == seen.st_dev && opened.st_ino == seen.st_ino;
    if !same || FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile {
        return Ok(Found::NotAFile);
    }
    Ok(Found::File(fd))
}

/// Whether an `O_NOFOLLOW` open failed on a symbolic link (`ELOOP`; `EMLINK` on FreeBSD).
fn is_link(e: Errno) -> bool {
    e == Errno::LOOP || e == Errno::MLINK
}

/// What `name` in `dir` is, which would not open as a directory: a link is not followed; a file
/// where a directory should be leaves the report missing.
fn not_a_dir(dir: &OwnedFd, name: &str) -> Found {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => Found::NotAFile,
        _ => Found::Missing,
    }
}

/// Takes in each of `reports`, declared by a check that ran in `cwd` (its `dir` in the tree),
/// as `before` saw them, into `intake`, in declared order, through the run's `budget`.
pub(crate) fn take_declared(
    intake: &mut Intake,
    cwd: &Utf8Path,
    dir: &str,
    reports: &[CheckReportOut],
    before: &Before,
    budget: &mut Budget,
) {
    for (at, report) in reports.iter().enumerate() {
        let index = intake.reports.len();
        let mut out =
            CheckReportReadOut::new(report.format, report.path.as_str(), CheckReportState::Read);
        let refuse = |out: &mut CheckReportReadOut, fault, message: String| {
            out.state = CheckReportState::Refused;
            out.fault = Some(fault);
            out.message = Some(one_line(message));
        };

        // 1. There, a regular file, written by this run.
        let fd = match find(cwd, &report.path) {
            Ok(Found::Missing) => {
                out.state = CheckReportState::Missing;
                intake.reports.push(out);
                continue;
            }
            Ok(Found::NotAFile) => {
                refuse(&mut out, CheckReportFault::NotAFile, NOT_A_FILE.to_owned());
                intake.reports.push(out);
                continue;
            }
            Err(e) => {
                refuse(
                    &mut out,
                    CheckReportFault::Io,
                    format!("it cannot be read: {e}"),
                );
                intake.reports.push(out);
                continue;
            }
            Ok(Found::File(fd)) => fd,
        };
        let mut file = std::fs::File::from(fd);
        let stamp = match file.metadata() {
            Ok(meta) => Stamp::of(&meta),
            Err(e) => {
                refuse(
                    &mut out,
                    CheckReportFault::Io,
                    format!("it cannot be read: {e}"),
                );
                intake.reports.push(out);
                continue;
            }
        };
        let unchanged = before.seen.get(at).copied().flatten() == Some(stamp);
        if unchanged || stamp.older_than(before.started) {
            out.state = CheckReportState::Stale;
            intake.reports.push(out);
            continue;
        }

        // 2. At most the bound, then read as its format.
        let mut bytes = Vec::new();
        if let Err(e) = (&mut file)
            .take(MAX_REPORT_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            refuse(
                &mut out,
                CheckReportFault::Io,
                format!("it cannot be read: {e}"),
            );
            intake.reports.push(out);
            continue;
        }
        drop(file);
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_REPORT_BYTES {
            let what = format!("past {} MiB: not read", MAX_REPORT_BYTES >> 20);
            refuse(&mut out, CheckReportFault::TooLarge, what);
            intake.reports.push(out);
            continue;
        }
        let read = match report.format {
            CheckReportFormat::Sarif => {
                let place = sarif::Place::new(dir, Some(cwd.as_str()));
                sarif::read_keeping(&bytes, place, budget.room()).map(|read| {
                    out.results = Some(read.results);
                    (read.findings, read.found)
                })
            }
            _ => junit::read_keeping(&bytes, dir, budget.room()).map(|read| {
                out.tests = Some(read.tests);
                out.failed = Some(read.failed);
                out.skipped = Some(read.skipped);
                (read.findings, read.failed)
            }),
        };
        drop(bytes);
        match read {
            Ok((findings, found)) => {
                out.findings = Some(found);
                intake.reports.push(out);
                intake.add(index, findings, found, budget);
            }
            Err((fault, message)) => {
                refuse(&mut out, fault, message);
                intake.reports.push(out);
            }
        }
    }
}

/// What a report that is not a regular file says.
const NOT_A_FILE: &str = "not a regular file: a symbolic link at a component of its path, a \
                          directory, a FIFO or another kind of entry, not read";

/// What the message of a report file past its bound says.
fn cut_words() -> String {
    format!(
        "past {} MiB: its lines after that were not read",
        MAX_REPORT_BYTES >> 20
    )
}

#[cfg(test)]
mod tests;
