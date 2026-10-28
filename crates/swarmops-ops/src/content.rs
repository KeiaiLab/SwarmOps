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
//! - A put's content is at most [`MAX_CONTENT_BYTES`], 512 MiB, and a commit's contents together
//!   at most [`MAX_COMMIT_CONTENT_BYTES`], 1 GiB. Each form is measured before it is converted:
//!   inline text by its length, base64 by the bytes it decodes to, and a file by its length
//!   before it is read; a blob named by id counts nothing, since the commit holds none of its
//!   bytes. Both limits are checked as the list's shape is, by the sizes the contents declare (a
//!   file's length is `stat`ed, not read; one that cannot be counts 0, and its read reports
//!   it), so a list past either costs no file reads. Then each content is converted, in order, and
//!   a file that grows while it is read (or a device without a length) is read no further than
//!   one byte past what either limit leaves it.
//!
//! A land's resolutions that put content follow the same rules for their contents, each said of
//! the path it resolves rather than of an index.
//!
//! A breach of a limit is `E_TOO_LARGE`, with the `size` and the `limit` in `details`, and the
//! `scope` that says which: `put` for one content, whose `size` is its own, or `commit` for the
//! contents together, whose `size` is theirs up to and with the change's (a client reading its
//! request also refuses it whole, `request`, before parsing it: [`MAX_REQUEST_BYTES`]). Every other
//! breach is `E_INVALID_ARG`. Either names the change, in its message and its `details`: its
//! `index` in the list (from 0) and its `path`, and the client `file` for file content or the
//! `blob` for blob content. The whole list is checked before any content is decoded or read, so a
//! malformed list costs no file reads, and nothing is written for a refused one.
//!
//! # Content out
//!
//! `read_file` returns bytes as UTF-8 text when they are UTF-8, and in standard, padded base64
//! otherwise ([`ContentEncoding`]): the bytes it returns, so that bytes read from an offset inside
//! a character, which do not start as text, come back in base64. Text stays text where
//! `max_bytes` cuts it: a cut inside a character moves back to the character's start, so the text
//! returned may be up to 3 bytes shorter than the cap, and the next page starts at that
//! character; bytes that are not text are cut at exactly the cap. `show` returns a
//! metadata value the same way, except that a base64 value goes under its key with `:b64`
//! appended, since a metadata map has no field for the encoding. That suffix is reserved: a
//! commit refuses a metadata key that ends in it, so `k:b64` is always the base64 of `k`'s
//! value.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use swarmops_api::{
    ChangeInput, ChangeOp, ContentEncoding, ContentInput, ErrorCode, ModeInput, OpError,
};
use swarmops_core::bytes;
use swarmops_core::{BlobId, LeafKind, ObjectId, RepoPath};
use swarmops_repo::Change;

use crate::error;

/// The most bytes a put's content may hold: 512 MiB, the most an object of the store holds
/// ([`swarmops_store::MAX_OBJECT_LEN`]).
pub const MAX_CONTENT_BYTES: u64 = swarmops_store::MAX_OBJECT_LEN;

/// The most bytes a commit's contents may hold together: 1 GiB, two puts of the most each may
/// hold. A blob named by id counts nothing.
pub const MAX_COMMIT_CONTENT_BYTES: u64 = 1 << 30;

/// The most bytes of a request's text that a client reads (operations spec §0): 64 MiB, the CLI's
/// `--changes` file or stdin, and each line of the MCP server's input. Larger contents go as a
/// `file`, or are stored first (`blob_put`) and named by id.
pub const MAX_REQUEST_BYTES: u64 = 64 << 20;

/// What breaks the limit that an `E_TOO_LARGE` names (`details.scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// One put's content: [`MAX_CONTENT_BYTES`].
    Put,
    /// A commit's contents together: [`MAX_COMMIT_CONTENT_BYTES`].
    Commit,
}

impl Scope {
    /// The scope as `details.scope` gives it.
    fn as_str(self) -> &'static str {
        match self {
            Scope::Put => "put",
            Scope::Commit => "commit",
        }
    }
}

/// The hint of a commit whose contents are past their total: what takes them out of it.
const BLOB_HINT: &str =
    "store large contents first with swarmops blob put, then name each by its id: {\"blob\": id}";

