//! `meta/lanes/`: the land queue's lane locks (the M22 core design I-4, §5): one runner per
//! target.
//!
//! ```text
//!  meta/lanes/<key>.lock     flock, try only: its holder is the target's one runner
//!  meta/lanes/<key>.groups   "pgid starttime boot_id slot" per running check group
//!                            (IPC, never synced)
//!
//!  runner A (dies) ----- holds <key>.lock, writes <key>.groups
//!  runner B ------------ try_acquire: lock taken -> takeover:
//!                          kill each group of this boot whose leader has its start time,
//!                          then clear <key>.groups
//! ```
//!
//! `<key>` is the hex of the first 16 bytes of the target's [`ref_name_key`]. The lock order is
//! a lane, then `meta/lock`: no one holds `meta/lock` while taking a lane. The files are the
//! runners' own: not the journal's projections, so the metadata check reads none of them, and
//! no store write goes through them (outside [`crate::FsOps`], like `meta/queue/`).

use std::fs::{self, DirBuilder, File, TryLockError};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::error::{FileOp, IoFailure, StoreError};
use crate::external_ids::lower_hex;
use crate::fs_ops::{self, FsMethod};
use crate::layout::Layout;
use crate::meta::frame::ref_name_key;
use crate::meta::lock::{Create, open_flock_file};

/// `meta/lanes`, relative to `meta/`.
const LANES_DIR: &str = "lanes";

/// The lanes directory's mode, as the store's other directories.
const DIR_MODE: u32 = 0o700;

/// How many bytes of the target's key name its lane files.
const KEY_BYTES: usize = 16;

/// Where Linux tells this boot's id.
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// A process's start time is field 22 of `/proc/<pid>/stat`; the fields after the
/// parenthesized name start at field 3.
const STARTTIME_AFTER_NAME: usize = 22 - 3;

/// The exclusive lane of one target, held until dropped: its holder is the target's one queue
/// runner. The kernel drops the flock of a process that dies, so a dead runner's lane is free
/// at once; what it left running is the next holder's takeover.
#[derive(Debug)]
pub struct LaneLock {
    _file: File,
    layout: Layout,
    groups: PathBuf,
}

/// One running check's process group, a line of the lane's `.groups` file: the group's leader
/// pid, its start time (clock ticks since boot, `/proc/<pid>/stat` field 22), the boot it ran
/// in and the runner's slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneGroup {
    pgid: u32,
    starttime: u64,
    boot_id: String,
    slot: u16,
}

impl LaneLock {
    /// Takes `target`'s lane on `layout`'s store without waiting: `None` while another holds
    /// it. Makes `meta/lanes/` before the lock file (`meta/` must exist). A new holder takes
    /// over first: it kills the check groups its predecessor's `.groups` names that still run
    /// (this boot, the same start time), then clears the file.
    pub fn try_acquire(layout: &Layout, target: &str) -> Result<Option<LaneLock>, StoreError> {
        // 1. The directory, then the lock file in it.
        let dir = layout.meta_path(LANES_DIR);
        let made = DirBuilder::new().mode(DIR_MODE).create(&dir);
        if let Err(e) = made
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(failure(layout, FileOp::Fs(FsMethod::CreateDirAll), &dir, e));
        }
        let key = lower_hex(&ref_name_key(target)[..KEY_BYTES]);
        let path = dir.join(format!("{key}.lock"));
        let file = open_flock_file(path.as_std_path(), Create::IfMissing)
            .map_err(|e| failure(layout, FileOp::Open, &path, e))?;

