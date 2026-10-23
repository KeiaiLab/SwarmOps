//! The git objects a changeset renders to: each tree's bytes in git's order and with git's modes,
//! and the commit's bytes, as git writes them (storage spec §7, "export 의 결정성" (2)). The
//! conversion's fidelity check hashes them ([`convert`](crate::convert)), and M8's export writes
//! them.

use std::cmp::Ordering;

use swarmops_core::{EntryTarget, LeafKind, Signature, Tree, TreeEntry};

use crate::{GitOid, RawCommit, RawIdent};

/// The bytes of the git tree `tree` renders to, without git's `tree <len>\0` header: its entries
/// in git's order (names compared as bytes, a directory's as if it ended in `/`), each
/// `mode SP name NUL` and the 20 bytes of the id `child` gives it, with the modes git writes:
/// `100644` a file, `100755` an executable, `120000` a symlink, `40000` a directory.
///
/// `child` gives the git id of an entry's object: a leaf's blob, whose bytes git hashes as they
/// are, and a subdirectory's own rendering.
///
/// `None` if an entry is of a kind newer than this build, which it has no git mode for.
pub fn tree_bytes(tree: &Tree, child: impl Fn(&TreeEntry) -> GitOid) -> Option<Vec<u8>> {
    let mut entries: Vec<&TreeEntry> = tree.entries().iter().collect();
    // A tree's names are unique, so no two of its entries compare equal.
    entries.sort_unstable_by(|a, b| {
        git_order(
            (a.name.as_bytes(), is_dir(a.target)),
            (b.name.as_bytes(), is_dir(b.target)),
        )
    });
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(git_mode(entry.target)?);
        out.push(b' ');
        out.extend_from_slice(entry.name.as_bytes());
        out.push(0);
        out.extend_from_slice(&child(entry).0);
    }
    Some(out)
}

/// The bytes of the git commit a changeset renders to, without git's `commit <len>\0` header:
/// `tree`, a `parent` line for each of `parents` in their order, `author`, `committer`, each
/// of `extra_headers` (the lines of a value after its first as continuation lines, each led by
/// one space), a blank line and `message`, as git writes them ([`RawCommit::render`]).
pub fn commit_bytes(
    tree: GitOid,
    parents: &[GitOid],
    author: &Signature,
    committer: &Signature,
    extra_headers: &[(Vec<u8>, Vec<u8>)],
    message: &[u8],
) -> Vec<u8> {
    RawCommit {
        tree,
        parents: parents.to_vec(),
        author: ident(author),
        committer: ident(committer),
        extra_headers: extra_headers.to_vec(),
        message: message.to_vec(),
    }
    .render()
}

/// git's order of two entries of one tree, each its name and whether it is a directory: the
/// names compared as bytes, a directory's as if it ended in `/` (git's `base_name_compare`), so
/// that the file `a.b` comes before the directory `a`, whose name is a prefix of it.
pub(crate) fn git_order(a: (&[u8], bool), b: (&[u8], bool)) -> Ordering {
    sort_key(a).cmp(sort_key(b))
}

/// The bytes git's order compares for an entry: its name's, and a `/` for a directory.
fn sort_key((name, dir): (&[u8], bool)) -> impl Iterator<Item = u8> + '_ {
    name.iter().copied().chain(dir.then_some(b'/'))
}

fn is_dir(target: EntryTarget) -> bool {
    matches!(target, EntryTarget::Dir(_))
}

/// The mode git writes for an entry of `target`'s kind; `None` for a kind newer than this build.
fn git_mode(target: EntryTarget) -> Option<&'static [u8]> {
    match target {
        EntryTarget::Leaf(LeafKind::File, _) => Some(b"100644"),
        EntryTarget::Leaf(LeafKind::Exec, _) => Some(b"100755"),
        EntryTarget::Leaf(LeafKind::Symlink, _) => Some(b"120000"),
        EntryTarget::Dir(_) => Some(b"40000"),
        _ => None,
    }
}

/// `signature` as a git ident: the same fields.
fn ident(signature: &Signature) -> RawIdent {
    RawIdent {
        name: signature.name.clone(),
        email: signature.email.clone(),
        secs: signature.secs,
        offset_secs: signature.offset_secs,
        negative_zero: signature.negative_zero,
    }
}

#[cfg(test)]
mod tests {
    use swarmops_core::{BlobId, ObjectId, TreeId};

    use super::*;
    use crate::{RawKind, RawTree, git_hash};

    #[test]
    fn git_order_puts_a_directory_after_the_names_it_prefixes() {
        let file = |name: &'static str| (name.as_bytes(), false);
        let dir = |name: &'static str| (name.as_bytes(), true);
        // `.` (0x2e) and `-` (0x2d) come before the `/` (0x2f) a directory's name ends in.
        assert_eq!(git_order(file("a.b"), dir("a")), Ordering::Less);
        assert_eq!(git_order(file("a-"), dir("a")), Ordering::Less);
        assert_eq!(git_order(dir("a"), file("a0")), Ordering::Less);
        // A file whose name the other's starts with comes first, as in byte order.
        assert_eq!(git_order(file("a"), file("a.b")), Ordering::Less);
        assert_eq!(git_order(file("a"), dir("a")), Ordering::Less);
        assert_eq!(git_order(dir("a"), dir("a")), Ordering::Equal);
    }

    #[test]
    fn a_tree_renders_in_git_order_with_git_modes() {
        let blob = BlobId(ObjectId([1; 32]));
        let tree = Tree::new(vec![
            TreeEntry::dir("a", TreeId(ObjectId([2; 32]))),
            TreeEntry::leaf("a.b", LeafKind::File, blob),
            TreeEntry::leaf("link", LeafKind::Symlink, blob),
            TreeEntry::leaf("run", LeafKind::Exec, blob),
        ])
        .unwrap();
        let child = |entry: &TreeEntry| GitOid([entry.name.as_bytes()[0]; 20]);
        let rendered = RawTree::parse(&tree_bytes(&tree, child)).unwrap();
        let entries: Vec<(&[u8], &[u8])> = (rendered.entries.iter())
            .map(|entry| (entry.mode.as_slice(), entry.name.as_slice()))
            .collect();
        assert_eq!(
            entries,
            [
                (&b"100644"[..], &b"a.b"[..]),
                (b"40000", b"a"),
                (b"120000", b"link"),
                (b"100755", b"run"),
            ]
        );
        assert_eq!(rendered.entries[1].oid, GitOid([b'a'; 20]));
        // The empty root renders to git's empty tree.
        assert_eq!(
            git_hash(RawKind::Tree, &tree_bytes(&Tree::empty_root(), child)).to_string(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }
}
