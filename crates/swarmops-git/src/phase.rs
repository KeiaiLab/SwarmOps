//! The phase log's git side (the M18 design §2.2), a test hook: under the `test-hooks` feature,
//! the time and the count of the export oid derive (`export.derive`: changesets, trees and blobs
//! derived), of the derived store's memo reads and writes (`export.memo`) and of the import's
//! conversion (`import.convert`: commits converted), added up in the process and written by
//! `write_phases` as `<phase> <microseconds> <count>` lines appended to the file
//! `SWARMOPS_TEST_PHASE_LOG` names. A phase written once a run, whatever the number of calls
//! that made it up: the export oids are derived by the fetch's import, by the sync and by the
//! push's export in one `sync`. Without the feature every item here is a no-op, and a release
//! build reads no such variable.
//!
//! ```text
//!   Span::start(ExportDerive) ── nested span of the same phase: no time of its own
//!     derived()                  one object derived, inside a derive span only
//!     Span::start(ExportMemo) ── one memo read or written
//!   write_phases()               the totals appended, then reset (the ops crate calls it)
//! ```

#[cfg(feature = "test-hooks")]
use std::cell::Cell;
#[cfg(feature = "test-hooks")]
use std::fmt::Write as _;
#[cfg(feature = "test-hooks")]
use std::io::Write as _;
#[cfg(feature = "test-hooks")]
use std::sync::{Mutex, OnceLock, PoisonError};
#[cfg(feature = "test-hooks")]
use std::time::Instant;

/// The variable that names the phase log.
#[cfg(feature = "test-hooks")]
const PHASE_LOG_ENV: &str = "SWARMOPS_TEST_PHASE_LOG";

/// How many phases the git crate adds up.
#[cfg(feature = "test-hooks")]
const PHASES: usize = 3;

/// A phase the git crate adds up.
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    /// The export oid derive.
    ExportDerive,
    /// A memo read or written.
    ExportMemo,
    /// The import's conversion of commits.
    ImportConvert,
}

#[cfg(feature = "test-hooks")]
impl Phase {
    /// Every phase, in the order the log writes them.
    const ALL: [Phase; PHASES] = [Phase::ExportDerive, Phase::ExportMemo, Phase::ImportConvert];

    /// The phase's name in the log.
    fn name(self) -> &'static str {
        match self {
            Phase::ExportDerive => "export.derive",
            Phase::ExportMemo => "export.memo",
            Phase::ImportConvert => "import.convert",
        }
    }

    /// The phase's slot in the totals.
    fn at(self) -> usize {
        self as usize
    }
}

/// Each phase's microseconds and count so far in the process.
#[cfg(feature = "test-hooks")]
static TOTALS: Mutex<[(u64, u64); PHASES]> = Mutex::new([(0, 0); PHASES]);

#[cfg(feature = "test-hooks")]
thread_local! {
    /// How many spans of each phase are open on this thread.
    static DEPTH: Cell<[u32; PHASES]> = const { Cell::new([0; PHASES]) };
}

/// One timed run of a phase, ended when it is dropped (an error's early return among the ways):
/// its time is added then, unless a span of the same phase was open on the thread when it started
/// (its time is in that one's).
#[must_use]
pub(crate) struct Span {
    /// The phase and when it started, when the log is asked for.
    #[cfg(feature = "test-hooks")]
    on: Option<(Phase, Instant)>,
    /// What it adds to its phase's count.
    #[cfg(feature = "test-hooks")]
    count: u64,
}

impl Span {
    /// A span of `phase`, from now.
    pub(crate) fn start(phase: Phase) -> Span {
        #[cfg(not(feature = "test-hooks"))]
        let _ = phase;
        Span {
            #[cfg(feature = "test-hooks")]
            on: logged().then(|| {
                DEPTH.with(|depth| {
                    let mut open = depth.get();
                    open[phase.at()] += 1;
                    depth.set(open);
                });
                (phase, Instant::now())
            }),
            #[cfg(feature = "test-hooks")]
            count: 0,
        }
    }

    /// Ends the span, adding `count` to its phase's count.
    pub(crate) fn end(self, count: u64) {
        #[cfg(not(feature = "test-hooks"))]
        let _ = (self, count);
        #[cfg(feature = "test-hooks")]
        {
            let mut ended = self;
            ended.count = count;
        }
    }
}

#[cfg(feature = "test-hooks")]
impl Drop for Span {
    fn drop(&mut self) {
        let Some((phase, start)) = self.on else {
            return;
        };
        let us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);

        // The span closed; only the outermost one adds its time.
        let outer = DEPTH.with(|depth| {
            let mut open = depth.get();
            open[phase.at()] -= 1;
            depth.set(open);
            open[phase.at()] == 0
        });
        add(phase, if outer { us } else { 0 }, self.count);
    }
}

/// One object (a changeset, a tree or a blob) derived, counted in `export.derive` when a derive
/// span is open on the thread: the import's own comparisons of git trees, outside one, are not
/// derives.
pub(crate) fn derived() {
    #[cfg(feature = "test-hooks")]
    if logged() {
        let inside = DEPTH.with(|depth| depth.get()[Phase::ExportDerive.at()] > 0);
        if inside {
            add(Phase::ExportDerive, 0, 1);
        }
    }
}

/// Appends each phase with time or a count since the last call to the phase log, one
/// `<phase> <microseconds> <count>` line each, and starts the totals again. Nothing without
/// `SWARMOPS_TEST_PHASE_LOG`; a log that cannot be written is passed over, as a test hook must
/// not fail the operation it watches. The test hooks only.
#[cfg(feature = "test-hooks")]
pub fn write_phases() {
    let Some(path) = std::env::var_os(PHASE_LOG_ENV) else {
        return;
    };

    // 1. The totals, taken and reset.
    let totals = {
        let mut held = TOTALS.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *held, [(0, 0); PHASES])
    };

    // 2. One line a phase that ran, appended in one write.
    let mut lines = String::new();
    for phase in Phase::ALL {
        let (us, count) = totals[phase.at()];
        if us > 0 || count > 0 {
            let _ = writeln!(lines, "{} {us} {count}", phase.name());
        }
    }
    if lines.is_empty() {
        return;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(lines.as_bytes());
    }
}

/// Whether the phase log is asked for: read once a process, as each memo read asks.
#[cfg(feature = "test-hooks")]
fn logged() -> bool {
    static LOGGED: OnceLock<bool> = OnceLock::new();
    *LOGGED.get_or_init(|| std::env::var_os(PHASE_LOG_ENV).is_some())
}

/// Adds `us` and `count` to `phase`'s totals.
#[cfg(feature = "test-hooks")]
fn add(phase: Phase, us: u64, count: u64) {
    let mut held = TOTALS.lock().unwrap_or_else(PoisonError::into_inner);
    let (time, n) = &mut held[phase.at()];
    *time = time.saturating_add(us);
    *n = n.saturating_add(count);
}
