//! The changed paths a plan reads: a list's entries, their bounds, and the store's diff.

use camino::Utf8PathBuf;
use swarmops_api::{
    ChangeInput, ChangeOp, CheckPlanWidened, CommitReq, ContentInput, Identity, StoreOps,
};

use super::{CHECK_FILE_PATHS, Changed, PLAN_DIFF_PAGE, Sep, read_bounded, read_from};
use crate::LocalOps;
use crate::check::{MAX_PLAN_LIST_BYTES, MAX_PLAN_PATH_BYTES, MAX_PLAN_PATHS};

/// The five paths of the fixture, one a line.
const LINES: &[u8] =
    include_bytes!("../../../../swarmops-cli/tests/fixtures/check/plan/changed.txt");

/// The same five, NUL-ended.
const NULS: &[u8] =
    include_bytes!("../../../../swarmops-cli/tests/fixtures/check/plan/changed.nul");

/// What reading `bytes` came to, and the paths seen, in order.
fn read(bytes: &[u8], sep: Sep) -> (Changed, Vec<String>) {
    let mut seen = Vec::new();
    let changed = Changed::from_list(bytes, sep, &mut |path| seen.push(path.to_owned()));
    (changed, seen)
}

/// The cause for which the one entry `entry` widens a list of lines, if any.
fn line(entry: &[u8]) -> Option<CheckPlanWidened> {
    read(entry, Sep::Line).0.cause()
}

/// Lines and NUL-ended entries read as the same paths, in the caller's order, one of them with
/// a space.
#[test]
fn lines_and_nuls_read_the_same_paths() {
    let expected = [
        "crates/a/src/lib.rs",
        "docs/status/M1/a b.txt",
        "web/app/page.tsx",
        "docs/guide.md",
        "README.md",
    ];
    for (bytes, sep) in [(LINES, Sep::Line), (NULS, Sep::Nul)] {
        let (changed, seen) = read(bytes, sep);
        assert_eq!(seen, expected, "{sep:?}");
        assert_eq!((changed.count(), changed.cause()), (5, None), "{sep:?}");
        assert!(!changed.check_file(), "{sep:?}");
    }
}

/// A last entry without its end is taken; empty entries are passed over; an empty list is a
/// change of no path, which is not an unknown one.
#[test]
fn a_last_entry_without_its_end_is_taken_and_blank_ones_passed_over() {
    let (changed, seen) = read(b"a\n\n\nb/c", Sep::Line);
    assert_eq!(seen, ["a", "b/c"]);
    assert_eq!((changed.count(), changed.cause()), (2, None));
    let (changed, seen) = read(b"\0a\0\0b/c", Sep::Nul);
    assert_eq!(seen, ["a", "b/c"]);
    assert_eq!((changed.count(), changed.cause()), (2, None));

    for sep in [Sep::Line, Sep::Nul] {
        let (empty, seen) = read(b"", sep);
        assert_eq!((empty.count(), empty.cause(), seen.len()), (0, None, 0));
        assert_eq!(empty, Changed::none());
    }
    assert_eq!(
        Changed::unknown().cause(),
        Some(CheckPlanWidened::PathsUnknown)
    );
}

/// Each kind of entry that is no changed path widens the plan, `path_unreadable`, and is not
/// seen; the paths around it still are.
#[test]
fn an_entry_that_is_no_path_is_unreadable() {
    let unreadable = Some(CheckPlanWidened::PathUnreadable);
    let too_long = "a".repeat(MAX_PLAN_PATH_BYTES + 1);
    let cases: &[&[u8]] = &[
        b"/etc/passwd",
        b"../x",
        b"a/../b",
        b"./a",
        b"a//b",
        b"a/",
        b"a\xffb",
        too_long.as_bytes(),
        b"\"docs/a b.md\"",
        b"a\tb",
        b"a\rb",
        b"a\x7fb",
    ];
    for entry in cases {
        assert_eq!(
            line(entry),
            unreadable,
            "{:?}",
            String::from_utf8_lossy(entry)
        );
    }
    let longest = "a".repeat(MAX_PLAN_PATH_BYTES);
    assert_eq!(line(longest.as_bytes()), None);

    // The paths before and after it are read all the same; the unreadable one is not counted.
    let (changed, seen) = read(b"a\n../x\nb\n", Sep::Line);
    assert_eq!(seen, ["a", "b"]);
    assert_eq!((changed.count(), changed.cause()), (2, unreadable));
}

