//! The workspace's lock (§2.4 of the M5 design): `.swarmws/lock`, held exclusively by every
//! mutation, waited for at most 5 s, then `E_WS_LOCKED`.
//!
//! The same primitive as the store's `meta/lock`: std's `File::lock` (`flock` on Linux), on a
//! file that is made if missing and never read or written. The lock belongs to the open file, so
//! two acquisitions conflict within one process too, and the kernel releases it when the process
//! dies: a killed mutation leaves no stale lock. Reads (`status`, `diff`) take none.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};

use crate::state::SWARMWS_DIR;

/// How long a mutation waits for another to let go of the lock.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// How often a waiting mutation tries the lock again.
const POLL: Duration = Duration::from_millis(10);

/// The lock file, in [`SWARMWS_DIR`].
const LOCK_FILE: &str = "lock";

/// The exclusive lock of one workspace, held until dropped.
#[derive(Debug)]
pub struct WsLock {
    /// The open lock file the lock belongs to.
    file: File,
}

impl Drop for WsLock {
    fn drop(&mut self) {
        // Closing the file would release it too, once no forked child shares the descriptor;
        // unlocking releases it at once. An error leaves the close to do it.
        let _ = self.file.unlock();
    }
}

/// Takes the lock of the workspace at `dir`, waiting up to [`LOCK_WAIT`] for a mutation that
/// holds it; `E_WS_LOCKED` (retryable) when it never lets go.
pub fn acquire(dir: &Utf8Path) -> Result<WsLock, OpError> {
    // 1. The lock file, made if missing, never written.
    let shown = format!("{SWARMWS_DIR}/{LOCK_FILE}");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(SWARMWS_DIR).join(LOCK_FILE))
        .map_err(|e| failed("open", &shown, &e))?;

    // 2. Tried at once, then again every POLL until the wait is over.
    let until = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(WsLock { file }),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(failed("lock", &shown, &e)),
        }
        if Instant::now() >= until {
            return Err(OpError::new(
                ErrorCode::WsLocked,
                format!(
                    "another mutation of this workspace held its lock for {} s",
                    LOCK_WAIT.as_secs()
                ),
            )
            .with_hint("run the command again once the other one has finished"));
        }
        thread::sleep(POLL);
    }
}

/// `E_IO` for the call `what` (`open`, `lock`) on the lock file `shown` that failed with `e`.
fn failed(what: &str, shown: &str, e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("{what} {shown}: {e}")).with_details(Map::from_iter([
        ("fs_op".to_owned(), Value::from(what)),
        ("file".to_owned(), Value::from(shown)),
    ]))
}
