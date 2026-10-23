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
//! its texts, and `truncated` says so. The paths whose resolutions did not apply, since the target
//! holds them otherwise than their `against`, are `stale`.
//!
//! The files go in, in path order, while their entries stay within [`MAX_ENTRY_BYTES`] as JSON,
//! the bytes of their texts aside: the first file always, with its hunks while they fit, at least
//! its first, and one entry without texts that spans the rest; a later file whole, with every
//! hunk the merge holds of it, or not at all. A merge holds a file's hunks past its first ones as
//! one hunk that spans them too, so each file in a report covers every line of it where the sides'
//! edits meet. From the first file left out, none follows, and `truncated` says so; the error's
//! message still counts every conflicting path. So a report stays within a bound however many
//! lines or files conflict:
//!
//! ```text
//! report = ids, kinds, auto_merged, stale        a few hundred bytes for short lists
//!        + files and hunks, texts aside          MAX_ENTRY_BYTES, or the first file and hunk
//!        + their texts                           max_conflict_bytes
//! ```

use std::io;
use std::ops::Range;

use swarmops_api::{ConflictKindOut, ConflictReport, FileConflictOut, HunkOut, LinesOut, OpError};
use swarmops_core::bytes;
use swarmops_repo::MergeConflicts;
use swarmops_repo::merge::conflict::{ConflictHunk, ConflictKind};
use swarmops_repo::merge::tree_diff::Leaf;
use swarmops_repo::merge::tree_merge::FileConflict;

use crate::error;
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

/// The most bytes a report's files and hunks take as JSON, the bytes of their texts aside, which
/// `max_conflict_bytes` bounds: with its default of 32 KiB, an `E_CONFLICT` envelope stays within
/// the operations spec's 64 KiB target for a default response (§9) while the report's
/// `auto_merged` and `stale` lists are short.
const MAX_ENTRY_BYTES: u64 = 24 * 1024;

/// What the `hunks` key adds to a file's entry that has hunks: `,"hunks":[]`.
const HUNKS_KEY_BYTES: u64 = 11;

/// `conflicts` as the report `E_CONFLICT` carries ([the module documentation](self)), its hunk
/// texts within `max_bytes`. `E_INTERNAL` for a conflict kind this build does not know.
pub(crate) fn report(
    conflicts: &MergeConflicts,
    max_bytes: u64,
) -> Result<ConflictReport, OpError> {
    let mut budgets = Budgets {
        texts: Budget::new(max_bytes),
        entries: Budget::new(MAX_ENTRY_BYTES),
    };
    let mut files: Vec<FileConflictOut> = Vec::new();
    for file in &conflicts.conflicts {
        let place = if files.is_empty() {
            Place::First
        } else {
            Place::Later
        };
        let Some(out) = file_out(file, conflicts, place, &mut budgets)? else {
            break;
        };
        files.push(out);
        // The first file's hunks past the budget: nothing follows them.
        if budgets.entries.cut {
            break;
        }
    }
    let kinds = (KINDS.into_iter())
        .filter(|kind| files.iter().any(|file| file.kind == *kind))
        .collect();
    let mut out = ConflictReport::new(
        conflicts.base.to_string(),
        conflicts.ours.to_string(),
        conflicts.theirs.to_string(),
        files,
        kinds,
        budgets.texts.cut || budgets.entries.cut,
    );
    out.auto_merged = (conflicts.auto_merged.iter())
        .map(ToString::to_string)
        .collect();
    out.stale = conflicts.stale.iter().map(ToString::to_string).collect();
    Ok(out)
}

/// What a report may still take: of its hunks' texts, and of its files' and hunks' entries.
struct Budgets {
    /// The bytes of hunk text: the request's `max_conflict_bytes` at first.
    texts: Budget,
    /// The bytes of the entries as JSON, their texts aside: [`MAX_ENTRY_BYTES`] at first.
    entries: Budget,
}

/// What one part of a report may still take.
struct Budget {
    /// The bytes that still fit.
    left: u64,
    /// Whether something was left out: once something is, everything later is.
    cut: bool,
}

