//! [`Identity`]: who asks for a mutation.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{ErrorCode, OpError};

/// The longest agent name the store can record: a ref move's actor has a one-byte length.
const MAX_AGENT_LEN: usize = 255;

/// The agent-name grammar as the JSON Schema states it; [`Identity::parse`] implements it.
const AGENT_PATTERN: &str = r"^[a-z0-9-]+/[A-Za-z0-9_-][A-Za-z0-9._-]*$";

/// The caller of a mutation: an agent name `<family>/<instance>` (`agent/worker-17`,
/// `human/purple`) and an optional run id. M1 trusts the name as claimed.
///
/// Every identity is checked by `Identity::parse`, also when it is read from JSON.
#[derive(Serialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `<family>/<instance>`: a family of `[a-z0-9-]+` and an instance of `[A-Za-z0-9._-]+` that
    /// does not start with `.`, at most 255 bytes in all.
    #[schemars(pattern(AGENT_PATTERN), length(max = MAX_AGENT_LEN))]
    agent: String,
    /// The run the agent acts in, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
}

impl Identity {
    /// Checks an agent name against `^[a-z0-9-]+/[A-Za-z0-9_-][A-Za-z0-9._-]*$` (the instance may
    /// not start with `.`) and the 255-byte limit; anything else is `E_INVALID_ARG`. The
    /// identity has no run id.
    pub fn parse(s: &str) -> Result<Self, OpError> {
        let valid = s.len() <= MAX_AGENT_LEN
            && s.split_once('/').is_some_and(|(family, instance)| {
                !family.is_empty()
                    && family
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !instance.is_empty()
                    && !instance.starts_with('.')
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
                "agent names look like agent/worker-17: family [a-z0-9-]+, instance [A-Za-z0-9._-]+ not starting with '.'",
            ));
        }
        Ok(Self {
            agent: s.to_owned(),
            run_id: None,
        })
    }

    /// The agent name.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// The run the agent acts in, when it has one.
    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    /// This identity, acting in run `run_id`.
    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    /// The family: the part of the name before the `/`.
    pub fn family(&self) -> &str {
        self.agent
            .split_once('/')
            .map_or(self.agent.as_str(), |(family, _)| family)
    }

    /// The instance: the part of the name after the `/`.
    pub fn instance(&self) -> &str {
        self.agent
            .split_once('/')
            .map_or("", |(_, instance)| instance)
    }
}

impl<'de> Deserialize<'de> for Identity {
    /// Reads `{agent, run_id?}` and checks `agent` with [`Identity::parse`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            agent: String,
            run_id: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let identity = Self::parse(&raw.agent).map_err(serde::de::Error::custom)?;
        Ok(Self {
            run_id: raw.run_id,
            ..identity
        })
    }
}