/// A name a tool would quote reads NUL-ended as it is: a quote, a tab or a line feed in a
/// path is unreadable only in a list of lines.
#[test]
fn a_nul_ended_list_takes_any_repository_path() {
    let (changed, seen) = read(b"\"quoted\"\0a\tb\0c\nd\0", Sep::Nul);
    assert_eq!(seen, ["\"quoted\"", "a\tb", "c\nd"]);
    assert_eq!((changed.count(), changed.cause()), (3, None));

    // A NUL in a list of lines is a control character: the two names glued by it are not read
    // as one path.
    let (glued, seen) = read(b"a\0b\n", Sep::Line);
    assert_eq!(seen.len(), 0);
    assert_eq!(glued.cause(), Some(CheckPlanWidened::PathUnreadable));

    // What is still no repository path stays unreadable.
    let (dotted, _) = read(b"../x\0", Sep::Nul);
    assert_eq!(dotted.cause(), Some(CheckPlanWidened::PathUnreadable));
}

/// 100,000 entries are read; one more cuts the plan, `paths_truncated`, and is not seen.
#[test]
fn more_entries_than_the_bound_cut_the_plan() {
    let list = |entries: u64| -> Vec<u8> {
        (0..entries)
            .flat_map(|i| format!("p/{i}\n").into_bytes())
            .collect()
    };
    let (whole, seen) = read(&list(MAX_PLAN_PATHS), Sep::Line);
    assert_eq!((whole.count(), whole.cause()), (MAX_PLAN_PATHS, None));
    assert_eq!(seen.len(), 100_000);

    let (cut, seen) = read(&list(MAX_PLAN_PATHS + 1), Sep::Line);
    assert_eq!(
        (cut.count(), cut.cause()),
        (MAX_PLAN_PATHS, Some(CheckPlanWidened::PathsTruncated))
    );
    assert_eq!(seen.len(), 100_000);

    // Paths given one by one share the bound with a list read after them.
    let mut both = Changed::none();
    let mut count = 0_u64;
    assert!(both.path("first", &mut |_| count += 1));
    both.list(&list(MAX_PLAN_PATHS), Sep::Line, &mut |_| count += 1);
    assert_eq!(both.cause(), Some(CheckPlanWidened::PathsTruncated));
    assert_eq!((both.count(), count), (MAX_PLAN_PATHS, MAX_PLAN_PATHS));
    assert!(
        !both.path("more", &mut |_| count += 1),
        "nothing more is read"
    );
}

/// A list one byte past 16 MiB is read to the bound, its last partial entry left out, and cuts
/// the plan; a list of exactly the bound is read whole.
#[test]
fn a_list_past_its_bound_is_read_to_it_and_cut() {
    let limit = usize::try_from(MAX_PLAN_LIST_BYTES).unwrap();
    // Entries of 4,096 bytes with their ends: 4,097 bytes each, so the bound falls inside one.
    let entry = format!("{}\n", "a".repeat(MAX_PLAN_PATH_BYTES));
    let mut list = entry.repeat(limit / entry.len() + 1).into_bytes();
    list.truncate(limit + 1);
    let whole_entries = u64::try_from(limit / entry.len()).unwrap();
    let (cut, seen) = read(&list, Sep::Line);
    assert_eq!(
        (cut.count(), cut.cause()),
        (whole_entries, Some(CheckPlanWidened::PathsTruncated))
    );
    assert!(seen.iter().all(|path| path.len() == MAX_PLAN_PATH_BYTES));

    list.truncate(limit);
    let (whole, _) = read(&list, Sep::Line);
    assert_eq!((whole.count(), whole.cause()), (whole_entries + 1, None));
}

/// The first cause in the causes' own order is the one named, whatever order they came in.
#[test]
fn the_first_cause_in_order_is_the_one_named() {
    let mut changed = Changed::none();
    changed.widen(CheckPlanWidened::TargetUnread);
    assert_eq!(changed.cause(), Some(CheckPlanWidened::TargetUnread));
    changed.widen(CheckPlanWidened::PathUnreadable);
    assert_eq!(changed.cause(), Some(CheckPlanWidened::PathUnreadable));
    changed.widen(CheckPlanWidened::CheckFileChanged);
    assert_eq!(changed.cause(), Some(CheckPlanWidened::PathUnreadable));
    changed.widen(CheckPlanWidened::PathsUnknown);
    assert_eq!(changed.cause(), Some(CheckPlanWidened::PathsUnknown));
}

/// The three paths whose change is a change of the checks are told; a path beside them is not.
#[test]
fn the_check_file_s_paths_are_recognised() {
    assert_eq!(
        CHECK_FILE_PATHS,
        [
            ".swarmops/checks.toml",
            ".swarmops/workspaces.json",
            ".swarmops"
        ]
    );
    for path in CHECK_FILE_PATHS {
        let (changed, seen) = read(format!("a\n{path}\n").as_bytes(), Sep::Line);
        assert!(changed.check_file(), "{path}");
        assert_eq!((seen.len(), changed.cause()), (2, None), "{path}");
    }
    for path in [
        ".swarmops/access.toml",
        "x/.swarmops/checks.toml",
        ".swarmops2",
    ] {
        assert!(!read(path.as_bytes(), Sep::Line).0.check_file(), "{path}");
    }
}

