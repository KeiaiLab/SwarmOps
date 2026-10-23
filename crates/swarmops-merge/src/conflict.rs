//! The conflict types of the three-way merge (storage spec §4). A merge returns its conflicts as
//! data, never as markers written into a file: [`ContentMerge`] is what one file's content merges
//! to, [`ConflictHunk`] a place where the two sides' edits of its lines conflict, and
//! [`ConflictKind`] why a path conflicts, under the name the operations spec's conflict report
//! gives it (operations spec §3). A [`ContentMerger`] is the content merge a tree merge calls.
//!
//! `ours` is the change being merged, `theirs` what it is merged with (the target, in a land), and
//! `base` their merge base.

use std::ops::Range;

use swarmops_core::LeafKind;

/// A place in a text file where both sides edited its lines, at the same place or next to each
/// other, and made different changes: the base's lines `base` became the lines `ours` in ours and
/// the lines `theirs` in theirs. Each range is 0-based and half-open over its own side's lines,
/// as a [`Hunk`](crate::line_diff::Hunk)'s are; an empty one is the position where that side has
/// no lines. The texts are those lines' bytes as they are: a line keeps its `\n`
/// ([`split_lines`](crate::line_diff::split_lines)).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ConflictHunk {
    /// The base's lines here.
    pub base: Range<u32>,
    /// Ours' lines in their place.
    pub ours: Range<u32>,
    /// Theirs' lines in their place.
    pub theirs: Range<u32>,
    /// The bytes of the base's lines `base`.
    pub base_text: Vec<u8>,
    /// The bytes of ours' lines `ours`.
    pub ours_text: Vec<u8>,
    /// The bytes of theirs' lines `theirs`.
    pub theirs_text: Vec<u8>,
}

impl ConflictHunk {
    /// The hunk with these ranges and texts, as they are.
    ///
    /// Invariant: each text is the bytes of its side's lines in its range. No validation exists:
    /// `new` checks nothing, and the caller keeps the invariant.
    pub fn new(
        base: Range<u32>,
        ours: Range<u32>,
        theirs: Range<u32>,
        base_text: Vec<u8>,
        ours_text: Vec<u8>,
        theirs_text: Vec<u8>,
    ) -> Self {
        Self {
            base,
            ours,
            theirs,
            base_text,
            ours_text,
            theirs_text,
        }
    }
}

/// What one file's content merges to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContentMerge {
    /// The merged content.
    Clean(Vec<u8>),
    /// The places where the two sides' edits conflict, in file order: at least one.
    Conflicted(Vec<ConflictHunk>),
    /// Both sides changed the content, differently, and the base's, ours' or theirs' content is
    /// not text ([`is_text`](crate::line_diff::is_text)) or is past the line diff's bound
    /// ([`MAX_DIFF_LINES`](crate::line_diff::MAX_DIFF_LINES) lines,
    /// [`MAX_DIFF_BYTES`](crate::line_diff::MAX_DIFF_BYTES) bytes): it is not merged by lines.
    Binary,
}

/// Why a path conflicts. Its [`name`](ConflictKind::name) is the one the operations spec's
/// conflict report gives it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConflictKind {
    /// Both sides edited the same lines of a text file, or lines next to each other.
    Content {
        /// Where, in file order.
        hunks: Vec<ConflictHunk>,
    },
    /// Both sides changed the content of a file that is not text, or that is past the line
    /// diff's bound, or of a symlink, differently.
    Binary,
    /// Ours deleted the file, and theirs changed it.
    DeleteModify,
    /// Ours changed the file, and theirs deleted it.
    ModifyDelete,
    /// Both sides added the file, where the base has none, with contents that do not merge.
    AddAdd {
        /// Where the two texts conflict, merged over an empty base, in file order.
        hunks: Vec<ConflictHunk>,
    },
    /// Both sides changed the file's kind (a file, an executable or a symlink), differently.
    Mode {
        /// The kind ours gave it.
        ours: LeafKind,
        /// The kind theirs gave it.
        theirs: LeafKind,
    },
    /// One side has a file where the other has a directory.
    TypeChange,
}

impl ConflictKind {
    /// The name the operations spec's conflict report gives this kind (a file's `kind`, and an
    /// entry of the report's `kinds`): `content`, `binary`, `delete_modify`, `modify_delete`,
    /// `add_add`, `mode` or `type_change`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Content { .. } => "content",
            Self::Binary => "binary",
            Self::DeleteModify => "delete_modify",
            Self::ModifyDelete => "modify_delete",
            Self::AddAdd { .. } => "add_add",
            Self::Mode { .. } => "mode",
            Self::TypeChange => "type_change",
        }
    }
}

/// A three-way merge of one file's content, as a tree merge takes it: called with the base's,
/// ours' and theirs' contents, in that order, it returns what they merge to.
pub type ContentMerger<'a> = &'a dyn Fn(&[u8], &[u8], &[u8]) -> ContentMerge;

#[cfg(test)]
mod tests {
    use swarmops_core::LeafKind;

    use super::ConflictKind;

    /// One kind of each has its name, in operations spec §3's order. The `match` has no
    /// wildcard, which only this crate may omit (`ConflictKind` is `#[non_exhaustive]`): a kind
    /// added fails to compile here until it is listed, here and in `tests/conflict.rs`.
    #[test]
    fn one_of_each_kind_is_named() {
        let kinds = [
            ConflictKind::Content { hunks: Vec::new() },
            ConflictKind::Binary,
            ConflictKind::DeleteModify,
            ConflictKind::ModifyDelete,
            ConflictKind::AddAdd { hunks: Vec::new() },
            ConflictKind::Mode {
                ours: LeafKind::Exec,
                theirs: LeafKind::Symlink,
            },
            ConflictKind::TypeChange,
        ];

        // 1. Every kind is listed: no wildcard.
        for kind in &kinds {
            match kind {
                ConflictKind::Content { .. }
                | ConflictKind::Binary
                | ConflictKind::DeleteModify
                | ConflictKind::ModifyDelete
                | ConflictKind::AddAdd { .. }
                | ConflictKind::Mode { .. }
                | ConflictKind::TypeChange => {}
            }
        }

        // 2. The names, in the spec's order.
        let names: Vec<&str> = kinds.iter().map(ConflictKind::name).collect();
        assert_eq!(
            names,
            [
                "content",
                "binary",
                "delete_modify",
                "modify_delete",
                "add_add",
                "mode",
                "type_change",
            ]
        );
    }
}