impl Budget {
    /// `bytes` of room, nothing left out yet.
    fn new(bytes: u64) -> Budget {
        Budget {
            left: bytes,
            cut: false,
        }
    }

    /// Whether `bytes` go in: they do while they fit and nothing earlier was left out. Spends them
    /// when they go in.
    fn take(&mut self, bytes: u64) -> bool {
        if self.cut || bytes > self.left {
            self.cut = true;
            return false;
        }
        self.left -= bytes;
        true
    }

    /// Spends `bytes`, which go in whether they fit or not: what a report always holds.
    fn force(&mut self, bytes: u64) {
        self.left = self.left.saturating_sub(bytes);
    }
}

/// Where a file's entry stands in a report.
#[derive(Clone, Copy)]
enum Place {
    /// The first file's: always in, with its first hunk.
    First,
    /// A later one's: in whole, or not at all.
    Later,
}

/// One conflicting file of `conflicts`, as the report gives it at `place`, if it goes in
/// ([the module documentation](self)): its entry from `budgets.entries`, its hunk texts from
/// `budgets.texts`. `None` leaves it out, and every file after it. `E_INTERNAL` for a conflict
/// kind this build does not know.
fn file_out(
    file: &FileConflict,
    conflicts: &MergeConflicts,
    place: Place,
    budgets: &mut Budgets,
) -> Result<Option<FileConflictOut>, OpError> {
    let blob = |leaf: Option<Leaf>| leaf.map(|leaf| leaf.blob.to_string());
    let (kind, hunks): (ConflictKindOut, &[ConflictHunk]) = match &file.kind {
        ConflictKind::Content { hunks } => (ConflictKindOut::Content, hunks),
        ConflictKind::Binary => (ConflictKindOut::Binary, &[]),
        ConflictKind::DeleteModify => (ConflictKindOut::DeleteModify, &[]),
        ConflictKind::ModifyDelete => (ConflictKindOut::ModifyDelete, &[]),
        ConflictKind::AddAdd { hunks } => (ConflictKindOut::AddAdd, hunks),
        ConflictKind::Mode { .. } => (ConflictKindOut::Mode, &[]),
        ConflictKind::TypeChange => (ConflictKindOut::TypeChange, &[]),
        _ => return Err(error::unknown_variant("ConflictKind")),
    };
    // Who changed the path on the target since the base: none where the walk did not reach it (a
    // base off the target can leave the change below it). An agent the changeset does not name
    // is `unknown/unknown`, as `show` attributes it.
    let touched = conflicts.last_touched.get(&file.path);
    let agent = |agent: &str| match agent {
        "" => UNKNOWN_AGENT.to_owned(),
        named => named.to_owned(),
    };
    let mut out = FileConflictOut::new(file.path.to_string(), kind);
    out.base_blob = blob(file.base);
    out.ours_blob = blob(file.ours);
    out.theirs_blob = blob(file.theirs);
    out.theirs_changeset = touched.map(|(changeset, _)| changeset.to_string());
    out.theirs_agent = touched.map(|(_, named)| agent(named));

    // The file's entry as JSON, then each hunk's, made one at a time: a hunk past the budget is
    // never made, nor its texts taken.
    let keyed = if hunks.is_empty() { 0 } else { HUNKS_KEY_BYTES };
    let header = entry_len(json_len(|to| serde_json::to_writer(to, &out)), keyed);
    let texts = &mut budgets.texts;
    let made = (hunks.iter().zip(0..)).map(|(hunk, id)| {
        let entry = hunk_out(hunk, id, texts);
        let len = entry_len(json_len(|to| serde_json::to_writer(to, &entry)), 0);
        let len = len.saturating_sub(text_len(&entry));
        (entry, len)
    });
    let entries = &mut budgets.entries;
    match place {
        Place::First => {
            entries.force(header);
            for (entry, len) in made {
                if out.hunks.is_empty() {
                    entries.force(len);
                } else if !entries.take(len) {
                    // This hunk and the ones after it, as one entry.
                    let rest = hunks.get(entry.id as usize..).unwrap_or_default();
                    out.hunks.extend(spanning(rest, entry.id));
                    break;
                }
                out.hunks.push(entry);
            }
        }
        Place::Later => {
            let mut whole = header;
            for (entry, len) in made {
                whole = whole.saturating_add(len);
                if whole > entries.left {
                    break;
                }
                out.hunks.push(entry);
            }
            if !entries.take(whole) {
                return Ok(None);
            }
        }
    }
    Ok(Some(out))
}

