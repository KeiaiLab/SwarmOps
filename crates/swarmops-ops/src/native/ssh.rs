//! The host's [`Link`] over ssh (the native sync design §2.1): `$GIT_SSH_COMMAND`, else `ssh`,
//! asked to run `swarmops serve --stdio '<store>'` on the upstream, its stdio the session.
//!
//! ```text
//!   ssh -o BatchMode=yes [-p <port>] [user@]host "swarmops serve --stdio '<store>'"
//!     env: the one git's children get (GitCli's cleaned environment), so both wires agree
//!     stdout ─▶ Sniff (keeps the first bytes) ─▶ StreamLink ─▶ the session
//! ```
//!
//! A server older than native sync answers the line with git's `ERR` pkt-line; the first bytes
//! kept show it ([`SshLink::git_refusal`]). Dropping the link closes the upstream's stdin and
//! waits a short while for it to end, then kills it.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use swarmops_api::OpError;
use swarmops_git::transport::GitCli;

use super::link::{Answer, Link, StreamLink};
use super::url::NativeUrl;
use super::{Cause, unreachable};
use crate::error;

/// The variable git takes its ssh command from, which native sync takes too.
const SSH_COMMAND_ENV: &str = "GIT_SSH_COMMAND";

/// The ssh program when [`SSH_COMMAND_ENV`] is unset.
const SSH_PROGRAM: &str = "ssh";

/// The command the upstream's forced command reads from `SSH_ORIGINAL_COMMAND`.
const STDIO_COMMAND: &str = "swarmops serve --stdio";

/// How many of the upstream's first bytes are kept to recognise a git server's refusal.
const SNIFF_BYTES: usize = 512;

/// The digits of a pkt-line's length, then git's refusal prefix.
const PKT_LEN_DIGITS: usize = 4;
const PKT_ERR: &[u8] = b"ERR ";

/// How long a closed session's upstream may take to end before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// How often the end of the upstream is looked for.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// One native session over an ssh child.
pub(crate) struct SshLink {
    /// The session; `None` once dropped, which closes the child's stdin.
    link: Option<StreamLink>,
    child: Child,
    /// What the upstream sent first, and whether it ended.
    sniffed: Arc<Mutex<Sniffed>>,
}

