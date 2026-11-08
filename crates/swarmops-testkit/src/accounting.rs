//! What a served connection cost the server: its CPU time and its peak resident memory, read
//! off the ssh stand-in (M17 design §1).
//!
//! ```text
//!   outer script (the stand-in)
//!     ├── sh -c '<serve>; rc=$?; times >"$ledger"; exit "$rc"'   ← the transport's stdin/stdout
//!     │      └── serve and the processes it starts and waits for
//!     └── sampler (background): every 50 ms, over the tree, the largest VmHWM and, per process
//!         name, the largest VmHWM, RssAnon, RssFile and Threads
//!   → one file per connection in [`ledger_dir`]: the `times` line of the waited-for children,
//!     the peak in KiB, then the peak's process name and the per-name record
//! ```
//!
//! The `times` builtin goes to a file, never to stdout (the transport stream), and the shell
//! exits with the server's status. The sampler runs beside that shell, not inside it, so the
//! `times` line does not hold its CPU.
//!
//! [`LockHolders`] attributes the holds of a store's `meta/lock` to the processes that held it,
//! read off `/proc/locks`, without taking the lock (the M17 closure design §3.1).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::serve::ServeSite;

/// How the ssh stand-in runs `serve`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accounting {
    /// `exec`: the stand-in's bytes as they were; no file is written.
    Off,
    /// Counted: one ledger file a connection.
    Cpu,
}

pub use holders::LockHolders;

/// The largest memory figures and thread count the sampler read for one process name, each the
/// largest over every process of that name and every sample (so they may come from different
/// samples).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Peak {
    /// `VmHWM`, in KiB.
    pub hwm_kib: u64,
    /// `RssAnon`, in KiB.
    pub anon_kib: u64,
    /// `RssFile`, in KiB.
    pub file_kib: u64,
    /// `Threads`.
    pub threads: u64,
}

impl Peak {
    /// Each figure the larger of `self`'s and `other`'s.
    pub fn merge(&mut self, other: &Peak) {
        self.hwm_kib = self.hwm_kib.max(other.hwm_kib);
        self.anon_kib = self.anon_kib.max(other.anon_kib);
        self.file_kib = self.file_kib.max(other.file_kib);
        self.threads = self.threads.max(other.threads);
    }
}

/// What one connection cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spent {
    /// User CPU time of the server and the processes it waited for.
    pub user: Duration,
    /// System CPU time of the same.
    pub system: Duration,
    /// The largest `VmHWM` over the server's process tree, in KiB.
    pub peak_kib: u64,
    /// The record of each process name of the tree (`/proc/<pid>/status`'s `Name:`, blanks as
    /// `_`).
    pub by_name: BTreeMap<String, Peak>,
    /// The name of the process whose `VmHWM` is [`Spent::peak_kib`]; empty when none was read.
    pub peak_name: String,
}

impl Spent {
    /// User and system CPU time.
    pub fn cpu(&self) -> Duration {
        self.user + self.system
    }

    /// The `times` builtin's line of the waited-for children (`0m0.150s 0m0.050s`: user, then
    /// system; `dash` and `bash` both print minutes and fractional seconds), the peak, and the
    /// names line: the peak's name (`-` for none), then `name:hwm:anon:file:threads` per name
    /// (`sort sh:900:120:780:1 sort:70000:65000:2000:4`).
    pub fn parse(times_line: &str, peak_kib: u64, names_line: &str) -> Option<Spent> {
        // 1. The CPU times.
        let mut words = times_line.split_whitespace();
        let user = clock(words.next()?)?;
        let system = clock(words.next()?)?;
        if words.next().is_some() {
            return None;
        }

        // 2. The peak's name, then one record per name.
        let mut names = names_line.split_whitespace();
        let peak_name = match names.next()? {
            "-" => String::new(),
            name => name.to_owned(),
        };
        let by_name = names.map(named).collect::<Option<_>>()?;
        Some(Spent {
            user,
            system,
            peak_kib,
            by_name,
            peak_name,
        })
    }
}