/// The entry of one hunk that spans `hunks`, a file's from its `id`-th on, in file order: each
/// side's lines from the first one's start to the last one's end, without texts, which the
/// conflicts hold only for the lines of each hunk. `None` for no hunks.
fn spanning(hunks: &[ConflictHunk], id: u32) -> Option<HunkOut> {
    let (first, last) = (hunks.first()?, hunks.last()?);
    let side = |from: &Range<u32>, to: &Range<u32>| {
        LinesOut::new(u64::from(from.start) + 1, u64::from(to.end))
    };
    Some(HunkOut::new(
        id,
        side(&first.base, &last.base),
        side(&first.ours, &last.ours),
        side(&first.theirs, &last.theirs),
    ))
}

/// An entry's length in a report's list: its JSON's `json` bytes, the `,` before it, and
/// `keyed` bytes of keys around what it lists.
fn entry_len(json: u64, keyed: u64) -> u64 {
    json.saturating_add(1).saturating_add(keyed)
}

/// The bytes of `hunk`'s texts, those it carries: the texts' budget counts them.
fn text_len(hunk: &HunkOut) -> u64 {
    let text = |side: &LinesOut| {
        side.text
            .as_ref()
            .map_or(0, |text| bytes::to_u64(text.len()))
    };
    text(&hunk.base) + text(&hunk.ours) + text(&hunk.theirs)
}

/// How many bytes `write` writes to the writer it is given: a value's length as compact JSON, for
/// `write` that serializes it there. A value that does not serialize fits nowhere.
fn json_len(write: impl FnOnce(&mut Counted) -> serde_json::Result<()>) -> u64 {
    let mut counted = Counted(0);
    match write(&mut counted) {
        Ok(()) => counted.0,
        Err(_) => u64::MAX,
    }
}

/// A writer that keeps only how many bytes it was given.
struct Counted(u64);

