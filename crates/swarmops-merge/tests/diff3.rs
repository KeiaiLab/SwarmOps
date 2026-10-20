//! Three-way text merge (`swarmops_merge::diff3`, storage spec §4): the merge identities, edits
//! that merge and edits that conflict, the final newline, CRLF and binary contents, and the two
//! oracles of storage spec §10 item 4 — `diffy` 0.5.2 in process, and the output of
//! `git merge-file` (git 2.43) on a committed corpus (`tests/fixtures/merge_file/`).

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use base64::Engine as _;
use proptest::prelude::*;
use proptest::sample::Index;
use swarmops_merge::conflict::{ConflictHunk, ContentMerge};
use swarmops_merge::diff3::merge_content;
use swarmops_merge::line_diff::split_lines;

/// A line of a few contents, so that lines repeat as code's lines do, ending in `\n` or `\r\n`.
fn line() -> impl Strategy<Value = Vec<u8>> {
    let contents = prop::sample::select(vec!["", "a", "b", "c", "}", "fn f() {", "    x += 1;"]);
    let ends = prop::sample::select(vec!["\n", "\r\n"]);
    (contents, ends).prop_map(|(content, end)| [content, end].concat().into_bytes())
}

/// The bytes of `lines`; unless `ends_a_line`, without the last `\n`.
fn joined(lines: &[Vec<u8>], ends_a_line: bool) -> Vec<u8> {
    let mut bytes = lines.concat();
    if !ends_a_line && bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

/// A file's content: some lines, its last one with its `\n` or without, or (one time in four)
/// bytes of any kind, text or not.
fn content() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => (prop::collection::vec(line(), 0..16), any::<bool>())
            .prop_map(|(lines, ends)| joined(&lines, ends)),
        1 => prop::collection::vec(any::<u8>(), 0..48),
    ]
}

/// One edit of a line vector; its position is taken modulo the length it applies to.
#[derive(Clone, Debug)]
enum Edit {
    Insert(Index, Vec<u8>),
    Delete(Index),
    Replace(Index, Vec<u8>),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<Index>(), line()).prop_map(|(at, line)| Edit::Insert(at, line)),
        any::<Index>().prop_map(Edit::Delete),
        (any::<Index>(), line()).prop_map(|(at, line)| Edit::Replace(at, line)),
    ]
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
            Edit::Delete(_) | Edit::Replace(..) => {}
        }
    }
    lines
}

/// A base and two sides, each the base after a few edits of its own. The lines repeat, so the
/// sides' edits often meet: some merges are clean, many conflict.
fn triples() -> impl Strategy<Value = (Vec<u8>, Vec<u8>, Vec<u8>)> {
    (
        prop::collection::vec(line(), 0..16),
        prop::collection::vec(edit(), 0..5),
        prop::collection::vec(edit(), 0..5),
        any::<[bool; 3]>(),
    )
        .prop_map(|(base, ours, theirs, ends)| {
            (
                joined(&base, ends[0]),
                joined(&edited(base.clone(), &ours), ends[1]),
                joined(&edited(base, &theirs), ends[2]),
            )
        })
}

/// A base and two sides whose edits are generated apart or together, with what they merge to.
#[derive(Clone, Debug)]
struct Sides {
    base: Vec<u8>,
    ours: Vec<u8>,
    theirs: Vec<u8>,
    /// The base with both sides' edits applied.
    both: Vec<u8>,
}

/// `count` lines `<name> 0\n`, `<name> 1\n`, …: no two the same, and none a line of another name.
fn named(name: &str, count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|n| format!("{name} {n}\n").into_bytes())
        .collect()
}

/// `lines` with the lines `at` replaced by `with`.
fn spliced(lines: &[Vec<u8>], at: Range<usize>, with: &[Vec<u8>]) -> Vec<u8> {
    [&lines[..at.start], with, &lines[at.end..]]
        .concat()
        .concat()
}

