//! [`Identity`]: who asks for a mutation.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{ErrorCode, OpError};

/// The longest agent name the store can record: a ref move's actor has a one-byte length.
const MAX_AGENT_LEN: usize = 255;

/// The bytes a family may hold, as a regex character class.
const FAMILY_BYTES: &str = "a-z0-9-";
/// The bytes an instance may start with: not `.`, so `.` and `..` are never instances.
const INSTANCE_FIRST_BYTES: &str = "A-Za-z0-9_-";
/// The bytes an instance may hold.
const INSTANCE_BYTES: &str = "A-Za-z0-9._-";

/// The agent-name grammar as the JSON Schema publishes it, built from the byte classes that
/// [`Identity::parse`] checks, so the two cannot drift apart.
fn agent_pattern() -> String {
    format!("^[{FAMILY_BYTES}]+/[{INSTANCE_FIRST_BYTES}][{INSTANCE_BYTES}]*$")
}

/// Whether `b` is in `class`, a regex character class of ranges (`a-z`) and single bytes, where
/// a `-` that cannot open a range stands for itself.
fn in_class(class: &str, b: u8) -> bool {
    let class = class.as_bytes();
    let mut i = 0;
    while i < class.len() {
        if i + 2 < class.len() && class[i + 1] == b'-' {
            if (class[i]..=class[i + 2]).contains(&b) {
                return true;
            }
            i += 3;
        } else {
            if class[i] == b {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// The caller of a mutation: an agent name `<family>/<instance>` (`agent/worker-17`,
/// `human/purple`) and an optional run id. M1 trusts the name as claimed.
///
/// The agent name always follows the grammar below, also in an identity read from JSON.
#[derive(Serialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `<family>/<instance>`, exactly as the schema's `pattern` and `maxLength` give it.
    #[schemars(pattern(agent_pattern()), length(max = MAX_AGENT_LEN))]
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
                    && family.bytes().all(|b| in_class(FAMILY_BYTES, b))
                    && instance
                        .bytes()
                        .next()
                        .is_some_and(|b| in_class(INSTANCE_FIRST_BYTES, b))
                    && instance.bytes().all(|b| in_class(INSTANCE_BYTES, b))
            });
        if !valid {
            return Err(OpError::new(
                ErrorCode::InvalidArg,
                format!(
                    "invalid agent name {s:?}: expected <family>/<instance> of at most {MAX_AGENT_LEN} bytes"
                ),
            )
            .with_hint(format!(
                "agent names look like agent/worker-17: family [{FAMILY_BYTES}]+, instance \
                 [{INSTANCE_BYTES}]+ not starting with '.'"
            )));
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
        #[serde(rename = "Identity", expecting = "struct Identity")]
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
