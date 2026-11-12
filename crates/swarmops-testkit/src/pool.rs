//! What the large-pool measurements add to [`landing`]'s: when a pool's lands arrive, how many
//! are in flight at once, the waves a herd lands in, each identity's account, and the host's
//! processes and memory around a run (the large-pool goals design, §3 and §4 L-2).
//!
//! - [`Arrival`]: a pool's arrival, spread over each interval as [`landing::slot`] spreads an
//!   open loop, or all at each mark (a herd).
//! - [`InFlight`]: lands in flight, counted as they enter and leave, and the peak.
//! - [`Wave`] and [`waves`]: a herd's lands cut by the mark they began after, each wave's drain.
//! - [`Tally`] and [`tally`]: the identities whose every scheduled land is recorded and landed.
//! - [`children_of`] and [`mem_available_mib`]: a process's live children and the host's
//!   available memory, read off `/proc`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::landing::{self, LandRecord};

/// [`Arrival::Spread`]'s name.
const SPREAD: &str = "spread";

/// [`Arrival::Herd`]'s name.
const HERD: &str = "herd";

/// What `/proc/meminfo` names the memory a new process can take without swapping, in KiB.
const MEM_AVAILABLE: &str = "MemAvailable:";

/// KiB in a MiB.
const KIB_PER_MIB: u64 = 1_024;

/// How a pool's lands arrive over each interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Arrival {
    /// Each identity at its own slot: [`landing::slot`]'s phase and jitter.
    Spread,
    /// Every identity at each mark: `0, interval, 2·interval, …`.
    Herd,
}

impl Arrival {
    /// The name a run is given it by: `spread` or `herd`.
    pub fn name(self) -> &'static str {
        match self {
            Arrival::Spread => SPREAD,
            Arrival::Herd => HERD,
        }
    }

    /// The arrival named `name`; `None` for a name that is no arrival.
    pub fn from_name(name: &str) -> Option<Arrival> {
        match name {
            SPREAD => Some(Arrival::Spread),
            HERD => Some(Arrival::Herd),
            _ => None,
        }
    }

    /// When identity `who`'s land `n` is due after the go, at one land per `interval`.
    pub fn due(self, seed: u64, who: usize, n: usize, interval: Duration) -> Duration {
        match self {
            Arrival::Spread => landing::slot(seed, who, n, interval),
            Arrival::Herd => {
                let marks = u32::try_from(n).expect("a pool's land number fits in u32");
                interval.saturating_mul(marks)
            }
        }
    }

    /// How many lands identity `who` makes in `duration` at one land per `interval`: one per
    /// due time before `duration`; none at a zero interval.
    pub fn count(self, seed: u64, who: usize, interval: Duration, duration: Duration) -> usize {
        match self {
            Arrival::Spread => landing::open_loop_lands(seed, who, interval, duration),
            Arrival::Herd => {
                if interval.is_zero() {
                    return 0;
                }

                // The marks 0, interval, … strictly before the duration.
                let marks = duration.as_nanos().div_ceil(interval.as_nanos());
                usize::try_from(marks).expect("a count of lands fits in usize")
            }
        }
    }
}

/// Lands in flight: how many entered, how many left, and the most inside at once.
#[derive(Debug, Default)]
pub struct InFlight {
    /// Inside now.
    current: AtomicUsize,
    /// The most inside at once.
    peak: AtomicUsize,
    /// Entered so far.
    entered: AtomicUsize,
    /// Left so far.
    left: AtomicUsize,
}

/// One land inside an [`InFlight`]: it leaves when dropped, a panicking holder's too.
#[derive(Debug)]
pub struct Entered<'f> {
    /// The count it leaves.
    flight: &'f InFlight,
}

impl InFlight {
    /// A count with nothing in flight.
    pub fn new() -> InFlight {
        InFlight::default()
    }

    /// One land enters; it leaves when the returned guard drops.
    pub fn enter(&self) -> Entered<'_> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let inside = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(inside, Ordering::SeqCst);
        Entered { flight: self }
    }

    /// The most lands inside at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// How many lands entered.
    pub fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    /// How many lands left.
    pub fn left(&self) -> usize {
        self.left.load(Ordering::SeqCst)
    }

    /// The fields a run prints: `inflight_max=<n> entered=<n> left=<n>`.
    pub fn line(&self) -> String {
        format!(
            "inflight_max={} entered={} left={}",
            self.peak(),
            self.entered(),
            self.left()
        )
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.flight.current.fetch_sub(1, Ordering::SeqCst);
        self.flight.left.fetch_add(1, Ordering::SeqCst);
    }
}