/// Two edits of a base of distinct lines, one for each side, with at least one unchanged line
/// between them: each replaces a run of base lines (none, for an insertion) by new lines (none,
/// for a deletion), and every new line is a line no other side has. Either side's edit may come
/// first.
fn separated() -> impl Strategy<Value = Sides> {
    (
        (0..6_usize, 0..4_usize, 1..4_usize, 0..4_usize, 0..6_usize),
        (0..4_usize, 0..4_usize, any::<bool>()),
    )
        .prop_map(
            |((lead, first, gap, second, tail), (first_new, second_new, ours_first))| {
                // Each edit changes something: an empty run gets a new line.
                let first_new = if first == 0 {
                    first_new.max(1)
                } else {
                    first_new
                };
                let second_new = if second == 0 {
                    second_new.max(1)
                } else {
                    second_new
                };
                let base = named("base", lead + first + gap + second + tail);
                let first_at = lead..lead + first;
                let second_at = first_at.end + gap..first_at.end + gap + second;
                let (first_name, second_name) = if ours_first {
                    ("ours", "theirs")
                } else {
                    ("theirs", "ours")
                };
                let (first_lines, second_lines) =
                    (named(first_name, first_new), named(second_name, second_new));
                let with_first = spliced(&base, first_at.clone(), &first_lines);
                let with_second = spliced(&base, second_at.clone(), &second_lines);
                let both = [
                    &base[..first_at.start],
                    &first_lines[..],
                    &base[first_at.end..second_at.start],
                    &second_lines[..],
                    &base[second_at.end..],
                ]
                .concat()
                .concat();
                let (ours, theirs) = if ours_first {
                    (with_first, with_second)
                } else {
                    (with_second, with_first)
                };
                Sides {
                    base: base.concat(),
                    ours,
                    theirs,
                    both,
                }
            },
        )
}

/// Two edits of a base of distinct lines whose runs of base lines share at least one line, and
/// which differ: new lines that only that side has, or (two deletions) other runs. `both` is
/// unused: they conflict.
fn overlapping() -> impl Strategy<Value = Sides> {
    (
        (0..6_usize, 1..6_usize, 0..6_usize),
        (any::<Index>(), any::<[Index; 4]>()),
        (0..3_usize, 0..3_usize),
    )
        .prop_filter_map(
            "two deletions of the same lines are the same change",
            |((lead, window, tail), (shared, ends), (ours_new, theirs_new))| {
                // Both runs hold the window's line `shared`.
                let shared = shared.index(window);
                let run = |start: Index, end: Index| {
                    lead + start.index(shared + 1)..lead + shared + 1 + end.index(window - shared)
                };
                let (ours_at, theirs_at) = (run(ends[0], ends[1]), run(ends[2], ends[3]));
                if ours_new == 0 && theirs_new == 0 && ours_at == theirs_at {
                    return None;
                }
                let base = named("base", lead + window + tail);
                Some(Sides {
                    ours: spliced(&base, ours_at, &named("ours", ours_new)),
                    theirs: spliced(&base, theirs_at, &named("theirs", theirs_new)),
                    base: base.concat(),
                    both: Vec::new(),
                })
            },
        )
}

/// `hunk` with its two sides swapped, as a merge with them swapped reports it.
fn swapped(hunk: &ConflictHunk) -> ConflictHunk {
    ConflictHunk {
        base: hunk.base.clone(),
        ours: hunk.theirs.clone(),
        theirs: hunk.ours.clone(),
        base_text: hunk.base_text.clone(),
        ours_text: hunk.theirs_text.clone(),
        theirs_text: hunk.ours_text.clone(),
    }
}

/// The bytes of the lines `at` of `bytes`.
fn lines_at(bytes: &[u8], at: &Range<u32>) -> Vec<u8> {
    split_lines(bytes)[at.start as usize..at.end as usize].concat()
}

