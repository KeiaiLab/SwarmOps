//! A store kept in sync while a swarm lands on it (the M18 design §2.4): landers through the
//! binary on an open loop, one syncer that runs a sync again as soon as one ends, and a sampler
//! that reads how far the remote trails `main` once a second.
//!
//! ```text
//!   CliLander 0..n ── every interval (landing::slot) ──▶ land on main
//!   syncer         ── sync, again, until the landers end
//!   sampler        ── every second: lag = lands on main not yet on the remote's main
//!                                            │
//!   lag_ratio(samples) = mean lag of the last minute / mean lag of the first
//! ```
//!
//! - [`SyncLane`]: the lane's shape and its run ([`SyncLane::run`]), which takes the sync and
//!   the lag reading as closures: the remote is the caller's.
//! - [`LagSample`] and [`lag_ratio`]: what the sampler records and the lane's verdict figure.

use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{Program, Store};
use crate::landing::{CliLander, LandRecord, Workload, change_for, open_loop_lands, slot};

/// The stretch at each end of a run that [`lag_ratio`] compares: a minute.
pub const RATIO_WINDOW: Duration = Duration::from_secs(60);

/// What each lander lands: files of its own, so that no land conflicts and every lag is a
/// sync's, never a merge's.
const WORKLOAD: Workload = Workload::Disjoint;

/// One reading of the lag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LagSample {
    /// When it was read, after the go.
    pub at: Duration,
    /// The lands on `main` not yet on the remote's `main`.
    pub lag: u64,
}

/// The mean lag of the samples in the last [`RATIO_WINDOW`] of the run over the mean of those
/// in its first: 1.0 for a flat series, above 1 for one that grows. The windows are measured
/// from the first and the last sample. Both means 0 reads 1.0; a first mean of 0 under a later
/// one reads infinity, so that a lane that falls behind from nothing never passes.
///
/// ```text
///   lag ▲            ·····   last minute's mean ┐
///       │      ·····                            ├─ ratio
///       │·····           first minute's mean    ┘
///       └──────────────────▶ at
/// ```
pub fn lag_ratio(samples: &[LagSample]) -> f64 {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return 1.0;
    };

    // 1. The two windows' means.
    let head = mean(samples.iter().filter(|s| s.at < first.at + RATIO_WINDOW));
    let tail = mean((samples.iter()).filter(|s| s.at + RATIO_WINDOW > last.at));

    // 2. Their ratio, a first window without lag read by what follows it.
    if head > 0.0 {
        return tail / head;
    }
    if tail > 0.0 { f64::INFINITY } else { 1.0 }
}

/// The mean lag of `samples`; 0 for none.
fn mean<'s>(samples: impl Iterator<Item = &'s LagSample>) -> f64 {
    let (sum, count) = samples.fold((0_u64, 0_u32), |(sum, count), s| (sum + s.lag, count + 1));
    if count == 0 {
        return 0.0;
    }
    // A sum of lags far below u32::MAX, exact as f64; past it, saturated.
    let sum = f64::from(u32::try_from(sum).unwrap_or(u32::MAX));
    sum / f64::from(count)
}

/// The lane's shape: how many landers land how often for how long, and how often the lag is
/// read.
#[derive(Debug, Clone, Copy)]
pub struct SyncLane {
    /// The landers, each through the binary, one process at a time.
    pub landers: usize,
    /// Each lander's interval between two lands.
    pub interval: Duration,
    /// How long the open loop offers lands.
    pub duration: Duration,
    /// The seed of the lands' slots and changes.
    pub seed: u64,
    /// How often the sampler reads the lag.
    pub every: Duration,
}

/// What a lane ran.
#[derive(Debug, Clone)]
pub struct LaneRun {
    /// Each lander's lands, in order, each with its start after the go.
    pub lands: Vec<Vec<LandRecord>>,
    /// The sampler's readings, in order.
    pub samples: Vec<LagSample>,
    /// How many syncs ran.
    pub syncs: u64,
    /// From the go to the last lander's end.
    pub wall: Duration,
}