impl SshLink {
    /// Spawns ssh to `url`'s host, asking for the session on its store, in the environment of
    /// git's children. `E_GIT_UNSUPPORTED` when no git is found (its environment rules are
    /// git's); `E_REMOTE_UNREACHABLE` when ssh cannot be started.
    pub(crate) fn connect(url: &NativeUrl) -> Result<SshLink, OpError> {
        // 1. The environment git's children get: git's rules, one for both wires.
        let git = GitCli::find()
            .map_err(|e| error::transport_error(e, &url.store, error::Phase::Fetch))?;
        let base = git.base_command();
        let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = (base.get_envs())
            .filter_map(|(key, value)| Some((key.to_owned(), value?.to_owned())))
            .collect();
        let ssh_command = (env.iter())
            .find(|(key, value)| key == SSH_COMMAND_ENV && !value.is_empty())
            .map(|(_, value)| value.clone());

        // 2. The command: the user's ssh command as git runs it (through sh, our words as its
        //    arguments, never in its text), else ssh itself.
        let mut command = match ssh_command {
            Some(line) => {
                let mut sh = Command::new("sh");
                let mut script = line.clone();
                script.push(" \"$@\"");
                sh.arg("-c").arg(script).arg(line);
                sh
            }
            None => Command::new(SSH_PROGRAM),
        };
        command.arg("-o").arg("BatchMode=yes");
        if let Some(port) = url.port {
            command.arg("-p").arg(port.to_string());
        }
        let line = format!("{STDIO_COMMAND} '{}'", url.store);
        command.arg(url.destination()).arg(line);

        // 3. Started, its stdio the session.
        command
            .env_clear()
            .envs(env)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|e| unreachable(Cause::of(&e)))?;
        let (Some(input), Some(output)) = (child.stdout.take(), child.stdin.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unreachable(Cause::Disconnected));
        };
        let sniffed = Arc::new(Mutex::new(Sniffed::default()));
        let sniff = Sniff {
            inner: input,
            sniffed: Arc::clone(&sniffed),
        };
        Ok(SshLink {
            link: Some(StreamLink::new(sniff, output)),
            child,
            sniffed,
        })
    }

    /// The message of git's `ERR` pkt-line, if that is how the upstream began its answer: a
    /// server that does not speak native sync. It ends the session first and waits for the
    /// upstream's answer to be read to its end, so that a refusal written as the upstream
    /// exited is seen.
    pub(crate) fn git_refusal(&mut self) -> Option<String> {
        self.close();
        let deadline = Instant::now() + EXIT_GRACE;
        loop {
            let sniffed = self.sniffed.lock().unwrap_or_else(PoisonError::into_inner);
            if sniffed.ended || Instant::now() >= deadline {
                return refusal_in(&sniffed.kept);
            }
            drop(sniffed);
            thread::sleep(EXIT_POLL);
        }
    }

    /// Closes the upstream's stdin, so that its session ends; waits [`EXIT_GRACE`] for it, then
    /// kills it.
    fn close(&mut self) {
        drop(self.link.take());
        let deadline = Instant::now() + EXIT_GRACE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(None) => thread::sleep(EXIT_POLL),
                _ => return,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The message of the `ERR` pkt-line `kept` starts with, to its end or its newline.
fn refusal_in(kept: &[u8]) -> Option<String> {
    let (len, rest) = kept.split_at_checked(PKT_LEN_DIGITS)?;
    if !len.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let message = rest.strip_prefix(PKT_ERR)?;
    let end = message
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(message.len());
    Some(String::from_utf8_lossy(&message[..end]).into_owned())
}

impl Link for SshLink {
    fn call(&mut self, line: &[u8], upload: &[u8]) -> Result<Answer<'_>, OpError> {
        let Some(session) = self.link.as_mut() else {
            return Err(unreachable(Cause::Disconnected));
        };
        session.call(line, upload)
    }
}

impl Drop for SshLink {
    /// [`SshLink::close`], once.
    fn drop(&mut self) {
        if self.link.is_some() {
            self.close();
        }
    }
}

/// What [`Sniff`] saw of the upstream's stdout.
#[derive(Default)]
struct Sniffed {
    /// Its first [`SNIFF_BYTES`].
    kept: Vec<u8>,
    /// Whether it ended (or failed).
    ended: bool,
}

/// The upstream's stdout, its first [`SNIFF_BYTES`] kept as they pass.
struct Sniff<R> {
    inner: R,
    sniffed: Arc<Mutex<Sniffed>>,
}

impl<R: Read> Read for Sniff<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf);
        let mut sniffed = self.sniffed.lock().unwrap_or_else(PoisonError::into_inner);
        match &read {
            Ok(0) | Err(_) => sniffed.ended = true,
            Ok(n) => {
                let room = SNIFF_BYTES.saturating_sub(sniffed.kept.len()).min(*n);
                sniffed.kept.extend_from_slice(&buf[..room]);
            }
        }
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git's refusal is read off the first bytes; anything else is none.
    #[test]
    fn a_git_err_line_is_recognised() {
        let read = |bytes: &[u8]| {
            let sniffed = Arc::new(Mutex::new(Sniffed::default()));
            let mut sniff = Sniff {
                inner: bytes,
                sniffed: Arc::clone(&sniffed),
            };
            io::copy(&mut sniff, &mut io::sink()).unwrap();
            let sniffed = sniffed.lock().unwrap();
            assert!(sniffed.ended);
            sniffed.kept.clone()
        };
        let err = read(b"0016ERR E_X: not here\n");
        assert_eq!(refusal_in(&err).as_deref(), Some("E_X: not here"));
        assert_eq!(refusal_in(&read(b"{\"ok\":true}\n")), None);
        assert_eq!(read(&[b'a'; 2 * SNIFF_BYTES]).len(), SNIFF_BYTES);
    }
}