/// Checks what conflict hunks promise: in file order, with at least one base line between two of
/// them, each text the bytes of its side's lines, and the two sides' texts different.
fn check_hunks(base: &[u8], ours: &[u8], theirs: &[u8], hunks: &[ConflictHunk]) {
    assert!(!hunks.is_empty(), "a conflicted merge has a hunk");
    for (n, hunk) in hunks.iter().enumerate() {
        assert_eq!(hunk.base_text, lines_at(base, &hunk.base), "{hunk:?}");
        assert_eq!(hunk.ours_text, lines_at(ours, &hunk.ours), "{hunk:?}");
        assert_eq!(hunk.theirs_text, lines_at(theirs, &hunk.theirs), "{hunk:?}");
        assert_ne!(
            hunk.ours_text, hunk.theirs_text,
            "the same change: {hunk:?}"
        );
        if let Some(before) = n.checked_sub(1).map(|before| &hunks[before]) {
            assert!(hunk.base.start > before.base.end, "touches: {hunk:?}");
            assert!(hunk.ours.start > before.ours.end, "touches: {hunk:?}");
            assert!(hunk.theirs.start > before.theirs.end, "touches: {hunk:?}");
        }
    }
}

proptest! {
    /// `merge(b, a, a) = a`, `merge(b, b, x) = x` and `merge(b, x, b) = x` (umbrella §8), for
    /// contents of any kind.
    #[test]
    fn merge_identities_hold(b in content(), a in content()) {
        prop_assert_eq!(merge_content(&b, &a, &a), ContentMerge::Clean(a.clone()));
        prop_assert_eq!(merge_content(&b, &b, &a), ContentMerge::Clean(a.clone()));
        prop_assert_eq!(merge_content(&b, &a, &b), ContentMerge::Clean(a.clone()));
    }

    /// Edits with an unchanged line between them merge, and the merge holds both.
    #[test]
    fn separated_edits_always_merge_and_keep_both(sides in separated()) {
        prop_assert_eq!(
            merge_content(&sides.base, &sides.ours, &sides.theirs),
            ContentMerge::Clean(sides.both.clone())
        );
    }

    /// The merge does not prefer a side: with the sides swapped it is as clean, to the same bytes,
    /// or conflicts in the same places with each hunk's sides swapped; and its conflict hunks hold
    /// what they promise.
    #[test]
    fn swapping_sides_swaps_conflicts((base, ours, theirs) in triples()) {
        let merged = merge_content(&base, &ours, &theirs);
        let swapped_merge = merge_content(&base, &theirs, &ours);
        match (&merged, &swapped_merge) {
            (ContentMerge::Clean(merged), ContentMerge::Clean(swapped_merge)) => {
                prop_assert_eq!(merged, swapped_merge);
            }
            (ContentMerge::Conflicted(hunks), ContentMerge::Conflicted(swapped_hunks)) => {
                check_hunks(&base, &ours, &theirs, hunks);
                let swapped_back: Vec<ConflictHunk> = swapped_hunks.iter().map(swapped).collect();
                prop_assert_eq!(hunks, &swapped_back);
            }
            (ContentMerge::Binary, ContentMerge::Binary) => {}
            _ => prop_assert!(false, "{merged:?} against {swapped_merge:?}"),
        }
    }

    /// Against `diffy` 0.5.2: edits apart merge clean in both, to the same bytes; edits of shared
    /// lines conflict in both.
    #[test]
    fn agrees_with_diffy_on_separated_edits(apart in separated(), together in overlapping()) {
        let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).expect("generated as text");
        let ContentMerge::Clean(merged) = merge_content(&apart.base, &apart.ours, &apart.theirs)
        else {
            panic!("{apart:?} conflicts");
        };
        prop_assert_eq!(
            diffy::merge(&text(&apart.base), &text(&apart.ours), &text(&apart.theirs)),
            Ok(text(&merged))
        );
        let merged = merge_content(&together.base, &together.ours, &together.theirs);
        prop_assert!(matches!(merged, ContentMerge::Conflicted(_)), "{together:?}: {merged:?}");
        prop_assert!(
            diffy::merge(&text(&together.base), &text(&together.ours), &text(&together.theirs))
                .is_err(),
            "{together:?}"
        );
    }
}

