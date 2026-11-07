//! What a served connection cost the server: its CPU time and its peak resident memory, read
//! off the ssh stand-in (M17 design §1).
//!
//! ```text
//!   outer script (the stand-in)
//!     ├── sh -c '<serve>; rc=$?; times >"$ledger"; exit "$rc"'   ← the transport's stdin/stdout
//!     │      └── serve and the processes it starts and waits for
//!     └── sampler (background): the largest VmHWM over the tree, every 50 ms
//!   → one file per connection in [`ledger_dir`]: the `times` line of the waited-for children,
//!     then the peak in KiB
//! ```
//!
//! The `times` builtin goes to a file, never to stdout (the transport stream), and the shell
//! exits with the server's status. The sampler runs beside that shell, not inside it, so the
//! `times` line does not hold its CPU.

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

/// What one connection cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spent {
    /// User CPU time of the server and the processes it waited for.
    pub user: Duration,
    /// System CPU time of the same.
    pub system: Duration,
    /// The largest `VmHWM` over the server's process tree, in KiB.
    pub peak_kib: u64,
}

impl Spent {
    /// User and system CPU time.
    pub fn cpu(&self) -> Duration {
        self.user + self.system
    }

    /// The `times` builtin's line of the waited-for children (`0m0.150s 0m0.050s`: user, then
    /// system; `dash` and `bash` both print minutes and fractional seconds) and the peak.
    pub fn parse(times_line: &str, peak_kib: u64) -> Option<Spent> {
        let mut words = times_line.split_whitespace();
        let user = clock(words.next()?)?;
        let system = clock(words.next()?)?;
        if words.next().is_some() {
            return None;
        }
        Some(Spent {
            user,
            system,
            peak_kib,
        })
    }
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

    // 2. Each: the times line, then the peak.
    files
        .into_iter()
        .skip(mark)
        .filter_map(|(_, _, path)| {
            let text = fs::read_to_string(path).ok()?;
            let mut lines = text.lines();
            let times = lines.next()?;
            let peak = lines.next()?.trim().parse().ok()?;
            Spent::parse(times, peak)
        })
        .collect()
}

/// How often the sampler reads `VmHWM`.
const SAMPLE: Duration = Duration::from_millis(50);

/// The largest `VmHWM` over the descendants of `$$` (not the sampler's own), in KiB.
const TREE_PEAK: &str = r#"cat /proc/[0-9]*/status 2>/dev/null | awk -v root="$$" -v skip="$me" '
/^Pid:/ { pid = $2 }
/^PPid:/ { par[pid] = $2; seen[pid] = 1 }
/^VmHWM:/ { hwm[pid] = $2 }
END {
  best = 0
  for (p in seen) {
    if (p == skip) { continue }
    q = par[p]
    hops = 0
    while (q > 1 && q != root && q != skip && hops < 64) { q = par[q]; hops++ }
    if (q == root && hwm[p] > best) { best = hwm[p] }
  }
  print best
}'"#;

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
         echo 0 > \"$peak\"\n\
         # The sampler runs beside the shell that times the server, so its CPU is not counted.\n\
         (\n\
         \x20 read me _ < /proc/self/stat\n\
         \x20 best=0\n\
         \x20 while [ ! -e \"$stop\" ]; do\n\
         \x20   now=$({TREE_PEAK})\n\
         \x20   if [ \"${{now:-0}}\" -gt \"$best\" ]; then best=$now; echo \"$best\" > \"$peak\"; fi\n\
         \x20   sleep {sample}\n\
         \x20 done\n\
         ) >/dev/null 2>&1 &\n\
         sampler=$!\n\
         # The server in the foreground (it owns the transport); `times` goes to the file.\n\
         sh -c '\"$@\"; rc=$?; times > \"$0\"; exit \"$rc\"' \"$spent\" {words}\n\
         rc=$?\n\
         : > \"$stop\"\n\
         wait \"$sampler\"\n\
         {{ sed -n 2p \"$spent\"; cat \"$peak\"; }} > \"$ledger/.$id.done\"\n\
         mv \"$ledger/.$id.done\" \"$ledger/$id\"\n\
         rm -f \"$stop\" \"$peak\" \"$spent\"\n\
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

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{Accounting, Spent, ledger_dir, spent_since};
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
        let dash = Spent::parse("0m0.150s 1m2.5s", 7).expect("parses");
        assert_eq!(dash.user.as_millis(), 150);
        assert_eq!(dash.system.as_millis(), 62_500);
        assert_eq!(dash.peak_kib, 7);
        assert_eq!(
            Spent::parse("0m0.204s 0m0.011s", 0)
                .expect("parses")
                .cpu()
                .as_millis(),
            215
        );
        assert_eq!(Spent::parse("0m0.1s", 0), None);
        assert_eq!(Spent::parse("fast slow", 0), None);
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
