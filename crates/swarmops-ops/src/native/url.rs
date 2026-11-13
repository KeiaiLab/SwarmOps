//! The URL of a native remote (the native sync design §2.1, decision 7):
//! `so://[user@]host[:port]/<store>`, reached over ssh.
//!
//! ```text
//!   so://ada@hub.example:2222/acme
//!        └user└host       └port └store: one segment, the remote-name grammar
//! ```
//!
//! No password (`user:secret@`), no user or host that starts with `-` (ssh would read it as an
//! option), no query, no fragment, no path below the store.

use swarmops_api::{ErrorCode, OpError};
use swarmops_git::transport::UrlRefused;

use crate::error;

/// The scheme every native URL starts with.
pub(crate) const SCHEME: &str = "so://";

/// The `details.reason` of a native URL that is no URL of this form.
const BAD_URL: &str = "url";

/// A parsed `so://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeUrl {
    /// The ssh user, if the URL names one.
    pub(crate) user: Option<String>,
    /// The host, as ssh takes it (an IPv6 address without its brackets).
    pub(crate) host: String,
    /// The ssh port, if the URL names one.
    pub(crate) port: Option<u16>,
    /// The served store's name.
    pub(crate) store: String,
}

/// Whether `url` names a native remote: any URL of the [`SCHEME`], whether it parses or not.
pub(crate) fn is_native(url: &str) -> bool {
    url.starts_with(SCHEME)
}

impl NativeUrl {
    /// `url` read, or `E_INVALID_ARG` with the reason it is refused.
    pub(crate) fn parse(url: &str) -> Result<NativeUrl, OpError> {
        let Some(rest) = url.strip_prefix(SCHEME) else {
            return Err(error::url_refused(UrlRefused::Scheme));
        };

        // 1. The authority and the path: exactly one segment after it.
        let Some((authority, path)) = rest.split_once('/') else {
            return Err(bad("names no store: so://[user@]host[:port]/<store>"));
        };
        if path.contains(['?', '#']) {
            return Err(bad("has a query or a fragment"));
        }

        // 2. The user: no password, not an option.
        let (user, host_port) = match authority.rsplit_once('@') {
            Some((user, host_port)) => (Some(user), host_port),
            None => (None, authority),
        };
        if user.is_some_and(|user| user.contains(':')) {
            return Err(error::url_refused(UrlRefused::CredentialInUrl));
        }
        if user.is_some_and(|user| user.starts_with('-')) || host_port.starts_with('-') {
            return Err(error::url_refused(UrlRefused::OptionLike));
        }
        if user.is_some_and(|user| !user.bytes().all(user_byte)) {
            return Err(bad(
                "has a user of other characters than letters, digits, . _ -",
            ));
        }

        // 3. The host and the port.
        let (host, port) = host_and_port(host_port)?;

        // 4. The store, one segment as a remote's name is.
        if swarmops_store::check_remote_segment(path).is_err() {
            return Err(bad(
                "names no store: [a-z0-9][a-z0-9._-]{0,62}, one segment",
            ));
        }
        Ok(NativeUrl {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            port,
            store: path.to_owned(),
        })
    }

    /// The destination ssh connects to: `[user@]host`.
    pub(crate) fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// The host of `host_port` (`host`, `host:port`, `[v6]` or `[v6]:port`) and its port.
fn host_and_port(host_port: &str) -> Result<(&str, Option<u16>), OpError> {
    // 1. The host, bracketed or not, and what follows it.
    let (host, after) = split_host(host_port)?;

    // 2. The port, if any: 1 to 65535.
    if after.is_empty() {
        return Ok((host, None));
    }
    let port = after
        .strip_prefix(':')
        .and_then(|port| port.parse::<u16>().ok());
    match port {
        Some(port) if port > 0 => Ok((host, Some(port))),
        _ => Err(bad("has a port that is no number from 1 to 65535")),
    }
}

/// The host of `host_port` and what follows it: a bracketed IPv6 address, or a name.
fn split_host(host_port: &str) -> Result<(&str, &str), OpError> {
    // 1. A name, up to the port.
    let Some(bracketed) = host_port.strip_prefix('[') else {
        let end = host_port.find(':').unwrap_or(host_port.len());
        let host = &host_port[..end];
        if host.is_empty() || !host.bytes().all(host_byte) {
            return Err(bad("has no host of letters, digits, . and -"));
        }
        return Ok((host, &host_port[end..]));
    };

    // 2. An IPv6 address in brackets.
    let Some((host, after)) = bracketed.split_once(']') else {
        return Err(bad("has an unclosed [ in its host"));
    };
    let v6 = !host.is_empty() && host.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
    if !v6 {
        return Err(bad("has a bracketed host that is no IPv6 address"));
    }
    Ok((host, after))
}

/// Whether `b` may stand in a URL's user.
fn user_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

/// Whether `b` may stand in a host name.
fn host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
}

/// `E_INVALID_ARG` `url`: the native URL `what`.
fn bad(what: &str) -> OpError {
    error::error(ErrorCode::InvalidArg, format!("a so:// URL that {what}"))
        .with_details(error::details([("reason", BAD_URL.into())]))
        .with_hint("give the store's URL as so://[user@]host[:port]/<store>")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL's forms read into their parts.
    #[test]
    fn a_so_url_reads_into_its_parts() {
        let read = NativeUrl::parse("so://ada@hub.example:2222/acme").unwrap();
        assert_eq!(read.user.as_deref(), Some("ada"));
        assert_eq!(read.host, "hub.example");
        assert_eq!(read.port, Some(2222));
        assert_eq!(read.store, "acme");
        assert_eq!(read.destination(), "ada@hub.example");

        let read = NativeUrl::parse("so://[::1]/a.b-c").unwrap();
        assert_eq!((read.host.as_str(), read.port), ("::1", None));
        assert_eq!(read.destination(), "::1");
    }

    /// Each refused form names its reason.
    #[test]
    fn a_bad_so_url_is_refused_with_its_reason() {
        let refused = [
            ("so://ada:secret@hub/acme", "credential_in_url"),
            ("so://-oProxyCommand=x/acme", "url_option"),
            ("so://-ada@hub/acme", "url_option"),
            ("so://hub", BAD_URL),
            ("so://hub/", BAD_URL),
            ("so://hub/a/b", BAD_URL),
            ("so://hub/Acme", BAD_URL),
            ("so://hub/acme?x", BAD_URL),
            ("so://hub:0/acme", BAD_URL),
            ("so://hub:x/acme", BAD_URL),
            ("so://h b/acme", BAD_URL),
            ("so://[zz]/acme", BAD_URL),
            ("so://a b@hub/acme", BAD_URL),
            ("ssh://hub/acme", "url_scheme"),
        ];
        for (url, reason) in refused {
            let e = NativeUrl::parse(url).unwrap_err();
            assert_eq!(e.code, "E_INVALID_ARG", "{url}");
            assert_eq!(e.details["reason"], reason, "{url}: {e:?}");
        }
    }
}
