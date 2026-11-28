//! The commit a native changeset exports to (storage spec §7; M8 Decision 2), as version 1 of
//! `git_oid` renders it. A changeset no git commit was imported into (it has no [`GIT_OID`]) is
//! rendered with its own signatures verbatim (the agent's name and
//! `<instance>@<family>.swarmops.local` at offset 0, as SwarmOps' operations sign a commit; a
//! land's author is its head's, its committer the lander), its extra headers (none, as SwarmOps
//! writes a changeset) and its message followed by SwarmOps' trailers ([`export_message`]). An
//! imported changeset keeps its message, its headers and its signatures as git had them.
//! [`render_commit`] gives a changeset's commit bytes, which [`ExportOid`](crate::ExportOid)
//! hashes. [`render_tag`] gives the bytes of a tag SwarmOps makes, signed as its commits are.
//!
//! The rendering is fixed as the hash contract is: a change to it (the trailers, their order, how
//! they join the message) is a new version of `git_oid`, which every branch exported before it
//! meets as divergence.
//!
//! A signed land (M42) takes no rule of its own here. Its changeset is made as the import of the
//! commit that was signed ([`crate::sign`]): it holds [`GIT_OID`], the signature as its `gpgsig`
//! header and the commit's message, so it is rendered as an imported changeset is, byte for
//! byte. [`sealed_message`] gives the message that commit is signed with: the land's trailers
//! without `SwarmOps-Changeset`, which a commit whose changeset's id hashes its signature cannot
//! name.

use swarmops_core::{Changeset, ChangesetId};

use crate::render;
use crate::{GIT_OID, GitOid, RawIdent};

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

/// The message of the commit a signed land is signed with (M42): `cs` is the changeset the land
/// makes before it is sealed, a native one, and the sealed changeset holds this message
/// verbatim. It is `cs`'s message followed by a trailer block as [`export_message`] writes one,
/// joined to the message the same way, but for two things.
///
/// - The block has no `SwarmOps-Changeset` line: the sealed changeset's id hashes the signature,
///   and the signature covers the message. It holds `<token>: <value>` for each key of
///   [`TRAILERS`] the metadata holds, in that order, then each trailer of `extra` (its token and
///   its value) in the order given. With none of them the message is `cs`'s, as it is.
/// - A trailer of one of those tokens (all three of [`TRAILERS`], and those of `extra`) that the
///   message's last paragraph holds already is removed first, with its value's continuation
///   lines, and the paragraph with its blank line when nothing of it is left. A land's message
///   is its head's unless the request gives one, so the message of a signed land that lands
///   again (onto another target, by a relocation, as a queue's candidate) ends in an older
///   block, of another key and chain: the commit carries one block, the new one. The paragraph
///   counts when it is not the message's first, starts with a line `<token>: <value>` and holds
///   only such lines and continuation lines (a line that starts with a space).
pub fn sealed_message(cs: &Changeset, extra: &[(&str, &[u8])]) -> Vec<u8> {
    // 1. The block: the metadata's trailers, then the extra ones.
    let mut block = Vec::new();
    for (key, token) in TRAILERS {
        if let Some(value) = metadata(cs, key) {
            trailer(&mut block, token, value);
        }
    }
    for &(token, value) in extra {
        trailer(&mut block, token, value);
    }

    // 2. The message, without the lines an older block of these tokens left.
    let own = |token: &[u8]| {
        let written = TRAILERS.iter().map(|&(_, written)| written);
        let given = extra.iter().map(|&(given, _)| given);
        written.chain(given).any(|ours| ours.as_bytes() == token)
    };
    let mut message = without_trailers(&cs.message, own);
    if block.is_empty() {
        return message;
    }

    // 3. Joined as `export_message` joins its block.
    if !message.is_empty() {
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        if !ends_in_trailers(&message) {
            message.push(b'\n');
        }
    }
    message.extend_from_slice(&block);
    message
}

