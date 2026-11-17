//! The `auth` commands: `auth login`, `auth status` and `auth logout`, on the user's login file
//! (`swarmops_ops::auth`).
//!
//! They open no store: the file is the user's, found from the environment
//! (`SWARMOPS_AUTH_FILE`, else the host's state directory), so `--repo` and `--as` are read and
//! not used, and the envelope has no `meta.repo`. `auth login --with-token` reads the token from
//! stdin, since an argument would show in the process list; nothing here prints it, and no
//! error quotes what stdin held.

use std::collections::BTreeMap;
use std::io::{self, Read};

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
}