impl io::Write for Counted {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes::to_u64(bytes.len()));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `hunk`, the `id`-th of its file, with its three texts if they go in ([`Budget::take`]).
fn hunk_out(hunk: &ConflictHunk, id: u32, texts: &mut Budget) -> HunkOut {
    let bytes = [&hunk.base_text, &hunk.ours_text, &hunk.theirs_text]
        .map(|text| bytes::to_u64(text.len()))
        .iter()
        .sum();
    let with_texts = texts.take(bytes);
    let side = |lines: &Range<u32>, text: &[u8]| {
        let mut out = LinesOut::new(u64::from(lines.start) + 1, u64::from(lines.end));
        out.text = with_texts.then(|| String::from_utf8_lossy(text).into_owned());
        out
    };
    HunkOut::new(
        id,
        side(&hunk.base, &hunk.base_text),
        side(&hunk.ours, &hunk.ours_text),
        side(&hunk.theirs, &hunk.theirs_text),
    )
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
                (RepoPath::new("b").unwrap(), (id(5), "agent/w-5".to_owned())),
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

    /// A file after the first goes in whole, with every hunk it holds, or not at all: one of fifty
    /// hunks after a first file goes in with all fifty; one whose hunks pass what is left of the
    /// budget is left out, and so is every file after it, one that would fit too; and the report
    /// says `truncated`, though it carries every text it holds.
    #[test]
    fn a_later_file_goes_in_whole_or_not_at_all() {
        let hunk = |i: u32| ConflictHunk {
            base: i..i + 1,
            ours: i..i + 1,
            theirs: i..i + 1,
            base_text: Vec::new(),
            ours_text: b"o\n".to_vec(),
            theirs_text: b"t\n".to_vec(),
        };
        let content = |hunks: u32| ConflictKind::Content {
            hunks: (0..hunks).map(hunk).collect(),
        };
        let of = |files: Vec<FileConflict>| MergeConflicts {
            base: id(1),
            ours: id(2),
            theirs: id(3),
            conflicts: files,
            auto_merged: Vec::new(),
            last_touched: BTreeMap::new(),
            stale: Vec::new(),
        };
        let listed = |report: &ConflictReport| -> Vec<(String, usize)> {
            (report.files.iter())
                .map(|file| (file.path.clone(), file.hunks.len()))
                .collect()
        };

        let every_text = u64::MAX;
        let two = vec![conflict("a", content(1)), conflict("b", content(50))];
        let fits = report(&of(two), every_text);
        assert_eq!(listed(&fits), [("a".to_owned(), 1), ("b".to_owned(), 50)]);
        assert!(!fits.truncated, "nothing left out");

        let files = vec![
            conflict("a", content(1)),
            conflict("b", content(1_000)),
            conflict("c", content(1)),
        ];
        let cut = report(&of(files), every_text);
        assert_eq!(listed(&cut), [("a".to_owned(), 1)]);
        assert!(cut.truncated);
    }

    /// A report of conflicts holding a hundred thousand hunks in a file and ten thousand files
    /// after it stays within the operations spec's 64 KiB target for a response (§9) with no
    /// texts, made in well under a second: the first file and its first hunk, with their
    /// positions, then what fits, the rest of its hunks as one entry that spans them, and
    /// `truncated`.
    #[test]
    fn a_report_of_very_many_hunks_and_files_is_bounded() {
        use std::time::{Duration, Instant};

        let hunk = |i: u32| ConflictHunk {
            base: 2 * i..2 * i + 1,
            ours: 2 * i..2 * i + 1,
            theirs: 2 * i..2 * i + 1,
            base_text: format!("l{i}\n").into_bytes(),
            ours_text: format!("o{i}\n").into_bytes(),
            theirs_text: format!("t{i}\n").into_bytes(),
        };
        let content = |hunks: u32| ConflictKind::Content {
            hunks: (0..hunks).map(hunk).collect(),
        };
        let mut files = vec![conflict("a", content(100_000))];
        files.extend((0..10_000).map(|i| conflict(&format!("b/{i:05}"), content(1))));
        let conflicts = MergeConflicts {
            base: id(1),
            ours: id(2),
            theirs: id(3),
            conflicts: files,
            auto_merged: Vec::new(),
            last_touched: BTreeMap::new(),
            stale: Vec::new(),
        };

        let started = Instant::now();
        let report = report(&conflicts, 0);
        let took = started.elapsed();
        let json = serde_json::to_vec(&report).expect("a report serializes");
        assert!(json.len() < 64 * 1024, "{} bytes", json.len());
        assert!(took < Duration::from_secs(1), "{took:?}");
        assert!(report.truncated);
        let first = &report.files[0];
        assert_eq!(first.path, "a");
        let lines = |first, last| LinesOut {
            first,
            last,
            text: None,
        };
        let sides = (
            &first.hunks[0].base,
            &first.hunks[0].ours,
            &first.hunks[0].theirs,
        );
        assert_eq!(sides, (&lines(1, 1), &lines(1, 1), &lines(1, 1)));
        let ids: Vec<u32> = first.hunks.iter().map(|hunk| hunk.id).collect();
        assert_eq!(ids, (0..).take(ids.len()).collect::<Vec<u32>>());
        let spanned = &first.hunks[first.hunks.len() - 1];
        let from = 2 * u64::from(spanned.id) + 1;
        let to = 2 * 99_999 + 1;
        assert_eq!(spanned.base, lines(from, to), "the rest, spanned");
        assert_eq!(
            (&spanned.ours, &spanned.theirs),
            (&lines(from, to), &lines(from, to))
        );
    }
}
