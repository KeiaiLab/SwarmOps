//! Who the caller is (operations spec §5): the [`Identity`] a mutation carries, from the
//! `--as` flag or the environment, and what `whoami` reports.
//!
//! The agent name is `--as <family>/<instance>` when the flag is given, and `SWARMOPS_AGENT`
//! otherwise; `SWARMOPS_RUN_ID`, when set, is the run the agent acts in, whichever source
//! named the agent. A variable set to the empty string counts as unset. A name that breaks the
//! grammar of [`Identity::parse`], or a variable that is not UTF-8, is `E_INVALID_ARG`, its
//! message prefixed with where it came from (`--as`, `SWARMOPS_AGENT`, `SWARMOPS_RUN_ID`),
//! which `details.source` also holds.
//!
//! Reads need no identity; a mutation (`commit`, `blob put`, `ref set`, `ref delete`) without one
//! is `E_IDENTITY_REQUIRED` ([`require_identity`]). M1 trusts the name as claimed: the store's
//! authentication mode is [`AUTH_TRUST`], and `whoami` reports [`AUTH_TRUST`] for an
//! identified caller and [`AUTH_ANONYMOUS`] for one without an identity.

use std::ffi::OsString;

use serde_json::Value;
use swarmops_api::{ErrorCode, Identity, IdentityInfo, OpError};

/// The environment variable that names the agent when `--as` is not given.
pub const AGENT_ENV: &str = "SWARMOPS_AGENT";

/// The environment variable that names the run the agent acts in.
pub const RUN_ID_ENV: &str = "SWARMOPS_RUN_ID";

/// The store's authentication mode in M1, which is also how an identified caller is
/// identified: by the name it claims, unverified.
pub const AUTH_TRUST: &str = "trust";

/// How a caller without an identity is identified.
pub const AUTH_ANONYMOUS: &str = "anonymous";

/// The caller's identity: from `flag` (`--as`) when given, else from the process
/// environment, as [the module documentation](self) says. `None` when neither names an agent.
pub fn resolve_identity(flag: Option<&str>) -> Result<Option<Identity>, OpError> {
    resolve_identity_from(flag, |name| std::env::var_os(name))
}

/// [`resolve_identity`], with environment variable `name` read as `env(name)`: tests and
/// embedders supply the environment, since setting the process's own is `unsafe`.
pub fn resolve_identity_from(
    flag: Option<&str>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<Identity>, OpError> {
    let (agent, source) = match flag {
        Some(agent) => (agent.to_owned(), "--as"),
        None => match var(&env, AGENT_ENV)? {
            Some(agent) => (agent, AGENT_ENV),
            None => return Ok(None),
        },
    };
    let identity = Identity::parse(&agent).map_err(|e| said_of(e, source))?;
    Ok(Some(match var(&env, RUN_ID_ENV)? {
        Some(run_id) => identity.with_run_id(run_id),
        None => identity,
    }))
}

/// The identity a mutation acts as: `identity`, or `E_IDENTITY_REQUIRED` (exit 5) when there
/// is none.
pub fn require_identity(identity: Option<Identity>) -> Result<Identity, OpError> {
    identity.ok_or_else(|| {
        OpError::new(
            ErrorCode::IdentityRequired,
            "this operation changes the store, so it needs the caller's identity",
        )
        .with_hint(format!(
            "pass --as <family>/<instance> or set {AGENT_ENV}, e.g. --as agent/worker-1"
        ))
    })
}

/// `whoami`: how the caller with `identity` (`None`: without one) is identified.
pub fn identity_info(identity: Option<&Identity>) -> IdentityInfo {
    match identity {
        Some(identity) => {
            let mut out = IdentityInfo::new(AUTH_TRUST.to_owned());
            out.agent = Some(identity.agent().to_owned());
            out.run_id = identity.run_id().map(str::to_owned);
            out
        }
        None => IdentityInfo::new(AUTH_ANONYMOUS.to_owned()),
    }
}

/// Environment variable `name` as `env` gives it: `None` when it is unset or empty,
/// `E_INVALID_ARG` when it is not UTF-8.
fn var(env: &impl Fn(&str) -> Option<OsString>, name: &str) -> Result<Option<String>, OpError> {
    match env(name) {
        Some(value) if !value.is_empty() => value.into_string().map(Some).map_err(|_| {
            said_of(
                OpError::new(ErrorCode::InvalidArg, "the value is not UTF-8"),
                name,
            )
        }),
        _ => Ok(None),
    }
}

/// `error`, said of `source` (the flag or the variable that gave the value): its message
/// prefixed with the source's name, which `details.source` holds too.
fn said_of(mut error: OpError, source: &str) -> OpError {
    error.message = format!("{source}: {}", error.message);
    error
        .details
        .insert("source".to_owned(), Value::from(source));
    error
}
