//! A rendezvous for the tests' [`swarmops_store::FsOps`] doubles, to tell calls made at once
//! from calls made one after another: it counts the calls in flight and the most that were at
//! once, and while armed it holds each call until that many are in flight together (or until
//! [`Rendezvous::PATIENCE`] has passed). Calls made one after another then never overlap, and
//! calls made at once all do. A module directory, so that Cargo does not take it for a test
//! target of its own.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// See the module documentation. Clones share the count.
#[derive(Clone, Default)]
pub struct Rendezvous(Arc<(Mutex<InFlight>, Condvar)>);

#[derive(Default)]
struct InFlight {
    want: usize,
    now: usize,
    most: usize,
}

impl Rendezvous {
    /// How long a call waits for the others.
    pub const PATIENCE: Duration = Duration::from_secs(5);

    /// From now on, each call waits until `want` are in flight together; 0 waits for none.
    /// Starts the count of the most at once afresh.
    pub fn arm(&self, want: usize) {
        let mut flight = self.0.0.lock().unwrap();
        (flight.want, flight.most) = (want, 0);
    }

    /// The most calls that were in flight at once since [`Rendezvous::arm`] (or ever).
    pub fn most(&self) -> usize {
        self.0.0.lock().unwrap().most
    }

    /// Runs `call` as one call in flight, after the wait.
    pub fn around<T>(&self, call: impl FnOnce() -> T) -> T {
        let (lock, changed) = &*self.0;
        let mut flight = lock.lock().unwrap();
        flight.now += 1;
        flight.most = flight.most.max(flight.now);
        changed.notify_all();
        let deadline = Instant::now() + Self::PATIENCE;
        // Once `most` reaches `want` every waiter goes on, even after some have finished.
        while flight.most < flight.want {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            flight = changed.wait_timeout(flight, left).unwrap().0;
        }
        drop(flight);
        let out = call();
        lock.lock().unwrap().now -= 1;
        out
    }
}