/// What one content is measured against: a put's limit, and what the commit's limit leaves of
/// itself once the contents converted before it are counted.
#[derive(Debug, Clone, Copy)]
struct Room {
    /// The most bytes one content may hold.
    put: u64,
    /// The most bytes the contents may hold together.
    total: u64,
    /// The bytes of the contents converted before this one.
    used: u64,
}

impl Room {
    /// Room for one content alone, outside any commit: `blob_put`'s.
    fn alone(put: u64) -> Room {
        Room {
            put,
            total: u64::MAX,
            used: 0,
        }
    }

    /// `E_TOO_LARGE` when content of `size` bytes is more than a put may hold (scope `put`), or
    /// brings the contents past what they may hold together (scope `commit`, the contents' size
    /// with it).
    fn check(self, size: u64) -> Result<(), OpError> {
        if size > self.put {
            return Err(too_large_for(size, self.put, Scope::Put));
        }
        let total = self.used.saturating_add(size);
        if total > self.total {
            return Err(too_large_for(total, self.total, Scope::Commit));
        }
        Ok(())
    }

    /// The most bytes of a file to read: one past the most either limit leaves the content.
    fn read_cap(self) -> u64 {
        let left = self.total.saturating_sub(self.used);
        self.put.min(left).saturating_add(1)
    }
}

/// The suffix of a metadata key whose value `show` gives in base64: the one the repository
/// refuses at the end of a stored key ([`swarmops_repo::check_metadata`]).
const BASE64_KEY_SUFFIX: &str = swarmops_repo::RESERVED_METADATA_KEY_SUFFIX;

/// The bytes `content` stands for: inline text as its UTF-8 bytes, base64 decoded, a client
/// file read. `E_INVALID_ARG` for base64 that is not standard and padded, a file that cannot be
/// read, or a blob named by id, whose bytes are the store's (with the `blob` in `details`);
/// `E_TOO_LARGE` for more than [`MAX_CONTENT_BYTES`], found before the content is converted. A
/// file's errors carry the `file` in `details`.
pub fn bytes(content: ContentInput) -> Result<Vec<u8>, OpError> {
    bytes_within(content, MAX_CONTENT_BYTES)
}

/// [`bytes()`], with at most `limit` bytes.
fn bytes_within(content: ContentInput, limit: u64) -> Result<Vec<u8>, OpError> {
    convert(content, Room::alone(limit))
}

/// The bytes `content` stands for, measured against `room` before it is converted.
fn convert(content: ContentInput, room: Room) -> Result<Vec<u8>, OpError> {
    match content {
        ContentInput::Inline(text) => {
            room.check(bytes::to_u64(text.len()))?;
            Ok(text.into_bytes())
        }
        ContentInput::Base64(encoded) => {
            room.check(decoded_len(&encoded))?;
            STANDARD.decode(encoded).map_err(|e| {
                OpError::new(
                    ErrorCode::InvalidArg,
                    format!("the content is not standard, padded base64: {e}"),
                )
            })
        }
        ContentInput::File(file) => read_within(&file, room).map_err(|mut e| {
            e.details.insert("file".to_owned(), Value::from(file));
            e
        }),
        ContentInput::Blob(id) => Err(OpError::new(
            ErrorCode::InvalidArg,
            "blob content names bytes the store holds already: give the content inline, in \
             base64 or as a file",
        )
        .with_details(Map::from_iter([("blob".to_owned(), Value::from(id))]))),
        _ => Err(error::unknown_variant("ContentInput")),
    }
}

/// The blob `id` names, 64 hex digits of either case: `E_INVALID_ARG` with the `blob` in
/// `details` for anything else.
pub(crate) fn blob_id(id: &str) -> Result<BlobId, OpError> {
    ObjectId::from_hex(id).map(BlobId::new).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("{id:?} is not a blob id: {e}"),
        )
        .with_details(Map::from_iter([("blob".to_owned(), Value::from(id))]))
    })
}