/// The bytes of the git commit changeset `id` (`cs`) exports to, without git's `commit <len>\0`
/// header ([`commit_bytes`]): tree `tree`, a parent line for each of `parents` in their order,
/// the changeset's author, committer and extra headers verbatim, and its [`export_message`].
/// `tree` is the git oid of the changeset's root ([`tree_oid`](crate::tree_oid)), and `parents`
/// are its parents' export oids ([`ExportOid`](crate::ExportOid)).
///
/// [`commit_bytes`]: render::commit_bytes
pub fn render_commit(id: ChangesetId, cs: &Changeset, parents: &[GitOid], tree: GitOid) -> Vec<u8> {
    render::commit_bytes(
        tree,
        parents,
        &cs.author,
        &cs.committer,
        &cs.extra_headers,
        &export_message(id, cs),
    )
}

/// The bytes of the annotated tag `name` of the commit `target`, without git's `tag <len>\0`
/// header, as `git mktag` takes them: `object <target>`, `type commit`, `tag <name>`,
/// `tagger <tagger>`, a blank line, and `message`, which gains a line break if it lacks one.
///
/// ```text
/// object 1111111111111111111111111111111111111111
/// type commit
/// tag v1
/// tagger agent <inst@fam.swarmops.local> 1700000000 +0000
///
/// release
/// ```
pub fn render_tag(name: &str, target: GitOid, tagger: &RawIdent, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + name.len() + message.len());
    out.extend_from_slice(format!("object {target}\ntype commit\ntag {name}\ntagger ").as_bytes());
    out.extend_from_slice(&tagger.render());
    out.extend_from_slice(b"\n\n");
    out.extend_from_slice(message);
    if !message.is_empty() && !message.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
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

