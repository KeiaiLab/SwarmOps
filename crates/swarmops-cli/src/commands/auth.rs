//! The `auth` commands: `auth login`, `auth status`, `auth logout` and `auth git-credential`, on
//! the user's login file (`swarmops_ops::auth`).
//!
//! They open no store: the file is the user's, found from the environment
//! (`SWARMOPS_AUTH_FILE`, else the host's state directory), so `--repo` and `--as` are read and
//! not used, and the envelope has no `meta.repo`. `auth login --with-token` reads the token from
//! stdin, since an argument would show in the process list; no error quotes what stdin held.
//!
//! `auth git-credential` is the one command that prints a token, and prints no envelope: git
//! runs it as a credential helper and reads its stdout as `key=value` lines. It is a protocol
//! operation, so whatever a run returns for stdout the entry writes to stderr instead: the
//! envelope of a failure, of a panic or of a command line clap refuses. The answer itself is
//! therefore written here, straight to stdout, and never returned.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};

use swarmops_api::{AuthLogin, AuthLogout, AuthStatus, ErrorCode, OpError};
use swarmops_ops::auth::{self, Provider, Source};

use super::Ctx;
use crate::cli::AuthCommand;
use crate::output::Printed;

/// The most bytes `auth login --with-token` reads of stdin: a token and the white space around
/// it.
const MAX_TOKEN_INPUT: u64 = 4096;

/// Runs `auth`'s `command`, which names operation `op`. Each provider flag is the grammar's to
/// require, and `--gh` is the one there is.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, command: &AuthCommand) -> Printed {
    let no_store = (None, BTreeMap::new(), None);
    match command {
        AuthCommand::Login { .. } => ctx.finish(op, no_store, login(ctx, Provider::Gh)),
        AuthCommand::Status => ctx.finish(op, no_store, status(ctx)),
        AuthCommand::Logout { .. } => ctx.finish(op, no_store, logout(ctx, Provider::Gh)),
        AuthCommand::GitCredential { operation } => match git_credential(ctx, operation) {
            // Nothing for the entry to print: the answer, if there was one, is out.
            Ok(()) => Printed {
                stdout: String::new(),
                exit: 0,
            },
            Err(error) => ctx.finish::<()>(op, no_store, Err(error)),
        },
    }
}

/// `auth login --with-token`: the token on stdin, kept as the login with `provider`.
fn login(ctx: &Ctx<'_>, provider: Provider) -> Result<AuthLogin, OpError> {
    let file = auth::file(ctx.env)?;
    let token = token_from(io::stdin().lock())?;
    auth::login(file.as_deref(), provider, Source::WithToken, None, &token)
}

/// `auth status`: the logins the file holds.
fn status(ctx: &Ctx<'_>) -> Result<AuthStatus, OpError> {
    let file = auth::file(ctx.env)?;
    auth::status(file.as_deref())
}

/// `auth logout`: the login with `provider` removed.
fn logout(ctx: &Ctx<'_>, provider: Provider) -> Result<AuthLogout, OpError> {
    let file = auth::file(ctx.env)?;
    auth::logout(file.as_deref(), provider)
}

/// `auth git-credential`: git's request read from stdin, and the answer to `operation`, if the
/// login file has one, written to stdout: a `username=` and a `password=` line. No answer
/// writes nothing. An error has written nothing either.
fn git_credential(ctx: &Ctx<'_>, operation: &str) -> Result<(), OpError> {
    let request = request_from(io::stdin().lock())?;
    let Some(answer) = auth::git_credential(operation, &request, ctx.env)? else {
        return Ok(());
    };
    let mut stdout = io::stdout().lock();
    answer
        .write_to(&mut stdout)
        .and_then(|()| stdout.flush())
        .map_err(|e| OpError::new(ErrorCode::Io, format!("cannot write git's answer: {e}")))
}