/// The bytes of client file `file`, measured against `room`: its length is checked before it is
/// read, and a file that grows meanwhile, or a device without a length, is read no further than
/// one byte past what the limits leave it.
fn read_within(file: &str, room: Room) -> Result<Vec<u8>, OpError> {
    let unreadable = |e: io::Error| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("cannot read the client file {file:?}: {e}"),
        )
    };
    let handle = File::open(file).map_err(unreadable)?;
    let len = handle.metadata().map_err(unreadable)?.len();
    room.check(len)?;
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or_default());
    handle
        .take(room.read_cap())
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    room.check(bytes::to_u64(bytes.len()))?;
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
    bytes::to_u64((encoded.len() / 4 * 3).saturating_sub(padding))
}

/// How many bytes `content` declares before it is converted: inline text's length, the bytes
/// base64 decodes to, a client file's length (a `stat`, not a read: 0 for one that cannot be
/// stat'ed, whose read then says why), and nothing for a blob named by id. `E_INTERNAL` for a
/// content this build does not know.
fn declared(content: &ContentInput) -> Result<u64, OpError> {
    match content {
        ContentInput::Inline(text) => Ok(bytes::to_u64(text.len())),
        ContentInput::Base64(encoded) => Ok(decoded_len(encoded)),
        ContentInput::File(file) => Ok(fs::metadata(file).map_or(0, |meta| meta.len())),
        ContentInput::Blob(_) => Ok(0),
        _ => Err(error::unknown_variant("ContentInput")),
    }
}

/// The declared size of the contents before `content` (`before`) with its own, once `content`'s
/// declared size ([`declared`]) is checked against a put's limit, `put_limit` (scope `put`), and
/// that sum against the contents' together, `total_limit` (scope `commit`): the shape pass's
/// check, which reads no file. A refusal names the client `file` for file content.
fn within_declared(
    content: &ContentInput,
    before: u64,
    put_limit: u64,
    total_limit: u64,
) -> Result<u64, OpError> {
    // The file a refusal names, if the content is one.
    let named = |mut error: OpError| {
        if let ContentInput::File(file) = content {
            (error.details).insert("file".to_owned(), Value::from(file.as_str()));
        }
        error
    };

    // 1. The content alone, against a put's limit.
    let size = declared(content)?;
    if size > put_limit {
        return Err(named(too_large_for(size, put_limit, Scope::Put)));
    }

    // 2. With the contents before it, against their limit together.
    let total = before.saturating_add(size);
    if total > total_limit {
        return Err(named(too_large_for(total, total_limit, Scope::Commit)));
    }
    Ok(total)
}

/// `E_TOO_LARGE` for `size` bytes over `limit`, the most `scope` may hold: `{size, limit,
/// scope}`, and for a commit's total, the hint that takes contents out of the request.
fn too_large_for(size: u64, limit: u64, scope: Scope) -> OpError {
    let message = match scope {
        Scope::Put => format!("the content is {size} bytes, more than the {limit} a put may hold"),
        Scope::Commit => format!(
            "the contents come to {size} bytes with this change's, more than the {limit} a \
             commit may hold together"
        ),
    };
    let error = OpError::new(ErrorCode::TooLarge, message).with_details(Map::from_iter([
        ("size".to_owned(), Value::from(size)),
        ("limit".to_owned(), Value::from(limit)),
        ("scope".to_owned(), Value::from(scope.as_str())),
    ]));
    match scope {
        Scope::Put => error,
        Scope::Commit => error.with_hint(BLOB_HINT),
    }
}

/// The repository changes `inputs` describe, by the rules in
/// [the module documentation](self): first every change's shape is checked, with the sizes its
/// content declares, then the contents are decoded and read, in order.
pub(crate) fn changes(inputs: Vec<ChangeInput>) -> Result<Vec<Change>, OpError> {
    changes_within(inputs, MAX_CONTENT_BYTES, MAX_COMMIT_CONTENT_BYTES)
}