/// Lines `line 0\n` to `line <count - 1>\n`, so that line `n` is the 0-based line `n` of a range.
fn numbered(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("line {n}\n")).collect()
}

/// `lines` with line `at` (0-based) replaced by `with`, which may be several lines or none.
fn replaced(lines: &[String], at: usize, with: &str) -> String {
    let mut lines = lines.to_vec();
    with.clone_into(&mut lines[at]);
    lines.concat()
}

/// `lines` with `with` inserted before line `at` (0-based; at the end if `at` is their number).
fn inserted(lines: &[String], at: usize, with: &str) -> String {
    let mut lines = lines.to_vec();
    lines.insert(at, with.to_owned());
    lines.concat()
}

/// `merge_content` of three texts.
fn merge(base: &str, ours: &str, theirs: &str) -> ContentMerge {
    merge_content(base.as_bytes(), ours.as_bytes(), theirs.as_bytes())
}

/// A clean merge to `text`.
fn clean(text: &str) -> ContentMerge {
    ContentMerge::Clean(text.as_bytes().to_vec())
}

/// One conflict hunk over the given ranges, with the given texts.
fn hunk(ranges: [Range<u32>; 3], texts: [&str; 3]) -> ConflictHunk {
    let [base, ours, theirs] = ranges;
    let [base_text, ours_text, theirs_text] = texts.map(|text| text.as_bytes().to_vec());
    ConflictHunk {
        base,
        ours,
        theirs,
        base_text,
        ours_text,
        theirs_text,
    }
}

/// Edits of lines 3 and 4, one on each side, touch: one side's edit ends at the line where the
/// other's starts. They conflict in one hunk over the base's lines 3..5, as git merge-file has
/// them conflict; with one unchanged line between them they merge.
#[test]
fn touching_edits_conflict() {
    let base = numbered(8);
    let ours = replaced(&base, 3, "ours 3\n");
    let theirs = replaced(&base, 4, "theirs 4\n");
    let expected = hunk(
        [3..5, 3..5, 3..5],
        ["line 3\nline 4\n", "ours 3\nline 4\n", "line 3\ntheirs 4\n"],
    );
    assert_eq!(
        merge(&base.concat(), &ours, &theirs),
        ContentMerge::Conflicted(vec![expected.clone()])
    );
    assert_eq!(
        merge(&base.concat(), &theirs, &ours),
        ContentMerge::Conflicted(vec![swapped(&expected)])
    );
    // A deletion of lines 3 and 4 against an edit of line 5 touches too: base 3..6, and ours has
    // no line of it left.
    let ours = [&base[..3], &base[5..]].concat().concat();
    let theirs = replaced(&base, 5, "theirs 5\n");
    assert_eq!(
        merge(&base.concat(), &ours, &theirs),
        ContentMerge::Conflicted(vec![hunk(
            [3..6, 3..4, 3..6],
            [
                "line 3\nline 4\nline 5\n",
                "line 5\n",
                "line 3\nline 4\ntheirs 5\n",
            ],
        )])
    );
    // Line 4 between the edits of lines 3 and 5: they merge.
    let ours = replaced(&base, 3, "ours 3\n");
    let both = replaced(&split_strings(&ours), 5, "theirs 5\n");
    assert_eq!(merge(&base.concat(), &ours, &theirs), clean(&both));
}

/// `text`'s lines, as owned strings.
fn split_strings(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_owned).collect()
}

