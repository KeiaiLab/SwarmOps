//! The conflict report (operations spec §3): what `E_CONFLICT` carries in `details.report` when
//! a land's change conflicts with its target, made from the conflicts the repository gives
//! ([`MergeConflicts`]).
//!
//! Each conflicting file, in path order, comes with its kind, the blob each side holds at its path
//! (none for a side without a file there), and the newest changeset on the target since the base
//! that touched the path, with its agent: attribution is per file. A text file both sides edited
//! has its hunks, each side's lines as `{first, last}`, numbered from 1 with both ends included,
//! as `read_file` takes `lines` (a side without lines there has `last` one less than `first`: its
//! place is before line `first`); the merge keeps them 0-based and half-open. The hunks' texts go
//! in, a hunk's three sides together and in the report's order, while their sum stays within the
//! request's `max_conflict_bytes`: from the first hunk whose texts would pass it, no hunk carries
//! its texts, and `truncated` says so. The positions and the blob ids are always there. The paths
//! whose resolutions did not apply, since the target holds them otherwise than their `against`,
//! are `stale`.

use std::ops::Range;

use swarmops_api::{ConflictKindOut, ConflictReport, FileConflictOut, HunkOut, LinesOut};
use swarmops_core::bytes::to_u64;
use swarmops_repo::MergeConflicts;
use swarmops_repo::merge::conflict::{ConflictHunk, ConflictKind};
use swarmops_repo::merge::tree_diff::Leaf;
use swarmops_repo::merge::tree_merge::FileConflict;

use crate::local::UNKNOWN_AGENT;

/// The kinds of conflict, in the order the report's `kinds` lists them: the operations spec's
/// (§3).
const KINDS: [ConflictKindOut; 7] = [
    ConflictKindOut::Content,
    ConflictKindOut::Binary,
    ConflictKindOut::DeleteModify,
    ConflictKindOut::ModifyDelete,
    ConflictKindOut::AddAdd,
    ConflictKindOut::Mode,
    ConflictKindOut::TypeChange,
];

/// `conflicts` as the report `E_CONFLICT` carries, its hunk texts within `max_bytes`.
pub(crate) fn report(conflicts: &MergeConflicts, max_bytes: u64) -> ConflictReport {
    let mut texts = Budget {
        left: max_bytes,
        cut: false,
    };
    let files: Vec<FileConflictOut> = (conflicts.conflicts.iter())
        .map(|file| file_out(file, conflicts, &mut texts))
        .collect();
    let kinds = (KINDS.into_iter())
        .filter(|kind| files.iter().any(|file| file.kind == *kind))
        .collect();
    ConflictReport {
        base: conflicts.base.to_string(),
        ours: conflicts.ours.to_string(),
        theirs: conflicts.theirs.to_string(),
        files,
        kinds,
        auto_merged: (conflicts.auto_merged.iter())
            .map(ToString::to_string)
            .collect(),
        stale: conflicts.stale.iter().map(ToString::to_string).collect(),
        truncated: texts.cut,
    }
}

/// What a report's hunk texts may still take.
struct Budget {
    /// The bytes of text that still fit.
    left: u64,
    /// Whether a hunk's texts were left out: once one is, every later one is.
    cut: bool,
}

impl Budget {
    /// Whether texts of `bytes` bytes in all go in: they do while they fit and no earlier hunk's
    /// were left out. Spends them when they go in.
    fn take(&mut self, bytes: u64) -> bool {
        if self.cut || bytes > self.left {
            self.cut = true;
            return false;
        }
        self.left -= bytes;
        true
    }
}

/// One conflicting file of `conflicts`, as the report gives it, its hunk texts from `texts`.
fn file_out(
    file: &FileConflict,
    conflicts: &MergeConflicts,
    texts: &mut Budget,
) -> FileConflictOut {
    let blob = |leaf: Option<Leaf>| leaf.map(|leaf| leaf.blob.to_string());
    let (kind, hunks): (ConflictKindOut, &[ConflictHunk]) = match &file.kind {
        ConflictKind::Content { hunks } => (ConflictKindOut::Content, hunks),
        ConflictKind::Binary => (ConflictKindOut::Binary, &[]),
        ConflictKind::DeleteModify => (ConflictKindOut::DeleteModify, &[]),
        ConflictKind::ModifyDelete => (ConflictKindOut::ModifyDelete, &[]),
        ConflictKind::AddAdd { hunks } => (ConflictKindOut::AddAdd, hunks),
        ConflictKind::Mode { .. } => (ConflictKindOut::Mode, &[]),
        ConflictKind::TypeChange => (ConflictKindOut::TypeChange, &[]),
    };
    // Who changed the path on the target since the base: none where the walk did not reach it (a
    // base off the target can leave the change below it). An agent the changeset does not name
    // is `unknown/unknown`, as `show` attributes it.
    let touched = conflicts.last_touched.get(&file.path);
    let agent = |agent: &str| match agent {
        "" => UNKNOWN_AGENT.to_owned(),
        named => named.to_owned(),
    };
    FileConflictOut {
        path: file.path.to_string(),
        kind,
        base_blob: blob(file.base),
        ours_blob: blob(file.ours),
        theirs_blob: blob(file.theirs),
        hunks: (hunks.iter().zip(0..))
            .map(|(hunk, id)| hunk_out(hunk, id, texts))
            .collect(),
        theirs_changeset: touched.map(|(changeset, _)| changeset.to_string()),
        theirs_agent: touched.map(|(_, named)| agent(named)),
    }
}