/// The lands that began after one mark of a herd and before the next.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Wave {
    /// The mark's number: the wave began at `n·interval`.
    pub n: u64,
    /// Lands that began in it.
    pub lands: usize,
    /// Of them, those that landed.
    pub landed: usize,
    /// From the mark to the end of its last land, in microseconds.
    pub drain_us: u64,
}

/// Every lander's records cut into waves of `interval`, in mark order; a mark no land began
/// after has no wave. A record's wave is `at_us / interval`; its end is `at_us + latency_us`.
pub fn waves(records: &[Vec<LandRecord>], interval: Duration) -> Vec<Wave> {
    let interval_us = u64::try_from(interval.as_micros()).expect("an interval fits in u64 µs");
    assert!(interval_us > 0, "a herd's waves need an interval");

    // 1. Each record joins the wave of the last mark at or before its start.
    let mut by_mark: BTreeMap<u64, Wave> = BTreeMap::new();
    for record in records.iter().flatten() {
        let n = record.at_us / interval_us;
        let wave = by_mark.entry(n).or_insert(Wave {
            n,
            lands: 0,
            landed: 0,
            drain_us: 0,
        });
        wave.lands += 1;
        wave.landed += usize::from(record.id.is_some());

        // 2. The drain runs from the mark to the latest end, e.g. mark 2 of 2 s, a land at
        //    4.0 s taking 0.2 s: 200,000 µs.
        let end_us = record.at_us.saturating_add(record.latency_us);
        wave.drain_us = wave.drain_us.max(end_us - n * interval_us);
    }
    by_mark.into_values().collect()
}

/// A pool's account of its identities.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Tally {
    /// Identities counted.
    pub identities: usize,
    /// Those with every scheduled land recorded and landed.
    pub complete: usize,
    /// The others, in order.
    pub short: Vec<usize>,
}

/// Each identity's records against `expected(identity)` lands: complete when it has exactly that
/// many and every one landed.
pub fn tally(records: &[Vec<LandRecord>], expected: impl Fn(usize) -> usize) -> Tally {
    let short: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(who, lands)| {
            lands.len() != expected(*who) || lands.iter().any(|land| land.id.is_none())
        })
        .map(|(who, _)| who)
        .collect();
    Tally {
        identities: records.len(),
        complete: records.len() - short.len(),
        short,
    }
}

/// The live children of process `pid`, in pid order: the processes whose `/proc/<n>/stat` names
/// it as parent. A process that ends while it is read is not listed.
pub fn children_of(pid: u32) -> io::Result<Vec<u32>> {
    let mut children = Vec::new();
    for entry in fs::read_dir("/proc")? {
        // 1. Only the numbered entries are processes.
        let Some(child) = entry?
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };

        // 2. A process that ended since the listing has no stat left: not a child.
        let stat = match fs::read_to_string(format!("/proc/{child}/stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if parent(&stat) == Some(pid) {
            children.push(child);
        }
    }
    children.sort_unstable();
    Ok(children)
}

/// The parent pid in a `/proc/<n>/stat` line, the field after the state that follows the
/// command's closing parenthesis (the command may hold spaces and parentheses itself):
/// `42 (sleep) S 7 …` gives 7.
fn parent(stat: &str) -> Option<u32> {
    let (_, after_command) = stat.rsplit_once(')')?;
    after_command.split_whitespace().nth(1)?.parse().ok()
}