/// [`changes`], with at most `put_limit` bytes in a put's content and `total_limit` in all of
/// them.
fn changes_within(
    inputs: Vec<ChangeInput>,
    put_limit: u64,
    total_limit: u64,
) -> Result<Vec<Change>, OpError> {
    let mut named = BTreeSet::new();
    let mut shaped = Vec::with_capacity(inputs.len());
    let mut declared_total: u64 = 0;
    for (index, input) in inputs.into_iter().enumerate() {
        let ChangeInput {
            path,
            op,
            content,
            mode,
            ..
        } = input;
        let refuse =
            |what: &str| in_change(OpError::new(ErrorCode::InvalidArg, what), index, &path);
        let repo_path = RepoPath::new(path.as_str())
            .map_err(|e| refuse(&format!("not a repository path: {e}")))?;
        let content = match (op, content, mode) {
            (ChangeOp::Put, Some(ContentInput::Blob(id)), mode) => {
                let id = blob_id(&id).map_err(|e| in_change(e, index, &path))?;
                Some((leaf_kind(mode)?, Content::Blob(id)))
            }
            (ChangeOp::Put, Some(content), mode) => {
                Some((leaf_kind(mode)?, Content::Bytes(content)))
            }
            (ChangeOp::Put, None, _) => return Err(refuse("a put needs content")),
            (ChangeOp::Delete, None, None) => None,
            (ChangeOp::Delete, Some(_), _) => return Err(refuse("a delete takes no content")),
            (ChangeOp::Delete, None, Some(_)) => return Err(refuse("a delete takes no mode")),
            _ => return Err(error::unknown_variant("ChangeOp")),
        };
        if !named.insert(repo_path.clone()) {
            return Err(refuse("an earlier change names this path too"));
        }
        if let Some((_, Content::Bytes(content))) = &content {
            declared_total = within_declared(content, declared_total, put_limit, total_limit)
                .map_err(|e| in_change(e, index, &path))?;
        }
        shaped.push((index, repo_path, content));
    }
    let mut used: u64 = 0;
    shaped
        .into_iter()
        .map(|(index, path, content)| {
            let op = match content {
                Some((kind, Content::Bytes(content))) => {
                    let room = Room {
                        put: put_limit,
                        total: total_limit,
                        used,
                    };
                    let content =
                        convert(content, room).map_err(|e| in_change(e, index, path.as_str()))?;
                    used = used.saturating_add(bytes::to_u64(content.len()));
                    swarmops_repo::ChangeOp::Put { kind, content }
                }
                Some((kind, Content::Blob(id))) => swarmops_repo::ChangeOp::PutBlob { kind, id },
                None => swarmops_repo::ChangeOp::Delete,
            };
            Ok(Change::new(path, op))
        })
        .collect()
}

/// The bytes of a land's content resolutions (operations spec §3, resolution path (b)), each
/// with the path it resolves, by the rules of a commit's contents ([the module
/// documentation](self)), `blob` content aside, which names bytes the store holds and is not
/// passed here: the sizes they declare are checked against a put's limit and, together, against
/// a commit's, before any is converted, so that resolutions past them cost no file reads; then
/// each is converted, in order. An error names the resolution, by its path, in its message and
/// its `details` ([`in_resolution`]).
pub(crate) fn resolution_bytes(
    contents: Vec<(RepoPath, ContentInput)>,
) -> Result<Vec<(RepoPath, Vec<u8>)>, OpError> {
    resolutions_within(contents, MAX_CONTENT_BYTES, MAX_COMMIT_CONTENT_BYTES)
}