/// A store whose `main` holds one file, and the identity that commits to it.
fn store() -> (tempfile::TempDir, LocalOps, Identity) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let (ops, _) = LocalOps::init(&root, "test").unwrap();
    let who = Identity::parse("agent/test").unwrap();
    (dir, ops, who)
}

/// A changeset of `ops` that puts `paths` (or deletes those of `deleted`), on `parent`.
fn commit(ops: &LocalOps, who: &Identity, parent: Option<&str>, paths: &[String]) -> String {
    let changes = (paths.iter())
        .map(|path| {
            let mut change = ChangeInput::new(path.clone(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(format!("{path}\n")));
            change
        })
        .collect();
    let mut req = CommitReq::new(changes, "change".to_owned());
    req.parent = parent.map(Into::into);
    if parent.is_none() {
        req.advance = Some("main".to_owned());
    }
    ops.commit(who, req).unwrap().changeset
}

/// A diff of three pages is read to its end, in path order, every status a changed path: an
/// added file, a modified one and a deleted one.
#[test]
fn the_store_s_diff_is_read_to_its_end() {
    let (_dir, ops, who) = store();
    let base = commit(&ops, &who, None, &["kept".to_owned(), "gone".to_owned()]);
    let added: Vec<String> = (0..2 * PLAN_DIFF_PAGE + 5)
        .map(|i| format!("new/f{i:05}"))
        .collect();
    let mid = commit(&ops, &who, Some(&base), &added);
    let mut change = vec![ChangeInput::new("gone".to_owned(), ChangeOp::Delete)];
    let mut edit = ChangeInput::new("kept".to_owned(), ChangeOp::Put);
    edit.content = Some(ContentInput::Inline("edited\n".to_owned()));
    change.push(edit);
    let mut req = CommitReq::new(change, "edit".to_owned());
    req.parent = Some(mid.as_str().into());
    let head = ops.commit(&who, req).unwrap().changeset;

    let mut seen = Vec::new();
    let mut see = |path: &str| seen.push(path.to_owned());
    let changed = Changed::from_store(&ops, &base, &head, None, &mut see).unwrap();
    let mut expected = vec!["gone".to_owned(), "kept".to_owned()];
    expected.extend(added);
    assert_eq!(seen, expected, "path order, every page");
    assert_eq!(changed.count(), 2 * 1_000 + 7);
    assert_eq!(changed.cause(), None);

    // A first page already read is taken, and the pages after it follow its cursor.
    let mut first = swarmops_api::MergeBaseReq::new(head.as_str(), base.as_str());
    (first.limit, first.max_bytes) = (1, 1);
    let found = ops.merge_base(first).unwrap();
    assert_eq!(found.base, base);
    let mut again = Vec::new();
    let mut see = |path: &str| again.push(path.to_owned());
    let changed = Changed::from_store(&ops, &base, &head, Some(found.diff), &mut see).unwrap();
    assert_eq!(again, expected);
    assert_eq!(changed.count(), 2_007);

    // No difference is a change of no path; a store error is the error.
    let same = Changed::from_store(&ops, &head, &head, None, &mut |_| {}).unwrap();
    assert_eq!(same, Changed::none());
    let missing = "0".repeat(64);
    let error = Changed::from_store(&ops, &missing, &head, None, &mut |_| {}).unwrap_err();
    assert_eq!(error.code, "E_REV_NOT_FOUND", "{error:?}");
}

/// The caller's list is read as it named it, bounded; a file that is not there is `E_IO`,
/// vouched, naming the path as given.
#[test]
fn a_list_s_file_is_read_bounded_or_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    std::fs::write(root.join("list"), LINES).unwrap();
    assert_eq!(read_from(&root.join("list")).unwrap(), LINES);
    std::os::unix::fs::symlink(root.join("list"), root.join("link")).unwrap();
    assert_eq!(
        read_from(&root.join("link")).unwrap(),
        LINES,
        "a link is followed"
    );

    let missing = root.join("nothing");
    let error = read_from(&missing).unwrap_err();
    assert_eq!(error.code, "E_IO", "{error:?}");
    assert_eq!(error.details["path"], missing.as_str());
    assert_eq!(error.details["vouched"], true);
    let error = read_from(&root).unwrap_err();
    assert_eq!(error.code, "E_IO", "a directory: {error:?}");

    // At most one byte past the bound is read of a longer list.
    let endless = std::io::repeat(b'a');
    let read = read_bounded(endless).unwrap();
    assert_eq!(u64::try_from(read.len()).unwrap(), MAX_PLAN_LIST_BYTES + 1);
}