/// `hunk`, the `id`-th of its file, with its three texts if they go in ([`Budget::take`]).
fn hunk_out(hunk: &ConflictHunk, id: u32, texts: &mut Budget) -> HunkOut {
    let bytes = [&hunk.base_text, &hunk.ours_text, &hunk.theirs_text]
        .map(|text| to_u64(text.len()))
        .iter()
        .sum();
    let with_texts = texts.take(bytes);
    let side = |lines: &Range<u32>, text: &[u8]| LinesOut {
        first: u64::from(lines.start) + 1,
        last: u64::from(lines.end),
        text: with_texts.then(|| String::from_utf8_lossy(text).into_owned()),
    };
    HunkOut {
        id,
        base: side(&hunk.base, &hunk.base_text),
        ours: side(&hunk.ours, &hunk.ours_text),
        theirs: side(&hunk.theirs, &hunk.theirs_text),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use swarmops_core::{BlobId, ChangesetId, LeafKind, ObjectId, RepoPath};

    use super::*;

    fn id(n: u8) -> ChangesetId {
        ChangesetId(ObjectId([n; 32]))
    }

    fn leaf(n: u8) -> Leaf {
        Leaf {
            kind: LeafKind::File,
            blob: BlobId(ObjectId([n; 32])),
        }
    }

    fn conflict(path: &str, kind: ConflictKind) -> FileConflict {
        FileConflict {
            path: RepoPath::new(path).unwrap(),
            kind,
            base: Some(leaf(1)),
            ours: None,
            theirs: Some(leaf(3)),
        }
    }

    /// The kinds map one to one, each listed once in the operations spec's order whatever the
    /// files' order; an added file's hunks, merged over an empty base, have an empty base side
    /// before line 1; a path no changeset on the walk touched has no attribution, and one whose
    /// changeset names no agent is `unknown/unknown`'s.
    #[test]
    fn every_kind_is_reported_and_listed_once() {
        let added = ConflictHunk {
            base: 0..0,
            ours: 0..1,
            theirs: 0..2,
            base_text: Vec::new(),
            ours_text: b"a\n".to_vec(),
            theirs_text: b"b\nc\n".to_vec(),
        };
        let mode = ConflictKind::Mode {
            ours: LeafKind::Exec,
            theirs: LeafKind::Symlink,
        };
        let conflicts = MergeConflicts {
            base: id(1),
            ours: id(2),
            theirs: id(3),
            conflicts: vec![
                conflict("a", ConflictKind::TypeChange),
                conflict("b", mode),
                conflict("c", ConflictKind::AddAdd { hunks: vec![added] }),
                conflict("d", ConflictKind::ModifyDelete),
                conflict("e", ConflictKind::Binary),
                conflict("f", ConflictKind::TypeChange),
            ],
            auto_merged: vec![RepoPath::new("g").unwrap()],
            last_touched: BTreeMap::from([
                (RepoPath::new("a").unwrap(), (id(4), String::new())),
                (
                    RepoPath::new("b").unwrap(),
                    (id(5), "agent/w-5".to_owned()),
                ),
            ]),
            stale: vec![RepoPath::new("e").unwrap(), RepoPath::new("h").unwrap()],
        };
        let report = report(&conflicts, 3);
        assert_eq!(
            report.kinds,
            [
                ConflictKindOut::Binary,
                ConflictKindOut::ModifyDelete,
                ConflictKindOut::AddAdd,
                ConflictKindOut::Mode,
                ConflictKindOut::TypeChange,
            ]
        );
        let kinds: Vec<ConflictKindOut> = report.files.iter().map(|file| file.kind).collect();
        assert_eq!(
            kinds,
            [
                ConflictKindOut::TypeChange,
                ConflictKindOut::Mode,
                ConflictKindOut::AddAdd,
                ConflictKindOut::ModifyDelete,
                ConflictKindOut::Binary,
                ConflictKindOut::TypeChange,
            ]
        );
        let attributed =
            |file: &FileConflictOut| (file.theirs_changeset.clone(), file.theirs_agent.clone());
        assert_eq!(
            attributed(&report.files[0]),
            (Some(id(4).to_string()), Some(UNKNOWN_AGENT.to_owned()))
        );
        assert_eq!(
            attributed(&report.files[1]),
            (Some(id(5).to_string()), Some("agent/w-5".to_owned()))
        );
        assert_eq!(attributed(&report.files[2]), (None, None));
        let [hunk] = &report.files[2].hunks[..] else {
            panic!("one hunk: {report:?}");
        };
        let lines = |first, last| LinesOut {
            first,
            last,
            text: None,
        };
        assert_eq!(
            (&hunk.base, &hunk.ours, &hunk.theirs),
            (&lines(1, 0), &lines(1, 1), &lines(1, 2)),
            "6 bytes of text, past a budget of 3"
        );
        assert!(report.truncated);
        let file = &report.files[0];
        let blob = |n| Some(leaf(n).blob.to_string());
        assert_eq!(
            (&file.base_blob, &file.ours_blob, &file.theirs_blob),
            (&blob(1), &None, &blob(3))
        );
        assert_eq!(report.auto_merged, ["g"]);
        assert_eq!(report.stale, ["e", "h"]);
        assert_eq!(
            (&report.base, &report.ours, &report.theirs),
            (&id(1).to_string(), &id(2).to_string(), &id(3).to_string())
        );
    }
}
