//! The commit a native changeset exports to (storage spec §7; M8 Decision 2), as version 1 of
//! `git_oid` renders it. A changeset no git commit was imported into (it has no [`GIT_OID`]) is
//! rendered with its own signatures verbatim (the agent's name and
//! `<instance>@<family>.swarmops.local` at offset 0, as SwarmOps' operations sign a commit; a
//! land's author is its head's, its committer the lander), its extra headers (none, as SwarmOps
//! writes a changeset) and its message followed by SwarmOps' trailers ([`export_message`]). An
//! imported changeset keeps its message, its headers and its signatures as git had them.
//! [`render_commit`] gives a changeset's commit bytes, which [`ExportOid`](crate::ExportOid)
//! hashes.
//!
//! The rendering is fixed as the hash contract is: a change to it (the trailers, their order, how
//! they join the message) is a new version of `git_oid`, which every branch exported before it
//! meets as divergence.

use swarmops_core::{Changeset, ChangesetId};

use crate::render::commit_bytes;
use crate::{GIT_OID, GitOid};

/// The metadata keys whose values a native changeset's commit carries as trailers, each with its
/// trailer's token, in the order the trailers follow `SwarmOps-Changeset`: the land's key (M4),
/// the change request (M7) and the changesets the land squashed (M4: hex ids, oldest first). No
/// other key is exported (`agent`, `run_id`, `lander`, `lander_run_id`, `commit_key`): the
/// signatures name the agents.
pub const TRAILERS: [(&str, &str); 3] = [
    ("land_key", "SwarmOps-Land-Key"),
    ("cr", "SwarmOps-CR"),
    ("squashed_from", "SwarmOps-Squashed-From"),
];

/// The token of the trailer every native changeset's commit carries first: its changeset's id.
const CHANGESET: &str = "SwarmOps-Changeset";

/// The message of the commit changeset `id` (`cs`) exports to. An imported changeset's (one with
/// [`GIT_OID`]) is its message, byte for byte. A native changeset's is its message followed by a
/// trailer block: `SwarmOps-Changeset: <id>` (64 hex digits), then `<token>: <value>` for each key
/// of [`TRAILERS`] its metadata holds, in that order, a line break in a value written as git
/// writes a continuation line (the line break, then one space).
///
/// The block follows the message as git reads trailers. A message that does not end in a line
/// break gains one. The block then joins the message's last paragraph, the lines after its last
/// blank line, when that paragraph is not its first (a blank line comes before it) and holds only
/// lines `<token>: <value>` with a token of `[A-Za-z0-9][A-Za-z0-9-]*`, so that a
/// `Co-Authored-By:` and SwarmOps' trailers are one block, as GitHub reads it; otherwise one blank
/// line comes first. An empty message is the block alone.
pub fn export_message(id: ChangesetId, cs: &Changeset) -> Vec<u8> {
    if metadata(cs, GIT_OID).is_some() {
        return cs.message.clone();
    }
    let mut message = cs.message.clone();
    if !message.is_empty() {
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        if !ends_in_trailers(&message) {
            message.push(b'\n');
        }
    }
    trailer(&mut message, CHANGESET, id.to_string().as_bytes());
    for (key, token) in TRAILERS {
        if let Some(value) = metadata(cs, key) {
            trailer(&mut message, token, value);
        }
    }
    message
}

/// The bytes of the git commit changeset `id` (`cs`) exports to, without git's `commit <len>\0`
/// header ([`commit_bytes`]): tree `tree`, a parent line for each of `parents` in their order,
/// the changeset's author, committer and extra headers verbatim, and its [`export_message`].
/// `tree` is the git oid of the changeset's root ([`tree_oid`](crate::tree_oid)), and `parents`
/// are its parents' export oids ([`ExportOid`](crate::ExportOid)).
pub fn render_commit(id: ChangesetId, cs: &Changeset, parents: &[GitOid], tree: GitOid) -> Vec<u8> {
    commit_bytes(
        tree,
        parents,
        &cs.author,
        &cs.committer,
        &cs.extra_headers,
        &export_message(id, cs),
    )
}

/// The value of `cs`'s metadata key `key`, if it has one.
pub(crate) fn metadata<'c>(cs: &'c Changeset, key: &str) -> Option<&'c [u8]> {
    cs.metadata
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_slice())
}

/// Appends the trailer `<token>: <value>` and a line break to `message`, each line break in
/// `value` followed by one space, as git continues a trailer's value on the next line.
fn trailer(message: &mut Vec<u8>, token: &str, value: &[u8]) {
    message.extend_from_slice(token.as_bytes());
    message.extend_from_slice(b": ");
    for &byte in value {
        message.push(byte);
        if byte == b'\n' {
            message.push(b' ');
        }
    }
    message.push(b'\n');
}

/// Whether `message`, which ends in a line break, ends in a trailer block: its last paragraph,
/// the lines after its last blank line, is not its first and holds only lines `<token>: <value>`.
fn ends_in_trailers(message: &[u8]) -> bool {
    let body = message.strip_suffix(b"\n").unwrap_or(message);
    let lines: Vec<&[u8]> = body.split(|&byte| byte == b'\n').collect();
    let Some(blank) = lines.iter().rposition(|line| line.is_empty()) else {
        // One paragraph: the title, which is never a trailer block.
        return false;
    };
    let paragraph = &lines[blank + 1..];
    !paragraph.is_empty() && paragraph.iter().all(|line| is_trailer(line))
}

/// Whether `line` is `<token>: <value>`, with a token of `[A-Za-z0-9][A-Za-z0-9-]*`.
fn is_trailer(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|&byte| byte == b':') else {
        return false;
    };
    let (token, rest) = line.split_at(colon);
    token.first().is_some_and(u8::is_ascii_alphanumeric)
        && token
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && rest.starts_with(b": ")
}