/// Two insertions at one point conflict, whatever they insert, unless they insert the same
/// lines: then the merge holds them once. Insertions with a line between them merge.
#[test]
fn insertions_at_one_point_conflict_unless_equal() {
    let base = numbered(6);
    let ours = inserted(&base, 3, "ours a\n");
    let theirs = inserted(&base, 3, "theirs a\ntheirs b\n");
    assert_eq!(
        merge(&base.concat(), &ours, &theirs),
        ContentMerge::Conflicted(vec![hunk(
            [3..3, 3..4, 3..5],
            ["", "ours a\n", "theirs a\ntheirs b\n"],
        )])
    );
    // At the top and at the end alike.
    let (ours, theirs) = (
        inserted(&base, 0, "ours a\n"),
        inserted(&base, 0, "theirs a\n"),
    );
    assert_eq!(
        merge(&base.concat(), &ours, &theirs),
        ContentMerge::Conflicted(vec![hunk(
            [0..0, 0..1, 0..1],
            ["", "ours a\n", "theirs a\n"]
        )])
    );
    let (ours, theirs) = (
        inserted(&base, 6, "ours a\n"),
        inserted(&base, 6, "theirs a\n"),
    );
    assert_eq!(
        merge(&base.concat(), &ours, &theirs),
        ContentMerge::Conflicted(vec![hunk(
            [6..6, 6..7, 6..7],
            ["", "ours a\n", "theirs a\n"]
        )])
    );
    // The same lines at one point, with an edit of line 0 on ours so that the sides differ.
    let same = inserted(&base, 3, "both a\nboth b\n");
    let ours = replaced(&split_strings(&same), 0, "ours 0\n");
    assert_eq!(merge(&base.concat(), &ours, &same), clean(&ours));
    // An insertion before line 3 and one before line 4: line 3 is between them.
    let ours = inserted(&base, 3, "ours a\n");
    let theirs = inserted(&base, 4, "theirs a\n");
    let both = inserted(&split_strings(&ours), 5, "theirs a\n");
    assert_eq!(merge(&base.concat(), &ours, &theirs), clean(&both));
    // An insertion touches an edit of the line before it, or of the line after it.
    for edited in [2, 3] {
        let theirs = replaced(&base, edited, "theirs\n");
        assert!(
            matches!(
                merge(&base.concat(), &ours, &theirs),
                ContentMerge::Conflicted(hunks) if hunks.len() == 1
            ),
            "an insertion before line 3 against an edit of line {edited}"
        );
    }
}

/// Both sides making the same change is one change: the same lines edited alike, deleted alike,
/// or inserted alike, beside the other changes of either side.
#[test]
fn identical_changes_merge() {
    let base = numbered(10);
    let same = replaced(&base, 2, "both 2\n");
    let ours = replaced(&split_strings(&same), 7, "ours 7\n");
    assert_eq!(merge(&base.concat(), &ours, &same), clean(&ours));
    assert_eq!(merge(&base.concat(), &same, &ours), clean(&ours));
    // Lines 4 to 6 deleted on both sides; theirs also edits line 9.
    let deleted = [&base[..4], &base[7..]].concat();
    let theirs = replaced(&deleted, 6, "theirs 9\n");
    assert_eq!(
        merge(&base.concat(), &deleted.concat(), &theirs),
        clean(&theirs)
    );
    // The same lines in place of lines 1 to 3 on both sides, and ours edits line 5 as well.
    let rewritten = [&base[..1], &["new a\n".to_owned()], &base[4..]].concat();
    let ours = replaced(&rewritten, 3, "ours 5\n");
    assert_eq!(
        merge(&base.concat(), &ours, &rewritten.concat()),
        clean(&ours)
    );
}