        // 2. The lock, tried once.
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(failure(layout, FileOp::Lock, &path, e)),
        }

        // 3. The takeover of what a dead holder left running.
        let lane = LaneLock {
            _file: file,
            layout: layout.clone(),
            groups: dir.join(format!("{key}.groups")).into_std_path_buf(),
        };
        lane.take_over()?;
        Ok(Some(lane))
    }

    /// The lines of the lane's `.groups` file; none when it is missing. A line that does not
    /// read is passed over: the file is written unsynced, and a torn line names no group.
    pub fn groups(&self) -> Result<Vec<LaneGroup>, StoreError> {
        let text = match fs::read_to_string(&self.groups) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.failed(FileOp::Fs(FsMethod::Read), e)),
        };
        Ok(text.lines().filter_map(LaneGroup::parse).collect())
    }

    /// Replaces the lane's `.groups` file with `groups`, one line each, through a temp and a
    /// rename, never synced. A boot id that is empty or holds white space is
    /// [`StoreError::BadCall`]: it would not read back.
    pub fn write_groups(&self, groups: &[LaneGroup]) -> Result<(), StoreError> {
        // 1. The lines, refused before anything is written if one would not read back.
        let mut text = String::new();
        for group in groups {
            if group.boot_id.is_empty() || group.boot_id.contains(char::is_whitespace) {
                return Err(StoreError::BadCall(format!(
                    "a lane group's boot id {:?} is empty or holds white space",
                    group.boot_id
                )));
            }
            text.push_str(&group.line());
        }

        // 2. Temp, then rename: a reader sees the old lines or the new, never half.
        let temp = self.groups.with_extension("groups.tmp");
        let write = fs::write(&temp, text).and_then(|()| fs::rename(&temp, &self.groups));
        write.map_err(|e| self.failed(FileOp::Fs(FsMethod::AtomicReplace), e))
    }

    /// Kills each group of the `.groups` file that still runs its recorded check, then removes
    /// the file. A line of another boot, or whose pid now names a process started at another
    /// time (a reused pid), is left alone.
    fn take_over(&self) -> Result<(), StoreError> {
        // 1. Each live group of this boot.
        let groups = self.groups()?;
        if !groups.is_empty()
            && let Some(boot) = boot_id()
        {
            for group in groups.iter().filter(|group| group.runs_in(&boot)) {
                kill_group(group.pgid);
            }
        }

        // 2. The file cleared: its groups are dead or not the lane's.
        match fs::remove_file(&self.groups) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(self.failed(FileOp::Fs(FsMethod::Remove), e))
            }
            _ => Ok(()),
        }
    }

    /// The failure of `op` on the lane's `.groups` file.
    fn failed(&self, op: FileOp, e: io::Error) -> StoreError {
        let file = fs_ops::store_relative(self.layout.root(), &self.groups);
        StoreError::Io(IoFailure::new(op, file, e))
    }
}

impl LaneGroup {
    /// The group led by `pgid`, in the runner's `slot`, as this boot sees it now: `None` when
    /// no such process runs, or the system tells no boot id (no `/proc`).
    pub fn of(pgid: u32, slot: u16) -> Option<LaneGroup> {
        Some(LaneGroup {
            pgid,
            starttime: starttime(pgid)?,
            boot_id: boot_id()?,
            slot,
        })
    }

    /// The group of these fields, as a `.groups` line holds them.
    pub fn new(pgid: u32, starttime: u64, boot_id: impl Into<String>, slot: u16) -> LaneGroup {
        LaneGroup {
            pgid,
            starttime,
            boot_id: boot_id.into(),
            slot,
        }
    }

    /// Its leader's pid, which is its process group id.
    pub fn pgid(&self) -> u32 {
        self.pgid
    }

    /// Its leader's start time, in clock ticks since boot.
    pub fn starttime(&self) -> u64 {
        self.starttime
    }

    /// The boot it ran in.
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// The runner's slot it ran in.
    pub fn slot(&self) -> u16 {
        self.slot
    }

    /// Its `.groups` line: `pgid starttime boot_id slot`.
    fn line(&self) -> String {
        format!(
            "{} {} {} {}\n",
            self.pgid, self.starttime, self.boot_id, self.slot
        )
    }

    /// The group a `.groups` line names; `None` for one that does not read.
    fn parse(line: &str) -> Option<LaneGroup> {
        let mut fields = line.split_whitespace();
        let group = LaneGroup {
            pgid: fields.next()?.parse().ok()?,
            starttime: fields.next()?.parse().ok()?,
            boot_id: fields.next()?.to_owned(),
            slot: fields.next()?.parse().ok()?,
        };
        fields.next().is_none().then_some(group)
    }

    /// Whether the group still runs what it ran: this boot's, and its leader's pid names a
    /// process started at the recorded time. Never pid 0 or 1, whatever a line says.
    fn runs_in(&self, boot: &str) -> bool {
        self.pgid > 1 && self.boot_id == boot && starttime(self.pgid) == Some(self.starttime)
    }
}

/// This boot's id; `None` where the system tells none.
fn boot_id() -> Option<String> {
    let text = fs::read_to_string(BOOT_ID).ok()?;
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// The start time of process `pid` (`/proc/<pid>/stat` field 22); `None` if it does not run.
/// The fields are read after the name's closing parenthesis: a name may hold spaces.
fn starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name
        .split_whitespace()
        .nth(STARTTIME_AFTER_NAME)?
        .parse()
        .ok()
}

/// Kills process group `pgid` with `SIGKILL`; one already gone is no failure.
#[cfg(target_os = "linux")]
fn kill_group(pgid: u32) {
    use rustix::process::{Pid, Signal};
    let Some(leader) = i32::try_from(pgid).ok().and_then(Pid::from_raw) else {
        return;
    };
    let _ = rustix::process::kill_process_group(leader, Signal::KILL);
}

/// No process group sweep off Linux yet (M12): no line reads as live there either.
#[cfg(not(target_os = "linux"))]
fn kill_group(_pgid: u32) {}

/// The failure of `op` on `path`, named relative to `layout`'s root.
fn failure(layout: &Layout, op: FileOp, path: &camino::Utf8Path, e: io::Error) -> StoreError {
    let file = fs_ops::store_relative(layout.root(), path.as_std_path());
    StoreError::Io(IoFailure::new(op, file, e))
}