/// git's request on `input`: its lines up to the blank line that ends it, or to the end of
/// input, read no further than one byte past [`auth::MAX_REQUEST_BYTES`]. A request that cannot
/// be read or is longer is `E_INVALID_ARG` (`details.reason` `credential_request`); the message
/// never holds what was read, which can be a password git asks a helper to store.
fn request_from(input: impl BufRead) -> Result<Vec<u8>, OpError> {
    let refused = |why: String| {
        let mut error = OpError::new(ErrorCode::InvalidArg, why);
        let reason = "credential_request".into();
        error.details.insert("reason".to_owned(), reason);
        error
    };
    let mut input = input.take(auth::MAX_REQUEST_BYTES + 1);
    let mut bytes = Vec::new();
    loop {
        let from = bytes.len();
        let read = input
            .read_until(b'\n', &mut bytes)
            .map_err(|e| refused(format!("cannot read git's request from stdin: {e}")))?;
        if read == 0 || bytes[from..] == *b"\n" {
            break;
        }
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > auth::MAX_REQUEST_BYTES {
        let most = auth::MAX_REQUEST_BYTES;
        let why = format!("stdin holds more than {most} bytes, which is no request of git's");
        return Err(refused(why));
    }
    Ok(bytes)
}

/// The token `input` holds: its text without the ASCII white space around it, read no further
/// than one byte past [`MAX_TOKEN_INPUT`]. Whether that is a token is the operation's to say.
/// An input that cannot be read, is longer, or is not UTF-8 is `E_INVALID_ARG`
/// (`details.reason` `token`); the message never holds what was read.
fn token_from(input: impl Read) -> Result<String, OpError> {
    let refused = |why: String| {
        let mut error = OpError::new(ErrorCode::InvalidArg, why);
        error.details.insert("reason".to_owned(), "token".into());
        error
    };
    let mut bytes = Vec::new();
    input
        .take(MAX_TOKEN_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| refused(format!("cannot read the token from stdin: {e}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TOKEN_INPUT {
        let why = format!("stdin holds more than {MAX_TOKEN_INPUT} bytes, which is no token");
        return Err(refused(why));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| refused("stdin is not UTF-8, which is no token".to_owned()))?;
    Ok(text
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token no provider issued, in no provider's shape.
    const TOKEN: &str = "example-token-not-a-secret";

    #[test]
    fn the_token_is_stdin_without_the_white_space_around_it() {
        for input in [
            TOKEN.to_owned(),
            format!("{TOKEN}\n"),
            format!("  {TOKEN}\r\n\n"),
            format!("\t{TOKEN} "),
        ] {
            assert_eq!(token_from(input.as_bytes()).unwrap(), TOKEN, "{input:?}");
        }

        // What is inside stays, for the operation to refuse.
        assert_eq!(token_from(&b"two words\n"[..]).unwrap(), "two words");
        assert_eq!(token_from(&b" \n"[..]).unwrap(), "");
    }

    #[test]
    fn an_input_that_is_no_token_is_refused_unquoted() {
        let long = format!("{TOKEN}{}", "x".repeat(4096));
        let cases: [&[u8]; 2] = [long.as_bytes(), b"example-\xff-token"];
        for input in cases {
            let error = token_from(input).unwrap_err();
            assert_eq!(error.code, "E_INVALID_ARG");
            assert_eq!(error.details["reason"], "token");
            assert!(!format!("{error:?}").contains("example"), "{error:?}");
        }
        let fits = "x".repeat(4095) + "\n";
        assert_eq!(token_from(fits.as_bytes()).unwrap().len(), 4095);
    }

    #[test]
    fn a_request_is_read_to_its_blank_line_and_no_further() {
        // 1. Up to the blank line, which is kept; what follows is not read.
        let mut input = &b"protocol=https\nhost=example.com\n\nhost=other.example\n"[..];
        let request = request_from(&mut input).unwrap();
        assert_eq!(request, b"protocol=https\nhost=example.com\n\n");
        assert_eq!(input, b"host=other.example\n");

        // 2. Or to the end of input, with or without a last line break; nothing is a request.
        for whole in [&b"protocol=https\nhost=example.com\n"[..], b"host=x", b""] {
            assert_eq!(request_from(whole).unwrap(), whole);
        }

        // 3. The bound: a request of exactly that many bytes is read, one more is refused
        //    without a word of it.
        let most = usize::try_from(auth::MAX_REQUEST_BYTES).unwrap();
        let fits = format!("password={}\n", "x".repeat(most - 10));
        assert_eq!(request_from(fits.as_bytes()).unwrap().len(), most);
        for long in [
            format!("password=example{}\n", "x".repeat(most)),
            "example\n".repeat(most / 8 + 1),
        ] {
            let error = request_from(long.as_bytes()).unwrap_err();
            assert_eq!(error.code, "E_INVALID_ARG");
            assert_eq!(error.details["reason"], "credential_request");
            assert!(!format!("{error:?}").contains("example"), "{error:?}");
        }
    }
}
