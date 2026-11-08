//! The phase log (the M18 design §2.2), a test hook: under the `test-hooks` feature, with
//! `SWARMOPS_TEST_PHASE_LOG=<file>`, a `sync` and a `push` append one line
//! `<phase> <microseconds> <count>` to the file at the end of each phase, and the git crate's
//! totals at the end of the operation. Without the feature every item here is a no-op, and a
//! release build reads no such variable.
//!
//! | Phase | What it times | Its count |
//! |---|---|---|
//! | `sync.fetch` | the sync's fetch, git's child and the import | commits imported |
//! | `sync.plan` | the base and the unpublished changes | unpublished changes |
//! | `sync.recognise` | the changes the remote holds | changes it holds |
//! | `sync.relocate` | the rest landed onto the remote's head | changes landed |
//! | `sync.move` | `main`'s compare-and-swap | moves |
//! | `push.export` | the seen commit known, the export into the staging branch | commits written |
//! | `push.git` | git's children: the ancestry check, the push, the housekeeping | pushes |
//! | `export.derive` | the export oid derive, its memos' time included | changesets, trees and blobs derived |
//! | `export.memo` | the derived store's memo reads and writes | memos read and written |
//! | `import.convert` | the import's frames | commits converted |
//!
//! The last three are the git crate's, added up over the operation (the fetch's import, the
//! sync's published changes and the push's export each derive export oids) and written once.

#[cfg(feature = "test-hooks")]
use std::io::Write as _;
#[cfg(feature = "test-hooks")]
use std::time::Instant;

/// The variable that names the phase log.
#[cfg(feature = "test-hooks")]
const PHASE_LOG_ENV: &str = "SWARMOPS_TEST_PHASE_LOG";

/// A phase of a sync or a push.
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    /// The sync's fetch.
    SyncFetch,
    /// The sync's plan.
    SyncPlan,
    /// What the remote holds of the plan.
    SyncRecognise,
    /// The rest landed onto the remote's head.
    SyncRelocate,
    /// `main` moved.
    SyncMove,
    /// The push's export.
    PushExport,
    /// The push's git children.
    PushGit,
}

#[cfg(feature = "test-hooks")]
impl Phase {
    /// The phase's name in the log.
    fn name(self) -> &'static str {
        match self {
            Phase::SyncFetch => "sync.fetch",
            Phase::SyncPlan => "sync.plan",
            Phase::SyncRecognise => "sync.recognise",
            Phase::SyncRelocate => "sync.relocate",
            Phase::SyncMove => "sync.move",
            Phase::PushExport => "push.export",
            Phase::PushGit => "push.git",
        }
    }
}

/// One run of a phase, timed from [`Span::start`] to [`Span::end`]; one dropped without an end
/// (an error's early return) writes nothing.
#[must_use]
pub(crate) struct Span {
    /// The phase.
    #[cfg(feature = "test-hooks")]
    phase: Phase,
    /// When it started.
    #[cfg(feature = "test-hooks")]
    start: Instant,
}

impl Span {
    /// A span of `phase`, from now.
    pub(crate) fn start(phase: Phase) -> Span {
        #[cfg(not(feature = "test-hooks"))]
        let _ = phase;
        Span {
            #[cfg(feature = "test-hooks")]
            phase,
            #[cfg(feature = "test-hooks")]
            start: Instant::now(),
        }
    }

    /// Ends the span: its line, with `count`, appended to the phase log when one is asked for.
    pub(crate) fn end(self, count: usize) {
        #[cfg(not(feature = "test-hooks"))]
        let _ = (self, count);
        #[cfg(feature = "test-hooks")]
        {
            let us = self.start.elapsed().as_micros();
            append(&format!("{} {us} {count}\n", self.phase.name()));
        }
    }
}

/// The git crate's totals appended to the phase log: at the end of a sync or a push.
pub(crate) fn write_git() {
    #[cfg(feature = "test-hooks")]
    swarmops_git::write_phases();
}

/// Appends `line` to the phase log, in one write; nothing without `SWARMOPS_TEST_PHASE_LOG`. A
/// log that cannot be written is passed over: a test hook does not fail the operation it watches.
#[cfg(feature = "test-hooks")]
fn append(line: &str) {
    let Some(path) = std::env::var_os(PHASE_LOG_ENV) else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(line.as_bytes());
    }
}
