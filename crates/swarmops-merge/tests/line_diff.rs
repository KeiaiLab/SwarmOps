//! Line hunks (`swarmops_merge::line_diff`): lines split after each `\n`, the histogram hunks
//! between two byte strings applied back, the unified hunks git prints with their context, the
//! text rule and the line counts.

use std::ops::Range;

use proptest::prelude::*;
use proptest::sample::Index;
use swarmops_merge::line_diff::{
    DiffLine, Hunk, LineStats, UnifiedHunk, is_text, line_hunks, line_stats, split_lines, unified,
};

/// A unified hunk's header: `(before_start, before_lines, after_start, after_lines)`, as
/// `@@ -before_start,before_lines +after_start,after_lines @@` prints it.
type Header = (u32, u32, u32, u32);

/// One edit of a line vector; its positions are taken modulo the length it applies to.
#[derive(Clone, Debug)]
enum Edit {
    Insert(Index, Vec<u8>),
    Delete(Index),
    Replace(Index, Vec<u8>),
    Move(Index, Index),
}

/// A line of a few contents, so that lines repeat as code's lines do, ending in `\n` or `\r\n`.
fn line() -> impl Strategy<Value = Vec<u8>> {
    let contents = prop::sample::select(vec!["", "a", "b", "c", "}", "fn f() {", "    x += 1;"]);
    let ends = prop::sample::select(vec!["\n", "\r\n"]);
    (contents, ends).prop_map(|(content, end)| [content, end].concat().into_bytes())
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<Index>(), line()).prop_map(|(at, line)| Edit::Insert(at, line)),
        any::<Index>().prop_map(Edit::Delete),
        (any::<Index>(), line()).prop_map(|(at, line)| Edit::Replace(at, line)),
        (any::<Index>(), any::<Index>()).prop_map(|(from, to)| Edit::Move(from, to)),
    ]
}

/// Two sides: some lines, and the same lines after a few inserts, deletes, replaces and moves;
/// each side's last line keeps its `\n` or not.
fn sides() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (
        prop::collection::vec(line(), 0..24),
        prop::collection::vec(edit(), 0..8),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(lines, edits, before_ends_a_line, after_ends_a_line)| {
            let after = edited(lines.clone(), &edits);
            (
                text(&lines, before_ends_a_line),
                text(&after, after_ends_a_line),
            )
        })
}

fn edited(mut lines: Vec<Vec<u8>>, edits: &[Edit]) -> Vec<Vec<u8>> {
    for edit in edits {
        let len = lines.len();
        match edit {
            Edit::Insert(at, line) => lines.insert(at.index(len + 1), line.clone()),
            Edit::Delete(at) if len > 0 => {
                lines.remove(at.index(len));
            }
            Edit::Replace(at, line) if len > 0 => lines[at.index(len)].clone_from(line),
            Edit::Move(from, to) if len > 0 => {
                let moved = lines.remove(from.index(len));
                lines.insert(to.index(len), moved);
            }
            Edit::Delete(_) | Edit::Replace(..) | Edit::Move(..) => {}
        }
    }
    lines
}

