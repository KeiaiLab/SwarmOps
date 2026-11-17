//! GitHub's device flow, which `auth login --gh` runs when no token is handed to it: SwarmOps
//! asks GitHub for a code, shows the person the code and the page to enter it on, and asks
//! again, at the pace GitHub sets, until they have approved it there and GitHub answers with a
//! token ([`grant`]).
//!
//! ```text
//!   auth login --gh ── POST /login/device/code ───────────────► github.com
//!        │          ◄── device_code, user_code, interval ──────
//!        ├── stderr: "Open https://github.com/login/device and enter the code ..."
//!        └── every `interval` seconds:
//!              POST /login/oauth/access_token ────────────────►
//!           ◄── authorization_pending | slow_down | a token | a refusal
//! ```
//!
//! SwarmOps has no network code of its own: each request is one run of the host's `curl`
//! ([`Curl`]), with a cleared environment and the whole request on its stdin, so that neither
//! the device code nor anything else of a request is an argument a process list shows. The two
//! addresses are constants: no flag, variable, store or answer moves a request to another host,
//! and curl follows no redirect.
//!
//! The runner has one other request, which the flow does not make ([`Curl::redirect`]): a HEAD
//! that reads where an address redirects to, and goes there no more than a posted form does.
//!
//! The app the login is asked in the name of is the client ID of `SWARMOPS_GH_CLIENT_ID`, else
//! the one built in ([`client_id`]). A client ID is public; the flow uses no client secret.
//!
//! Nothing GitHub answers is trusted to be what the flow expects: the code shown and the page
//! named are checked before they reach the terminal, an answer with a refresh token is refused
//! (this build could not renew the token it comes with), and no error holds an answer's body,
//! the device code or the token.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_ops::auth::Provider;

use crate::context::{self, Env};

/// The variable that names the client ID of the OAuth app a login is asked in the name of.
pub(super) const GH_CLIENT_ID_ENV: &str = "SWARMOPS_GH_CLIENT_ID";

/// The client ID built in, used when [`GH_CLIENT_ID_ENV`] names none. It stays `None` until
/// SwarmOps has registered an OAuth app of its own, and never holds another product's.
const GH_CLIENT_ID: Option<&str> = None;

/// The variable that names the `curl` to run, by an absolute path; else the first on `PATH`.
pub(super) const CURL_ENV: &str = "SWARMOPS_CURL";

/// The program's name on `PATH`.
const CURL_NAME: &str = "curl";

/// curl's whole command line: no `.curlrc` (`-q`, which curl reads only as its first argument),
/// and the request as a configuration on stdin.
const CURL_ARGS: [&str; 3] = ["-q", "--config", "-"];

/// The variables a curl inherits, of all the caller's: where programs are, the proxies and the
/// certificate stores.
const CURL_ENV_KEPT: [&str; 10] = [
    "PATH",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
];

/// What every request's configuration holds besides its address: https alone, also after a
/// redirect, of which none is followed; TLS 1.2 or later; nothing printed but the answer; ten
/// seconds to connect and thirty in all.
const COMMON_CONFIG: &str = concat!(
    "proto = \"=https\"\n",
    "proto-redir = \"=https\"\n",
    "max-redirs = 0\n",
    "tlsv1.2\n",
    "silent\n",
    "connect-timeout = 10\n",
    "max-time = 30\n",
);

/// What a posted form's configuration adds, before the form: no answer over 64 KiB, the answer
/// asked for as JSON, and the program named with its version.
const POST_CONFIG: &str = concat!(
    "max-filesize = 65536\n",
    "header = \"Accept: application/json\"\n",
    "header = \"User-Agent: swarmops/",
    env!("CARGO_PKG_VERSION"),
    "\"\n",
);

/// What the configuration of a request for a redirect adds: a HEAD, whose headers curl would
/// print before the two lines asked for and so writes nowhere, and the program named without
/// its version, which the host asked has no need of. No `max-filesize`: there is no body, and
/// curl holds the bound against the length a HEAD's answer names all the same, which would
/// make a failure of a plain 404 with a long page. The `Accept` header is curl's own.
const REDIRECT_CONFIG: &str = concat!(
    "head\n",
    "output = \"/dev/null\"\n",
    "header = \"User-Agent: swarmops-update\"\n",
);

/// The most bytes read of what a curl prints: an answer and its status line.
const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// Where a device code is asked for.
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// Where the token is asked for, once a code has been given.
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// What the page a person enters the code on must start with: https, at the login's host.
const PAGE_ORIGIN: &str = "https://github.com";

/// The grant type of a device code (RFC 8628), as a form value.
const GRANT_TYPE: &str = "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code";

/// The scope asked for when `--scope` names none: read and write access to repositories.
const DEFAULT_SCOPE: &str = "repo";

/// How long a code lasts when the answer does not say, and the longest the flow waits whatever
/// it says, in seconds.
const DEFAULT_EXPIRY: u64 = 900;
const MAX_EXPIRY: u64 = 1800;

/// The seconds between two polls when the answer does not say, the bounds of what it may say,
/// and what a `slow_down` without an interval adds.
const DEFAULT_INTERVAL: u64 = 5;
const MIN_INTERVAL: u64 = 1;
const MAX_INTERVAL: u64 = 60;
const SLOW_DOWN: u64 = 5;

/// How many polls in a row may get no answer before the login gives up.
const MAX_LOST_POLLS: u32 = 3;

/// The most bytes of a token: what the login file keeps (`swarmops_ops::auth`).
const MAX_TOKEN_BYTES: usize = 1024;

/// What a request got: the HTTP status and the body, or neither when what curl printed is no
/// answer (too long, or without its status line).
pub(super) struct Answer {
    /// The HTTP status.
    status: Option<u16>,
    /// The body. It can hold the device code or the token: it is read and never shown.
    body: Vec<u8>,
}

impl Answer {
    /// The answer in what a curl printed: the body, a line break, and the three digits
    /// [`config`] asks for.
    fn of(printed: &[u8]) -> Answer {
        let unread = Answer {
            status: None,
            body: Vec::new(),
        };
        if printed.len() > MAX_ANSWER_BYTES {
            return unread;
        }
        let Some(at) = printed.iter().rposition(|byte| *byte == b'\n') else {
            return unread;
        };
        let digits = &printed[at + 1..];
        let status: Option<u16> = std::str::from_utf8(digits)
            .ok()
            .filter(|digits| digits.len() == 3 && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok());
        match status {
            Some(status) => Answer {
                status: Some(status),
                body: printed[..at].to_vec(),
            },
            None => unread,
        }
    }

    /// The body as the JSON object every answer of the flow is; `None` for anything else.
    fn said(&self) -> Option<Map<String, Value>> {
        match serde_json::from_slice(&self.body) {
            Ok(Value::Object(said)) => Some(said),
            _ => None,
        }
    }
}

/// What a request for a redirect got: the HTTP status and, of a redirect, where it points; or
/// neither when what curl printed is no such answer.
#[allow(dead_code, reason = "`update`, which comes next, reads it")]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Redirect {
    /// The HTTP status.
    pub(super) status: Option<u16>,
    /// Where the answer redirects to, as curl resolved its `Location`; `None` when it names no
    /// place. It is what a host said and nothing more: its reader checks it.
    pub(super) to: Option<String>,
}

impl Redirect {
    /// The answer in what a curl printed: the two lines [`redirect_config`] asks for and no
    /// byte more, the address (or nothing) and the status's three digits. An address holding a
    /// space, a control character or anything outside ASCII is no answer.
    fn of(printed: &[u8]) -> Redirect {
        let unread = Redirect {
            status: None,
            to: None,
        };
        if printed.len() > MAX_ANSWER_BYTES {
            return unread;
        }
        let Some((to, digits)) = std::str::from_utf8(printed)
            .ok()
            .and_then(|printed| printed.split_once('\n'))
        else {
            return unread;
        };
        if !to.bytes().all(|byte| byte.is_ascii_graphic()) {
            return unread;
        }
        if digits.len() != 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return unread;
        }
        Redirect {
            status: digits.parse().ok(),
            to: (!to.is_empty()).then(|| to.to_owned()),
        }
    }
}

