//! Three-way text merge (diff3, storage spec §4): what one file's content merges to, given the
//! base's, ours' and theirs' contents, with conflicts returned as data, never as markers.
//!
//! The rules are `git merge-file`'s, over the same hunks: the same merges are clean, to the same
//! bytes, and a conflict hunk is the block git's `--diff3` style shows (its default style narrows
//! each block to the lines where the two sides differ, which can split it, and joins blocks that
//! only three lines or fewer, or lines without a letter or digit, keep apart). Equal sides, or one
//! side equal to the base, decide at once: that side, whatever its bytes. Otherwise contents that
//! are not all text ([`is_text`]) are not merged by lines, and the merge is
//! [`ContentMerge::Binary`]. Text is merged by lines: a line is the bytes up to and including a
//! `\n`, a last line may have none, and `\r` is a byte of its line
//! ([`split_lines`](crate::line_diff::split_lines)). The two sides' hunks against the base
//! ([`line_hunks`]: the histogram diff, placed as git places its hunks) are walked in lockstep over
//! the base's lines, in chunks: a chunk starts at the next hunk of either side and grows while a
//! hunk of either side overlaps it or touches it (starts at the line where the chunk ends). A chunk
//! only one side changed takes that side's lines; one both sides changed takes their lines when
//! those are the same (the same change made twice) and is a [`ConflictHunk`] otherwise; the base's
//! lines between chunks stay. So two edits merge only with at least one unchanged line between
//! them, and edits that touch conflict, as in git.

use std::ops::Range;

use crate::conflict::{ConflictHunk, ContentMerge};
use crate::line_diff::{Hunk, is_text, line_hunks};

/// Merges what ours and theirs made of `base` (module doc): the merged bytes, the hunks where the
/// two sides' edits conflict (in file order, their ranges 0-based and half-open over each side's
/// lines), or `Binary`. Its signature is a [`ContentMerger`](crate::conflict::ContentMerger)'s.
///
/// # Panics
///
/// If a content has `i32::MAX` lines or more, [`line_hunks`]' bound (2 GiB at least).
pub fn merge_content(base: &[u8], ours: &[u8], theirs: &[u8]) -> ContentMerge {
    if ours == theirs || theirs == base {
        return ContentMerge::Clean(ours.to_vec());
    }
    if ours == base {
        return ContentMerge::Clean(theirs.to_vec());
    }
    if !(is_text(base) && is_text(ours) && is_text(theirs)) {
        return ContentMerge::Binary;
    }
    merge_lines(base, ours, theirs)
}

/// The lockstep walk of the module doc over two sides that both changed `base`, all three text.
fn merge_lines(base: &[u8], ours: &[u8], theirs: &[u8]) -> ContentMerge {
    let base_lines = Lines::new(base);
    let mut ours_side = Side::new(ours, line_hunks(base, ours));
    let mut theirs_side = Side::new(theirs, line_hunks(base, theirs));
    let mut merged = Vec::with_capacity(ours.len().max(theirs.len()));
    let mut conflicts = Vec::new();
    // The base's lines before `copied` are in `merged`, or in a conflict hunk.
    let mut copied = 0;
    loop {
        let start = match (ours_side.next_start(), theirs_side.next_start()) {
            (Some(ours_start), Some(theirs_start)) => ours_start.min(theirs_start),
            (Some(start), None) | (None, Some(start)) => start,
            (None, None) => break,
        };
        let (ours_from, theirs_from) = (ours_side.shift, theirs_side.shift);
        let (mut end, mut ours_took, mut theirs_took) = (start, false, false);
        loop {
            if let Some(hunk_end) = ours_side.take_hunk_from(end) {
                end = end.max(hunk_end);
                ours_took = true;
            } else if let Some(hunk_end) = theirs_side.take_hunk_from(end) {
                end = end.max(hunk_end);
                theirs_took = true;
            } else {
                break;
            }
        }
        merged.extend_from_slice(base_lines.bytes(copied..start));
        let chunk = start..end;
        let ours_lines = shifted(&chunk, ours_from, ours_side.shift);
        let theirs_lines = shifted(&chunk, theirs_from, theirs_side.shift);
        let ours_text = ours_side.lines.bytes(ours_lines.clone());
        let theirs_text = theirs_side.lines.bytes(theirs_lines.clone());
        if !theirs_took || ours_text == theirs_text {
            merged.extend_from_slice(ours_text);
        } else if !ours_took {
            merged.extend_from_slice(theirs_text);
        } else {
            conflicts.push(ConflictHunk {
                base_text: base_lines.bytes(chunk.clone()).to_vec(),
                ours_text: ours_text.to_vec(),
                theirs_text: theirs_text.to_vec(),
                base: chunk,
                ours: ours_lines,
                theirs: theirs_lines,
            });
        }
        copied = end;
    }
    if conflicts.is_empty() {
        merged.extend_from_slice(base_lines.bytes(copied..base_lines.count()));
        ContentMerge::Clean(merged)
    } else {
        ContentMerge::Conflicted(conflicts)
    }
}