/// The bytes of `lines`; unless `ends_a_line`, without the last `\n`.
fn text(lines: &[Vec<u8>], ends_a_line: bool) -> Vec<u8> {
    let mut bytes = lines.concat();
    if !ends_a_line && bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

fn at(lines: Range<u32>) -> Range<usize> {
    lines.start as usize..lines.end as usize
}

/// `before` with each hunk's `before` lines replaced by its `after` lines. On the way it checks
/// what the hunks promise: in order, never empty on both sides, at least one unchanged line
/// between two of them, and as many unchanged lines on each side before each one and after the
/// last.
fn apply(before: &[u8], after: &[u8], hunks: &[Hunk]) -> Vec<u8> {
    let (old, new) = (split_lines(before), split_lines(after));
    let mut rebuilt = Vec::new();
    let (mut old_at, mut new_at) = (0, 0);
    for (n, hunk) in hunks.iter().enumerate() {
        assert!(
            !hunk.before.is_empty() || !hunk.after.is_empty(),
            "empty on both sides: {hunk:?}"
        );
        assert!(
            hunk.before.start >= old_at && hunk.after.start >= new_at,
            "out of order: {hunk:?}"
        );
        assert!(
            n == 0 || hunk.before.start > old_at,
            "touches the hunk before: {hunk:?}"
        );
        assert_eq!(
            hunk.before.start - old_at,
            hunk.after.start - new_at,
            "unaligned: {hunk:?}"
        );
        rebuilt.extend(old[at(old_at..hunk.before.start)].concat());
        rebuilt.extend(new[at(hunk.after.clone())].concat());
        (old_at, new_at) = (hunk.before.end, hunk.after.end);
    }
    assert_eq!(
        old.len() - old_at as usize,
        new.len() - new_at as usize,
        "unaligned after the last hunk"
    );
    rebuilt.extend(old[old_at as usize..].concat());
    rebuilt
}

proptest! {
    #[test]
    fn applying_the_hunks_to_before_gives_after((before, after) in sides()) {
        let hunks = line_hunks(&before, &after).unwrap();
        prop_assert_eq!(apply(&before, &after, &hunks), after.clone());
        let stats = line_stats(&hunks);
        prop_assert_eq!(
            split_lines(&before).len() as u64 + stats.insertions,
            split_lines(&after).len() as u64 + stats.deletions
        );
        prop_assert!(line_hunks(&before, &before).unwrap().is_empty());
    }

    #[test]
    fn unified_hunks_show_both_sides_with_their_context(
        (before, after) in sides(),
        context in 0_u32..5,
    ) {
        let hunks = line_hunks(&before, &after).unwrap();
        let shown = unified(&before, &after, &hunks, context);
        prop_assert_eq!(shown.is_empty(), hunks.is_empty());
        let (removed, added) = check_unified(&before, &after, &shown, context);
        let stats = line_stats(&hunks);
        prop_assert_eq!((removed, added), (stats.deletions, stats.insertions));
    }
}

/// The 0-based first line of a side a header names (an empty side names the line before it).
fn first_line(start: u32, lines: u32) -> usize {
    if lines == 0 {
        start as usize
    } else {
        start as usize - 1
    }
}

/// Checks every unified hunk against both sides: its context and removed lines are the old
/// side's lines its header names, its context and added lines the new side's; the hunks are in
/// order with unchanged lines between them that neither shows; `context` lines stand before the
/// first change and after the last (fewer only at an end of the file) and at most twice that many
/// between two changes. Returns the removed and the added lines it saw.
fn check_unified(
    before: &[u8],
    after: &[u8],
    shown: &[UnifiedHunk<'_>],
    context: u32,
) -> (u64, u64) {
    let (old, new) = (split_lines(before), split_lines(after));
    let (mut removed, mut added) = (0, 0);
    let (mut old_end, mut new_end) = (0, 0);
    for (n, hunk) in shown.iter().enumerate() {
        let (old_first, new_first) = (
            first_line(hunk.before_start, hunk.before_lines),
            first_line(hunk.after_start, hunk.after_lines),
        );
        let side = |keep: fn(&DiffLine<'_>) -> bool| -> Vec<&[u8]> {
            hunk.lines
                .iter()
                .filter(|line| keep(line))
                .map(|line| match *line {
                    DiffLine::Context(bytes)
                    | DiffLine::Removed(bytes)
                    | DiffLine::Added(bytes) => bytes,
                    _ => panic!("a variant this test does not know"),
                })
                .collect()
        };
        let old_lines = &old[old_first..old_first + hunk.before_lines as usize];
        let new_lines = &new[new_first..new_first + hunk.after_lines as usize];
        assert_eq!(
            side(|line| !matches!(line, DiffLine::Added(_))),
            old_lines,
            "{hunk:?}"
        );
        assert_eq!(
            side(|line| !matches!(line, DiffLine::Removed(_))),
            new_lines,
            "{hunk:?}"
        );
        assert!(
            n == 0 || (old_first > old_end && new_first > new_end),
            "joins: {hunk:?}"
        );
        (old_end, new_end) = (old_first + old_lines.len(), new_first + new_lines.len());
        // The runs of context lines: before the first change, between two changes, after the
        // last one.
        let mut runs = vec![0_u32];
        let mut in_a_change = false;
        for line in &hunk.lines {
            match line {
                DiffLine::Context(_) if in_a_change => {
                    runs.push(1);
                    in_a_change = false;
                }
                DiffLine::Context(_) => *runs.last_mut().unwrap() += 1,
                DiffLine::Removed(_) | DiffLine::Added(_) => {
                    in_a_change = true;
                    if matches!(line, DiffLine::Removed(_)) {
                        removed += 1;
                    } else {
                        added += 1;
                    }
                }
                _ => panic!("a variant this test does not know"),
            }
        }
        if in_a_change {
            runs.push(0);
        }
        assert!(runs.len() >= 2, "no change: {hunk:?}");
        let (lead, inner, trail) = (runs[0], &runs[1..runs.len() - 1], runs[runs.len() - 1]);
        assert!(
            lead == context || (lead < context && old_first == 0),
            "{hunk:?}"
        );
        assert!(
            trail == context || (trail < context && old_end == old.len() && new_end == new.len()),
            "{hunk:?}"
        );
        assert!(
            inner.iter().all(|&run| (1..=2 * context).contains(&run)),
            "{hunk:?}"
        );
    }
    (removed, added)
}

#[test]
fn identical_inputs_have_no_hunks() {
    let long = "line a\nline b\n\nline a\n".repeat(250);
    for text in [
        "",
        "a",
        "a\n",
        "\n\n",
        "a\r\nb\r\n",
        "x\n\ny",
        long.as_str(),
    ] {
        assert!(
            line_hunks(text.as_bytes(), text.as_bytes())
                .unwrap()
                .is_empty(),
            "{text:?}"
        );
        assert!(
            unified(text.as_bytes(), text.as_bytes(), &[], 3).is_empty(),
            "{text:?}"
        );
    }
}

#[test]
fn a_last_line_without_a_newline_is_a_line() {
    let lines: [&[u8]; 2] = [b"a\n", b"b"];
    assert_eq!(split_lines(b"a\nb"), lines);
    let lines: [&[u8]; 2] = [b"a\r\n", b"b\r"];
    assert_eq!(split_lines(b"a\r\nb\r"), lines);
    let lines: [&[u8]; 2] = [b"\n", b"\n"];
    assert_eq!(split_lines(b"\n\n"), lines);
    let none: [&[u8]; 0] = [];
    assert_eq!(split_lines(b""), none);
    assert_eq!(
        line_hunks(b"a\nb", b"a\nb\n").unwrap(),
        [Hunk::new(1..2, 1..2)]
    );
}

#[test]
fn hunks_sit_where_git_places_them() {
    // The histogram diff alone inserts `x` and then changes the last `a` into `a\n`; git compacts
    // that into one run of two inserted lines and a deleted last line (git 2.43.0, `git diff
    // --histogram -U0`, with its indent heuristic or without), and so does `line_hunks`.
    assert_eq!(
        line_hunks(b"a\na", b"x\na\na\n").unwrap(),
        [Hunk::new(0..0, 0..2), Hunk::new(1..2, 3..3)]
    );
    // Of two equal lines, the lower one goes: git's placement without its indent heuristic, which
    // `git merge-file` uses (`git diff`'s indent heuristic takes the upper one).
    let before = b"    x += 1;\nb\n}\n}\n    x += 1;\nif x {\n";
    let after = b"    x += 1;\nb\n}\n    x += 1;\nif x {\n";
    assert_eq!(line_hunks(before, after).unwrap(), [Hunk::new(3..4, 3..3)]);
}

fn headers(hunks: &[UnifiedHunk<'_>]) -> Vec<Header> {
    hunks
        .iter()
        .map(|hunk| {
            (
                hunk.before_start,
                hunk.before_lines,
                hunk.after_start,
                hunk.after_lines,
            )
        })
        .collect()
}

/// The unified hunks' headers of `before` against `after` with `context` lines.
fn unified_headers(before: &str, after: &str, context: u32) -> Vec<Header> {
    let (before, after) = (before.as_bytes(), after.as_bytes());
    headers(&unified(
        before,
        after,
        &line_hunks(before, after).unwrap(),
        context,
    ))
}

/// Lines `1\n` to `last\n`, the numbers in `changed` written `x<n>`.
fn numbered(last: u32, changed: &[u32]) -> String {
    (1..=last)
        .map(|n| {
            if changed.contains(&n) {
                format!("x{n}\n")
            } else {
                format!("{n}\n")
            }
        })
        .collect()
}

#[test]
fn unified_context_joins_close_hunks() {
    let before = numbered(20, &[]);
    // As `git diff -U3` (git 2.43.0) prints them: up to 6 unchanged lines between two changes
    // share a hunk, 7 do not.
    let cases: [(u32, &[Header]); 3] = [
        (4, &[(3, 11, 3, 11)]),
        (7, &[(3, 14, 3, 14)]),
        (8, &[(3, 7, 3, 7), (11, 7, 11, 7)]),
    ];
    for (apart, expected) in cases {
        let after = numbered(20, &[6, 6 + apart]);
        assert_eq!(
            line_hunks(before.as_bytes(), after.as_bytes())
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            unified_headers(&before, &after, 3),
            expected,
            "{apart} apart"
        );
    }
}

#[test]
fn unified_headers_match_git() {
    let ten = numbered(10, &[]);
    let twelve = numbered(12, &[]);
    let top_and_end = format!("0\n{}", numbered(11, &[]));
    // `(before, after, the headers git diff -U3 prints)`, from git 2.43.0 (`git diff --no-index`,
    // the same with `--histogram` and with `--no-indent-heuristic`).
    let cases: [(&str, &str, &[Header]); 5] = [
        (&ten, &numbered(10, &[5]), &[(2, 7, 2, 7)]),
        ("", "a\nb\nc\n", &[(0, 0, 1, 3)]),
        ("a\nb\nc\n", "", &[(1, 3, 0, 0)]),
        (&twelve, &top_and_end, &[(1, 3, 1, 4), (9, 4, 10, 3)]),
        ("a\nb\nc", "a\nb\nc\nd\n", &[(1, 3, 1, 4)]),
    ];
    for (before, after, expected) in cases {
        assert_eq!(
            unified_headers(before, after, 3),
            expected,
            "{before:?} to {after:?}"
        );
    }
    // A hunk's lines as git prints them: each change's removed lines before its added lines.
    let (before, after) = (b"a\nb\nc", b"a\nb\nc\nd\n");
    let shown = unified(before, after, &line_hunks(before, after).unwrap(), 3);
    assert_eq!(
        shown[0].lines,
        [
            DiffLine::Context(b"a\n"),
            DiffLine::Context(b"b\n"),
            DiffLine::Removed(b"c"),
            DiffLine::Added(b"c\n"),
            DiffLine::Added(b"d\n"),
        ]
    );
}

#[test]
fn unified_headers_match_git_without_context() {
    // From `git diff -U0` (git 2.43.0): a side with no line names the line before it, so 0 only
    // at the top of the file.
    let top_and_end = format!("0\n{}", numbered(11, &[]));
    let cases: [(&str, &str, &[Header]); 5] = [
        ("a\nb\nc\n", "a\nb\nx\nc\n", &[(2, 0, 3, 1)]),
        ("a\nb\nc\n", "a\nc\n", &[(2, 1, 1, 0)]),
        ("a\n", "x\na\n", &[(0, 0, 1, 1)]),
        (
            &numbered(12, &[]),
            &top_and_end,
            &[(0, 0, 1, 1), (12, 1, 12, 0)],
        ),
        ("a\nb\nc\nd\n", "a\nX\nY\nZ\nd\n", &[(2, 2, 2, 3)]),
    ];
    for (before, after, expected) in cases {
        assert_eq!(
            unified_headers(before, after, 0),
            expected,
            "{before:?} to {after:?}"
        );
    }
}

#[test]
fn is_text_refuses_nul_and_invalid_utf8() {
    assert!(is_text(b""));
    assert!(is_text(b"plain\r\n"));
    assert!(is_text("é, 한글, \u{1F600}\n".as_bytes()));
    assert!(!is_text(b"a\0b"));
    assert!(!is_text(b"\xff\n"));
    // A character cut short.
    assert!(!is_text(b"a\xc3"));
    // The NUL probe covers the first 8,000 bytes; UTF-8 holds for the whole.
    let nul_at = |at: usize| {
        let mut bytes = vec![b'a'; 10_000];
        bytes[at] = 0;
        bytes
    };
    assert!(!is_text(&nul_at(7_999)));
    assert!(is_text(&nul_at(8_000)));
    let mut invalid_at_the_end = vec![b'a'; 10_000];
    invalid_at_the_end[9_999] = 0xff;
    assert!(!is_text(&invalid_at_the_end));
}

#[test]
fn line_stats_count_removed_and_added_lines() {
    assert_eq!(line_stats(&[]), LineStats::default());
    let hunks = [Hunk::new(0..2, 0..1), Hunk::new(5..5, 4..7)];
    assert_eq!(line_stats(&hunks), LineStats::new(4, 2));
    // b and c become x, y and z; e goes.
    let hunks = line_hunks(b"a\nb\nc\nd\ne\n", b"a\nx\ny\nz\nd\n").unwrap();
    assert_eq!(line_stats(&hunks), LineStats::new(3, 3));
}

/// The line diff's bound: a side of more than `MAX_DIFF_LINES` lines or `MAX_DIFF_BYTES` bytes
/// gets no hunks (`TooLarge`), whatever the other side holds, an empty one too; a side at the
/// bound still gets them.
mod bound {
    use swarmops_merge::line_diff::{Hunk, MAX_DIFF_BYTES, MAX_DIFF_LINES, TooLarge, line_hunks};

    /// `count` lines `<n>\n`, line `edited` written `edited\n`.
    fn lines(count: usize, edited: usize) -> Vec<u8> {
        let text: String = (0..count)
            .map(|n| {
                if n == edited {
                    "edited\n".to_owned()
                } else {
                    format!("{n}\n")
                }
            })
            .collect();
        text.into_bytes()
    }

    /// The one hunk of an edit of line `at`.
    fn edit_of(at: u32) -> Vec<Hunk> {
        vec![Hunk::new(at..at + 1, at..at + 1)]
    }

    /// `MAX_DIFF_LINES` lines a side get their hunks; one line more, on either side, gets none,
    /// with or without its last `\n`.
    #[test]
    fn a_side_past_max_diff_lines_gets_no_hunks() {
        let at = lines(MAX_DIFF_LINES, usize::MAX);
        let edited = lines(MAX_DIFF_LINES, 7);
        assert_eq!(line_hunks(&at, &edited), Ok(edit_of(7)));
        let past = lines(MAX_DIFF_LINES + 1, usize::MAX);
        let unended = &past[..past.len() - 1];
        for side in [&past[..], unended] {
            assert_eq!(line_hunks(side, &edited), Err(TooLarge::default()));
            assert_eq!(line_hunks(&edited, side), Err(TooLarge::default()));
            assert_eq!(line_hunks(b"", side), Err(TooLarge::default()));
            assert_eq!(line_hunks(side, b""), Err(TooLarge::default()));
        }
    }

    /// `MAX_DIFF_BYTES` bytes a side, here in two lines, get their hunks; one byte more, on either
    /// side, gets none.
    #[test]
    fn a_side_past_max_diff_bytes_gets_no_hunks() {
        let line = |byte: u8| [vec![byte; MAX_DIFF_BYTES / 2 - 1], b"\n".to_vec()].concat();
        let at = [line(b'a'), line(b'b')].concat();
        let edited = [line(b'a'), line(b'c')].concat();
        assert_eq!(at.len(), MAX_DIFF_BYTES);
        assert_eq!(line_hunks(&at, &edited), Ok(edit_of(1)));
        let past = [&at[..], b"d"].concat();
        assert_eq!(line_hunks(&past, &edited), Err(TooLarge::default()));
        assert_eq!(line_hunks(&edited, &past), Err(TooLarge::default()));
    }
}