/// What runs curl, for the words of the refusal where there is none.
pub(super) struct Caller {
    /// What runs it, as the refusal's subject.
    pub(super) what: &'static str,
    /// What does without curl, which ends the hint; `None` when nothing does.
    pub(super) without: Option<&'static str>,
}

/// The device flow, which a login with a token handed to it does without.
pub(super) const DEVICE_FLOW: Caller = Caller {
    what: "the device flow",
    without: Some("a login with --with-token needs none"),
};

/// A request that got no answer.
pub(super) enum Failed {
    /// curl could not be run at all: the caller's error, which no second try changes.
    Curl(OpError),
    /// curl ran and reached no answer; `exit` is its exit code, `None` when a signal ended it.
    Network {
        /// curl's exit code.
        exit: Option<i32>,
    },
}

/// The host's curl, and the environment its runs get.
pub(super) struct Curl {
    /// The program.
    binary: PathBuf,
    /// The variables of [`CURL_ENV_KEPT`] the caller has set.
    env: Vec<(&'static str, OsString)>,
    /// What runs it.
    caller: &'static Caller,
}

impl Curl {
    /// The curl `env` names, for `caller` to run: [`CURL_ENV`]'s absolute path when set, else
    /// the first `curl` on `PATH`. `E_INVALID_ARG` (`details.reason` `curl_missing`) when there
    /// is none.
    pub(super) fn find(env: Env<'_>, caller: &'static Caller) -> Result<Curl, OpError> {
        let binary = match env(CURL_ENV).filter(|named| !named.is_empty()) {
            Some(named) => {
                let named = PathBuf::from(named);
                (named.is_absolute() && named.is_file()).then_some(named)
            }
            None => env("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(CURL_NAME))
                    .find(|candidate| candidate.is_file())
            }),
        };
        let Some(binary) = binary else {
            let why = format!(
                "{} runs curl, and there is none: {CURL_ENV} names no file by an absolute path, \
                 or no directory of PATH holds one",
                caller.what
            );
            return Err(missing(caller, why));
        };
        let kept = CURL_ENV_KEPT
            .iter()
            .filter_map(|name| env(name).map(|value| (*name, value)));
        Ok(Curl {
            binary,
            env: kept.collect(),
            caller,
        })
    }

    /// Posts `form` to `url`, in one run of curl: the request on its stdin, the answer on its
    /// stdout, what it says on stderr dropped.
    pub(super) fn post(&self, url: &str, form: &str) -> Result<Answer, Failed> {
        let Some(config) = config(url, form) else {
            let why = "a login request holds a character curl's configuration cannot carry";
            return Err(Failed::Curl(OpError::new(ErrorCode::Internal, why)));
        };
        let (status, printed) = self.run(&config)?;

        // An answer too long was not read to its end, which curl may count as a failure of its
        // own: it is an answer all the same, and one the flow refuses.
        if status.success() || printed.len() > MAX_ANSWER_BYTES {
            Ok(Answer::of(&printed))
        } else {
            Err(Failed::Network {
                exit: status.code(),
            })
        }
    }

    /// Asks `url` for its headers, in one run of curl, and follows no redirect: the answer is
    /// the status and where a redirect points ([`Redirect`]). The request goes to `url` alone,
    /// and no further.
    #[allow(dead_code, reason = "`update`, which comes next, asks with it")]
    pub(super) fn redirect(&self, url: &str) -> Result<Redirect, Failed> {
        let Some(config) = redirect_config(url) else {
            let why = "a request's address holds a character curl's configuration cannot carry";
            return Err(Failed::Curl(OpError::new(ErrorCode::Internal, why)));
        };
        let (status, printed) = self.run(&config)?;
        if status.success() {
            Ok(Redirect::of(&printed))
        } else {
            Err(Failed::Network {
                exit: status.code(),
            })
        }
    }

    /// Runs curl once with `config` as its whole request: how it ended, and what it printed, of
    /// which no more than one byte over [`MAX_ANSWER_BYTES`] is read.
    fn run(&self, config: &str) -> Result<(ExitStatus, Vec<u8>), Failed> {
        let cannot_run = |e: io::Error| {
            let binary = self.binary.display();
            Failed::Curl(missing(self.caller, format!("cannot run {binary}: {e}")))
        };
        let mut child = Command::new(&self.binary)
            .args(CURL_ARGS)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(cannot_run)?;

        // The request, then the answer. A curl that ended before it read the one or wrote the
        // other says why by its exit code, so neither failure is looked at here.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(config.as_bytes());
        }
        let mut printed = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            let most = u64::try_from(MAX_ANSWER_BYTES).unwrap_or(u64::MAX);
            let _ = stdout.take(most + 1).read_to_end(&mut printed);
        }
        let status = child.wait().map_err(cannot_run)?;
        Ok((status, printed))
    }
}

/// The configuration of one request: `form` posted to `url`, and the HTTP status printed on a
/// line of its own after the body. `None` when a value could add a line or an option of its
/// own: [`quoted`]'s rule, and a form curl would read as a file's name.
fn config(url: &str, form: &str) -> Option<String> {
    if form.starts_with('@') {
        return None;
    }
    let url = quoted(url)?;
    let form = quoted(form)?;
    Some(format!(
        "url = {url}\n{COMMON_CONFIG}{POST_CONFIG}data = {form}\nwrite-out = \
         \"\\n%{{http_code}}\"\n"
    ))
}

/// The configuration of a request for where `url` redirects to: its headers asked for and
/// dropped, and two lines printed, the address a redirect names (empty when the answer is
/// none) and the HTTP status. `None` when `url` could add a line or an option of its own
/// ([`quoted`]'s rule).
fn redirect_config(url: &str) -> Option<String> {
    let url = quoted(url)?;
    Some(format!(
        "url = {url}\n{COMMON_CONFIG}{REDIRECT_CONFIG}write-out = \
         \"%{{redirect_url}}\\n%{{http_code}}\"\n"
    ))
}

/// `value` as a quoted value of curl's configuration; `None` for one that holds a quote, a
/// backslash, a space, a control character or anything outside ASCII, so that no value ends its
/// quotes or its line.
fn quoted(value: &str) -> Option<String> {
    let plain = value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\');
    plain.then(|| format!("\"{value}\""))
}

/// The client ID a login is asked in the name of: [`GH_CLIENT_ID_ENV`]'s, else the one built
/// in. `E_INVALID_ARG` when there is neither (`details.reason` `client_id_unset`,
/// `details.variable`) and for one that is not 1 to 64 letters, digits, dots, hyphens and
/// underscores (`client_id`, `details.source`).
pub(super) fn client_id(env: Env<'_>) -> Result<String, OpError> {
    let named = context::var(env, GH_CLIENT_ID_ENV)?.map(|id| (id, GH_CLIENT_ID_ENV));
    let built_in = || GH_CLIENT_ID.map(|id| (id.to_owned(), "build"));
    let Some((id, source)) = named.or_else(built_in) else {
        let why = format!(
            "this build has no client ID for GitHub's device flow: name an OAuth app's with \
             {GH_CLIENT_ID_ENV}"
        );
        let hint = "or hand the login a token: swarmops auth login --gh --with-token";
        let mut details = reason("client_id_unset");
        details.insert("variable".to_owned(), GH_CLIENT_ID_ENV.into());
        let refused = OpError::new(ErrorCode::InvalidArg, why);
        return Err(refused.with_details(details).with_hint(hint));
    };
    if is_client_id(&id) {
        return Ok(id);
    }
    let why = format!(
        "{source}: the client ID is not 1 to 64 letters, digits, dots, hyphens and underscores"
    );
    let mut details = reason("client_id");
    details.insert("source".to_owned(), source.into());
    Err(OpError::new(ErrorCode::InvalidArg, why).with_details(details))
}

