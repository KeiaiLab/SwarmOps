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
//! - A put's content is at most [`MAX_CONTENT_BYTES`], 512 MiB. Each form is measured before it
//!   is converted: inline text by its length, base64 by the bytes it decodes to, and a file by
//!   its length before it is read; a file that grows while it is read (or a device without a
//!   length) is read no further than one byte past the limit.
//!
//! A breach of the limit is `E_TOO_LARGE`, with the content's `size` and the `limit` in
//! `details`; every other breach is `E_INVALID_ARG`. Either names the change, in its message
//! and its `details`: its `index` in the list (from 0) and its `path`, and the client `file` for
//! file content. The whole list is checked before any content is decoded or read, so a
//! malformed list costs no file reads, and nothing is written for a refused one.
//!
//! # Content out
//!
//! `read_file` returns bytes as UTF-8 text when they are UTF-8, and in standard, padded base64
//! otherwise ([`ContentEncoding`]). Text stays text where `max_bytes` cuts it: a cut inside a
//! character moves back to the character's start, so the text returned may be up to 3 bytes
//! shorter than the cap; bytes that are not text are cut at exactly the cap. `show` returns a
//! metadata value the same way, except that a base64 value goes under its key with `:b64`
//! appended, since a metadata map has no field for the encoding.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use swarmops_api::{
    ChangeInput, ChangeOp, ContentEncoding, ContentInput, ErrorCode, ModeInput, OpError,
};
use swarmops_core::{EntryKind, RepoPath};
use swarmops_repo::Change;

/// The most bytes a put's content may hold: 512 MiB.
pub const MAX_CONTENT_BYTES: u64 = 512 * 1024 * 1024;

/// The suffix of a metadata key whose value `show` gives in base64.
const BASE64_KEY_SUFFIX: &str = ":b64";

/// The bytes `content` stands for: inline text as its UTF-8 bytes, base64 decoded, a client
/// file read. `E_INVALID_ARG` for base64 that is not standard and padded, or a file that
/// cannot be read; `E_TOO_LARGE` for more than [`MAX_CONTENT_BYTES`], found before the content
/// is converted. A file's errors carry the `file` in `details`.
pub fn bytes(content: ContentInput) -> Result<Vec<u8>, OpError> {
    bytes_within(content, MAX_CONTENT_BYTES)
}

/// [`bytes`], with at most `limit` bytes.
fn bytes_within(content: ContentInput, limit: u64) -> Result<Vec<u8>, OpError> {
    match content {
        ContentInput::Inline(text) => {
            within(count(text.len()), limit)?;
            Ok(text.into_bytes())
        }
        ContentInput::Base64(encoded) => {
            within(decoded_len(&encoded), limit)?;
            STANDARD.decode(encoded).map_err(|e| {
                OpError::new(
                    ErrorCode::InvalidArg,
                    format!("the content is not standard, padded base64: {e}"),
                )
            })
        }
        ContentInput::File(file) => read_within(&file, limit).map_err(|mut e| {
            e.details.insert("file".to_owned(), Value::from(file));
            e
        }),
    }
}

/// The bytes of client file `file`, at most `limit` of them: its length is checked before it is
/// read, and a file that grows meanwhile, or a device without a length, is read no further than
/// one byte past the limit.
fn read_within(file: &str, limit: u64) -> Result<Vec<u8>, OpError> {
    let unreadable = |e: io::Error| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("cannot read the client file {file:?}: {e}"),
        )
    };
    let handle = File::open(file).map_err(unreadable)?;
    let len = handle.metadata().map_err(unreadable)?.len();
    within(len, limit)?;
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or_default());
    handle
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    within(count(bytes.len()), limit)?;
    Ok(bytes)
}

/// How many bytes base64 `encoded` decodes to: 3 for every 4 characters, less the padding. For a
/// string that is not base64 the count is rough, and decoding refuses the string after.
fn decoded_len(encoded: &str) -> u64 {
    let padding = encoded
        .bytes()
        .rev()
        .take(2)
        .take_while(|&b| b == b'=')
        .count();
    count((encoded.len() / 4 * 3).saturating_sub(padding))
}