/// The lines of one content: where each starts in its bytes.
struct Lines<'a> {
    bytes: &'a [u8],
    /// The byte offset of each line's start, then the content's length.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter(|&(_, &byte)| byte == b'\n')
                .map(|(at, _)| at + 1),
        );
        if starts.last() != Some(&bytes.len()) {
            starts.push(bytes.len());
        }
        Self { bytes, starts }
    }

    fn count(&self) -> u32 {
        u32::try_from(self.starts.len() - 1).expect("fewer lines than i32::MAX (line_hunks)")
    }

    /// The bytes of the lines `lines`.
    fn bytes(&self, lines: Range<u32>) -> &'a [u8] {
        &self.bytes[self.starts[lines.start as usize]..self.starts[lines.end as usize]]
    }
}

/// One side of the walk: its lines, its hunks against the base, and how far the walk has come.
struct Side<'a> {
    lines: Lines<'a>,
    hunks: Vec<Hunk>,
    /// The next hunk the walk has not taken.
    next: usize,
    /// This side's line number minus the base's, for the lines after the hunks taken so far.
    shift: i64,
}

impl<'a> Side<'a> {
    fn new(bytes: &'a [u8], hunks: Vec<Hunk>) -> Self {
        Self {
            lines: Lines::new(bytes),
            hunks,
            next: 0,
            shift: 0,
        }
    }

    /// Where the next hunk starts in the base, if one is left.
    fn next_start(&self) -> Option<u32> {
        self.hunks.get(self.next).map(|hunk| hunk.before.start)
    }

    /// Takes the next hunk if it overlaps or touches a chunk that ends at the base line `end`
    /// (whose start it cannot precede: the walk takes hunks in order), and returns where it ends.
    fn take_hunk_from(&mut self, end: u32) -> Option<u32> {
        let hunk = self
            .hunks
            .get(self.next)
            .filter(|hunk| hunk.before.start <= end)?;
        self.shift += lines(&hunk.after) - lines(&hunk.before);
        self.next += 1;
        Some(hunk.before.end)
    }
}

fn lines(range: &Range<u32>) -> i64 {
    i64::from(range.end) - i64::from(range.start)
}

/// A side's lines over the base's lines `chunk`: its start moved by the side's shift before the
/// chunk's hunks, its end by the shift after them.
fn shifted(chunk: &Range<u32>, from: i64, to: i64) -> Range<u32> {
    let at = |line: u32, shift: i64| {
        u32::try_from(i64::from(line) + shift).expect("a side's line is a line of that side")
    };
    at(chunk.start, from)..at(chunk.end, to)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{ContentMerge, merge_lines};

    /// A content of lines that repeat, the last one with its `\n` or without.
    fn content() -> impl Strategy<Value = Vec<u8>> {
        let line = prop::sample::select(vec!["a\n", "b\n", "c\r\n", "\n", "}\n"]);
        (prop::collection::vec(line, 0..12), any::<bool>()).prop_map(|(lines, ends)| {
            let mut bytes = lines.concat().into_bytes();
            if !ends && bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            bytes
        })
    }

    proptest! {
        /// The walk alone keeps the identities that `merge_content` decides before it: the same
        /// change twice is that change, and a side equal to the base gives the other side.
        #[test]
        fn the_walk_keeps_the_identities(base in content(), side in content()) {
            prop_assert_eq!(merge_lines(&base, &side, &side), ContentMerge::Clean(side.clone()));
            prop_assert_eq!(merge_lines(&base, &base, &side), ContentMerge::Clean(side.clone()));
            prop_assert_eq!(merge_lines(&base, &side, &base), ContentMerge::Clean(side.clone()));
        }
    }
}
