//! [`Identity`]: who asks for a mutation.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, OpError};

/// The longest agent name the store can record: a ref move's actor has a one-byte length.
const MAX_AGENT_LEN: usize = 255;

/// The caller of a mutation: an agent name `<family>/<instance>` (`agent/worker-17`,
/// `human/purple`) and an optional run id. M1 trusts the name as claimed.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `<family>/<instance>`: a family of `[a-z0-9-]+` and an instance of `[A-Za-z0-9._-]+`.
    pub agent: String,
    /// The run the agent acts in, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Identity {
    /// Checks an agent name against `^[a-z0-9-]+/[A-Za-z0-9._-]+$` and the 255-byte limit;
    /// anything else is `E_INVALID_ARG`. The identity has no run id.
    pub fn parse(s: &str) -> Result<Self, OpError> {
        let valid = s.len() <= MAX_AGENT_LEN
            && s.split_once('/').is_some_and(|(family, instance)| {
                !family.is_empty()
                    && family
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !instance.is_empty()
                    && instance
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            });
        if !valid {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!(
                    "invalid agent name {s:?}: expected <family>/<instance> of at most {MAX_AGENT_LEN} bytes"
                ),
            )
            .with_hint(
                "agent names look like agent/worker-17: family [a-z0-9-]+, instance [A-Za-z0-9._-]+",
            ));
        }
        Ok(Self {
            agent: s.to_owned(),
            run_id: None,
        })
    }

    /// The part of the name before the `/` (all of it if there is none).
    pub fn family(&self) -> &str {
        self.agent
            .split_once('/')
            .map_or(self.agent.as_str(), |(family, _)| family)
    }

    /// The part of the name after the `/` (empty if there is none).
    pub fn instance(&self) -> &str {
        self.agent
            .split_once('/')
            .map_or("", |(_, instance)| instance)
    }
}
