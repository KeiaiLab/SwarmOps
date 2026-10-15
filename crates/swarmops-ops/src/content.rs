//! Content, in and out (operations spec §0): a commit's change list checked and its contents
//! turned into bytes, and stored bytes turned back into JSON text.
//!
//! # Change lists
//!
//! A commit's [`ChangeInput`]s become the repository's [`Change`]s by these rules:
//!
//! - Each `path` is a repository path (`/`-separated and relative, with no empty, `.` or `..`
//!   component, and no NUL), named by one change only.
//! - A `put` needs content, and writes a regular file unless `mode` says `exec` or `symlink`
//!   (whose content is the link's target). A `delete` takes neither content nor a mode.
//! - `inline` content is UTF-8 text, taken verbatim; `base64` content is standard, padded
//!   base64; `file` content is a file on the client's filesystem (a relative path is taken
//!   from the current directory), read here, on the client, so that the store is never asked
//!   to read a path.
//!
//! Every breach is `E_INVALID_ARG`, its message and `details` naming the change: its `index`
//! in the list (from 0) and its `path`, and the client `file` when that cannot be read. The
//! whole list is checked before any content is decoded or read, so a malformed list costs no
//! file reads.
//!
//! # Content out
//!
//! `read_file` returns bytes as UTF-8 text when they are UTF-8, and in standard, padded base64
//! otherwise ([`ContentEncoding`]). `show` returns a metadata value the same way, except that
//! a base64 value goes under its key with `:b64` appended, since a metadata map has no field
//! for the encoding.

use std::collections::BTreeSet;
use std::fs;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use swarmops_api::{
    ChangeInput, ChangeOp, ContentEncoding, ContentInput, ErrorCode, ModeInput, OpError,
};
use swarmops_core::{EntryKind, RepoPath};
use swarmops_repo::Change;

/// The suffix of a metadata key whose value `show` gives in base64.
const BASE64_KEY_SUFFIX: &str = ":b64";

/// The bytes `content` stands for: inline text as its UTF-8 bytes, base64 decoded, a client
/// file read. `E_INVALID_ARG` for base64 that is not standard and padded, or a file that
/// cannot be read (with the `file` in `details`).
pub fn bytes(content: ContentInput) -> Result<Vec<u8>, OpError> {
    match content {
        ContentInput::Inline(text) => Ok(text.into_bytes()),
        ContentInput::Base64(encoded) => STANDARD.decode(encoded).map_err(|e| {
            OpError::new(
                ErrorCode::InvalidArg,
                format!("the content is not standard, padded base64: {e}"),
            )
        }),
        ContentInput::File(file) => fs::read(&file).map_err(|e| {
            OpError::new(
                ErrorCode::InvalidArg,
                format!("cannot read the client file {file:?}: {e}"),
            )
            .with_details(Map::from_iter([("file".to_owned(), Value::from(file))]))
        }),
    }
}

/// The repository changes `inputs` describe, by the rules in
/// [the module documentation](self): first every change's shape is checked, then the
/// contents are decoded and read, in order.
pub(crate) fn changes(inputs: Vec<ChangeInput>) -> Result<Vec<Change>, OpError> {
    let mut named = BTreeSet::new();
    let mut shaped = Vec::with_capacity(inputs.len());
    for (index, input) in inputs.into_iter().enumerate() {
        let ChangeInput {
            path,
            op,
            content,
            mode,
        } = input;
        let refuse =
            |what: &str| in_change(OpError::new(ErrorCode::InvalidArg, what), index, &path);
        let repo_path = RepoPath::new(path.as_str())
            .map_err(|e| refuse(&format!("not a repository path: {e}")))?;
        let content = match (op, content, mode) {
            (ChangeOp::Put, Some(content), mode) => Some((entry_kind(mode), content)),
            (ChangeOp::Put, None, _) => return Err(refuse("a put needs content")),
            (ChangeOp::Delete, None, None) => None,
            (ChangeOp::Delete, Some(_), _) => return Err(refuse("a delete takes no content")),
            (ChangeOp::Delete, None, Some(_)) => return Err(refuse("a delete takes no mode")),
        };
        if !named.insert(repo_path.clone()) {
            return Err(refuse("an earlier change names this path too"));
        }
        shaped.push((index, repo_path, content));
    }
    shaped
        .into_iter()
        .map(|(index, path, content)| {
            let op = match content {
                Some((kind, content)) => swarmops_repo::ChangeOp::Put {
                    kind,
                    content: bytes(content).map_err(|e| in_change(e, index, path.as_str()))?,
                },
                None => swarmops_repo::ChangeOp::Delete,
            };
            Ok(Change { path, op })
        })
        .collect()
}

/// `path` as a repository path; `E_INVALID_ARG` with the `path` in `details` if it is not one.
pub(crate) fn repo_path(path: &str) -> Result<RepoPath, OpError> {
    RepoPath::new(path).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{path:?} is not a repository path: {e}"),
        )
        .with_details(Map::from_iter([("path".to_owned(), Value::from(path))]))
    })
}

/// How `read_file` returns `bytes`: as UTF-8 text when they are UTF-8, else in base64.
pub(crate) fn encode(bytes: &[u8]) -> (ContentEncoding, String) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (ContentEncoding::Utf8, text.to_owned()),
        Err(_) => (ContentEncoding::Base64, STANDARD.encode(bytes)),
    }
}

/// How `show` returns metadata entry `key`, `value`: the value as UTF-8 text when it is UTF-8,
/// else in base64 under the key with `:b64` appended.
pub(crate) fn metadata_entry(key: &str, value: &[u8]) -> (String, String) {
    match std::str::from_utf8(value) {
        Ok(text) => (key.to_owned(), text.to_owned()),
        Err(_) => (format!("{key}{BASE64_KEY_SUFFIX}"), STANDARD.encode(value)),
    }
}

/// The kind of entry a put of `mode` writes: a regular file when it names none.
fn entry_kind(mode: Option<ModeInput>) -> EntryKind {
    match mode {
        None | Some(ModeInput::File) => EntryKind::File,
        Some(ModeInput::Exec) => EntryKind::Exec,
        Some(ModeInput::Symlink) => EntryKind::Symlink,
    }
}

/// `error`, said of change `index` of the list, at `path`: its message prefixed with them,
/// and both in its `details`.
fn in_change(mut error: OpError, index: usize, path: &str) -> OpError {
    error.message = format!("change {index} ({path:?}): {}", error.message);
    error.details.insert("index".to_owned(), Value::from(index));
    error.details.insert("path".to_owned(), Value::from(path));
    error
}