/// `message` without the trailers of its last paragraph whose token `own` takes, each with the
/// continuation lines of its value, as [`sealed_message`] says: the message as it is when its
/// last paragraph is no trailer block or holds no such trailer; otherwise the lines left, each
/// ended by a line break, the paragraph's blank line gone with it when no line of it is left.
fn without_trailers(message: &[u8], own: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let body = message.strip_suffix(b"\n").unwrap_or(message);
    let lines: Vec<&[u8]> = body.split(|&byte| byte == b'\n').collect();
    let Some(blank) = lines.iter().rposition(|line| line.is_empty()) else {
        // One paragraph: the title, which is never a trailer block.
        return message.to_vec();
    };

    // 1. The last paragraph must be a trailer block, continuation lines among its lines.
    let paragraph = &lines[blank + 1..];
    let continues = |line: &[u8]| line.starts_with(b" ");
    let block = paragraph.first().is_some_and(|line| is_trailer(line))
        && (paragraph.iter()).all(|&line| is_trailer(line) || continues(line));
    if !block {
        return message.to_vec();
    }

    // 2. Its lines but those trailers: a continuation line goes or stays with the trailer whose
    // value it continues.
    let mut kept: Vec<&[u8]> = Vec::with_capacity(paragraph.len());
    let mut dropped = false;
    for &line in paragraph {
        if !continues(line) {
            let colon = line.iter().position(|&byte| byte == b':');
            dropped = own(&line[..colon.unwrap_or(line.len())]);
        }
        if !dropped {
            kept.push(line);
        }
    }
    if kept.len() == paragraph.len() {
        return message.to_vec();
    }

    // 3. What is before the paragraph, with its blank line while a line of it is left.
    let before = if kept.is_empty() {
        &lines[..blank]
    } else {
        &lines[..=blank]
    };
    let mut out = Vec::with_capacity(message.len());
    for line in before.iter().chain(&kept) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use swarmops_core::{Signature, Tree};

    use super::*;

    /// The head a land squashes, as its `squashed_from` names it.
    const HEAD: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    /// A changeset no store holds, with `message` and `metadata` (its keys ascending).
    fn unstored(message: &str, metadata: &[(&str, &str)]) -> Changeset {
        let signature = Signature::new(b"a".to_vec(), b"a@b".to_vec(), 1_700_000_000, 0, false);
        let signature = signature.expect("a zero offset is a valid signature");
        Changeset::new(
            Vec::new(),
            Tree::empty_root().id(),
            Vec::new(),
            signature.clone(),
            signature,
            message.as_bytes().to_vec(),
            Vec::new(),
            (metadata.iter())
                .map(|&(key, value)| (key.to_owned(), value.as_bytes().to_vec()))
                .collect(),
        )
        .expect("a canonical changeset")
    }

    /// [`sealed_message`] of a changeset with `message` and `metadata`, as text.
    fn sealed(message: &str, metadata: &[(&str, &str)]) -> String {
        let cs = unstored(message, metadata);
        String::from_utf8(sealed_message(&cs, &[])).expect("the messages here are UTF-8")
    }

    #[test]
    fn a_sealed_message_has_the_land_s_trailers_and_no_changeset_line() {
        // 1. A land's metadata: the three keys of `TRAILERS` in their order, and no other key.
        let land = [
            ("agent", "agent/worker-17"),
            ("cr", "cr-7"),
            ("land_key", "k-1"),
            ("lander", "swarmops/lander"),
            ("squashed_from", HEAD),
        ];
        let block =
            format!("SwarmOps-Land-Key: k-1\nSwarmOps-CR: cr-7\nSwarmOps-Squashed-From: {HEAD}\n");
        assert_eq!(sealed("Add a\n", &land), format!("Add a\n\n{block}"));
        assert!(!sealed("Add a\n", &land).contains(CHANGESET), "its own id");

        // 2. The block joins the message as the export joins a native changeset's: after one
        // blank line, a message without a final line break gaining one; an empty message is the
        // block alone.
        assert_eq!(sealed("Add a", &land), format!("Add a\n\n{block}"));
        let why = "Add a\n\nWhy.\n";
        assert_eq!(sealed(why, &land), format!("{why}\n{block}"));
        assert_eq!(sealed("", &land), block);

        // 3. The same bytes as the export's message for these keys, less its first trailer.
        let cs = unstored(why, &land);
        let id = cs.id().expect("a canonical changeset");
        let exported = String::from_utf8(export_message(id, &cs)).expect("UTF-8");
        let own = format!("{CHANGESET}: {id}\n");
        assert_eq!(exported.replacen(&own, "", 1), sealed(why, &land));

        // 4. A line break in a value continues on the next line after one space.
        let broken = [("land_key", "k\n1")];
        let expected = "Add a\n\nSwarmOps-Land-Key: k\n 1\n";
        assert_eq!(sealed("Add a\n", &broken), expected);

        // 5. Metadata with none of the keys gives no block, and the message stays as it is.
        let none = [("agent", "agent/worker-17"), ("lander", "swarmops/lander")];
        assert_eq!(sealed("Add a", &none), "Add a");
        assert_eq!(sealed("Add a\n", &none), "Add a\n");

        // 6. Extra trailers follow the three, in the order given.
        let cs = unstored("Add a\n", &[("land_key", "k-1")]);
        let extra: [(&str, &[u8]); 2] = [("SwarmOps-Lander", b"claude/1"), ("X-Note", b"n")];
        let expected = "Add a\n\nSwarmOps-Land-Key: k-1\nSwarmOps-Lander: claude/1\nX-Note: n\n";
        assert_eq!(sealed_message(&cs, &extra), expected.as_bytes());
    }

    #[test]
    fn a_land_of_a_signed_land_carries_one_block() {
        let land = [("land_key", "k-2"), ("squashed_from", HEAD)];
        let block = format!("SwarmOps-Land-Key: k-2\nSwarmOps-Squashed-From: {HEAD}\n");

        // 1. The head's message is a signed land's: it ends in the block of another key and
        // another chain. The new land's block replaces it.
        let old = "c".repeat(64);
        let head = format!("Add a\n\nSwarmOps-Land-Key: k-1\nSwarmOps-Squashed-From: {old}\n");
        let message = sealed(&head, &land);
        assert_eq!(message, format!("Add a\n\n{block}"));
        assert_eq!(message.matches("SwarmOps-Land-Key").count(), 1);
        assert_eq!(message.matches("SwarmOps-Squashed-From").count(), 1);

        // 2. Sealed again with the same metadata, it is the same message.
        assert_eq!(sealed(&message, &land), message);

        // 3. A line this land does not write again is removed all the same (an unkeyed land of
        // a keyed one), with its value's continuation lines.
        let head = "Add a\n\nSwarmOps-Land-Key: k\n 1\nSwarmOps-CR: cr-7\n";
        let unkeyed = format!("Add a\n\nSwarmOps-Squashed-From: {HEAD}\n");
        assert_eq!(sealed(head, &[("squashed_from", HEAD)]), unkeyed);

        // 4. A token of the extra trailers is removed too, and written once.
        let cs = unstored("Add a\n\nSwarmOps-Lander: claude/1\n", &[]);
        let extra: [(&str, &[u8]); 1] = [("SwarmOps-Lander", b"claude/2")];
        let expected = "Add a\n\nSwarmOps-Lander: claude/2\n";
        assert_eq!(sealed_message(&cs, &extra), expected.as_bytes());

        // 5. Only the last paragraph is read as a block: a line of the same shape above it, in
        // a last paragraph that holds other text, or as the title, stays.
        let above = "Add a\n\nSwarmOps-Land-Key: quoted\n\nWhy.\n";
        assert_eq!(sealed(above, &land), format!("{above}\n{block}"));
        let prose = "Add a\n\nSwarmOps-Land-Key: quoted\nand more\n";
        assert_eq!(sealed(prose, &land), format!("{prose}\n{block}"));
        let title = "SwarmOps-Land-Key: a title\n";
        assert_eq!(sealed(title, &land), format!("{title}\n{block}"));

        // 6. An imported commit's `SwarmOps-Changeset` line names another changeset, and stays.
        let named = format!("Add a\n\nSwarmOps-Changeset: {old}\n");
        let imported = format!("{named}SwarmOps-Land-Key: k-1\n");
        assert_eq!(sealed(&imported, &land), format!("{named}{block}"));
    }

    #[test]
    fn a_co_authored_by_line_is_kept_in_the_block() {
        let land = [("land_key", "k-2"), ("squashed_from", HEAD)];
        let block = format!("SwarmOps-Land-Key: k-2\nSwarmOps-Squashed-From: {HEAD}\n");

        // 1. A person's trailer and the land's are one block, as the export joins them.
        let message = "Add a\n\nCo-Authored-By: X <x@y>\n";
        assert_eq!(sealed(message, &land), format!("{message}{block}"));
        let unbroken = "Add a\n\nCo-Authored-By: X <x@y>";
        assert_eq!(sealed(unbroken, &land), format!("{message}{block}"));

        // 2. Beside an older land's lines it stays, in its place, and they go.
        let old = "c".repeat(64);
        let head = format!(
            "Add a\n\nSigned-off-by: A <a@b>\nSwarmOps-Land-Key: k-1\nCo-Authored-By: X <x@y>\n\
             SwarmOps-Squashed-From: {old}\n"
        );
        let kept = "Add a\n\nSigned-off-by: A <a@b>\nCo-Authored-By: X <x@y>\n";
        assert_eq!(sealed(&head, &land), format!("{kept}{block}"));

        // 3. Its own continuation line stays with it; the paragraph is then no block as the
        // export reads one, so the land's block follows a blank line.
        let wrapped = "Add a\n\nCo-Authored-By: X\n <x@y>\nSwarmOps-Land-Key: k-1\n";
        let kept = "Add a\n\nCo-Authored-By: X\n <x@y>\n";
        assert_eq!(sealed(wrapped, &land), format!("{kept}\n{block}"));
    }
}
