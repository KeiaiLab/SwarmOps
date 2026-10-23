//! Line hunks: the histogram diff of two byte strings split into lines, and the hunks a unified
//! diff prints, with their context (storage spec §4: the hunks diff3 walks; operations spec §1:
//! the hunks of `diff`).
//!
//! A line is the bytes up to and including a `\n`; a last line without one is a line, and `\r`
//! is part of its line, so a CRLF line and its LF twin differ ([`split_lines`]). [`line_hunks`]
//! runs `imara-diff`'s histogram diff over those lines and compacts it as git compacts its own: a
//! run of changed lines that could sit at more than one place slides as far down as it can,
//! unless it can line up with a change on the other side (git's placement without its indent
//! heuristic, as `git merge-file` and `git diff --no-indent-heuristic` place it). Its hunks are
//! 0-based, half-open line ranges of each side, in order, with at least one unchanged line
//! between two of them. [`unified`] groups them into the hunks `git diff -U<context>` prints, and
//! [`is_text`] is the rule for whether a file gets line hunks at all.
//!
//! A side past the line diff's bound, [`MAX_DIFF_LINES`] lines or [`MAX_DIFF_BYTES`] bytes, gets
//! none either ([`TooLarge`]), as git treats a file past its size threshold as binary: the
//! histogram diff can take a pass over the rest of both sides for each change it finds, a time
//! that grows with the square of the lines.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

/// How many leading bytes [`is_text`] searches for a NUL: git's `FIRST_FEW_BYTES`.
const TEXT_PROBE: usize = 8_000;

/// The most lines a side may have for [`line_hunks`] to diff it: 48 Ki, a MiB of source in lines
/// of 21⅓ bytes on average. The slowest line diff measured at this bound takes seconds.
pub const MAX_DIFF_LINES: usize = 48 << 10;

/// The most bytes a side may have for [`line_hunks`] to diff it: 8 MiB, so that a side within
/// [`MAX_DIFF_LINES`] but of long lines is not read into lines, hunks and texts either (an object
/// holds up to 512 MiB).
pub const MAX_DIFF_BYTES: usize = 8 << 20;

/// What [`line_hunks`] gives for a side past the line diff's bound, more than [`MAX_DIFF_LINES`]
/// lines or [`MAX_DIFF_BYTES`] bytes: no hunks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TooLarge;

/// The lines of `bytes`: each ends after its `\n` and keeps it, a last line without one is a line,
/// and an empty input has none.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    lines(bytes).collect()
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|&byte| byte == b'\n')
}

/// One change between two sequences of lines: the old side's lines `before` give way to the new
/// side's lines `after`. Both are 0-based, half-open ranges of line numbers; one of them is empty
/// for a pure insertion or deletion (at that position), never both.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hunk {
    /// The old side's lines that go.
    pub before: Range<u32>,
    /// The new side's lines that come in their place.
    pub after: Range<u32>,
}

impl Hunk {
    /// The hunk where the old side's lines `before` give way to the new side's `after`.
    ///
    /// Invariant: `before` and `after` are never both empty. No validation exists: `new` checks
    /// nothing, and only [`line_hunks`] makes hunks outside tests.
    pub fn new(before: Range<u32>, after: Range<u32>) -> Self {
        Self { before, after }
    }
}

/// The changes that turn `before` into `after`, line by line ([`split_lines`]): the histogram
/// diff, compacted as the module doc says. The hunks are in order, with at least one unchanged
/// line between two of them, and the lines outside them pair up equal from both sides in order;
/// identical inputs have none. [`TooLarge`] when a side is past the line diff's bound, found
/// before any line is diffed.
pub fn line_hunks(before: &[u8], after: &[u8]) -> Result<Vec<Hunk>, TooLarge> {
    if !(diffable(before) && diffable(after)) {
        return Err(TooLarge);
    }
    let mut input = InternedInput::<&[u8]>::default();
    input.update_before(lines(before));
    input.update_after(lines(after));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_no_heuristic(&input);
    Ok(diff
        .hunks()
        .map(|hunk| Hunk {
            before: hunk.before,
            after: hunk.after,
        })
        .collect())
}

/// Whether `bytes` is within the line diff's bound: at most [`MAX_DIFF_BYTES`] bytes, in at most
/// [`MAX_DIFF_LINES`] lines.
pub(crate) fn diffable(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_DIFF_BYTES && lines(bytes).nth(MAX_DIFF_LINES).is_none()
}

/// Whether `bytes` is text: UTF-8, with no NUL in its first 8,000 bytes (git's probe). Only text
/// gets line hunks.
pub fn is_text(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(TEXT_PROBE)];
    !probe.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// The lines a diff inserts and deletes, as `git diff --stat` counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineStats {
    /// The new side's lines the hunks bring in.
    pub insertions: u64,
    /// The old side's lines the hunks take out.
    pub deletions: u64,
}

impl LineStats {
    /// The counts of `insertions` and `deletions`.
    pub fn new(insertions: u64, deletions: u64) -> Self {
        Self {
            insertions,
            deletions,
        }
    }
}