/// `E_TOO_LARGE` when content of `size` bytes is more than `limit`, the most a put may hold.
fn within(size: u64, limit: u64) -> Result<(), OpError> {
    if size <= limit {
        return Ok(());
    }
    Err(OpError::new(
        ErrorCode::TooLarge,
        format!("the content is {size} bytes, more than the {limit} a put may hold"),
    )
    .with_details(Map::from_iter([
        ("size".to_owned(), Value::from(size)),
        ("limit".to_owned(), Value::from(limit)),
    ])))
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

/// How `read_file` returns the first `keep` bytes of blob `bytes`: as UTF-8 text when they are
/// text, a cut inside a character moved back to the character's start (at most 3 bytes) when
/// the bytes after the cut complete that character; otherwise exactly the `keep` bytes, in
/// base64.
pub(crate) fn encode(bytes: &[u8], keep: usize) -> (ContentEncoding, String) {
    let kept = &bytes[..keep];
    let text = match std::str::from_utf8(kept) {
        Ok(text) => Some(text),
        // The kept bytes end inside a character, which the bytes after the cut complete: the
        // text is what comes before that character.
        Err(e) if e.error_len().is_none() && starts_with_char(&bytes[e.valid_up_to()..]) => {
            std::str::from_utf8(&kept[..e.valid_up_to()]).ok()
        }
        Err(_) => None,
    };
    match text {
        Some(text) => (ContentEncoding::Utf8, text.to_owned()),
        None => (ContentEncoding::Base64, STANDARD.encode(kept)),
    }
}

/// Whether `bytes` starts with a whole UTF-8 character.
fn starts_with_char(bytes: &[u8]) -> bool {
    match std::str::from_utf8(&bytes[..bytes.len().min(4)]) {
        Ok(text) => !text.is_empty(),
        Err(e) => e.valid_up_to() > 0,
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

fn count(n: usize) -> u64 {
    u64::try_from(n).expect("a usize fits in a u64")
}

/// `error`, said of change `index` of the list, at `path`: its message prefixed with them,
/// and both in its `details`.
fn in_change(mut error: OpError, index: usize, path: &str) -> OpError {
    error.message = format!("change {index} ({path:?}): {}", error.message);
    error.details.insert("index".to_owned(), Value::from(index));
    error.details.insert("path".to_owned(), Value::from(path));
    error
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// The error `result` holds, which must be `E_TOO_LARGE` for `size` bytes over `limit`.
    #[track_caller]
    fn too_large(result: Result<Vec<u8>, OpError>, size: u64, limit: u64) -> OpError {
        let error = result.expect_err("over the limit");
        assert_eq!(error.code, ErrorCode::TooLarge.as_str(), "{error:?}");
        assert_eq!(error.details["size"], size, "{error:?}");
        assert_eq!(error.details["limit"], limit, "{error:?}");
        error
    }

    #[test]
    fn the_limit_is_512_mib() {
        assert_eq!(MAX_CONTENT_BYTES, 512 * 1024 * 1024);
    }

    #[test]
    fn content_is_measured_against_the_limit_before_it_is_converted() {
        let inline = |text: &str| ContentInput::Inline(text.to_owned());
        let base64 = |bytes: &[u8]| ContentInput::Base64(STANDARD.encode(bytes));
        // At the limit, each form is taken; one byte over, each is refused.
        assert_eq!(bytes_within(inline("abcd"), 4).unwrap(), b"abcd");
        too_large(bytes_within(inline("abcde"), 4), 5, 4);
        for n in 1..=6 {
            let content = vec![7; n];
            let limit = u64::try_from(n).unwrap();
            assert_eq!(bytes_within(base64(&content), limit).unwrap(), content);
            too_large(bytes_within(base64(&content), limit - 1), limit, limit - 1);
        }

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("four.bin");
        fs::write(&file, b"four").unwrap();
        let named = || ContentInput::File(file.to_str().unwrap().to_owned());
        assert_eq!(bytes_within(named(), 4).unwrap(), b"four");
        let error = too_large(bytes_within(named(), 3), 4, 3);
        assert_eq!(error.details["file"], file.to_str().unwrap());
        // A file whose length says nothing (a device that never ends) is read no further than
        // one byte past the limit.
        let endless = ContentInput::File("/dev/zero".to_owned());
        too_large(bytes_within(endless, 4), 5, 4);
    }
}
