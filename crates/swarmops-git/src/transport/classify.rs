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
//!                                             [remote rejected] (stale: … fetch first), or
//!                                               (failed to update ref) with this ref's lost lock
//!                                                                                  → Stale
//!                                             [remote rejected] (unpacker error)  → stderr's row,
//!                                               but not Disconnected when our pack-objects died
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
            "unable to unpack",
            "cannot read existing object",
            "pack is corrupted (sha1 mismatch)",
            "final sha1 did not match",
            "is corrupt",
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
            // `swarmops serve`'s refusal, as git relays it.
            "remote error: e_auth:",
        ],
        Verdict::Rejected(RejectReason::Auth),
    ),
    (
        &[
            "repository not found",
            "returned error: 404",
            "does not appear to be a git repository",
            "' not found",
            "remote error: e_not_a_repo:",
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
    match row(said) {
        Some(verdict) => judged(verdict, Vec::new(), said),
        None => rejected(RejectReason::Unclassified, Vec::new(), said),
    }
}

/// The verdict of the first row of [`ROWS`] that matches `said`'s stderr.
fn row(said: &Said<'_>) -> Option<Verdict> {
    let text = format!("\n{}", said.stderr.to_ascii_lowercase());
    ROWS.iter()
        .find(|(needles, _)| needles.iter().any(|needle| text.contains(needle)))
        .map(|(_, verdict)| *verdict)
}

/// The error `verdict` names; a refusal carries `refs`.
fn judged(verdict: Verdict, refs: Vec<(String, String)>, said: &Said<'_>) -> TransportError {
    match verdict {
        Verdict::Sha256 => TransportError::Sha256,
        Verdict::Unreachable(cause) => TransportError::Unreachable {
            cause,
            stderr_tail: tail(said.stderr),
        },
        Verdict::Rejected(reason) => rejected(reason, refs, said),
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
    if ours {
        return rejected(RejectReason::Declined, refs, said);
    }

    // 1. The remote lost a race for the ref: serve's hook saw it move, or receive-pack lost
    //    the ref's lock to another push. The lease reads it as stale.
    let serve_stale = reason.starts_with(SERVE_STALE) && reason.ends_with(FETCH_FIRST);
    let lock_lost = reason == LOCK_FAILED && lost_lock(said.stderr, dst);
    if serve_stale || lock_lost {
        return TransportError::Stale {
            branch: branch.to_owned(),
        };
    }

    // 2. The remote could not unpack what we sent: the stderr says why, unless our own
    //    pack-objects died (the remote saw the end of a stream nothing cut).
    if reason == UNPACKER_ERROR {
        let local = said.stderr.to_ascii_lowercase().contains(PACK_DIED);
        match row(said) {
            Some(Verdict::Unreachable(Cause::Disconnected)) if local => {}
            Some(verdict) => return judged(verdict, refs, said),
            None => {}
        }
    }
    rejected(RejectReason::Declined, refs, said)
}

/// serve's hook reason for a ref that moved: `stale: <ref> moved; fetch first`.
const SERVE_STALE: &str = "stale: ";
const FETCH_FIRST: &str = "fetch first";

/// receive-pack's reason for a ref it could not move; its stderr says why.
const LOCK_FAILED: &str = "failed to update ref";

/// receive-pack's reason for a pack it could not unpack.
const UNPACKER_ERROR: &str = "unpacker error";

/// Our own pack-objects' end, lower-cased: `error: pack-objects died of signal 9`.
const PACK_DIED: &str = "pack-objects died";

/// The lengths of a SHA-1 and a SHA-256 object id in hex.
const OID_HEX: [usize; 2] = [40, 64];

/// Whether `stderr` says `dst`'s lock was lost to another push:
/// `cannot lock ref '<dst>': is at <hex> but expected <hex>`.
fn lost_lock(stderr: &str, dst: &str) -> bool {
    let head = format!("cannot lock ref '{dst}': is at ");
    stderr.lines().any(|line| {
        let Some((_, rest)) = line.split_once(&head) else {
            return false;
        };
        let Some((at, expected)) = rest.split_once(" but expected ") else {
            return false;
        };
        is_oid(at) && is_oid(expected.trim_end())
    })
}

/// Whether `text` is an object id in hex.
fn is_oid(text: &str) -> bool {
    OID_HEX.contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
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

    /// A needle read before, and the first needle of `rows` that holds one read before it. One in
    /// a row above takes its every match to that row's verdict; one before it in its row leaves it
    /// no match of its own, the verdict the same.
    fn first_shadowed<'a>(rows: &[(&[&'a str], Verdict)]) -> Option<(&'a str, &'a str)> {
        let needles: Vec<&str> = rows
            .iter()
            .flat_map(|(needles, _)| needles.iter().copied())
            .collect();
        needles.iter().enumerate().find_map(|(at, needle)| {
            let earlier = needles[..at]
                .iter()
                .find(|before| needle.contains(*before))?;
            Some((*earlier, *needle))
        })
    }

    /// Every needle can decide a match: none holds a needle read before it.
    #[test]
    fn no_needle_is_shadowed() {
        // 1. The check finds a shadow across rows: a bad-objects `"denied"` would take Auth's
        //    `"permission denied ("`, so an ssh key refusal would read as bad objects.
        let bad: [(&[&str], Verdict); 2] = [
            (
                &["fsck error", "denied"],
                Verdict::Rejected(RejectReason::BadObjects),
            ),
            (
                &["permission denied ("],
                Verdict::Rejected(RejectReason::Auth),
            ),
        ];
        assert_eq!(
            first_shadowed(&bad),
            Some(("denied", "permission denied ("))
        );

        // 2. The table has none.
        assert_eq!(first_shadowed(&ROWS), None);
    }

    #[test]
    fn bad_objects_rows() {
        // git 2.43.0's wording: the fsck refusal of index-pack and unpack-objects, the fetch's
        // connectivity check, a corrupt object of the mirror, a pack garbled in transit, and
        // rev-list's traversal of a missing or unreadable object.
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
            // A mirror's own loose object garbled: as the tip index-pack checks, as a tree the
            // fetch walks.
            "error: unable to unpack 7777 header
fatal: cannot read existing object info 7777
fatal: index-pack failed",
            "fatal: loose object 8888 (stored in ./objects/88/88) is corrupt",
            // A pack whose bytes changed in transit: index-pack's checksum refusal.
            "fatal: pack is corrupted (SHA1 mismatch)
fatal: index-pack failed",
        ];
        // unpack-objects' checksum refusal of the same pack: git's wording, which no fetch here
        // gives (`fetch.fsckObjects` makes git receive every pack with index-pack).
        let unpacked = "fatal: final sha1 did not match