/// The lines `hunks` insert and delete: the sums of their `after` and their `before` lengths.
pub fn line_stats(hunks: &[Hunk]) -> LineStats {
    let lines = |range: &Range<u32>| u64::from(range.end.saturating_sub(range.start));
    hunks
        .iter()
        .fold(LineStats::default(), |stats, hunk| LineStats {
            insertions: stats.insertions + lines(&hunk.after),
            deletions: stats.deletions + lines(&hunk.before),
        })
}

/// One line of a unified hunk, with its `\n` if it has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLine<'a> {
    /// A line both sides have, around a change.
    Context(&'a [u8]),
    /// A line of the old side that the new side does not have.
    Removed(&'a [u8]),
    /// A line of the new side that the old side does not have.
    Added(&'a [u8]),
}

/// A hunk as a unified diff prints it: one or more changes, the unchanged lines around them, and
/// its header `@@ -before_start,before_lines +after_start,after_lines @@`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedHunk<'a> {
    /// The old side's first line in the hunk, 1-based; when the hunk has none of the old side's
    /// lines, the line before where they would be (0 at the top), as git prints it.
    pub before_start: u32,
    /// How many of the old side's lines the hunk holds: its context and removed lines.
    pub before_lines: u32,
    /// The new side's first line in the hunk, counted as `before_start` is.
    pub after_start: u32,
    /// How many of the new side's lines the hunk holds: its context and added lines.
    pub after_lines: u32,
    /// The lines in order; each change's removed lines come before its added lines.
    pub lines: Vec<DiffLine<'a>>,
}

impl<'a> UnifiedHunk<'a> {
    /// The hunk with this header and these lines, as they are.
    ///
    /// Invariant: `before_lines` counts the context and removed lines of `lines`, `after_lines`
    /// the context and added ones. No validation exists: `new` checks nothing, and only
    /// [`unified`] makes hunks outside tests.
    pub fn new(
        before_start: u32,
        before_lines: u32,
        after_start: u32,
        after_lines: u32,
        lines: Vec<DiffLine<'a>>,
    ) -> Self {
        Self {
            before_start,
            before_lines,
            after_start,
            after_lines,
            lines,
        }
    }
}

/// The unified hunks of `hunks`, which are `line_hunks(before, after)`'s, with `context`
/// unchanged lines around each change, as `git diff -U<context>` prints them: two changes with at
/// most `2 × context` unchanged lines between them share a hunk.
///
/// # Panics
///
/// If a hunk names a line that `before` or `after` does not have.
pub fn unified<'a>(
    before: &'a [u8],
    after: &'a [u8],
    hunks: &[Hunk],
    context: u32,
) -> Vec<UnifiedHunk<'a>> {
    let (old, new) = (split_lines(before), split_lines(after));
    let old_len = u32::try_from(old.len()).unwrap_or(u32::MAX);
    let new_len = u32::try_from(new.len()).unwrap_or(u32::MAX);
    let mut shown = Vec::new();
    let mut rest = hunks;
    while let [first, ..] = rest {
        // The changes that share this hunk: each one's unchanged lines since the one before are
        // at most `2 × context`.
        let joined = 1 + rest
            .windows(2)
            .take_while(|pair| {
                let between = pair[1].before.start.saturating_sub(pair[0].before.end);
                u64::from(between) <= 2 * u64::from(context)
            })
            .count();
        let (group, tail) = rest.split_at(joined);
        rest = tail;
        let last = &group[joined - 1];
        let old_start = first.before.start.saturating_sub(context);
        let new_start = first.after.start.saturating_sub(context);
        let old_end = last.before.end.saturating_add(context).min(old_len);
        let new_end = last.after.end.saturating_add(context).min(new_len);
        let mut lines = Vec::new();
        let mut unchanged = old_start;
        for hunk in group {
            lines.extend(
                old[at(unchanged..hunk.before.start)]
                    .iter()
                    .copied()
                    .map(DiffLine::Context),
            );
            lines.extend(
                old[at(hunk.before.clone())]
                    .iter()
                    .copied()
                    .map(DiffLine::Removed),
            );
            lines.extend(
                new[at(hunk.after.clone())]
                    .iter()
                    .copied()
                    .map(DiffLine::Added),
            );
            unchanged = hunk.before.end;
        }
        lines.extend(
            old[at(unchanged..old_end)]
                .iter()
                .copied()
                .map(DiffLine::Context),
        );
        shown.push(UnifiedHunk {
            before_start: header_start(old_start, old_end - old_start),
            before_lines: old_end - old_start,
            after_start: header_start(new_start, new_end - new_start),
            after_lines: new_end - new_start,
            lines,
        });
    }
    shown
}

fn at(lines: Range<u32>) -> Range<usize> {
    lines.start as usize..lines.end as usize
}

/// The start a unified header prints for a side's lines `start..start + lines` (0-based): the
/// first one, 1-based, or with no line the one before (0 at the top).
fn header_start(start: u32, lines: u32) -> u32 {
    if lines == 0 { start } else { start + 1 }
}