/// [`resolution_bytes`], with at most `put_limit` bytes in a resolution's content and
/// `total_limit` in all of them.
fn resolutions_within(
    contents: Vec<(RepoPath, ContentInput)>,
    put_limit: u64,
    total_limit: u64,
) -> Result<Vec<(RepoPath, Vec<u8>)>, OpError> {
    let mut declared_total: u64 = 0;
    for (path, content) in &contents {
        declared_total = within_declared(content, declared_total, put_limit, total_limit)
            .map_err(|e| in_resolution(e, path.as_str()))?;
    }
    let mut used: u64 = 0;
    (contents.into_iter())
        .map(|(path, content)| {
            let room = Room {
                put: put_limit,
                total: total_limit,
                used,
            };
            let bytes = convert(content, room).map_err(|e| in_resolution(e, path.as_str()))?;
            used = used.saturating_add(bytes::to_u64(bytes.len()));
            Ok((path, bytes))
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

/// The most bytes of a character that can follow a cut inside it: what `read_file` reads past its
/// `max_bytes`, so that [`encode`] can tell a cut character from bytes that only look like the
/// start of one.
pub(crate) const CHAR_TAIL: u64 = 3;

/// Bytes as `read_file` returns them ([`encode`]).
pub(crate) struct Encoded {
    /// How `content` encodes the bytes.
    pub(crate) encoding: ContentEncoding,
    /// The bytes, as text or in base64.
    pub(crate) content: String,
    /// How many bytes `content` holds: where the next page starts, from the first of them.
    pub(crate) len: usize,
}

/// How `read_file` returns the first `keep` bytes of `bytes`, which start where the read does
/// (the file's start, an offset or a line) and run up to [`CHAR_TAIL`] bytes past the cut when
/// the file does: as UTF-8 text when they are text, a cut inside a character moved back to the
/// character's start (at most 3 bytes) when the bytes after the cut complete that character;
/// otherwise exactly the `keep` bytes, in base64. Bytes that start inside a character are not
/// text.
pub(crate) fn encode(bytes: &[u8], keep: usize) -> Encoded {
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
        Some(text) => Encoded {
            encoding: ContentEncoding::Utf8,
            content: text.to_owned(),
            len: text.len(),
        },
        None => Encoded {
            encoding: ContentEncoding::Base64,
            content: STANDARD.encode(kept),
            len: keep,
        },
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
/// `key` in `details`. The rule is the repository's ([`swarmops_repo::check_metadata`]), which
/// every writer of changesets runs, `Repo::commit` among them.
pub(crate) fn check_metadata(metadata: &[(String, Vec<u8>)]) -> Result<(), OpError> {
    swarmops_repo::check_metadata(metadata).map_err(crate::error::op)
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
/// leaf's, so no put is a directory's. `E_INTERNAL` for a mode this build does not know.
pub(crate) fn leaf_kind(mode: Option<ModeInput>) -> Result<LeafKind, OpError> {
    match mode {
        None | Some(ModeInput::File) => Ok(LeafKind::File),
        Some(ModeInput::Exec) => Ok(LeafKind::Exec),
        Some(ModeInput::Symlink) => Ok(LeafKind::Symlink),
        Some(_) => Err(error::unknown_variant("ModeInput")),
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

/// `error`, said of a land's resolution of `path`: its message prefixed with it, and the `path`
/// in its `details`.
pub(crate) fn in_resolution(mut error: OpError, path: &str) -> OpError {
    error.message = format!("the resolution of {path:?}: {}", error.message);
    error.details.insert("path".to_owned(), Value::from(path));
    error
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// The error `result` holds, which must be `E_TOO_LARGE` for one content of `size` bytes over
    /// `limit` (scope `put`).
    #[track_caller]
    fn too_large(result: Result<Vec<u8>, OpError>, size: u64, limit: u64) -> OpError {
        let error = result.expect_err("over the limit");
        assert_eq!(error.code, ErrorCode::TooLarge.as_str(), "{error:?}");
        assert_eq!(error.details["size"], size, "{error:?}");
        assert_eq!(error.details["limit"], limit, "{error:?}");
        assert_eq!(error.details["scope"], "put", "{error:?}");
        error
    }

    /// The error `result` holds, which must be `E_TOO_LARGE` for contents of `size` bytes
    /// together over `limit` (scope `commit`), with the hint that takes contents out of the
    /// request.
    #[track_caller]
    fn too_large_together(result: Result<Vec<Change>, OpError>, size: u64, limit: u64) -> OpError {
        let error = result.expect_err("over the total");
        assert_eq!(error.code, ErrorCode::TooLarge.as_str(), "{error:?}");
        assert_eq!(error.details["size"], size, "{error:?}");
        assert_eq!(error.details["limit"], limit, "{error:?}");
        assert_eq!(error.details["scope"], "commit", "{error:?}");
        assert_eq!(error.hint.as_deref(), Some(BLOB_HINT), "{error:?}");
        error
    }

    /// A put of `content` at `path`.
    fn put(path: &str, content: ContentInput) -> ChangeInput {
        let mut change_input = ChangeInput::new(path.to_owned(), ChangeOp::Put);
        change_input.content = Some(content);
        change_input
    }

    fn inline(text: &str) -> ContentInput {
        ContentInput::Inline(text.to_owned())
    }

    fn file(path: &str) -> ContentInput {
        ContentInput::File(path.to_owned())
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

    /// A commit's contents hold 1 GiB together, two puts of the most one may hold; a request's
    /// text 64 MiB (Decision 11).
    #[test]
    fn a_commit_holds_1_gib_and_a_request_64_mib() {
        assert_eq!(MAX_COMMIT_CONTENT_BYTES, 1 << 30);
        assert_eq!(MAX_COMMIT_CONTENT_BYTES, 2 * MAX_CONTENT_BYTES);
        assert_eq!(MAX_REQUEST_BYTES, 64 << 20);
    }

    /// The contents' total is checked with the list's shape, by the sizes they declare, before
    /// any content is converted: inline puts of 3 and 2 bytes come to 5, past a total of 4, at
    /// the second change; base64 counts the bytes it decodes to; at the total the list is taken,
    /// and a blob named by id counts nothing.
    #[test]
    fn the_total_is_checked_with_the_lists_shape() {
        let limit = MAX_CONTENT_BYTES;
        let two = vec![put("a", inline("abc")), put("b", inline("de"))];
        let error = too_large_together(changes_within(two, limit, 4), 5, 4);
        assert_eq!(
            (&error.details["index"], &error.details["path"]),
            (&Value::from(1), &Value::from("b"))
        );
        assert!(error.message.starts_with("change 1 (\"b\"): "), "{error:?}");
        let encoded = vec![put("a", ContentInput::Base64(STANDARD.encode(b"abcde")))];
        too_large_together(changes_within(encoded, limit, 4), 5, 4);
        let at_the_total = vec![put("a", inline("ab")), put("b", inline("de"))];
        assert_eq!(changes_within(at_the_total, limit, 4).unwrap().len(), 2);
        let by_id = ContentInput::Blob("ab".repeat(32));
        let beside = vec![put("a", inline("abcd")), put("b", by_id)];
        assert_eq!(changes_within(beside, limit, 4).unwrap().len(), 2);
    }

    /// A client file is counted by its length, not read: one that does not exist counts 0 (its
    /// read would fail), so a list whose next change passes the total is refused for the total,
    /// at that change, not for the file. A file past the total is refused by its length, unread,
    /// and named.
    #[test]
    fn the_total_is_checked_before_any_file_is_read() {
        let missing = vec![
            put("a", file("/nonexistent/swarmops-test/x")),
            put("b", inline("abcde")),
        ];
        let error = too_large_together(changes_within(missing, MAX_CONTENT_BYTES, 4), 5, 4);
        assert_eq!(error.details["index"], 1, "{error:?}");
        assert!(error.details.get("file").is_none(), "{error:?}");

        let dir = tempfile::tempdir().unwrap();
        let five = dir.path().join("five.bin");
        fs::write(&five, b"fives").unwrap();
        let five = five.to_str().unwrap();
        let past = vec![put("a", inline("a")), put("f", file(five))];
        let error = too_large_together(changes_within(past, MAX_CONTENT_BYTES, 4), 6, 4);
        assert_eq!(
            (&error.details["index"], &error.details["file"]),
            (&Value::from(1), &Value::from(five))
        );
    }

    /// A put's own limit is checked with the list's shape too, by the size its content declares,
    /// before any content is converted: a list whose second content is past a put's limit is
    /// refused at that change, scope `put`, without reading the first change's file (which does
    /// not exist, so its read would fail first); a file past it is refused by its length,
    /// unread, and named.
    #[test]
    fn a_put_past_its_limit_is_refused_before_any_file_is_read() {
        let missing = || file("/nonexistent/swarmops-test/x");
        let inline_past = vec![put("a", missing()), put("b", inline("abcdef"))];
        let error = changes_within(inline_past, 5, 100).expect_err("over a put's limit");
        assert_eq!(
            (
                &error.details["size"],
                &error.details["limit"],
                &error.details["scope"],
                &error.details["index"],
            ),
            (
                &Value::from(6),
                &Value::from(5),
                &Value::from("put"),
                &Value::from(1)
            )
        );
        assert_eq!(error.code, ErrorCode::TooLarge.as_str(), "{error:?}");

        let dir = tempfile::tempdir().unwrap();
        let six = dir.path().join("six.bin");
        fs::write(&six, b"sixsix").unwrap();
        let six = six.to_str().unwrap();
        let file_past = vec![put("a", missing()), put("f", file(six))];
        let error = changes_within(file_past, 5, 100).expect_err("over a put's limit");
        assert_eq!(
            (&error.details["scope"], &error.details["file"]),
            (&Value::from("put"), &Value::from(six))
        );

        let resolutions = vec![
            (RepoPath::new("a").unwrap(), missing()),
            (RepoPath::new("b").unwrap(), inline("abcdef")),
        ];
        let error = resolutions_within(resolutions, 5, 100).expect_err("over a put's limit");
        assert_eq!(
            (&error.details["scope"], &error.details["path"]),
            (&Value::from("put"), &Value::from("b"))
        );
    }

    /// The conversion pass counts what it converted: after 2 inline bytes, a device that never
    /// ends is read no further than one byte past the 2 the total of 4 leaves it, under a put's
    /// limit of 3, and the refusal is the total's, its `size` the 2 with the 3 read. Were the
    /// earlier bytes not counted, it would read 4 and break the put's limit instead.
    #[test]
    fn the_conversion_pass_counts_the_contents_before() {
        let after = vec![put("a", inline("ab")), put("z", file("/dev/zero"))];
        let error = too_large_together(changes_within(after, 3, 4), 5, 4);
        assert_eq!(error.details["index"], 1, "{error:?}");
    }

    /// A client file is counted by its `stat`ed length, not by a read: a file of 5 bytes that
    /// cannot be opened is refused for a total of 4 by its length, named, rather than for its
    /// read. Needs a user that file modes bind (not root): the open is checked to fail.
    #[test]
    fn a_file_is_counted_by_its_length_unopened() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked.bin");
        fs::write(&locked, b"fives").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            File::open(&locked).is_err(),
            "a mode-000 file opens: run this test as a user that file modes bind, not root"
        );
        let locked = locked.to_str().unwrap();
        let list = vec![put("l", file(locked))];
        let error = too_large_together(changes_within(list, MAX_CONTENT_BYTES, 4), 5, 4);
        assert_eq!(
            (&error.details["index"], &error.details["file"]),
            (&Value::from(0), &Value::from(locked))
        );
    }

    /// A file whose length says nothing (a device that never ends, which declares 0) meets the
    /// growth guard: it is read no further than one byte past what the total leaves it, alone or
    /// after contents converted before it; where a put's own limit is the smaller, that limit
    /// binds, and the refusal is scope `put`.
    #[test]
    fn a_file_that_outgrows_its_length_meets_the_total() {
        let alone = vec![put("z", file("/dev/zero"))];
        let error = too_large_together(changes_within(alone, MAX_CONTENT_BYTES, 4), 5, 4);
        assert_eq!(
            (&error.details["index"], &error.details["file"]),
            (&Value::from(0), &Value::from("/dev/zero"))
        );
        let after = vec![put("a", inline("abc")), put("z", file("/dev/zero"))];
        let error = too_large_together(changes_within(after, MAX_CONTENT_BYTES, 4), 5, 4);
        assert_eq!(error.details["index"], 1, "{error:?}");
        let put_binds = vec![put("z", file("/dev/zero"))];
        let error = changes_within(put_binds, 4, 100).expect_err("over a put's limit");
        assert_eq!(
            (
                &error.details["size"],
                &error.details["limit"],
                &error.details["scope"]
            ),
            (&Value::from(5), &Value::from(4), &Value::from("put"))
        );
    }

    /// A put's content and an object of the store hold at most as much as each other: the
    /// content limit is the store's object limit, so that no content the check lets through is
    /// refused by the store.
    #[test]
    fn the_object_limit_is_the_content_limit() {
        assert_eq!(MAX_CONTENT_BYTES, swarmops_store::MAX_OBJECT_LEN);
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
