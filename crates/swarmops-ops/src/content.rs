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
//!   to read a path; `blob` content names, by its id (64 hex digits, of either case), a blob the
//!   store holds already, whose bytes the commit neither sends nor reads (an id the store holds
//!   no blob under is the repository's refusal, `E_INVALID_ARG` too, when the commit runs).
//! - A put's content is at most [`MAX_CONTENT_BYTES`], 512 MiB. Each form is measured before it
//!   is converted: inline text by its length, base64 by the bytes it decodes to, and a file by
//!   its length before it is read; a file that grows while it is read (or a device without a
//!   length) is read no further than one byte past the limit.
//!
//! A breach of the limit is `E_TOO_LARGE`, with the content's `size` and the `limit` in
//! `details`; every other breach is `E_INVALID_ARG`. Either names the change, in its message
//! and its `details`: its `index` in the list (from 0) and its `path`, and the client `file` for
//! file content or the `blob` for blob content. The whole list is checked before any content is
//! decoded or read, so a malformed list costs no file reads, and nothing is written for a
//! refused one.
//!
//! # Content out
//!
//! `read_file` returns bytes as UTF-8 text when they are UTF-8, and in standard, padded base64
//! otherwise ([`ContentEncoding`]). Text stays text where `max_bytes` cuts it: a cut inside a
//! character moves back to the character's start, so the text returned may be up to 3 bytes
//! shorter than the cap; bytes that are not text are cut at exactly the cap. `show` returns a
//! metadata value the same way, except that a base64 value goes under its key with `:b64`
//! appended, since a metadata map has no field for the encoding. That suffix is reserved: a
//! commit refuses a metadata key that ends in it, so `k:b64` is always the base64 of `k`'s
//! value.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use swarmops_api::{
    ChangeInput, ChangeOp, ContentEncoding, ContentInput, ErrorCode, ModeInput, OpError,
};
use swarmops_core::bytes::to_u64;
use swarmops_core::{BlobId, LeafKind, ObjectId, RepoPath};
use swarmops_repo::Change;

/// The most bytes a put's content may hold: 512 MiB.
pub const MAX_CONTENT_BYTES: u64 = 512 * 1024 * 1024;

/// The suffix of a metadata key whose value `show` gives in base64.
const BASE64_KEY_SUFFIX: &str = ":b64";

/// The bytes `content` stands for: inline text as its UTF-8 bytes, base64 decoded, a client
/// file read. `E_INVALID_ARG` for base64 that is not standard and padded, a file that cannot be
/// read, or a blob named by id, whose bytes are the store's (with the `blob` in `details`);
/// `E_TOO_LARGE` for more than [`MAX_CONTENT_BYTES`], found before the content is converted. A
/// file's errors carry the `file` in `details`.
pub fn bytes(content: ContentInput) -> Result<Vec<u8>, OpError> {
    bytes_within(content, MAX_CONTENT_BYTES)
}

/// [`bytes`], with at most `limit` bytes.
fn bytes_within(content: ContentInput, limit: u64) -> Result<Vec<u8>, OpError> {
    match content {
        ContentInput::Inline(text) => {
            within(to_u64(text.len()), limit)?;
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
        ContentInput::Blob(id) => Err(OpError::new(
            ErrorCode::InvalidArg,
            "blob content names bytes the store holds already: give the content inline, in \
             base64 or as a file",
        )
        .with_details(Map::from_iter([("blob".to_owned(), Value::from(id))]))),
    }
}

/// The blob `id` names, 64 hex digits of either case: `E_INVALID_ARG` with the `blob` in
/// `details` for anything else.
pub(crate) fn blob_id(id: &str) -> Result<BlobId, OpError> {
    ObjectId::from_hex(id).map(BlobId).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{id:?} is not a blob id: {e}"),
        )
        .with_details(Map::from_iter([("blob".to_owned(), Value::from(id))]))
    })
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
    within(to_u64(bytes.len()), limit)?;
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
    to_u64((encoded.len() / 4 * 3).saturating_sub(padding))
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
            (ChangeOp::Put, Some(ContentInput::Blob(id)), mode) => {
                let id = blob_id(&id).map_err(|e| in_change(e, index, &path))?;
                Some((leaf_kind(mode), Content::Blob(id)))
            }
            (ChangeOp::Put, Some(content), mode) => {
                Some((leaf_kind(mode), Content::Bytes(content)))
            }
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
                Some((kind, Content::Bytes(content))) => swarmops_repo::ChangeOp::Put {
                    kind,
                    content: bytes(content).map_err(|e| in_change(e, index, path.as_str()))?,
                },
                Some((kind, Content::Blob(id))) => swarmops_repo::ChangeOp::PutBlob { kind, id },
                None => swarmops_repo::ChangeOp::Delete,
            };
            Ok(Change { path, op })
        })
        .collect()
}

/// A put's content once its change's shape is checked: bytes to decode or read, or a blob the
/// store holds, by its id.
enum Content {
    /// Inline, base64 or file content.
    Bytes(ContentInput),
    /// A blob named by its id.
    Blob(BlobId),
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

/// Refuses metadata with a key that ends in `:b64`, the suffix `show` puts on a key whose value
/// is not UTF-8, so that `k:b64` always means `k`'s value in base64: `E_INVALID_ARG` with the
/// `key` in `details`.
pub(crate) fn check_metadata(metadata: &[(String, Vec<u8>)]) -> Result<(), OpError> {
    match metadata
        .iter()
        .find(|(key, _)| key.ends_with(BASE64_KEY_SUFFIX))
    {
        Some((key, _)) => Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("metadata key {key:?} ends in {BASE64_KEY_SUFFIX:?}, which show reserves"),
        )
        .with_details(Map::from_iter([(
            "key".to_owned(),
            Value::from(key.as_str()),
        )]))),
        None => Ok(()),
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

/// The kind of leaf a put of `mode` writes: a regular file when it names none. Every mode is a
/// leaf's, so no put is a directory's.
fn leaf_kind(mode: Option<ModeInput>) -> LeafKind {
    match mode {
        None | Some(ModeInput::File) => LeafKind::File,
        Some(ModeInput::Exec) => LeafKind::Exec,
        Some(ModeInput::Symlink) => LeafKind::Symlink,
    }
}

/// `error`, said of change `index` of the list, at `path`: its message prefixed with them,
/// and both in its `details`.
pub(crate) fn in_change(mut error: OpError, index: usize, path: &str) -> OpError {
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
    fn metadata_keys_may_not_end_in_the_base64_suffix() {
        let entry = |key: &str| (key.to_owned(), b"v".to_vec());
        assert!(check_metadata(&[entry("agent"), entry("run_id"), entry("b64")]).is_ok());
        for key in ["note:b64", ":b64", "agent:b64"] {
            let error = check_metadata(&[entry("agent"), entry(key)]).expect_err(key);
            assert_eq!(error.code, ErrorCode::InvalidArg.as_str(), "{error:?}");
            assert_eq!(error.details["key"], key, "{error:?}");
        }
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