/// `sort:70000:65000:2000:4` as the name and its record; the name may hold `:`.
fn named(word: &str) -> Option<(String, Peak)> {
    let mut fields = word.rsplitn(5, ':');
    let threads = fields.next()?.parse().ok()?;
    let file_kib = fields.next()?.parse().ok()?;
    let anon_kib = fields.next()?.parse().ok()?;
    let hwm_kib = fields.next()?.parse().ok()?;
    let name = fields.next().filter(|name| !name.is_empty())?;
    let peak = Peak {
        hwm_kib,
        anon_kib,
        file_kib,
        threads,
    };
    Some((name.to_owned(), peak))
}

/// `1m2.500s` as a duration.
fn clock(word: &str) -> Option<Duration> {
    let (minutes, seconds) = word.split_once('m')?;
    let seconds: f64 = seconds.strip_suffix('s')?.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs(minutes * 60) + Duration::from_secs_f64(seconds))
}

/// Where the per-connection files of `site`'s stand-ins go.
pub fn ledger_dir(site: &ServeSite) -> PathBuf {
    site.dir().join("ledger")
}

/// The connections finished after the first `mark`, oldest first: pass the length of an earlier
/// `spent_since(dir, 0)` as `mark`. A file that does not parse is skipped.
pub fn spent_since(dir: &Path, mark: usize) -> Vec<Spent> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    // 1. The finished files (temporary ones start with a dot), in the order they were written.
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let written = entry.metadata().ok()?.modified().ok()?;
            Some((written, entry.file_name(), entry.path()))
        })
        .collect();
    files.sort();

    // 2. Each: the times line, the peak, then the names line.
    files
        .into_iter()
        .skip(mark)
        .filter_map(|(_, _, path)| {
            let text = fs::read_to_string(path).ok()?;
            let mut lines = text.lines();
            let times = lines.next()?;
            let peak = lines.next()?.trim().parse().ok()?;
            Spent::parse(times, peak, lines.next()?)
        })
        .collect()
}

/// How often the sampler reads `VmHWM`.
const SAMPLE: Duration = Duration::from_millis(50);

/// One sample over the descendants of `$$` (not the sampler's own), merged into the state the
/// sampler keeps (read first, from the `=` lines): `=best <KiB> <name>`, the largest `VmHWM` and
/// its process name, then `= <name> <hwm> <anon> <file> <threads>` per name, each the largest
/// read. A process with no `VmHWM` (a kernel thread, a zombie) is not counted.
const TREE_PEAK: &str = r#"awk -v root="$$" -v skip="$me" '
/^=best / { best = $2; bestname = $3; next }
/^= / { n = $2; mh[n] = $3; ma[n] = $4; mf[n] = $5; mt[n] = $6; next }
/^Name:/ { nm = $0; sub(/^Name:[ \t]*/, "", nm); gsub(/[ \t]+/, "_", nm); if (nm == "") { nm = "?" }; next }
/^Pid:/ { pid = $2; name[pid] = nm }
/^PPid:/ { par[pid] = $2; seen[pid] = 1 }
/^VmHWM:/ { hwm[pid] = $2 }
/^RssAnon:/ { anon[pid] = $2 }
/^RssFile:/ { file[pid] = $2 }
/^Threads:/ { thr[pid] = $2 }
END {
  for (p in seen) {
    if (p == skip || hwm[p] == "") { continue }
    q = par[p]
    hops = 0
    while (q > 1 && q != root && q != skip && hops < 64) { q = par[q]; hops++ }
    if (q != root) { continue }
    n = name[p]
    if (hwm[p] + 0 > mh[n] + 0) { mh[n] = hwm[p] }
    if (anon[p] + 0 > ma[n] + 0) { ma[n] = anon[p] }
    if (file[p] + 0 > mf[n] + 0) { mf[n] = file[p] }
    if (thr[p] + 0 > mt[n] + 0) { mt[n] = thr[p] }
    if (hwm[p] + 0 > best + 0) { best = hwm[p]; bestname = n }
  }
  print "=best", best + 0, (bestname == "" ? "-" : bestname)
  for (n in mh) { print "=", n, mh[n] + 0, ma[n] + 0, mf[n] + 0, mt[n] + 0 }
}'"#;