/// Whether `id` has a client ID's shape.
fn is_client_id(id: &str) -> bool {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
    (1..=64).contains(&id.len()) && id.bytes().all(allowed)
}

/// The scopes to ask for: `asked` (`--scope`), each once, or `repo` alone when it names none.
/// `E_INVALID_ARG` (`details.reason` `scope`) for one that is not 1 to 64 lower-case letters,
/// underscores and colons. The refusal states the rule and quotes no value: a token typed where
/// a scope goes is an argument like any other of `auth login`, and no envelope repeats one.
pub(super) fn scopes(asked: &[String]) -> Result<Vec<String>, OpError> {
    if asked.is_empty() {
        return Ok(vec![DEFAULT_SCOPE.to_owned()]);
    }
    let mut scopes: Vec<String> = Vec::new();
    for scope in asked {
        if !is_scope(scope) {
            let why = "--scope: a scope is 1 to 64 lower-case letters, underscores and colons";
            return Err(OpError::new(ErrorCode::InvalidArg, why).with_details(reason("scope")));
        }
        if !scopes.contains(scope) {
            scopes.push(scope.clone());
        }
    }
    Ok(scopes)
}

/// Whether `scope` has a scope's shape.
fn is_scope(scope: &str) -> bool {
    let allowed = |byte: u8| byte.is_ascii_lowercase() || matches!(byte, b'_' | b':');
    (1..=64).contains(&scope.len()) && scope.bytes().all(allowed)
}

/// What the flow ends with: the token, and the scopes GitHub says it has, when it says.
pub(super) struct Granted {
    /// The token.
    pub(super) token: String,
    /// The token's scopes.
    pub(super) scopes: Option<Vec<String>>,
}

impl fmt::Debug for Granted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Granted")
            .field("token", &"<redacted>")
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Runs the device flow for the app `client_id` names and `scopes`: asks for a code, writes the
/// code and its page to `notice` (two lines, flushed before the first wait), then waits and
/// asks for the token in turns until the answer is a token or a refusal. `post` sends one
/// request and `wait` lets that many seconds pass.
///
/// `E_AUTH` when GitHub refuses (`details.reason` `access_denied`, `expired_token`,
/// `device_flow_disabled`, `bad_client_id`, `expiring_token` for an answer with a refresh
/// token, `unexpected_answer` for one the flow does not have), with `details.provider`,
/// `details.host`, GitHub's own word when it is one (`details.provider_error`) and the HTTP
/// status (`details.http_status`), and never the answer's body. `E_REMOTE_UNREACHABLE`
/// (`details.phase` `login`) when the first request, or three polls in a row, got no answer.
///
/// The code's life is counted in the seconds waited, not those the requests took: a code
/// GitHub has let expire meanwhile is its `expired_token`.
pub(super) fn grant(
    client_id: &str,
    scopes: &[String],
    post: &mut dyn FnMut(&str, &str) -> Result<Answer, Failed>,
    wait: &mut dyn FnMut(u64),
    notice: &mut dyn Write,
) -> Result<Granted, OpError> {
    // 1. The code.
    let asked: Vec<String> = scopes
        .iter()
        .map(|scope| scope.replace(':', "%3A"))
        .collect();
    let form = format!("client_id={client_id}&scope={}", asked.join("%20"));
    let code = match post(DEVICE_CODE_URL, &form) {
        Ok(answer) => Code::of(&answer)?,
        Err(Failed::Curl(error)) => return Err(error),
        Err(Failed::Network { exit }) => return Err(unreachable(exit)),
    };

    // 2. The person's part, said before the first wait.
    let told = writeln!(
        notice,
        "Open {} and enter the code {}\nWaiting for approval (the code expires in {}).",
        code.page,
        code.user,
        lasting(code.expires_in)
    );
    told.and_then(|()| notice.flush())
        .map_err(|e| OpError::new(ErrorCode::Io, format!("cannot write the code: {e}")))?;

    // 3. The token, asked for no faster than GitHub says and no longer than the code lasts.
    let form = format!(
        "client_id={client_id}&device_code={}&grant_type={GRANT_TYPE}",
        code.device
    );
    let mut interval = code.interval;
    let mut waited = 0;
    let mut lost = 0;
    loop {
        if waited + interval > code.expires_in {
            return Err(refused("expired_token", None, None));
        }
        wait(interval);
        waited += interval;
        let answer = match post(TOKEN_URL, &form) {
            Ok(answer) => answer,
            Err(Failed::Curl(error)) => return Err(error),
            Err(Failed::Network { exit }) => {
                lost += 1;
                if lost == MAX_LOST_POLLS {
                    return Err(unreachable(exit));
                }
                continue;
            }
        };
        lost = 0;
        match polled(&answer)? {
            Polled::Pending => {}
            Polled::SlowDown(named) => {
                let slower = named.unwrap_or(interval + SLOW_DOWN);
                interval = slower.clamp(MIN_INTERVAL, MAX_INTERVAL);
            }
            Polled::Granted(granted) => return Ok(granted),
        }
    }
}

/// How long `seconds` is, for a person: whole minutes, or seconds under one.
fn lasting(seconds: u64) -> String {
    match seconds / 60 {
        0 => format!("{seconds} seconds"),
        1 => "1 minute".to_owned(),
        minutes => format!("{minutes} minutes"),
    }
}

/// A device code, as the first answer gives it.
struct Code {
    /// The code the polls name. A secret until the flow ends: never shown.
    device: String,
    /// The code the person enters.
    user: String,
    /// The page they enter it on.
    page: String,
    /// The seconds the code lasts.
    expires_in: u64,
    /// The seconds between two polls.
    interval: u64,
}

impl Code {
    /// The code in `answer`, checked: status 200; a `device_code` of 8 to 128 letters, digits,
    /// hyphens and underscores; a `user_code` of 4 to 32 letters, digits and hyphens, so that
    /// nothing else reaches the terminal; and a `verification_uri` at the login's host
    /// ([`is_page`]). A refusal GitHub names, or a 404, is that refusal; anything else is
    /// `unexpected_answer`.
    fn of(answer: &Answer) -> Result<Code, OpError> {
        let said = answer.said();
        let word = said.as_ref().and_then(|said| text(said, "error"));
        if let Some(word) = word {
            let reason = match refusal_of(word) {
                "unexpected_answer" if answer.status == Some(404) => "bad_client_id",
                reason => reason,
            };
            return Err(refused(reason, Some(answer), Some(word)));
        }
        if answer.status == Some(404) {
            return Err(refused("bad_client_id", Some(answer), None));
        }
        let unexpected = || refused("unexpected_answer", Some(answer), None);
        let said = said
            .filter(|_| answer.status == Some(200))
            .ok_or_else(unexpected)?;

        let code_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-';
        let device = text(&said, "device_code")
            .filter(|code| (8..=128).contains(&code.len()))
            .filter(|code| code.bytes().all(|byte| code_byte(byte) || byte == b'_'))
            .ok_or_else(unexpected)?;
        let user = text(&said, "user_code")
            .filter(|code| (4..=32).contains(&code.len()))
            .filter(|code| code.bytes().all(code_byte))
            .ok_or_else(unexpected)?;
        let page = text(&said, "verification_uri")
            .filter(|page| is_page(page))
            .ok_or_else(unexpected)?;
        let expires_in = seconds(&said, "expires_in").unwrap_or(DEFAULT_EXPIRY);
        let interval = seconds(&said, "interval").unwrap_or(DEFAULT_INTERVAL);
        Ok(Code {
            device: device.to_owned(),
            user: user.to_owned(),
            page: page.to_owned(),
            expires_in: expires_in.min(MAX_EXPIRY),
            interval: interval.clamp(MIN_INTERVAL, MAX_INTERVAL),
        })
    }
}