impl SyncLane {
    /// The lane on `store`'s `main`, from one go: the landers, a syncer running `sync` again
    /// while any lander still runs, and a sampler reading `lag` every [`SyncLane::every`] until
    /// then. A panic in any of them fails the run once all have ended.
    pub fn run<P: Program>(
        &self,
        store: &Store<P>,
        sync: impl Fn() + Sync,
        lag: impl Fn() -> u64 + Sync,
    ) -> LaneRun {
        let start = Barrier::new(self.landers + 2);
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            // 1. The landers, the syncer and the sampler, each waiting for the go.
            let (start, stop, sync, lag) = (&start, &stop, &sync, &lag);
            let landers: Vec<_> = (0..self.landers)
                .map(|lander| scope.spawn(move || self.lander(store, lander, start)))
                .collect();
            let syncer = scope.spawn(move || {
                start.wait();
                let mut syncs = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    sync();
                    syncs += 1;
                }
                syncs
            });

            // 2. The go; the sampler runs on this thread until the landers end.
            start.wait();
            let go = Instant::now();
            let mut samples = Vec::new();
            while !landers.iter().all(thread::ScopedJoinHandle::is_finished) {
                samples.push(LagSample {
                    at: go.elapsed(),
                    lag: lag(),
                });
                let due = go + self.every.saturating_mul(count(&samples));
                thread::sleep(due.saturating_duration_since(Instant::now()));
            }

            // 3. Every record, then the syncer stopped.
            let join = |t: thread::ScopedJoinHandle<'_, Vec<LandRecord>>| {
                t.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
            };
            let lands = landers.into_iter().map(join).collect();
            let wall = go.elapsed();
            stop.store(true, Ordering::Relaxed);
            let syncs = syncer
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e));
            LaneRun {
                lands,
                samples,
                syncs,
                wall,
            }
        })
    }

    /// Lander `lander`'s open loop on `store`'s `main` after the go at `start`: each land at its
    /// slot, its start recorded.
    fn lander<P: Program>(
        &self,
        store: &Store<P>,
        lander: usize,
        start: &Barrier,
    ) -> Vec<LandRecord> {
        let cli = CliLander::new(store, lander, "main");
        start.wait();
        let go = Instant::now();
        let lands = open_loop_lands(self.seed, lander, self.interval, self.duration);
        (0..lands)
            .map(|land| {
                let due = go + slot(self.seed, lander, land, self.interval);
                thread::sleep(due.saturating_duration_since(Instant::now()));
                let at = go.elapsed();
                let change = change_for(self.seed, lander, land, WORKLOAD);
                let mut record = cli.land(land, &change);
                record.at_us = u64::try_from(at.as_micros()).unwrap_or(u64::MAX);
                record
            })
            .collect()
    }
}

/// How many samples `samples` holds, as the multiplier of the next one's due time.
fn count(samples: &[LagSample]) -> u32 {
    u32::try_from(samples.len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` samples a second apart, the lag at second `s` given by `lag`.
    fn series(secs: u64, lag: impl Fn(u64) -> u64) -> Vec<LagSample> {
        (0..secs)
            .map(|s| LagSample {
                at: Duration::from_secs(s),
                lag: lag(s),
            })
            .collect()
    }

    /// The ratio's self-check: a lag growing by one a second over ten minutes reads far above
    /// the 1.5 target, a flat one 1.0, one that falls below 1, and a lane that falls behind
    /// from no lag at all never passes.
    #[test]
    fn lag_ratio_reads_a_growing_series() {
        let growing = lag_ratio(&series(600, |s| s));
        assert!(growing > 1.5, "a growing series read {growing}");

        assert!((lag_ratio(&series(600, |_| 7)) - 1.0).abs() < 1e-9);
        assert!((lag_ratio(&series(600, |_| 0)) - 1.0).abs() < 1e-9);
        assert!(lag_ratio(&series(600, |s| 600 - s)) < 1.0);
        assert!(lag_ratio(&series(600, |s| u64::from(s > 300))).is_infinite());
        assert!((lag_ratio(&[]) - 1.0).abs() < 1e-9);
    }

    /// The two windows are each a minute, never the whole run: a series flat but for its last
    /// minute reads that minute over the flat lag (2.0, not 1.1), one flat but for its first
    /// minute the converse (0.5, not 0.55).
    #[test]
    fn lag_ratio_reads_one_minute_at_each_end() {
        let late = lag_ratio(&series(600, |s| if s >= 540 { 14 } else { 7 }));
        assert!((late - 2.0).abs() < 1e-9, "a late rise read {late}");

        let early = lag_ratio(&series(600, |s| if s < 60 { 14 } else { 7 }));
        assert!((early - 0.5).abs() < 1e-9, "an early fall read {early}");
    }
}