/// The ledger's second and third lines out of the sampler's state: the peak, then the names line
/// [`Spent::parse`] reads.
const LEDGER_LINES: &str = r#"awk '
/^=best / { best = $2; bestname = $3 }
/^= / { names = names " " $2 ":" $3 ":" $4 ":" $5 ":" $6 }
END { print best + 0; print (bestname == "" ? "-" : bestname) names }'"#;

/// The tail of a stand-in's script that runs `words` (a shell command, the server) counted: the
/// ledger file is written in `ledger` and the script exits with the server's status.
pub(crate) fn counted(ledger: &Path, words: &str) -> String {
    let dir = crate::remote::sh_quote(crate::remote::path_str(ledger));
    let sample = format!("{:.2}", SAMPLE.as_secs_f64());
    format!(
        "ledger={dir}\n\
         id=\"$(date +%s%N)-$$\"\n\
         stop=\"$ledger/.$id.stop\"\n\
         peak=\"$ledger/.$id.peak\"\n\
         spent=\"$ledger/.$id.times\"\n\
         echo '=best 0 -' > \"$peak\"\n\
         # The sampler runs beside the shell that times the server, so its CPU is not counted.\n\
         (\n\
         \x20 read me _ < /proc/self/stat\n\
         \x20 while [ ! -e \"$stop\" ]; do\n\
         \x20   cat \"$peak\" /proc/[0-9]*/status 2>/dev/null | {TREE_PEAK} > \"$peak.new\" \\\n\
         \x20     && mv \"$peak.new\" \"$peak\"\n\
         \x20   sleep {sample}\n\
         \x20 done\n\
         ) >/dev/null 2>&1 &\n\
         sampler=$!\n\
         # The server in the foreground (it owns the transport); `times` goes to the file.\n\
         sh -c '\"$@\"; rc=$?; times > \"$0\"; exit \"$rc\"' \"$spent\" {words}\n\
         rc=$?\n\
         : > \"$stop\"\n\
         wait \"$sampler\"\n\
         {{ sed -n 2p \"$spent\"; {LEDGER_LINES} \"$peak\"; }} > \"$ledger/.$id.done\"\n\
         mv \"$ledger/.$id.done\" \"$ledger/$id\"\n\
         rm -f \"$stop\" \"$peak\" \"$peak.new\" \"$spent\"\n\
         exit \"$rc\"\n"
    )
}

/// The peak resident memory (`VmHWM`) of one process, sampled while it runs.
#[derive(Debug)]
pub struct PeakWatch {
    /// Set to end the sampling.
    stop: Arc<AtomicBool>,
    /// The sampler, which returns the largest value it read.
    sampler: JoinHandle<u64>,
}

impl PeakWatch {
    /// Samples process `pid` every 50 ms until [`PeakWatch::finish`].
    pub fn start(pid: u32) -> PeakWatch {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let sampler = thread::spawn(move || {
            let mut best = 0;
            while !flag.load(Ordering::Relaxed) {
                best = best.max(peak_kib(pid).unwrap_or(0));
                thread::sleep(SAMPLE);
            }
            best
        });
        PeakWatch { stop, sampler }
    }

    /// Ends the sampling: the largest `VmHWM` read, in KiB (0 when none was readable).
    pub fn finish(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.sampler.join().unwrap_or(0)
    }
}

/// `VmHWM` of process `pid` in KiB, if it runs.
fn peak_kib(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?;
    line.split_whitespace().next()?.parse().ok()
}

/// Who held a store's `meta/lock`, read off `/proc/locks` (the M17 closure design §3.1).
///
/// ```text
///   /proc/locks, every 1 ms:  "21: FLOCK  ADVISORY  WRITE 3182874 09:00:211833246 0 EOF"
///                                                           pid   dev     inode of meta/lock
///   → per pid, the intervals it was seen holding: [first sample seen, first sample not seen)
/// ```
///
/// A line with `->` is a waiter, not a holder. The sampler never opens the lock file for a lock,
/// so it adds no demand. An interval runs to the first sample that no longer sees it, so a hold
/// is never undercounted: the figures are upper bounds, to about a sampling period each.
mod holders {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    /// How often the sampler reads `/proc/locks`.
    const EVERY: Duration = Duration::from_millis(1);