/// The last line without its `\n` is a line of its own content: dropping or adding the final
/// newline changes the last line, so it merges with edits apart from that line and conflicts
/// with an edit of it or of the line before it.
#[test]
fn a_missing_final_newline_is_a_change_of_the_last_line() {
    let base = "a\nb\nc\nd\n";
    let no_newline = "a\nb\nc\nd";
    assert_eq!(merge(base, no_newline, "A\nb\nc\nd\n"), clean("A\nb\nc\nd"));
    assert_eq!(
        merge(base, no_newline, "a\nb\nc\nD\n"),
        ContentMerge::Conflicted(vec![hunk([3..4, 3..4, 3..4], ["d\n", "d", "D\n"])])
    );
    assert_eq!(
        merge(base, no_newline, "a\nb\nC\nd\n"),
        ContentMerge::Conflicted(vec![hunk([2..4, 2..4, 2..4], ["c\nd\n", "c\nd", "C\nd\n"])])
    );
    // Both drop it, and ours edits line 0 too: the same change.
    assert_eq!(merge(base, "A\nb\nc\nd", no_newline), clean("A\nb\nc\nd"));
    // Adding it is a change of the last line as well.
    assert_eq!(merge(no_newline, base, "A\nb\nc\nd"), clean("A\nb\nc\nd\n"));
    // An empty last line is a line: "d\n\n" ends in one, "d\n" does not.
    assert_eq!(
        merge(base, "a\nb\nc\nd\n\n", "A\nb\nc\nd\n"),
        clean("A\nb\nc\nd\n\n")
    );
}

/// Lines end after `\n` and `\r` belongs to its line: CRLF lines merge byte for byte, a line
/// turned from CRLF to LF is a change of that line, and a lone `\r` does not end a line.
#[test]
fn crlf_lines_merge_byte_for_byte() {
    let base = "a\r\nb\r\nc\r\nd\r\ne\r\n";
    assert_eq!(
        merge(
            base,
            "A\r\nb\r\nc\r\nd\r\ne\r\n",
            "a\r\nb\r\nc\r\nd\r\nE\r\n"
        ),
        clean("A\r\nb\r\nc\r\nd\r\nE\r\n")
    );
    // Theirs turns line 4 to LF: one changed line, merged beside ours' edit of line 0.
    assert_eq!(
        merge(base, "A\r\nb\r\nc\r\nd\r\ne\r\n", "a\r\nb\r\nc\r\nd\r\ne\n"),
        clean("A\r\nb\r\nc\r\nd\r\ne\n")
    );
    // Ours turns line 2 to LF, theirs edits it: they conflict, each text byte for byte.
    assert_eq!(
        merge(base, "a\r\nb\r\nc\nd\r\ne\r\n", "a\r\nb\r\nC\r\nd\r\ne\r\n"),
        ContentMerge::Conflicted(vec![hunk([2..3, 2..3, 2..3], ["c\r\n", "c\n", "C\r\n"])])
    );
    // `a\rb\n` is one line: edits of `b` after the `\r` and of line 2 merge, with line 1 between.
    assert_eq!(
        merge("a\rb\nc\nd\n", "a\rB\nc\nd\n", "a\rb\nc\nD\n"),
        clean("a\rB\nc\nD\n")
    );
}

/// A content that is not text (a NUL in its first 8,000 bytes, or not UTF-8) is never merged by
/// lines: changed on both sides, the merge is `Binary`; changed on one side, it is that side.
#[test]
fn binary_sides_are_not_line_merged() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"A\nb\nc\nd\n";
    // Theirs' edit of line 3 would merge as text, but holds a NUL, or a byte UTF-8 does not have.
    for theirs in [&b"a\nb\nc\nd\0\n"[..], &b"a\nb\nc\n\xff\n"[..]] {
        assert_eq!(merge_content(base, ours, theirs), ContentMerge::Binary);
        assert_eq!(merge_content(base, theirs, ours), ContentMerge::Binary);
        // One side changed: that side, binary or not.
        assert_eq!(
            merge_content(base, base, theirs),
            ContentMerge::Clean(theirs.to_vec())
        );
        assert_eq!(
            merge_content(base, theirs, base),
            ContentMerge::Clean(theirs.to_vec())
        );
        assert_eq!(
            merge_content(base, theirs, theirs),
            ContentMerge::Clean(theirs.to_vec())
        );
    }
    // A base that is not text: both sides are text, but there is nothing to diff them against.
    assert_eq!(
        merge_content(b"a\0\nb\nc\nd\n", ours, b"a\nb\nc\nD\n"),
        ContentMerge::Binary
    );
    // A NUL past the first 8,000 bytes is text (git's probe): merged by lines.
    let mut lines = numbered(1_000);
    lines.push("tail\0\n".to_owned());
    let base = lines.concat();
    assert!(base.find('\0').unwrap() >= 8_000);
    let ours = replaced(&lines, 0, "ours 0\n");
    let theirs = replaced(&lines, 1_000, "TAIL\0\n");
    let both = replaced(&split_strings(&ours), 1_000, "TAIL\0\n");
    assert_eq!(merge(&base, &ours, &theirs), clean(&both));
}

