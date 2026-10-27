//! The classifier of git's failures (operations spec §6; the M13 brief's §3.5): one table that
//! reads the exit status, push's porcelain flag and reason for a ref, and fixed English
//! substrings of git's own messages (the child runs with `LC_ALL=C`). The first row that matches
//! wins; nothing that matches no row is retried.
//!
//! ```text
//!   push's porcelain line of the branch:  =  → AlreadyThere      ' ' * + → Sent
//!                                          !  [rejected] (stale info)              → Stale
//!                                             [rejected] (non-fast-forward | fetch first)
//!                                                                                  → NotFastForward
//!                                             anything else                        → Rejected declined
//!   no line, or another command:  stderr against ROWS, in order  → Sha256 | Rejected | Unreachable
//!                                 no row                         → Rejected unclassified
//!   the closure check (rev-list):  stderr against TRAVERSAL      → Rejected bad_objects
//!                                  else as another command
//! ```

use super::{Cause, PushOutcome, RejectReason, TransportError};

/// The bytes of git's stderr an error carries: its end.
const TAIL: usize = 4 << 10;

/// The `remote:` lines an error carries at most.
const MAX_REMOTE_MESSAGES: usize = 50;

/// What a row of the table says the failure is.
#[derive(Clone, Copy)]
enum Verdict {
    /// The remote is a SHA-256 repository.
    Sha256,
    /// The remote or git refused, for this reason.
    Rejected(RejectReason),
    /// The network or the host did not answer, for this cause.
    Unreachable(Cause),
}

/// The table: substrings of git's messages, lower-cased, and what they mean. `\n` starts a line.
const ROWS: [(&[&str], Verdict); 10] = [
    (&["mismatched algorithms", "server sha256"], Verdict::Sha256),
    (
        &[
            "fsck error",
            "error in object",
            "did not send all necessary objects",
            "missing blob object",
            "missing tree object",
            "missing commit object",
            "missing tag object",
            "bad object",
            "bad tree object",
            "bad blob object",
        ],
        Verdict::Rejected(RejectReason::BadObjects),
    ),
    (
        &[
            "authentication failed",
            "could not read username",
            "could not read password",
            "terminal prompts disabled",
            "permission denied (",
            "returned error: 401",
            "returned error: 403",
            "invalid username or password",
        ],
        Verdict::Rejected(RejectReason::Auth),
    ),
    (
        &[
            "repository not found",
            "returned error: 404",
            "does not appear to be a git repository",
            "' not found",
        ],
        Verdict::Rejected(RejectReason::NotFound),
    ),
    (
        &[
            "could not resolve host",
            "name or service not known",
            "temporary failure in name resolution",
            "nodename nor servname",
        ],
        Verdict::Unreachable(Cause::Dns),
    ),
    (
        &[
            "ssl certificate problem",
            "ssl_connect",
            "ssl connect error",
            "gnutls",
            "tls handshake",
            "certificate verify failed",
            "ssl routines",
        ],
        Verdict::Unreachable(Cause::Tls),
    ),
    (
        &["timed out", "operation too slow", "timeout was reached"],
        Verdict::Unreachable(Cause::Timeout),
    ),
    (
        &[
            "\nssh: ",
            "connection closed by",
            "kex_exchange_identification",
        ],
        Verdict::Unreachable(Cause::Ssh),
    ),
    (
        &[
            "connection refused",
            "failed to connect to",
            "couldn't connect to server",
            "connection reset",
            "network is unreachable",
            "no route to host",
        ],
        Verdict::Unreachable(Cause::Connect),
    ),
    (
        &[
            "early eof",
            "the remote end hung up unexpectedly",
            "unexpected disconnect",
            "transfer closed with outstanding read data",
            "rpc failed",
            "unexpected end of file",
        ],
        Verdict::Unreachable(Cause::Disconnected),
    ),
];

/// `rev-list`'s own traversal failures, lower-cased: an object the closure check cannot read.
/// Read for that command only: ssh's "could not read from remote repository" is no object's.
const TRAVERSAL: [&str; 3] = ["unable to read", "could not read", "is corrupt"];

/// What git said: its exit, its stderr (redacted), and whether the remote's token variable is
/// unset.
pub(super) struct Said<'a> {
    /// The exit status; `None` when a signal ended it.
    pub(super) exit: Option<i32>,
    /// The end of its stderr, redacted.
    pub(super) stderr: &'a str,
    /// Whether the remote names a token variable that holds no token.
    pub(super) token_env_unset: bool,
}

/// What a failed command's `said` means, by the first row of [`ROWS`] that matches.
pub(super) fn failure(said: &Said<'_>) -> TransportError {
    let text = format!("\n{}", said.stderr.to_ascii_lowercase());
    let verdict = ROWS
        .iter()
        .find(|(needles, _)| needles.iter().any(|needle| text.contains(needle)))
        .map(|(_, verdict)| *verdict);
    match verdict {
        Some(Verdict::Sha256) => TransportError::Sha256,
        Some(Verdict::Unreachable(cause)) => TransportError::Unreachable {
            cause,
            stderr_tail: tail(said.stderr),
        },
        Some(Verdict::Rejected(reason)) => rejected(reason, Vec::new(), said),
        None => rejected(RejectReason::Unclassified, Vec::new(), said),
    }
}