    /// One pid's intervals, oldest first, none overlapping.
    type Spans = Vec<(Instant, Instant)>;

    /// The holders of one lock file, sampled until [`LockHolders::stop`].
    #[derive(Debug)]
    pub struct LockHolders {
        /// Set to end the sampling.
        stop: Arc<AtomicBool>,
        /// The sampler, which returns the intervals it saw.
        sampler: Option<JoinHandle<BTreeMap<u32, Spans>>>,
        /// The intervals per pid, once stopped.
        holds: BTreeMap<u32, Spans>,
    }

    impl LockHolders {
        /// Samples the holders of the lock file at `lock` (a store's `meta/lock`) every 1 ms;
        /// a file not made yet is looked for again at each sample.
        pub fn start(lock: &Path) -> LockHolders {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let lock = lock.to_path_buf();
            let sampler = thread::spawn(move || sample(&lock, &flag));
            LockHolders {
                stop,
                sampler: Some(sampler),
                holds: BTreeMap::new(),
            }
        }

        /// Ends the sampling; the queries below read what it saw.
        pub fn stop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(sampler) = self.sampler.take() {
                self.holds = sampler.join().expect("the lock sampler ran");
            }
        }

        /// How long `pid` held the lock inside `[from, to]`.
        pub fn held_by(&self, pid: u32, from: Instant, to: Instant) -> Duration {
            let Some(spans) = self.holds.get(&pid) else {
                return Duration::ZERO;
            };

            // The first interval that ends after `from`, then each that starts before `to`.
            let first = spans.partition_point(|&(_, end)| end <= from);
            spans[first..]
                .iter()
                .take_while(|&&(start, _)| start < to)
                .map(|&(start, end)| end.min(to).saturating_duration_since(start.max(from)))
                .sum()
        }

        /// `pid`'s longest hold and how many holds it made.
        pub fn longest(&self, pid: u32) -> (Duration, usize) {
            let Some(spans) = self.holds.get(&pid) else {
                return (Duration::ZERO, 0);
            };
            let longest = spans.iter().map(|&(start, end)| end - start).max();
            (longest.unwrap_or_default(), spans.len())
        }