/// What `git merge-file -p` made of one corpus case (`expected.json`).
struct GitMerge {
    /// Its exit status: the number of conflicts, 0 for a clean merge.
    status: usize,
    /// What it printed: the merged file, with conflict markers where it conflicts.
    output: Vec<u8>,
}

/// `name`'s JSON object of `dir`, keyed by case name.
fn read_json(dir: &Path, name: &str) -> serde_json::Map<String, serde_json::Value> {
    let path = dir.join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    match serde_json::from_str(&text) {
        Ok(serde_json::Value::Object(map)) => map,
        other => panic!("{} is not a JSON object: {other:?}", path.display()),
    }
}

/// The corpus of `tests/fixtures/merge_file/`: each case's base, ours and theirs merged by
/// `merge_content` as `git merge-file -p` (git 2.43, recorded by `generate.sh`) merged them —
/// clean exactly when git's exit status is 0, to git's bytes, and conflicted in as many hunks as
/// git's exit status counts conflicts. A case `known_differences.json` names is skipped, with the
/// reason it records; that file starts empty.
#[test]
fn agrees_with_the_git_merge_file_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge_file");
    let expected: BTreeMap<String, GitMerge> = read_json(&dir, "expected.json")
        .into_iter()
        .map(|(name, value)| {
            let status = value["status"].as_u64().expect("an exit status");
            let output = base64::engine::general_purpose::STANDARD
                .decode(value["output"].as_str().expect("an output"))
                .expect("base64");
            let status = usize::try_from(status).expect("a small exit status");
            (name, GitMerge { status, output })
        })
        .collect();
    let known: BTreeMap<String, String> = read_json(&dir, "known_differences.json")
        .into_iter()
        .map(|(name, reason)| {
            let reason = reason.as_str().unwrap_or_default().to_owned();
            assert!(
                !reason.is_empty(),
                "known difference {name} without a reason"
            );
            (name, reason)
        })
        .collect();
    let mut cases: Vec<String> = std::fs::read_dir(dir.join("cases"))
        .expect("the cases")
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no case under {}", dir.display());
    assert_eq!(
        cases,
        expected.keys().cloned().collect::<Vec<_>>(),
        "cases/ and expected.json name other cases: run generate.sh"
    );
    for name in known.keys() {
        assert!(cases.contains(name), "known difference {name} is no case");
    }
    for name in &cases {
        if let Some(reason) = known.get(name) {
            eprintln!("skipped {name}: {reason}");
            continue;
        }
        let read = |side: &str| std::fs::read(dir.join("cases").join(name).join(side)).unwrap();
        let (base, ours, theirs) = (read("base"), read("ours"), read("theirs"));
        let git = &expected[name];
        match merge_content(&base, &ours, &theirs) {
            ContentMerge::Clean(merged) => {
                assert_eq!(
                    git.status, 0,
                    "{name}: clean here, {} conflicts in git",
                    git.status
                );
                assert_eq!(
                    String::from_utf8_lossy(&merged),
                    String::from_utf8_lossy(&git.output),
                    "{name}"
                );
                assert_eq!(merged, git.output, "{name}");
            }
            ContentMerge::Conflicted(hunks) => {
                check_hunks(&base, &ours, &theirs, &hunks);
                assert_eq!(
                    hunks.len(),
                    git.status,
                    "{name}: {} conflict hunks here, git counts {}",
                    hunks.len(),
                    git.status
                );
            }
            ContentMerge::Binary => panic!("{name}: binary here, git merges it as text"),
        }
    }
}
