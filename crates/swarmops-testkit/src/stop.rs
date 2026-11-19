//! A stop flag that is set however the code holding it ends.
//!
//! A test that runs loops beside its main work (committers, readers, samplers: threads that
//! run until a flag is set) sets the flag after the work. If the work panics first, the flag
//! stays clear, `thread::scope` joins loops that never end, and the test hangs instead of
//! reporting its failure. Hold a [`StopOnDrop`] across the work and the flag is set on unwind
//! as well:
//!
//! ```text
//!   thread::scope(|scope| {
//!       let loops = scope.spawn(|| while !done.load(..) { .. });
//!       let _stop = StopOnDrop::new(&done);   // set on return and on panic
//!       work();                               // a panic here still ends `loops`
//!       loops.join()
//!   })
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

/// Sets its flag when dropped.
pub struct StopOnDrop<'a>(&'a AtomicBool);

impl<'a> StopOnDrop<'a> {
    /// A guard that sets `flag` when it goes out of scope, by return or by panic.
    pub fn new(flag: &'a AtomicBool) -> Self {
        Self(flag)
    }
}

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// How long the scope below may take to end once its work panicked.
    const BOUND: Duration = Duration::from_secs(10);

    /// A scope whose loop runs until the flag is set and whose work panics: it ends, and the
    /// panic is the work's. Without the guard the scope joins the loop for ever, so the scope
    /// runs on a thread of its own and is waited for with a bound.
    #[test]
    fn a_panicking_foreground_still_ends_the_loops() {
        let (ended, wait) = mpsc::channel();
        thread::spawn(move || {
            let done = AtomicBool::new(false);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                thread::scope(|scope| {
                    scope.spawn(|| {
                        while !done.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    });
                    let _stop = StopOnDrop::new(&done);
                    panic!("the import failed");
                });
            }));
            let _ = ended.send(outcome.is_err());
        });
        let panicked = wait
            .recv_timeout(BOUND)
            .expect("the scope did not end: the loop kept running after the work panicked");
        assert!(panicked, "the work's panic reaches the caller");
    }

    /// A normal return sets the flag too.
    #[test]
    fn a_return_sets_the_flag() {
        let done = AtomicBool::new(false);
        drop(StopOnDrop::new(&done));
        assert!(done.load(Ordering::SeqCst));
    }
}