/// Whether `page` is a page of the login's host over https: [`PAGE_ORIGIN`] alone, or followed
/// by a path of at most 200 letters, digits, slashes, dots, hyphens and underscores. No other
/// host, no user part, no port, no query, and nothing a terminal would act on.
fn is_page(page: &str) -> bool {
    let Some(path) = page.strip_prefix(PAGE_ORIGIN) else {
        return false;
    };
    let allowed =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_');
    path.is_empty() || (path.starts_with('/') && path.len() <= 200 && path.bytes().all(allowed))
}

/// What a poll's answer says.
enum Polled {
    /// Not approved yet.
    Pending,
    /// Asked too fast: the interval to keep from now on, when the answer names one.
    SlowDown(Option<u64>),
    /// Approved.
    Granted(Granted),
}

/// What `answer`, a poll's, says: read from the JSON's `error` word, not from the HTTP status,
/// which is 200 for a refusal too. An answer with a `refresh_token` is refused as
/// `expiring_token`; a token that is not 1 to 1024 visible ASCII characters, a `scope` that is
/// not a comma-separated list of scopes, and anything else is `unexpected_answer`.
fn polled(answer: &Answer) -> Result<Polled, OpError> {
    let unexpected = || refused("unexpected_answer", Some(answer), None);
    let said = answer.said().ok_or_else(unexpected)?;
    if let Some(word) = text(&said, "error") {
        return match word {
            "authorization_pending" => Ok(Polled::Pending),
            "slow_down" => Ok(Polled::SlowDown(seconds(&said, "interval"))),
            _ => Err(refused(refusal_of(word), Some(answer), Some(word))),
        };
    }
    if said.contains_key("refresh_token") {
        return Err(refused("expiring_token", Some(answer), None));
    }
    let token = text(&said, "access_token")
        .filter(|token| (1..=MAX_TOKEN_BYTES).contains(&token.len()))
        .filter(|token| token.bytes().all(|byte| byte.is_ascii_graphic()))
        .ok_or_else(unexpected)?;
    let scopes = match said.get("scope") {
        None | Some(Value::Null) => None,
        Some(Value::String(list)) => {
            let scopes: Vec<&str> = list.split(',').filter(|scope| !scope.is_empty()).collect();
            if !scopes.iter().all(|scope| is_scope(scope)) {
                return Err(unexpected());
            }
            Some(scopes.into_iter().map(str::to_owned).collect())
        }
        Some(_) => return Err(unexpected()),
    };
    Ok(Polled::Granted(Granted {
        token: token.to_owned(),
        scopes,
    }))
}

/// `said`'s `key`, when it is a string.
fn text<'a>(said: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    said.get(key).and_then(Value::as_str)
}

/// `said`'s `key`, when it is a whole number of seconds.
fn seconds(said: &Map<String, Value>, key: &str) -> Option<u64> {
    said.get(key).and_then(Value::as_u64)
}

/// The `details.reason` of the refusal GitHub names `word`.
fn refusal_of(word: &str) -> &'static str {
    match word {
        "access_denied" => "access_denied",
        "expired_token" => "expired_token",
        "device_flow_disabled" => "device_flow_disabled",
        "incorrect_client_credentials" => "bad_client_id",
        _ => "unexpected_answer",
    }
}

/// `E_AUTH` for the refusal `reason` names. `answer`, when the refusal is one, gives
/// `details.http_status`, and `word`, GitHub's own for it, `details.provider_error` when it is
/// 1 to 64 lower-case letters and underscores. Nothing else of an answer enters the error.
fn refused(reason_key: &'static str, answer: Option<&Answer>, word: Option<&str>) -> OpError {
    let host = Provider::Gh.host();
    let again = "run the login again for a new code";
    let (message, hint) = match reason_key {
        "access_denied" => (format!("the code was not approved at {host}"), Some(again)),
        "expired_token" => (
            format!("the code expired before it was approved at {host}"),
            Some(again),
        ),
        "device_flow_disabled" => (
            format!("the OAuth app the client ID names has the device flow off at {host}"),
            Some("turn the device flow on in the app's settings, or name another app's client ID"),
        ),
        "bad_client_id" => (
            format!("{host} knows no OAuth app by the client ID"),
            Some("check the client ID SWARMOPS_GH_CLIENT_ID names"),
        ),
        "expiring_token" => (
            format!(
                "{host} answered with a token that expires and a refresh token, which this \
                 build does not use: no login was kept"
            ),
            Some(
                "name the client ID of an OAuth app, whose tokens do not expire, or log in with \
                 --with-token",
            ),
        ),
        _ => (
            format!("{host} answered the login with what is no answer of the device flow"),
            None,
        ),
    };
    let mut details = reason(reason_key);
    details.insert("provider".to_owned(), Provider::Gh.key().into());
    details.insert("host".to_owned(), host.into());
    let is_word = |word: &&str| {
        let allowed = |byte: u8| byte.is_ascii_lowercase() || byte == b'_';
        (1..=64).contains(&word.len()) && word.bytes().all(allowed)
    };
    if let Some(word) = word.filter(is_word) {
        details.insert("provider_error".to_owned(), word.into());
    }
    if let Some(status) = answer.and_then(|answer| answer.status) {
        details.insert("http_status".to_owned(), status.into());
    }
    let error = OpError::new(ErrorCode::Auth, message).with_details(details);
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
}

/// `E_REMOTE_UNREACHABLE` for a request that got no answer, curl having ended with `exit`:
/// `details.phase` `login`, `details.host`, `details.cause` and `details.curl_exit`. It has no
/// `details.remote`: a login is of no store's remote.
fn unreachable(exit: Option<i32>) -> OpError {
    let host = Provider::Gh.host();
    let cause = cause(exit);
    let mut details = Map::new();
    details.insert("phase".to_owned(), "login".into());
    details.insert("host".to_owned(), host.into());
    details.insert("cause".to_owned(), cause.into());
    if let Some(exit) = exit {
        details.insert("curl_exit".to_owned(), exit.into());
    }
    OpError::new(
        ErrorCode::RemoteUnreachable,
        format!("{host} did not answer the login ({cause})"),
    )
    .with_details(details)
    .with_hint("run it again once the network is back: no login was kept")
}

/// Why a curl that ended with `exit` reached no answer, in the words a remote's failure has.
fn cause(exit: Option<i32>) -> &'static str {
    match exit {
        Some(6) => "dns",
        Some(7) => "connect",
        Some(28) => "timeout",
        Some(35 | 51 | 58 | 60 | 77) => "tls",
        _ => "disconnected",
    }
}

/// `E_INVALID_ARG` for a host with no curl for `caller` to run (`details.reason`
/// `curl_missing`).
fn missing(caller: &Caller, why: String) -> OpError {
    let hint = match caller.without {
        Some(without) => format!("install curl, or name it with {CURL_ENV}; {without}"),
        None => format!("install curl, or name it with {CURL_ENV}"),
    };
    OpError::new(ErrorCode::InvalidArg, why)
        .with_details(reason("curl_missing"))
        .with_hint(hint)
}

