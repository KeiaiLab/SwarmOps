//! A running command's memory, sampled (`check run`'s run lines, M31-11): the resident memory of
//! every process of its process group, summed, read from `/proc` on Linux.
//!
//! ```text
//!   /proc/<pid>/stat    "<pid> (<comm>) <state> <ppid> <pgrp> …" ─▶ group_of: <pgrp>
//!   /proc/<pid>/status  "VmRSS:    1234 kB"                      ─▶ rss_kib_of: 1234
//!   sample(group) = Σ VmRSS of every <pid> whose <pgrp> is the group
//! ```
//!
//! A sample, not the kernel's high-water mark: what the group held between two samples is not
//! seen, pages two of its processes share are counted for each, and a process that left the
//! group is not counted. Other platforms give no sample.

/// The resident memory of the process group `group` now, in KiB: the sum over every process of
/// `/proc` whose group it is; none when no process of the group was seen. An entry that does not
/// read (a process gone since it was listed, another account's hidden one) is passed over, and a
/// process that ended between its two reads counts nothing.
#[cfg(target_os = "linux")]
pub(super) fn sample(group: u32) -> Option<u64> {
    let mut sum = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        // 1. A process, of the group.
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if group_of(&stat) != Some(group) {
            continue;
        }

        // 2. What it holds.
        let held = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| rss_kib_of(&status))
            .unwrap_or(0);
        sum = Some(sum.unwrap_or(0_u64).saturating_add(held));
    }
    sum
}

/// No sample: only Linux's `/proc` is read.
#[cfg(not(target_os = "linux"))]
pub(super) fn sample(_group: u32) -> Option<u64> {
    None
}

/// The process group a `/proc/<pid>/stat` line names: its fifth field, counted from the end of
/// the command's name, the last `)`, since the name may hold spaces and parentheses.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub(super) fn group_of(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_ascii_whitespace().nth(2)?.parse().ok()
}

/// The resident memory a `/proc/<pid>/status` text gives, in KiB: its `VmRSS` line's number.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub(super) fn rss_kib_of(status: &str) -> Option<u64> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?;
    line.split_ascii_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{group_of, rss_kib_of};

    /// The group is the fifth field, counted after the command's name, whatever the name holds.
    #[test]
    fn the_group_is_read_past_a_name_with_spaces_and_parentheses() {
        let plain = "4242 (sleep) S 4000 4100 4000 34816 4100 4194304 95 0 0 0 0 0";
        assert_eq!(group_of(plain), Some(4100), "a plain name");
        let odd = "4242 (a b) c) (d) R 1 77 77 0 -1 4194560 1 0";
        assert_eq!(
            group_of(odd),
            Some(77),
            "a name with spaces and parentheses"
        );
        assert_eq!(group_of("4242 (sleep"), None, "no end of the name");
        assert_eq!(group_of("4242 (sleep) S 1"), None, "cut before the group");
        assert_eq!(group_of("4242 (sleep) S 1 x"), None, "no number");
        assert_eq!(group_of(""), None, "nothing");
    }

    /// The resident memory is `VmRSS`'s number, in kB; a process without one (a kernel thread, a
    /// zombie) has none.
    #[test]
    fn the_resident_memory_is_vm_rss() {
        let status = "Name:\tsleep\nVmPeak:\t    9000 kB\nVmRSS:\t    1234 kB\nRssAnon:\t 100 kB\n";
        assert_eq!(rss_kib_of(status), Some(1234), "its VmRSS");
        assert_eq!(
            rss_kib_of("Name:\tkthreadd\nState:\tS (sleeping)\n"),
            None,
            "none"
        );
        assert_eq!(rss_kib_of("VmRSS:\tmany kB\n"), None, "no number");
    }

    /// On Linux, this process's own group holds memory.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sample_of_this_process_s_group_is_above_zero() {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let group = group_of(&stat).unwrap();
        let sampled = super::sample(group);
        assert!(sampled.is_some_and(|kib| kib > 0), "{sampled:?}");
    }

    /// A group no process is of gives no sample.
    #[test]
    fn a_group_of_no_process_gives_no_sample() {
        assert_eq!(super::sample(u32::MAX), None, "no process is of this group");
    }
}