/// What the closure check's failure `said` means: [`RejectReason::BadObjects`] for an object
/// [`ROWS`] or [`TRAVERSAL`] names, else as [`failure`] reads it.
pub(super) fn closure(said: &Said<'_>) -> TransportError {
    let text = said.stderr.to_ascii_lowercase();
    if TRAVERSAL.iter().any(|needle| text.contains(needle)) {
        return rejected(RejectReason::BadObjects, Vec::new(), said);
    }
    failure(said)
}

/// What a push did to `dst` (the branch `branch`), by its porcelain line in `porcelain`; with no
/// line for it, the push failed before any ref, and `said` is classified.
pub(super) fn pushed(
    porcelain: &str,
    dst: &str,
    branch: &str,
    said: &Said<'_>,
) -> Result<PushOutcome, TransportError> {
    // `<flag>\t<from>:<to>\t<summary>`, the line of our ref.
    let line = porcelain.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let flag = fields.next()?;
        let (_, to) = fields.next()?.rsplit_once(':')?;
        (to == dst).then(|| (flag, fields.next().unwrap_or_default()))
    });
    let Some((flag, summary)) = line else {
        return Err(failure(said));
    };

    // The flag: `=` up to date, ` ` `*` `+` moved, `!` refused.
    match flag {
        "=" if said.exit == Some(0) => Ok(PushOutcome::AlreadyThere),
        " " | "*" | "+" if said.exit == Some(0) => Ok(PushOutcome::Sent),
        "!" => Err(refused(summary, dst, branch, said)),
        _ => Err(failure(said)),
    }
}

/// Why the remote refused `dst`, by the porcelain `summary` (`[rejected] (stale info)`).
fn refused(summary: &str, dst: &str, branch: &str, said: &Said<'_>) -> TransportError {
    let reason = summary
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .unwrap_or(summary);
    let ours = summary.starts_with("[rejected]");
    if ours && reason == "stale info" {
        return TransportError::Stale {
            branch: branch.to_owned(),
        };
    }
    if ours && (reason == "non-fast-forward" || reason == "fetch first") {
        return TransportError::NotFastForward {
            branch: branch.to_owned(),
        };
    }
    let refs = vec![(dst.to_owned(), reason.to_owned())];
    rejected(RejectReason::Declined, refs, said)
}

/// [`TransportError::Rejected`] for `reason`, with `refs` and what `said` holds.
fn rejected(reason: RejectReason, refs: Vec<(String, String)>, said: &Said<'_>) -> TransportError {
    TransportError::Rejected {
        reason,
        refs,
        remote_messages: remote_messages(said.stderr),
        git_exit: said.exit,
        stderr_tail: tail(said.stderr),
        token_env_unset: said.token_env_unset,
    }
}

/// The remote's own lines (`remote: …`), at most [`MAX_REMOTE_MESSAGES`].
fn remote_messages(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("remote: "))
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(MAX_REMOTE_MESSAGES)
        .map(str::to_owned)
        .collect()
}