        /// The pids seen holding the lock.
        pub fn holders(&self) -> Vec<u32> {
            self.holds.keys().copied().collect()
        }
    }

    impl Drop for LockHolders {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// The sampler's loop: per pid, the intervals it was seen holding `lock`, until `stop`.
    fn sample(lock: &Path, stop: &AtomicBool) -> BTreeMap<u32, Spans> {
        let mut holds: BTreeMap<u32, Spans> = BTreeMap::new();
        let mut key: Option<String> = None;
        let mut open: Option<(u32, Instant)> = None;
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();

            // 1. The lock file's `/proc/locks` key, once the file exists.
            if key.is_none() {
                key = lock_key(lock);
            }

            // 2. Who holds it now; an interval under way ends when its pid is no longer seen.
            let holder = key.as_deref().and_then(holder_of);
            if let Some((pid, since)) = open
                && holder != Some(pid)
            {
                holds.entry(pid).or_default().push((since, now));
                open = None;
            }
            if open.is_none() {
                open = holder.map(|pid| (pid, now));
            }
            thread::sleep(EVERY);
        }

        // 3. The interval under way ends at the stop.
        if let Some((pid, since)) = open {
            holds.entry(pid).or_default().push((since, Instant::now()));
        }
        holds
    }

    /// `maj:min:inode` of the file at `lock` as `/proc/locks` prints it (hex device numbers,
    /// two digits at least; Linux's `dev_t` split).
    fn lock_key(lock: &Path) -> Option<String> {
        let meta = fs::metadata(lock).ok()?;
        let dev = meta.dev();
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
        let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
        Some(format!("{major:02x}:{minor:02x}:{}", meta.ino()))
    }

    /// The pid of the `FLOCK` holder of the file `key` names, if any.
    fn holder_of(key: &str) -> Option<u32> {
        let locks = fs::read_to_string("/proc/locks").ok()?;
        locks.lines().find_map(|line| {
            // `<id>: FLOCK ADVISORY WRITE <pid> <key> 0 EOF`; a waiter has `->` after the id.
            let words: Vec<_> = line.split_whitespace().collect();
            let [_, "FLOCK", _, _, pid, at, ..] = words.as_slice() else {
                return None;
            };
            if *at != key {
                return None;
            }
            pid.parse().ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{Accounting, LockHolders, Peak, Spent, ledger_dir, spent_since};
    use crate::child;
    use crate::remote::write_script;
    use crate::serve::ServeSite;

    /// What a server that burns CPU runs: a child (forked shell) loops until its own user and
    /// system time reach 25 clock ticks (250 ms at 100 Hz), however loaded the host is.
    const BURN: &str = "( while :; do\n\
         \x20 read -r _ _ _ _ _ _ _ _ _ _ _ _ _ user system _ < /proc/self/stat\n\
         \x20 [ $((user + system)) -ge 25 ] && break\n\
         done )\n\
         exit 3\n";

    #[test]
    fn parses_both_shells_formats() {
        let dash = Spent::parse("0m0.150s 1m2.5s", 7, "-").expect("parses");
        assert_eq!(dash.user.as_millis(), 150);
        assert_eq!(dash.system.as_millis(), 62_500);
        assert_eq!(dash.peak_kib, 7);
        assert_eq!(dash.peak_name, "");
        assert!(dash.by_name.is_empty(), "{dash:?}");
        assert_eq!(
            Spent::parse("0m0.204s 0m0.011s", 0, "-")
                .expect("parses")
                .cpu()
                .as_millis(),
            215
        );
        assert_eq!(Spent::parse("0m0.1s", 0, "-"), None);
        assert_eq!(Spent::parse("fast slow", 0, "-"), None);
    }

    #[test]
    fn parses_the_names_line() {
        let line = "sort sh:900:120:780:1 sort:70000:65000:2000:4 a:b:5:4:3:2";
        let spent = Spent::parse("0m0.000s 0m0.000s", 70_000, line).expect("parses");
        assert_eq!(spent.peak_name, "sort");
        let sort = Peak {
            hwm_kib: 70_000,
            anon_kib: 65_000,
            file_kib: 2_000,
            threads: 4,
        };
        assert_eq!(spent.by_name.get("sort"), Some(&sort));
        assert_eq!(spent.by_name.get("a:b").map(|peak| peak.threads), Some(2));
        assert_eq!(spent.by_name.len(), 3, "{spent:?}");
        assert_eq!(Spent::parse("0m0.0s 0m0.0s", 0, ""), None);
        assert_eq!(Spent::parse("0m0.0s 0m0.0s", 0, "- sh:1:2:3"), None);
        assert_eq!(Spent::parse("0m0.0s 0m0.0s", 0, "- :1:2:3:4"), None);
    }

    /// I12: the per-name record names the process that peaked. A server that runs
    /// `sort -S 100M` over 64 MiB of lines: `sort` is the peak, with `RssAnon` of 48 MiB or more.
    #[test]
    fn accounting_names_the_peaking_process() {
        /// 64 MiB of lines of 16 bytes.
        const LINES: u64 = 4 << 20;
        let work = tempfile::tempdir().expect("a temp dir");

        // 1. The lines, out of order (an LCG's high bits).
        let mut text = String::with_capacity(64 << 20);
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..LINES {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            writeln!(text, "{:015x}", state >> 4).expect("write to a string");
        }
        let input = work.path().join("input");
        fs::write(&input, text).expect("write the input");

        // 2. The server sorts it, its scratch beside it.
        let program = work.path().join("swarmops");
        let input = input.to_str().expect("a UTF-8 path");
        let scratch = work.path().to_str().expect("a UTF-8 path");
        let script = format!(
            "#!/bin/sh\nPATH=/usr/bin:/bin\nsort -S 100M -T '{scratch}' '{input}' > /dev/null\n"
        );
        write_script(&program, &script);
        let site = ServeSite::with_program(program).with_accounting(Accounting::Cpu);
        let ssh = site.ssh_command("person/ada");
        let output =
            child::run(Command::new(ssh_path(&ssh)).args(["host", "git-upload-pack", "'x.git'"]));
        assert!(output.status.success(), "{output:?}");

        // 3. The record: sort peaked, and most of it is anonymous memory.
        let spent = spent_since(&ledger_dir(&site), 0);
        assert_eq!(spent.len(), 1, "{spent:?}");
        let spent = &spent[0];
        assert_eq!(spent.peak_name, "sort", "{spent:?}");
        let sort = spent.by_name.get("sort").expect("a record for sort");
        assert_eq!(sort.hwm_kib, spent.peak_kib, "{spent:?}");
        assert!(sort.anon_kib >= 48 << 10, "{spent:?}");
        assert!(sort.threads >= 1, "{spent:?}");
    }

    /// I11: the holder sampler attributes a known hold. With no holder nothing is attributed; a
    /// child holding the lock 300 ms is seen with its pid for 250 ms or more.
    #[test]
    fn the_lock_sampler_names_the_holder() {
        let work = tempfile::tempdir().expect("a temp dir");
        let lock = work.path().join("lock");
        fs::write(&lock, b"").expect("make the lock file");

        // 1. Nobody holds it.
        let before = Instant::now();
        let mut idle = LockHolders::start(&lock);
        thread::sleep(Duration::from_millis(100));
        idle.stop();
        assert_eq!(idle.holders(), Vec::<u32>::new());

        // 2. A child holds it 300 ms (`flock(1)` takes `flock(2)` itself, then runs the sleep).
        let mut holders = LockHolders::start(&lock);
        let from = Instant::now();
        let mut child = Command::new("flock")
            .arg("--exclusive")
            .arg(&lock)
            .args(["sleep", "0.3"])
            .spawn()
            .expect("run flock(1)");
        let pid = child.id();
        assert!(child.wait().expect("flock(1) ran").success());
        let to = Instant::now();
        holders.stop();
        let held = holders.held_by(pid, from, to);
        assert!(held >= Duration::from_millis(250), "{held:?}");
        assert!(held <= to - from, "{held:?}");
        assert_eq!(holders.holders(), vec![pid]);
        assert_eq!(holders.longest(pid).1, 1);
        assert_eq!(holders.held_by(pid, before, from), Duration::ZERO);
    }

    #[test]
    fn accounting_counts_a_known_burn() {
        let work = tempfile::tempdir().expect("a temp dir");
        let program = work.path().join("swarmops");
        write_script(&program, &format!("#!/bin/sh\n{BURN}"));
        let site = ServeSite::with_program(program).with_accounting(Accounting::Cpu);
        let ssh = site.ssh_command("person/ada");

        // 1. The stand-in exits with the server's status and puts nothing on the transport.
        let output =
            child::run(Command::new(ssh_path(&ssh)).args(["host", "git-upload-pack", "'x.git'"]));
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);

        // 2. One ledger file holds the burn.
        let spent = spent_since(&ledger_dir(&site), 0);
        assert_eq!(spent.len(), 1, "{spent:?}");
        assert!(spent[0].cpu().as_millis() >= 150, "{spent:?}");
        assert_eq!(spent_since(&ledger_dir(&site), 1), vec![]);
    }

    #[test]
    fn accounting_off_writes_no_file() {
        let work = tempfile::tempdir().expect("a temp dir");
        let program = work.path().join("swarmops");
        write_script(&program, &format!("#!/bin/sh\n{BURN}"));
        let site = ServeSite::with_program(program);
        let ssh = site.ssh_command("person/ada");
        let script = std::fs::read_to_string(ssh_path(&ssh)).expect("the stand-in");
        assert!(script.contains("exec env -i "), "{script}");
        assert!(!script.contains("times"), "{script}");

        let output =
            child::run(Command::new(ssh_path(&ssh)).args(["host", "git-upload-pack", "'x.git'"]));
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        assert_eq!(spent_since(&ledger_dir(&site), 0), vec![]);
    }

    /// The stand-in's path out of the quoted `GIT_SSH_COMMAND` value.
    fn ssh_path(quoted: &str) -> String {
        quoted.trim_matches('\'').to_owned()
    }
}
