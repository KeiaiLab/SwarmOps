//! `authors.toml` (M13; operations spec §5): the optional map from an agent family to the email
//! (and name) its new changesets are signed with, in place of
//! `<instance>@<family>.swarmops.local`, so that a hosting service can link them to an account.
//! An existing changeset's signatures never change.
//!
//! Written by the M13 plan's task C9.

use std::collections::BTreeSet;
use std::io;

use camino::Utf8Path;
use serde::Deserialize;
use swarmops_api::{ErrorCode, OpError};

use crate::error::{details, error};

/// The file's name in the store's root, and the `store_file` its errors name.
const FILE: &str = "authors.toml";

/// The only `version` this build reads.
const VERSION: u32 = 1;

/// Characters a signature cannot carry: they would end the field or the line.
const FORBIDDEN: [char; 4] = ['<', '>', '\n', '\r'];

/// One agent family's signing identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Author {
    /// The address new commits carry.
    pub(crate) email: String,
    /// The name new commits carry; `None` keeps the agent name.
    pub(crate) name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    #[serde(default)]
    author: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    family: String,
    email: String,
    name: Option<String>,
}

/// The author recorded for `family` in the store's `authors.toml`, read now, never cached.
/// `None` if there is no file or no entry for the family. `E_STORE_CORRUPT`
/// `{store_file: "authors.toml"}` if the file does not parse, has another version, gives a family
/// twice, or holds a value with `<`, `>`, a line break or a NUL.
pub(crate) fn author_of(store: &Utf8Path, family: &str) -> Result<Option<Author>, OpError> {
    // 1. No file is the common case: signatures stay as they were.
    let text = match std::fs::read_to_string(store.join(FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return Err(corrupt("is not UTF-8"));
        }
        Err(e) => {
            return Err(error(ErrorCode::Io, format!("{FILE} cannot be read: {e}"))
                .with_details(details([("store_file", FILE.into())])));
        }
    };

    // 2. The shape, then the rules a parser cannot state.
    let file: File = toml::from_str(&text).map_err(|e| corrupt(e.message()))?;
    if file.version != VERSION {
        return Err(corrupt(&format!(
            "has version {}, not {VERSION}",
            file.version
        )));
    }
    let mut seen = BTreeSet::new();
    for entry in &file.author {
        let values = [Some(&entry.family), Some(&entry.email), entry.name.as_ref()];
        let bad = values
            .into_iter()
            .flatten()
            .any(|v| v.is_empty() || v.contains(FORBIDDEN) || v.contains('\0'));
        if bad {
            return Err(corrupt(&format!(
                "has a value that is empty or holds <, >, a line break or a NUL (family {:?})",
                entry.family
            )));
        }
        if !seen.insert(entry.family.as_str()) {
            return Err(corrupt(&format!(
                "has two entries for family {:?}",
                entry.family
            )));
        }
    }

    // 3. The family's entry, if any.
    let found = (file.author.into_iter())
        .find(|entry| entry.family == family)
        .map(|entry| Author {
            email: entry.email,
            name: entry.name,
        });
    Ok(found)
}

fn corrupt(what: &str) -> OpError {
    error(ErrorCode::StoreCorrupt, format!("{FILE} {what}"))
        .with_details(details([("store_file", FILE.into())]))
}
