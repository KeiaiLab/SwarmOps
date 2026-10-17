//! Counts a process's system calls, sync calls by file, and bytes written, with ptrace: strace
//! is not installed on this host, so this is a small tracer of its own, in safe Rust through
//! nix (x86_64 Linux only). It seizes every thread of a running process, follows the threads
//! it creates, and counts until the process exits. A syscall stop is an entry when `rax`
//! holds `-ENOSYS` (the kernel sets it before the entry stop) and an exit otherwise.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// What one traced process did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    /// Every system call entered.
    pub syscalls: u64,
    /// The sync calls (`fsync`, `fdatasync`, `sync_file_range`, `msync`, `syncfs`, `sync`),
    /// the write calls and the renames, by name.
    pub by_name: BTreeMap<String, u64>,
    /// The sync calls by name and file: `fdatasync meta/journal.log`, `fsync meta/refs/` (a
    /// directory ends in `/`), temp-file names and numbers folded (`.tmp*`, `#`).
    pub syncs_by_target: BTreeMap<String, u64>,
    /// What the write calls returned, summed.
    pub written_bytes: u64,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    use nix::errno::Errno;
    use nix::sys::ptrace::{self, Options};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    use super::{Counts, normalize};

    const ENOSYS: i64 = 38;

    fn name(nr: u64) -> Option<&'static str> {
        Some(match nr {
            1 => "write",
            18 => "pwrite64",
            20 => "writev",
            26 => "msync",
            74 => "fsync",
            75 => "fdatasync",
            82 => "rename",
            162 => "sync",
            264 => "renameat",
            277 => "sync_file_range",
            296 => "pwritev",
            306 => "syncfs",
            316 => "renameat2",
            328 => "pwritev2",
            _ => return None,
        })
    }

    fn is_sync(nr: u64) -> bool {
        matches!(nr, 26 | 74 | 75 | 162 | 277 | 306)
    }

    fn is_write(nr: u64) -> bool {
        matches!(nr, 1 | 18 | 20 | 296 | 328)
    }

    pub struct Tracer {
        pid: Pid,
        root: PathBuf,
        counts: Counts,
    }

    impl Tracer {
        /// Seizes every thread of process `pid` and starts tracing its system calls; the
        /// files of `root` are named relative to it.
        pub fn attach(pid: u32, root: &Path) -> Result<Tracer, String> {
            let pid = Pid::from_raw(i32::try_from(pid).map_err(|e| e.to_string())?);
            let options = Options::PTRACE_O_TRACESYSGOOD
                | Options::PTRACE_O_TRACECLONE
                | Options::PTRACE_O_EXITKILL;
            let tids: Vec<Pid> = fs::read_dir(format!("/proc/{pid}/task"))
                .map_err(|e| format!("list the threads: {e}"))?
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .map(Pid::from_raw)
                .collect();
            for &tid in &tids {
                ptrace::seize(tid, options).map_err(|e| format!("seize {tid}: {e}"))?;
                ptrace::interrupt(tid).map_err(|e| format!("interrupt {tid}: {e}"))?;
            }
            for &tid in &tids {
                loop {
                    match waitpid(tid, Some(WaitPidFlag::__WALL)) {
                        Ok(WaitStatus::PtraceEvent(..)) => break,
                        Ok(WaitStatus::Stopped(_, sig)) => {
                            // A signal arrived first: deliver it and wait for the interrupt.
                            let _ = ptrace::cont(tid, sig);
                        }
                        Ok(other) => return Err(format!("{tid} at attach: {other:?}")),
                        Err(e) => return Err(format!("wait for {tid}: {e}")),
                    }
                }
                ptrace::syscall(tid, None).map_err(|e| format!("resume {tid}: {e}"))?;
            }
            Ok(Tracer {
                pid,
                root: root.to_path_buf(),
                counts: Counts::default(),
            })
        }

        /// Follows the process until it and every thread it made have exited (and reaps it).
        pub fn run(mut self) -> Result<Counts, String> {
            loop {
                let status = match waitpid(None::<Pid>, Some(WaitPidFlag::__WALL)) {
                    Ok(status) => status,
                    Err(Errno::ECHILD) => break,
                    Err(e) => return Err(format!("waitpid: {e}")),
                };
                match status {
                    WaitStatus::PtraceSyscall(tid) => {
                        self.on_syscall(tid);
                        let _ = ptrace::syscall(tid, None);
                    }
                    WaitStatus::PtraceEvent(tid, _, _) => {
                        let _ = ptrace::syscall(tid, None);
                    }
                    WaitStatus::Stopped(tid, sig) => {
                        let _ = ptrace::syscall(tid, sig);
                    }
                    WaitStatus::Exited(tid, _) | WaitStatus::Signaled(tid, _, _)
                        if tid == self.pid =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
            Ok(self.counts)
        }

        fn on_syscall(&mut self, tid: Pid) {
            let Ok(regs) = ptrace::getregs(tid) else {
                return;
            };
            let nr = regs.orig_rax;
            let entry = regs.rax.cast_signed() == -ENOSYS;
            if entry {
                self.counts.syscalls += 1;
                if let Some(name) = name(nr) {
                    *self.counts.by_name.entry(name.to_owned()).or_default() += 1;
                    if is_sync(nr) {
                        let target = match nr {
                            26 => "(a mapping)".to_owned(),
                            162 => "(every filesystem)".to_owned(),
                            _ => self.file(regs.rdi),
                        };
                        *self
                            .counts
                            .syncs_by_target
                            .entry(format!("{name} {target}"))
                            .or_default() += 1;
                    }
                }
            } else if is_write(nr) && regs.rax.cast_signed() > 0 {
                self.counts.written_bytes += regs.rax;
            }
        }

        /// The file that descriptor `fd` of the traced process names, relative to the root.
        fn file(&self, fd: u64) -> String {
            match fs::read_link(format!("/proc/{}/fd/{fd}", self.pid)) {
                Ok(path) => {
                    let dir = fs::metadata(&path).is_ok_and(|m| m.is_dir());
                    let rel = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
                    let mut name = normalize(&rel);
                    if dir {
                        name.push('/');
                    }
                    name
                }
                Err(_) => format!("(fd {fd})"),
            }
        }
    }
}

/// `rel` with temp-file suffixes folded (`.tmpAbC123` → `.tmp*`) and digit runs folded (`#`), so
/// that one file kind counts as one target.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // `.tmp` names a prefix here
fn normalize(rel: &Path) -> String {
    let text = rel.display().to_string();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            out.push('#');
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
        } else {
            out.push(c);
            if out.ends_with(".tmp") {
                out.push('*');
                while chars.peek().is_some_and(char::is_ascii_alphanumeric) {
                    chars.next();
                }
            }
        }
    }
    if out.is_empty() { ".".to_owned() } else { out }
}

/// Traces process `pid` (stopped in a read of its stdin, at a point of its choosing) from now
/// to its exit; `go` is called once the tracer is attached, to let the process go on.
pub fn trace(pid: u32, root: &Path, go: impl FnOnce()) -> Result<Counts, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let tracer = imp::Tracer::attach(pid, root)?;
        go();
        tracer.run()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (pid, root, go);
        Err("the sync-call count needs x86_64 Linux".into())
    }
}

/// Where the counts are relative to: a store directory.
pub fn root_of(dir: &Path) -> PathBuf {
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
}