/// Details holding `details.reason` alone.
fn reason(reason: &str) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("reason".to_owned(), reason.into());
    details
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;

    /// A client ID no provider issued.
    const CLIENT: &str = "example-client-id";

    /// A device code, a user code and a token in no provider's shape.
    const DEVICE: &str = "example-device-code-not-a-secret";
    const USER: &str = "ABCD-1234";
    const TOKEN: &str = "example-token-not-a-secret";

    /// An answer with `status` and `body`.
    fn answer(status: u16, body: &Value) -> Answer {
        Answer {
            status: Some(status),
            body: body.to_string().into_bytes(),
        }
    }

    /// The first answer of a flow that goes on: a code that lasts `expires_in` seconds, polled
    /// every `interval`.
    fn code(expires_in: u64, interval: u64) -> Answer {
        let body = json!({
            "device_code": DEVICE, "user_code": USER,
            "verification_uri": "https://github.com/login/device",
            "expires_in": expires_in, "interval": interval,
        });
        answer(200, &body)
    }

    /// A poll's answer that names `error`.
    fn saying(error: &str) -> Answer {
        answer(
            200,
            &json!({"error": error, "error_description": "example words"}),
        )
    }

    /// What a run of the flow did.
    struct Ran {
        /// How it ended.
        ended: Result<Granted, OpError>,
        /// Each request: its address and its form.
        posts: Vec<(String, String)>,
        /// Each wait, in seconds.
        waits: Vec<u64>,
        /// What the person was told.
        notice: String,
    }

    /// Runs the flow asking for `scopes`, each request answered by the next of `answers`.
    fn flow(scopes: &[&str], answers: Vec<Result<Answer, Failed>>) -> Ran {
        let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        let mut answers: VecDeque<Result<Answer, Failed>> = answers.into();
        let mut posts = Vec::new();
        let mut waits = Vec::new();
        let mut notice = Vec::new();
        let ended = grant(
            CLIENT,
            &scopes,
            &mut |url, form| {
                posts.push((url.to_owned(), form.to_owned()));
                answers.pop_front().expect("a request with no answer left")
            },
            &mut |seconds| waits.push(seconds),
            &mut notice,
        );
        assert!(answers.is_empty(), "an answer no request asked for");
        Ran {
            ended,
            posts,
            waits,
            notice: String::from_utf8(notice).expect("the notice is text"),
        }
    }

    /// The error's code and `details.reason`.
    fn why(error: &OpError) -> (&str, &str) {
        let reason = error.details.get("reason").and_then(Value::as_str);
        (error.code.as_str(), reason.unwrap_or_default())
    }

    /// Asserts that nothing of `error` holds a code or a token.
    fn holds_no_secret(error: &OpError) {
        let shown = format!("{error:?}");
        for secret in [DEVICE, TOKEN, "example words"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
    }

    #[test]
    fn the_flow_shows_the_code_and_polls_to_a_token() {
        let lost = || Err(Failed::Network { exit: Some(7) });
        let slower = answer(200, &json!({"error": "slow_down", "interval": 12}));
        let token = json!({"access_token": TOKEN, "token_type": "bearer",
                           "scope": "repo,read:org"});
        let ran = flow(
            &["repo", "read:org"],
            vec![
                Ok(code(900, 5)),
                Ok(saying("authorization_pending")),
                Ok(saying("slow_down")),
                lost(),
                lost(),
                Ok(slower),
                Ok(answer(200, &token)),
            ],
        );

        // 1. The token and the scopes GitHub named.
        let granted = ran.ended.unwrap();
        assert_eq!(granted.token, TOKEN);
        let scopes = granted.scopes.as_deref().unwrap();
        assert_eq!(scopes, ["repo", "read:org"]);
        assert!(!format!("{granted:?}").contains(TOKEN), "{granted:?}");

        // 2. One request for the code, then the same poll each time: two fixed addresses, the
        //    scopes joined as a form joins them.
        let ask = format!("client_id={CLIENT}&scope=repo%20read%3Aorg");
        assert_eq!(ran.posts[0], (DEVICE_CODE_URL.to_owned(), ask));
        let poll = format!("client_id={CLIENT}&device_code={DEVICE}&grant_type={GRANT_TYPE}");
        assert_eq!(ran.posts.len(), 7);
        for post in &ran.posts[1..] {
            assert_eq!(*post, (TOKEN_URL.to_owned(), poll.clone()));
        }

        // 3. A wait before every poll: the answer's interval; five seconds more after a
        //    slow_down that names none; the interval a slow_down names; a lost poll changes
        //    nothing.
        assert_eq!(ran.waits, [5, 5, 10, 10, 10, 12]);

        // 4. The person is told the page and the code, and never the device code.
        assert_eq!(
            ran.notice,
            "Open https://github.com/login/device and enter the code ABCD-1234\n\
             Waiting for approval (the code expires in 15 minutes).\n"
        );
    }

    #[test]
    fn what_the_first_answer_leaves_out_has_a_default_and_a_bound() {
        // 1. Neither number: 900 seconds, five between polls. No scope in the token's answer.
        let bare = json!({"device_code": DEVICE, "user_code": USER,
                          "verification_uri": "https://github.com/login/device"});
        let token = json!({"access_token": TOKEN});
        let ran = flow(
            &["repo"],
            vec![Ok(answer(200, &bare)), Ok(answer(200, &token))],
        );
        assert_eq!(ran.ended.unwrap().scopes, None);
        assert_eq!(ran.waits, [5]);
        assert!(ran.notice.contains("15 minutes"), "{}", ran.notice);

        // 2. An interval of none is one second, and one of an hour a minute; the code lasts
        //    half an hour at most, so one such wait and no second.
        let ran = flow(
            &["repo"],
            vec![Ok(code(1, 0)), Ok(saying("authorization_pending"))],
        );
        assert_eq!(why(&ran.ended.unwrap_err()), ("E_AUTH", "expired_token"));
        assert_eq!(ran.waits, [1]);
        assert!(ran.notice.contains("in 1 seconds"), "{}", ran.notice);
        let mut answers = vec![Ok(code(86_400, 3600))];
        answers.extend((0..30).map(|_| Ok(saying("authorization_pending"))));
        let ran = flow(&["repo"], answers);
        assert_eq!(why(&ran.ended.unwrap_err()), ("E_AUTH", "expired_token"));
        assert_eq!(ran.waits, [60; 30]);
        assert!(ran.notice.contains("30 minutes"), "{}", ran.notice);

        // 3. A code that lasts less than one interval is never polled.
        let ran = flow(&["repo"], vec![Ok(code(4, 5))]);
        assert_eq!(why(&ran.ended.unwrap_err()), ("E_AUTH", "expired_token"));
        assert_eq!(ran.waits, [0; 0]);
    }

    /// A first answer that is a code, as JSON.
    fn good_code() -> Value {
        json!({"device_code": DEVICE, "user_code": USER,
               "verification_uri": "https://github.com/login/device"})
    }

    /// An answer with `status` whose body is `body` as it stands.
    fn raw(status: Option<u16>, body: &str) -> Answer {
        Answer {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    /// Runs the flow to the refusal of `first`, its first answer: `E_AUTH` with `reason`, not
    /// to be run again, and nothing shown or waited for.
    fn refused_at_once(what: &str, first: Answer, reason: &str) -> OpError {
        let ran = flow(&["repo"], vec![Ok(first)]);
        let error = ran.ended.unwrap_err();
        assert_eq!(why(&error), ("E_AUTH", reason), "{what}");
        assert_eq!((error.exit, error.retryable), (5, false), "{what}");
        assert_eq!(error.details["provider"], "gh", "{what}");
        assert_eq!(error.details["host"], "github.com", "{what}");
        holds_no_secret(&error);
        assert_eq!(ran.notice, "", "{what}: nothing is shown");
        assert_eq!(ran.waits, [0; 0], "{what}");
        error
    }

    #[test]
    fn a_first_answer_that_is_no_code_is_refused() {
        let good = good_code();
        let with = |key: &str, value: &str| {
            let mut body = good.clone();
            body[key] = value.into();
            answer(200, &body)
        };
        let mut no_user_code = good.clone();
        no_user_code["user_code"] = Value::Null;
        let page = "verification_uri";
        let cases = [
            ("no JSON", raw(Some(200), "<html>")),
            ("a list", answer(200, &json!([good]))),
            ("no status", raw(None, &good.to_string())),
            ("a redirect", answer(302, &good)),
            ("a short device code", with("device_code", "short")),
            (
                "a quote in the device code",
                with("device_code", "a\"device"),
            ),
            ("no user code", answer(200, &no_user_code)),
            (
                "an escape in the user code",
                with("user_code", "AB\u{1b}[2JCD"),
            ),
            ("a page elsewhere", with(page, "https://example.com/device")),
            (
                "a longer host",
                with(page, "https://github.com.example.com/"),
            ),
            ("a user part", with(page, "https://github.com@example.com/")),
            ("a port", with(page, "https://github.com:8443/login/device")),
            ("http", with(page, "http://github.com/login/device")),
            ("a query", with(page, "https://github.com/login?next=x")),
            (
                "an escape in the page",
                with(page, "https://github.com/\u{1b}[2J"),
            ),
        ];
        for (what, first) in cases {
            refused_at_once(what, first, "unexpected_answer");
        }
    }

    #[test]
    fn a_first_answer_that_refuses_says_which_refusal() {
        let saying = |status: u16, error: &str| answer(status, &json!({"error": error}));
        let cases = [
            (saying(200, "device_flow_disabled"), "device_flow_disabled"),
            (saying(200, "incorrect_client_credentials"), "bad_client_id"),
            (saying(404, "Not Found"), "bad_client_id"),
            (raw(Some(404), "Not Found"), "bad_client_id"),
            (saying(400, "unsupported_grant_type"), "unexpected_answer"),
        ];
        for (first, reason) in cases {
            refused_at_once(reason, first, reason);
        }

        // GitHub's word is given when it is a word, with the status.
        let first = saying(400, "unsupported_grant_type");
        let error = refused_at_once("a word", first, "unexpected_answer");
        assert_eq!(error.details["provider_error"], "unsupported_grant_type");
        assert_eq!(error.details["http_status"], 400);
        let error = refused_at_once("no word", saying(404, "Not Found"), "bad_client_id");
        assert!(error.details.get("provider_error").is_none(), "{error:?}");
        assert_eq!(error.details["http_status"], 404);
    }

    #[test]
    fn a_poll_s_answer_that_is_no_token_ends_the_flow() {
        let expiring = json!({"access_token": TOKEN, "expires_in": 28800,
                              "refresh_token": "example-refresh-not-a-secret"});
        let token = |token: &str| answer(200, &json!({"access_token": token}));
        let scoped = |scope: Value| answer(200, &json!({"access_token": TOKEN, "scope": scope}));
        let unexpected = "unexpected_answer";
        let cases = [
            ("denied", saying("access_denied"), "access_denied"),
            ("expired", saying("expired_token"), "expired_token"),
            (
                "off",
                saying("device_flow_disabled"),
                "device_flow_disabled",
            ),
            (
                "a bad ID",
                saying("incorrect_client_credentials"),
                "bad_client_id",
            ),
            (
                "a bad device code",
                saying("incorrect_device_code"),
                unexpected,
            ),
            (
                "a token that expires",
                answer(200, &expiring),
                "expiring_token",
            ),
            ("no JSON", raw(Some(502), "Bad Gateway"), unexpected),
            (
                "neither token nor error",
                answer(200, &json!({})),
                unexpected,
            ),
            ("an empty token", token(""), unexpected),
            ("a token of two words", token("example token"), unexpected),
            ("a token too long", token(&"x".repeat(1025)), unexpected),
            (
                "a scope that is none",
                scoped(json!("repo,No Scope")),
                unexpected,
            ),
            (
                "a scope that is no text",
                scoped(json!(["repo"])),
                unexpected,
            ),
        ];
        for (what, poll, reason) in cases {
            let ran = flow(&["repo"], vec![Ok(code(900, 5)), Ok(poll)]);
            let error = ran.ended.unwrap_err();
            assert_eq!(why(&error), ("E_AUTH", reason), "{what}");
            holds_no_secret(&error);
            let shown = format!("{error:?}");
            assert!(!shown.contains("example-refresh"), "{what}: {shown}");
            assert_eq!(ran.waits, [5], "{what}");
        }

        // A token with no scope named at all, or an empty list, is a token.
        let empty = json!({"access_token": TOKEN, "scope": ""});
        let ran = flow(&["repo"], vec![Ok(code(900, 5)), Ok(answer(200, &empty))]);
        assert_eq!(ran.ended.unwrap().scopes, Some(Vec::new()));
    }

    #[test]
    fn a_request_without_an_answer_is_the_network_s() {
        // 1. The first request: at once, with curl's exit code and what it means.
        for (exit, cause) in [
            (Some(6), "dns"),
            (Some(7), "connect"),
            (Some(28), "timeout"),
            (Some(35), "tls"),
            (Some(51), "tls"),
            (Some(58), "tls"),
            (Some(60), "tls"),
            (Some(77), "tls"),
            (Some(56), "disconnected"),
            (None, "disconnected"),
        ] {
            let ran = flow(&["repo"], vec![Err(Failed::Network { exit })]);
            let error = ran.ended.unwrap_err();
            assert_eq!(error.code, "E_REMOTE_UNREACHABLE", "{exit:?}");
            assert_eq!((error.exit, error.retryable), (4, true), "{exit:?}");
            let mut want = json!({"phase": "login", "host": "github.com", "cause": cause});
            if let Some(exit) = exit {
                want["curl_exit"] = exit.into();
            }
            assert_eq!(Value::Object(error.details.clone()), want, "{exit:?}");
        }

        // 2. A poll: the third in a row. An answer in between starts the count again.
        let lost = || Err(Failed::Network { exit: Some(28) });
        let pending = || Ok(saying("authorization_pending"));
        let ran = flow(
            &["repo"],
            vec![
                Ok(code(900, 5)),
                lost(),
                lost(),
                pending(),
                lost(),
                lost(),
                lost(),
            ],
        );
        let error = ran.ended.unwrap_err();
        assert_eq!(error.code, "E_REMOTE_UNREACHABLE");
        assert_eq!(error.details["cause"], "timeout");
        assert_eq!(ran.waits, [5; 6]);

        // 3. A curl that cannot be run is not tried again.
        let gone = || {
            Err(Failed::Curl(missing(
                &DEVICE_FLOW,
                "cannot run it".to_owned(),
            )))
        };
        let ran = flow(&["repo"], vec![Ok(code(900, 5)), gone()]);
        assert_eq!(
            why(&ran.ended.unwrap_err()),
            ("E_INVALID_ARG", "curl_missing")
        );
    }

    #[test]
    fn an_answer_is_a_body_and_a_status_line() {
        let read = Answer::of(b"{\"a\": 1}\n200");
        assert_eq!(
            (read.status, read.body.as_slice()),
            (Some(200), &b"{\"a\": 1}"[..])
        );
        let lines = Answer::of(b"{\n\"a\": 1\n}\n\n404");
        assert_eq!(lines.status, Some(404));
        assert_eq!(lines.said().unwrap()["a"], 1);
        let empty = Answer::of(b"\n500");
        assert_eq!((empty.status, empty.body.len()), (Some(500), 0));
        assert!(empty.said().is_none(), "no JSON");

        // No status line, one that is no status, or more than the bound: no answer.
        let long = [vec![b'x'; MAX_ANSWER_BYTES - 3], b"\n200".to_vec()].concat();
        for printed in [
            &b""[..],
            b"200",
            b"{}\n20",
            b"{}\n2000",
            b"{}\n20x",
            b"{}\n200\n",
            &long,
        ] {
            let read = Answer::of(printed);
            assert_eq!((read.status, read.body.len()), (None, 0), "{printed:?}");
        }
        let fits = [vec![b'x'; MAX_ANSWER_BYTES - 4], b"\n200".to_vec()].concat();
        assert_eq!(Answer::of(&fits).status, Some(200));
    }

    #[test]
    fn a_request_is_a_configuration_no_value_adds_a_line_to() {
        // 1. The whole configuration: the address, the fixed part, the form, the status line.
        let form = format!("client_id={CLIENT}&scope=repo%20read%3Aorg");
        let config = config(DEVICE_CODE_URL, &form).unwrap();
        let version = env!("CARGO_PKG_VERSION");
        let want = format!(
            "url = \"https://github.com/login/device/code\"\n\
             proto = \"=https\"\n\
             proto-redir = \"=https\"\n\
             max-redirs = 0\n\
             tlsv1.2\n\
             silent\n\
             connect-timeout = 10\n\
             max-time = 30\n\
             max-filesize = 65536\n\
             header = \"Accept: application/json\"\n\
             header = \"User-Agent: swarmops/{version}\"\n\
             data = \"{form}\"\n\
             write-out = \"\\n%{{http_code}}\"\n"
        );
        assert_eq!(config, want);
        assert!(!config.contains("location"), "no redirect is followed");
        assert!(quoted(version).is_some(), "the version is a plain word");

        // 2. A value that could end its quotes or its line, or name a file, makes no request.
        for bad in [
            "a\"b",
            "a\\b",
            "a b",
            "a\nb",
            "a\tb",
            "a\u{7f}b",
            "caf\u{e9}",
        ] {
            assert_eq!(quoted(bad), None, "{bad:?}");
            assert_eq!(super::config(DEVICE_CODE_URL, bad), None, "{bad:?}");
            assert_eq!(super::config(bad, &form), None, "{bad:?}");
        }
        assert_eq!(super::config(DEVICE_CODE_URL, "@/etc/passwd"), None);

        // 3. The addresses are the login's host, over https.
        let host = Provider::Gh.host();
        for url in [DEVICE_CODE_URL, TOKEN_URL] {
            assert!(url.starts_with(&format!("https://{host}/")), "{url}");
        }
        assert_eq!(PAGE_ORIGIN, format!("https://{host}"));
        assert_eq!(CURL_ARGS, ["-q", "--config", "-"]);

        // 4. The test kit's stand-in for curl is named by the variable this module reads.
        assert_eq!(CURL_ENV, swarmops_testkit::curl::CURL_ENV);
    }

    /// An environment holding `vars` only.
    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| {
            vars.iter()
                .find(|(n, _)| n == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn the_built_in_client_id_is_unset_or_well_formed() {
        assert!(
            GH_CLIENT_ID.is_none_or(is_client_id),
            "the client ID built in is no client ID"
        );
    }

    #[test]
    fn the_client_id_is_the_variable_s_else_the_build_s() {
        // 1. The variable's.
        let named = env_of(&[(GH_CLIENT_ID_ENV, CLIENT)]);
        assert_eq!(client_id(&named).unwrap(), CLIENT);

        // 2. One that is no client ID is refused, said of where it came from.
        let long = "x".repeat(65);
        for bad in ["two words", "a&scope=admin", "a\"b", long.as_str()] {
            let error = client_id(&env_of(&[(GH_CLIENT_ID_ENV, bad)])).unwrap_err();
            assert_eq!(why(&error), ("E_INVALID_ARG", "client_id"), "{bad:?}");
            assert_eq!(error.details["source"], GH_CLIENT_ID_ENV, "{bad:?}");
        }

        // 3. Unset, or set to nothing: the build's, and this build has none.
        for unset in [env_of(&[]), env_of(&[(GH_CLIENT_ID_ENV, "")])] {
            match (client_id(&unset), GH_CLIENT_ID) {
                (Ok(id), Some(built_in)) => assert_eq!(id, built_in),
                (Err(error), None) => {
                    assert_eq!(why(&error), ("E_INVALID_ARG", "client_id_unset"));
                    assert_eq!(error.details["variable"], GH_CLIENT_ID_ENV);
                    assert_eq!(error.exit, 2);
                }
                (other, built_in) => panic!("{other:?} with {built_in:?} built in"),
            }
        }
    }

    #[test]
    fn the_scopes_are_the_ones_named_or_repo() {
        let named = |scopes: &[&str]| {
            let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
            super::scopes(&scopes)
        };
        assert_eq!(named(&[]).unwrap(), ["repo"]);
        assert_eq!(
            named(&["repo", "workflow", "repo"]).unwrap(),
            ["repo", "workflow"]
        );
        assert_eq!(named(&["read:org"]).unwrap(), ["read:org"]);
        let long = "x".repeat(65);
        for bad in [
            "",
            "Repo",
            "repo workflow",
            "repo,workflow",
            "repo&x=y",
            long.as_str(),
        ] {
            let error = named(&["repo", bad]).unwrap_err();
            assert_eq!(why(&error), ("E_INVALID_ARG", "scope"), "{bad:?}");
        }

        // The refusal quotes no value: a token typed where a scope goes is not repeated.
        for typed in [TOKEN, "example words", "Repo"] {
            let error = named(&["repo", typed]).unwrap_err();
            assert_eq!(why(&error), ("E_INVALID_ARG", "scope"), "{typed:?}");
            let shown = format!("{error:?}");
            assert!(!shown.contains(typed), "{typed} in {shown}");
            assert_eq!(
                error.message,
                "--scope: a scope is 1 to 64 lower-case letters, underscores and colons"
            );
        }
    }

    #[test]
    fn curl_is_the_one_named_else_the_first_on_the_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("make a directory");
        let on_path = bin.join(CURL_NAME);
        let named = dir.path().join("my-curl");
        for file in [&on_path, &named] {
            std::fs::write(file, "").expect("write a file");
        }
        let text = |path: &std::path::Path| path.to_str().expect("a UTF-8 path").to_owned();
        let empty = text(&dir.path().join("empty"));
        let path = format!("{empty}:{}", text(&bin));

        // 1. The variable's file, else the first on PATH; and of the caller's variables, only
        //    the listed ones.
        let env = env_of(&[
            (CURL_ENV, &text(&named)),
            ("PATH", &path),
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("HOME", "/home/me"),
            ("SWARMOPS_AUTH_FILE", "/home/me/auth.toml"),
        ]);
        let curl = Curl::find(&env, &DEVICE_FLOW).unwrap();
        assert_eq!(curl.binary, named);
        let kept: Vec<&str> = curl.env.iter().map(|(name, _)| *name).collect();
        assert_eq!(kept, ["PATH", "HTTPS_PROXY"]);
        let curl = Curl::find(&env_of(&[("PATH", &path), (CURL_ENV, "")]), &DEVICE_FLOW).unwrap();
        assert_eq!(curl.binary, on_path);

        // 2. A variable that names no file by an absolute path is no curl, whatever PATH holds;
        //    nor is a PATH without one, or none.
        let gone = text(&dir.path().join("gone"));
        for env in [
            env_of(&[(CURL_ENV, &gone), ("PATH", &path)]),
            env_of(&[(CURL_ENV, "my-curl"), ("PATH", &path)]),
            env_of(&[(CURL_ENV, &text(&bin)), ("PATH", &path)]),
            env_of(&[("PATH", &empty)]),
            env_of(&[]),
        ] {
            let Err(error) = Curl::find(&env, &DEVICE_FLOW) else {
                panic!("a curl was found where there is none");
            };
            assert_eq!(why(&error), ("E_INVALID_ARG", "curl_missing"));
            assert_eq!(error.exit, 2);
        }
    }

    /// The refusal names what runs curl, and its hint what does without: the login's words
    /// are the ones it had before the runner had a second caller.
    #[test]
    fn a_missing_curl_is_said_in_its_caller_s_words() {
        const OTHER: Caller = Caller {
            what: "another command",
            without: None,
        };

        let Err(login) = Curl::find(&env_of(&[]), &DEVICE_FLOW) else {
            panic!("a curl was found where there is none");
        };
        assert_eq!(
            login.message,
            "the device flow runs curl, and there is none: SWARMOPS_CURL names no file by an \
             absolute path, or no directory of PATH holds one"
        );
        assert_eq!(
            login.hint.as_deref(),
            Some(
                "install curl, or name it with SWARMOPS_CURL; a login with --with-token needs \
                 none"
            )
        );

        let Err(other) = Curl::find(&env_of(&[]), &OTHER) else {
            panic!("a curl was found where there is none");
        };
        assert_eq!(why(&other), ("E_INVALID_ARG", "curl_missing"));
        assert!(
            other
                .message
                .starts_with("another command runs curl, and there is none: "),
            "{}",
            other.message
        );
        assert_eq!(
            other.hint.as_deref(),
            Some("install curl, or name it with SWARMOPS_CURL")
        );
    }

    /// An address whose redirect is asked for, in these tests.
    const LATEST: &str = "https://github.com/KeiaiLab/SwarmOps/releases/latest";

    #[test]
    fn a_redirect_is_asked_for_and_not_followed() {
        // 1. The whole configuration: the address, the common part, a HEAD whose headers go
        //    nowhere, the program without its version, and the two lines to print.
        let config = redirect_config(LATEST).unwrap();
        let want = "url = \"https://github.com/KeiaiLab/SwarmOps/releases/latest\"\n\
                    proto = \"=https\"\n\
                    proto-redir = \"=https\"\n\
                    max-redirs = 0\n\
                    tlsv1.2\n\
                    silent\n\
                    connect-timeout = 10\n\
                    max-time = 30\n\
                    head\n\
                    output = \"/dev/null\"\n\
                    header = \"User-Agent: swarmops-update\"\n\
                    write-out = \"%{redirect_url}\\n%{http_code}\"\n";
        assert_eq!(config, want);
        assert!(!config.contains("location"), "no redirect is followed");
        assert!(
            !config.contains(env!("CARGO_PKG_VERSION")),
            "the request does not say which version asks"
        );
        for absent in ["data", "Accept", "max-filesize"] {
            assert!(!config.contains(absent), "{absent} in {config}");
        }

        // 2. An address that could end its quotes or its line makes no request.
        for bad in [
            "https://a\"b",
            "https://a b",
            "https://a\nhead",
            "https://caf\u{e9}",
        ] {
            assert_eq!(redirect_config(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_redirect_s_answer_is_an_address_line_and_a_status() {
        let to = "https://github.com/KeiaiLab/SwarmOps/releases/tag/v9.9.9";
        let read = Redirect::of(format!("{to}\n302").as_bytes());
        assert_eq!((read.status, read.to.as_deref()), (Some(302), Some(to)));

        // No redirect: the address's line is empty.
        let none = Redirect {
            status: Some(404),
            to: None,
        };
        assert_eq!(Redirect::of(b"\n404"), none);
        assert_eq!(Redirect::of(b"\n200").status, Some(200));

        // Anything else is no answer: no line break, a status that is none, a line more (a
        // header curl printed, or an address that holds a line break), an address with a space
        // or outside ASCII, bytes that are no text, or more than the bound.
        let unread = Redirect {
            status: None,
            to: None,
        };
        let long = [vec![b'x'; MAX_ANSWER_BYTES - 3], b"\n302".to_vec()].concat();
        for printed in [
            &b""[..],
            b"302",
            b"https://github.com/a",
            b"https://github.com/a\n30",
            b"https://github.com/a\n3020",
            b"https://github.com/a\n30x",
            b"https://github.com/a\n302\n",
            b"HTTP/2 302\nhttps://github.com/a\n302",
            b"https://github.com/a\nb\n302",
            b"https://github.com/a b\n302",
            b"https://github.com/a\r\n302",
            b"https://github.com/caf\xc3\xa9\n302",
            b"https://github.com/\xff\n302",
            &long,
        ] {
            assert_eq!(Redirect::of(printed), unread, "{printed:?}");
        }
        let fits = [vec![b'x'; MAX_ANSWER_BYTES - 4], b"\n302".to_vec()].concat();
        assert_eq!(Redirect::of(&fits).status, Some(302));
    }

    /// Both requests through a stand-in for curl: what it is run with, and what its answer is
    /// read as.
    #[test]
    fn each_request_is_one_run_of_curl() {
        use swarmops_testkit::curl::FakeCurl;

        let fake = FakeCurl::new();
        let stand_in = fake.path();
        let env = env_of(&[
            (CURL_ENV, stand_in.to_str().expect("a UTF-8 path")),
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/me"),
            ("SWARMOPS_AUTO_UPDATE", "on"),
        ]);
        let curl = Curl::find(&env, &DEVICE_FLOW).unwrap();
        let ended = |asked: Result<Redirect, Failed>| match asked {
            Ok(read) => Ok(read),
            Err(Failed::Network { exit }) => Err(exit),
            Err(Failed::Curl(error)) => panic!("curl was not run: {error:?}"),
        };

        // 1. A redirect, an answer that is none, a failure of curl's, and a curl that printed
        //    the headers before the two lines.
        let to = "https://github.com/KeiaiLab/SwarmOps/releases/tag/v9.9.9";
        fake.print(&format!("{to}\n302"))
            .print("\n404")
            .fail(7)
            .print("HTTP/2 302\nlocation: /a\n\nhttps://github.com/a\n302");
        let found = Redirect {
            status: Some(302),
            to: Some(to.to_owned()),
        };
        assert_eq!(ended(curl.redirect(LATEST)), Ok(found));
        let none = Redirect {
            status: Some(404),
            to: None,
        };
        assert_eq!(ended(curl.redirect(LATEST)), Ok(none));
        assert_eq!(ended(curl.redirect(LATEST)), Err(Some(7)));
        let unread = Redirect {
            status: None,
            to: None,
        };
        assert_eq!(ended(curl.redirect(LATEST)), Ok(unread));

        // 2. A posted form, by the same runner.
        fake.answer(200, "{\"a\": 1}");
        let Ok(answer) = curl.post(DEVICE_CODE_URL, "client_id=x") else {
            panic!("the form got no answer");
        };
        assert_eq!(answer.status, Some(200));
        assert_eq!(answer.said().unwrap()["a"], 1);

        // 3. Every run: the three arguments, the request on stdin, and of the caller's
        //    variables only the listed ones.
        let calls = fake.calls();
        assert_eq!(calls.len(), 5);
        for call in &calls {
            assert_eq!(call.args, CURL_ARGS);
            for name in ["HOME", "SWARMOPS_AUTO_UPDATE", CURL_ENV] {
                assert!(
                    !call.env.iter().any(|set| set == name),
                    "{name} was passed on"
                );
            }
            assert!(call.env.iter().any(|set| set == "PATH"), "{:?}", call.env);
        }
        for call in &calls[..4] {
            assert_eq!(call.stdin, redirect_config(LATEST).unwrap());
        }
        assert_eq!(
            calls[4].stdin,
            config(DEVICE_CODE_URL, "client_id=x").unwrap()
        );

        // 4. An address curl's configuration cannot carry is never asked: no run.
        let Err(Failed::Curl(error)) = curl.redirect("https://github.com/a b") else {
            panic!("an address with a space was asked");
        };
        assert_eq!(error.code, "E_INTERNAL");
        assert_eq!(fake.calls().len(), 5);
    }
}