/// The last [`TAIL`] bytes of `stderr`, cut at a character boundary.
fn tail(stderr: &str) -> String {
    let mut start = stderr.len().saturating_sub(TAIL);
    while !stderr.is_char_boundary(start) {
        start += 1;
    }
    stderr[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stderr` from a git that exited 128, with a token variable set.
    fn said(stderr: &str) -> Said<'_> {
        Said {
            exit: Some(128),
            stderr,
            token_env_unset: false,
        }
    }

    /// The cause `stderr` classifies as, or a panic.
    fn cause(stderr: &str) -> Cause {
        match failure(&said(stderr)) {
            TransportError::Unreachable { cause, .. } => cause,
            other => panic!("{stderr:?}: {other:?}"),
        }
    }

    #[test]
    fn rows_the_loopback_server_cannot_make() {
        // git's and curl's own wording (git 2.43.0 with libcurl), one line each.
        let rows = [
            (
                "fatal: unable to access 'https://github.invalid/a.git/': Could not resolve host: github.invalid",
                Cause::Dns,
            ),
            (
                "ssh: Could not resolve hostname github.invalid: Name or service not known",
                Cause::Dns,
            ),
            (
                "fatal: unable to access 'https://h/a.git/': SSL certificate problem: self-signed certificate",
                Cause::Tls,
            ),
            (
                "fatal: unable to access 'https://h/a.git/': gnutls_handshake() failed: An unexpected TLS packet was received.",
                Cause::Tls,
            ),
            (
                "fatal: unable to access 'https://h/a.git/': Failed to connect to h port 443 after 130000 ms: Connection timed out",
                Cause::Timeout,
            ),
            (
                "error: RPC failed; curl 28 Operation too slow. Less than 1000 bytes/sec transferred the last 60 seconds",
                Cause::Timeout,
            ),
            (
                "Connection closed by 140.82.121.4 port 22\nfatal: Could not read from remote repository.",
                Cause::Ssh,
            ),
            (
                "fatal: unable to access 'https://h/a.git/': Recv failure: Connection reset by peer",
                Cause::Connect,
            ),
            (
                "fetch-pack: unexpected disconnect while reading sideband packet\nfatal: early EOF",
                Cause::Disconnected,
            ),
        ];
        for (stderr, expected) in rows {
            assert_eq!(cause(stderr), expected, "{stderr:?}");
        }
    }

    #[test]
    fn github_answers_classify() {
        // A private repository the credential cannot see; a token without write access.
        let missing = "remote: Repository not found.\nfatal: repository 'https://github.com/a/b.git/' not found";
        let forbidden = "remote: Permission to a/b.git denied to someone.\nfatal: unable to access 'https://github.com/a/b.git/': The requested URL returned error: 403";
        for (stderr, expected) in [
            (missing, RejectReason::NotFound),
            (forbidden, RejectReason::Auth),
        ] {
            let TransportError::Rejected {
                reason,
                remote_messages,
                ..
            } = failure(&said(stderr))
            else {
                panic!("{stderr:?}");
            };
            assert_eq!(reason, expected);
            assert_eq!(remote_messages.len(), 1);
        }
    }

    /// The reason `stderr` classifies as, by `classify`, or a panic.
    fn reason(classify: fn(&Said<'_>) -> TransportError, stderr: &str) -> RejectReason {
        match classify(&said(stderr)) {
            TransportError::Rejected { reason, .. } => reason,
            other => panic!("{stderr:?}: {other:?}"),
        }
    }

    #[test]
    fn bad_objects_rows() {
        // git 2.43.0's wording: the fsck refusal of index-pack and unpack-objects, the fetch's
        // connectivity check, and rev-list's traversal of a missing or unreadable object.
        let fetched = [
            "error: object 8e01: duplicateEntries: contains duplicate file entries
fatal: fsck error in packed object
fatal: index-pack failed",
            "error: error in object: unshallow 8e01
fatal: unpack-objects failed",
            "fatal: bad object 8e01
error: http://127.0.0.1:1/r.git did not send all necessary objects",
            "fatal: missing blob object '1111'",
            "fatal: bad tree object 2222",
            "fatal: bad object 3333",
        ];
        for stderr in fetched {
            assert_eq!(
                reason(failure, stderr),
                RejectReason::BadObjects,
                "{stderr:?}"
            );
            assert_eq!(
                reason(closure, stderr),
                RejectReason::BadObjects,
                "{stderr:?}"
            );
        }
        let traversal = [
            "error: Could not read 4444
fatal: revision walk setup failed",
            "fatal: unable to read tree 5555",
            "error: object file .git/objects/66/66 is corrupt",
        ];
        for stderr in traversal {
            assert_eq!(
                reason(closure, stderr),
                RejectReason::BadObjects,
                "{stderr:?}"
            );
        }

        // Outside the closure check, ssh's "could not read" is the network's, as before.
        let ssh = "ssh: connect to host h port 22: Connection refused
fatal: Could not read from remote repository.";
        assert_eq!(cause(ssh), Cause::Ssh);
        let odd = "fatal: the flux capacitor is out of alignment";
        assert_eq!(reason(closure, odd), RejectReason::Unclassified);
    }

    #[test]
    fn porcelain_flags() {
        let ok = Said {
            exit: Some(0),
            stderr: "",
            token_env_unset: false,
        };
        let dst = "refs/heads/main";
        let line =
            |flag: &str, summary: &str| format!("To u\n{flag}\tabc:{dst}\t{summary}\nDone\n");

        // Moved, made, forced by the lease, up to date.
        for (flag, outcome) in [
            (" ", PushOutcome::Sent),
            ("*", PushOutcome::Sent),
            ("+", PushOutcome::Sent),
            ("=", PushOutcome::AlreadyThere),
        ] {
            assert_eq!(pushed(&line(flag, "x"), dst, "main", &ok), Ok(outcome));
        }

        // Refused: fetch first is not a fast-forward; another ref's line is not ours.
        let refused = said("");
        assert_eq!(
            pushed(
                &line("!", "[rejected] (fetch first)"),
                dst,
                "main",
                &refused
            ),
            Err(TransportError::NotFastForward {
                branch: "main".to_owned()
            })
        );
        let other = "!\tabc:refs/heads/other\t[rejected] (stale info)\n";
        assert!(matches!(
            pushed(other, dst, "main", &refused),
            Err(TransportError::Rejected {
                reason: RejectReason::Unclassified,
                ..
            })
        ));
    }

    #[test]
    fn tail_keeps_the_end_at_a_char_boundary() {
        let text = format!("{}é{}", "a".repeat(10), "b".repeat(TAIL - 1));
        let kept = tail(&text);
        assert!(kept.len() <= TAIL);
        assert!(kept.ends_with('b'));
        assert_eq!(tail("short"), "short");
    }
}