fatal: unpack-objects failed";
        for stderr in fetched.into_iter().chain([unpacked]) {
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

    /// The branch of the race tests, and its ref.
    const MAIN: &str = "main";
    const DST: &str = "refs/heads/main";

    /// What a push refused with `summary` on `DST` gives, `stderr` from a git that exited 1.
    fn refused_with(summary: &str, stderr: &str) -> Result<PushOutcome, TransportError> {
        let porcelain = format!("To u\n!\tabc:{DST}\t{summary}\nDone\n");
        let said = Said {
            exit: Some(1),
            stderr,
            token_env_unset: false,
        };
        pushed(&porcelain, DST, MAIN, &said)
    }

    /// The reason a refused push classifies as, and the refs it names, or a panic.
    fn declined_refs(summary: &str, stderr: &str) -> (RejectReason, Vec<(String, String)>) {
        match refused_with(summary, stderr) {
            Err(TransportError::Rejected { reason, refs, .. }) => (reason, refs),
            other => panic!("{summary:?} {stderr:?}: {other:?}"),
        }
    }

    /// The stale outcome of `MAIN`.
    fn stale() -> Result<PushOutcome, TransportError> {
        Err(TransportError::Stale {
            branch: MAIN.to_owned(),
        })
    }

    #[test]
    fn serve_stale_is_stale() {
        // serve's pre-receive hook refuses a ref that moved since the advertisement.
        let summary = "[remote rejected] (stale: refs/heads/main moved; fetch first)";
        assert_eq!(refused_with(summary, ""), stale());
    }

    #[test]
    fn lock_race_is_stale() {
        // 1. Probe 4e: receive-pack lost the ref lock to another push, SHA-1.
        let summary = "[remote rejected] (failed to update ref)";
        let sha1 = "remote: error: cannot lock ref 'refs/heads/main': is at 4b56fe10d1cb715b57e0dd432b24f2a0c613de86 but expected c58be8c9a8644805c530f1f7572f18489cbcaded        \nerror: failed to push some refs to 'ssh://remote.invalid/acme.git'\n";
        assert_eq!(refused_with(summary, sha1), stale());

        // 2. The same race in a SHA-256 repository.
        let at = "a".repeat(64);
        let expected = "b".repeat(64);
        let sha256 = format!(
            "remote: error: cannot lock ref 'refs/heads/main': is at {at} but expected {expected}\n"
        );
        assert_eq!(refused_with(summary, &sha256), stale());

        // 3. Another ref's race is not this ref's.
        let other = sha1.replace("refs/heads/main", "refs/heads/other");
        assert_eq!(
            declined_refs(summary, &other).0,
            RejectReason::Declined,
            "{other:?}"
        );
    }

    #[test]
    fn a_lock_failure_that_is_no_race_stays_declined() {
        // A lock file left behind, a ref in the way of another, a bare failure, GitHub's wording.
        let failed = "[remote rejected] (failed to update ref)";
        let cases = [
            (
                failed,
                "remote: error: cannot lock ref 'refs/heads/main': Unable to create '/srv/r.git/refs/heads/main.lock': File exists.\n",
            ),
            (
                failed,
                "remote: error: cannot lock ref 'refs/heads/a/b': 'refs/heads/a' exists; cannot create 'refs/heads/a/b'\n",
            ),
            (failed, ""),
            ("[remote rejected] (incorrect old value provided)", ""),
        ];
        for (summary, stderr) in cases {
            assert_eq!(
                declined_refs(summary, stderr).0,
                RejectReason::Declined,
                "{summary:?} {stderr:?}"
            );
        }
    }

    #[test]
    fn cut_send_is_disconnected() {
        // 1. Probe 7k: the upload cut after 300 KB, the remote alive.
        let summary = "[remote rejected] (unpacker error)";
        let cut = "remote: fatal: early EOF        \nerror: remote unpack failed: unpack-objects abnormal exit\nerror: failed to push some refs to 'ssh://remote.invalid/acme.git'\n";
        assert!(
            matches!(
                refused_with(summary, cut),
                Err(TransportError::Unreachable {
                    cause: Cause::Disconnected,
                    ..
                })
            ),
            "{cut:?}"
        );

        // 2. A pack the remote's fsck refused stays bad objects.
        let fsck = format!("remote: fatal: fsck error in packed object\n{cut}");
        assert_eq!(declined_refs(summary, &fsck).0, RejectReason::BadObjects);

        // 3. An unpacker error no row explains stays the remote's reason.
        let odd = "error: remote unpack failed: index-pack abnormal exit\n";
        assert_eq!(
            declined_refs(summary, odd),
            (
                RejectReason::Declined,
                vec![(DST.to_owned(), "unpacker error".to_owned())]
            )
        );
    }

    #[test]
    fn a_local_pack_failure_is_not_disconnected() {
        // Our own pack-objects died: the remote saw the stream end, nothing on the wire failed.
        let summary = "[remote rejected] (unpacker error)";
        let died = "error: pack-objects died of signal 9\nremote: fatal: early EOF        \nerror: remote unpack failed: unpack-objects abnormal exit\nerror: failed to push some refs to 'ssh://remote.invalid/acme.git'\n";
        assert_eq!(
            declined_refs(summary, died),
            (
                RejectReason::Declined,
                vec![(DST.to_owned(), "unpacker error".to_owned())]
            )
        );
    }

    #[test]
    fn serve_auth_is_auth() {
        // Probe G→S 5.6 and 5.7: serve's JSON envelope before and after git's line.
        let no_grant = r#"{"ok":false,"op":"serve","api":1,"error":{"code":"E_AUTH","message":"no grant of access.toml names person/mallory","retryable":false,"exit":5,"details":{"reason":"unknown_identity"},"hint":"the operator writes access.toml in the store's root: version = 1, then one [[grant]] per set of callers (who, read, push, force, land)"},"meta":{"elapsed_ms":25,"resolved":{},"auth":"trust"}}
fatal: remote error: E_AUTH: no grant of access.toml names person/mallory
"#;
        let no_caller = r#"fatal: remote error: E_AUTH: serve names no caller: --as is not given, and SWARMOPS_AGENT does not name one for serve
{"ok":false,"op":"serve","api":1,"error":{"code":"E_AUTH","message":"serve names no caller: --as is not given, and SWARMOPS_AGENT does not name one for serve","retryable":false,"exit":5,"details":{"reason":"no_identity"},"hint":"name the caller with --as <family>/<instance> on serve's command line: the command= of its authorized_keys line, or the web server's CGI entry"},"meta":{"elapsed_ms":1,"resolved":{},"auth":"trust"}}
"#;
        for stderr in [no_grant, no_caller] {
            assert_eq!(reason(failure, stderr), RejectReason::Auth, "{stderr:?}");
        }
    }

    #[test]
    fn serve_not_a_repo_is_not_found() {
        // Probe G→S 5.8: no store of that name is served.
        let stderr = r#"{"ok":false,"op":"serve","api":1,"error":{"code":"E_NOT_A_REPO","message":"no store named nope is served","retryable":false,"exit":6,"details":{},"hint":"ask the server's operator for the store's name"},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
fatal: remote error: E_NOT_A_REPO: no store named nope is served
"#;
        assert_eq!(reason(failure, stderr), RejectReason::NotFound);
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