/// The host's `MemAvailable` in MiB; `None` where `/proc/meminfo` gives none.
pub fn mem_available_mib() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix(MEM_AVAILABLE))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kib / KIB_PER_MIB)
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    /// The interval the tests pace at.
    const TWO_S: Duration = Duration::from_secs(2);

    /// A record that began `at_us` after the go and took `latency_us`, landed or not.
    fn record(at_us: u64, latency_us: u64, landed: bool) -> LandRecord {
        LandRecord {
            id: landed.then(|| "00".repeat(32)),
            at_us,
            latency_us,
            ..LandRecord::default()
        }
    }

    #[test]
    fn a_herd_is_due_at_each_mark() {
        // 1. Every identity's land n is due at n·interval, whatever the seed.
        for who in [0, 7, 999] {
            for n in 0..5 {
                let due = Arrival::Herd.due(1, who, n, TWO_S);
                assert_eq!(due, TWO_S * u32::try_from(n).unwrap());
            }
        }

        // 2. The marks before the duration: 0 and 2 s in 4 s, 0, 2 and 4 s in 5 s, none at 0.
        assert_eq!(Arrival::Herd.count(1, 3, TWO_S, Duration::from_secs(4)), 2);
        assert_eq!(Arrival::Herd.count(1, 3, TWO_S, Duration::from_secs(5)), 3);
        assert_eq!(Arrival::Herd.count(1, 3, Duration::ZERO, TWO_S), 0);
    }

    #[test]
    fn a_spread_is_the_landing_slot() {
        let interval = Duration::from_secs(60);
        let duration = Duration::from_secs(600);

        // Spread is the open loop's slot and count, draw for draw.
        for who in 0..100 {
            let n = who % 10;
            assert_eq!(
                Arrival::Spread.due(1, who, n, interval),
                landing::slot(1, who, n, interval)
            );
            assert_eq!(
                Arrival::Spread.count(1, who, interval, duration),
                landing::open_loop_lands(1, who, interval, duration)
            );
        }
    }

    #[test]
    fn an_unknown_arrival_has_no_name() {
        // 1. Each arrival is found by its own name.
        for arrival in [Arrival::Spread, Arrival::Herd] {
            assert_eq!(Arrival::from_name(arrival.name()), Some(arrival));
        }

        // 2. Another name, a case change or an empty one is none.
        for name in ["", "Herd", "burst", "spread "] {
            assert_eq!(Arrival::from_name(name), None);
        }
    }

    #[test]
    fn the_peak_counts_lands_in_flight() {
        const HOLDERS: usize = 8;
        let flight = Arc::new(InFlight::new());
        let inside = Arc::new(Barrier::new(HOLDERS));

        // 1. Eight holders enter and wait for each other inside; holder 0 then panics.
        let holders: Vec<_> = (0..HOLDERS)
            .map(|holder| {
                let flight = Arc::clone(&flight);
                let inside = Arc::clone(&inside);
                thread::spawn(move || {
                    let _entered = flight.enter();
                    inside.wait();
                    assert_ne!(holder, 0, "holder 0 panics while inside");
                })
            })
            .collect();
        let panicked = holders
            .into_iter()
            .map(thread::JoinHandle::join)
            .filter(Result::is_err)
            .count();

        // 2. All eight were inside at once, and all eight left, the panicking one too.
        assert_eq!(panicked, 1);
        assert_eq!(flight.peak(), HOLDERS);
        assert_eq!(flight.entered(), HOLDERS);
        assert_eq!(flight.left(), HOLDERS);
        assert_eq!(flight.line(), "inflight_max=8 entered=8 left=8");
    }

    #[test]
    fn waves_cut_by_the_mark_and_time_the_last() {
        // Two landers, a mark every 2 s: three lands after the first mark, one unlanded; two
        // after the third; none after the second.
        let records = vec![
            vec![
                record(10_000, 50_000, true),
                record(4_100_000, 30_000, true),
            ],
            vec![
                record(0, 120_000, true),
                record(1_999_999, 5_000, false),
                record(4_000_000, 200_000, true),
            ],
        ];

        // The last end sets the drain: 1,999,999 + 5,000 after mark 0; 4,000,000 + 200,000
        // less 4 s after mark 2.
        let expected = vec![
            Wave {
                n: 0,
                lands: 3,
                landed: 2,
                drain_us: 2_004_999,
            },
            Wave {
                n: 2,
                lands: 2,
                landed: 2,
                drain_us: 200_000,
            },
        ];
        assert_eq!(waves(&records, TWO_S), expected);
    }

    #[test]
    fn a_short_identity_is_tallied() {
        // Three identities of two lands each: 0 complete, 1 a record short, 2 one unlanded.
        let records = vec![
            vec![record(0, 1, true), record(1, 1, true)],
            vec![record(0, 1, true)],
            vec![record(0, 1, true), record(1, 1, false)],
        ];

        let tallied = tally(&records, |_| 2);
        assert_eq!(
            tallied,
            Tally {
                identities: 3,
                complete: 1,
                short: vec![1, 2],
            }
        );
    }

    #[test]
    fn a_live_child_is_listed_and_a_reaped_one_is_not() {
        let me = std::process::id();

        // 1. A sleeping child is listed under this process.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(children_of(me).unwrap().contains(&child.id()));

        // 2. Killed and reaped, it is gone; no other child is left.
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(children_of(me).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn a_command_with_parentheses_keeps_its_parent() {
        assert_eq!(parent("42 (a) b) S 7 42 42 0"), Some(7));
        assert_eq!(parent("42 (sleep) S"), None);
    }

    #[test]
    fn the_host_has_memory_available() {
        assert!(mem_available_mib().is_some_and(|mib| mib > 0));
    }
}
