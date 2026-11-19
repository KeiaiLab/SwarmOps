//! A check's inputs resolved against trees committed to a test store (M30-3's acceptance).

use std::cell::RefCell;

use camino::Utf8PathBuf;
use swarmops_api::{
    AncestryReq, AncestryResp, BlobGetReq, BlobGetResp, BlobHasReq, BlobHasResp, BlobPutReq,
    BlobPutResp, ChangeInput, ChangeOp, ChangesetDetail, CommitReq, CommitResp, ContentInput,
    DeriveReq, DeriveResp, Diff, DiffReq, FileContent, Identity, InfoReq, LandReq, LandResp,
    LogPage, LogReq, ModeInput, OpError, OpLogPage, OpLogReq, PathHistoryPage, PathHistoryReq,
    ReadFileReq, ReadTreeReq, RebaseReq, RebaseResp, RefDeleteReq, RefListReq, RefListResp,
    RefSetReq, RefSetResp, RepoInfo, ResolveRevReq, ResolvedRev, ShowReq, StoreOps, TreeListing,
    UndoReq, UndoResp, VerifyReport, VerifyReq,
};

use super::{Inputs, resolve, resolve_paths};
use crate::LocalOps;

/// T1: a repository's usual shape.
const T1: &[(&str, ModeInput)] = &[
    ("CHANGELOG.md", ModeInput::File),
    ("README.md", ModeInput::File),
    ("crates/a/src/lib.rs", ModeInput::File),
    ("docs/guide.md", ModeInput::File),
    ("docs/status/M1/a.txt", ModeInput::File),
    ("web/app/page.tsx", ModeInput::File),
    ("web/package.json", ModeInput::File),
];

/// T2: one name at several depths.
const T2: &[(&str, ModeInput)] = &[
    ("a/k.rs", ModeInput::File),
    ("a/x/k.rs", ModeInput::File),
    ("a/y/k.rs", ModeInput::File),
    ("a/y/z/k.rs", ModeInput::File),
    ("b/x/k.rs", ModeInput::File),
    ("k.rs", ModeInput::File),
];

/// T3: an executable, a file named as a directory elsewhere is, and a symbolic link.
const T3: &[(&str, ModeInput)] = &[
    (".swarmops/checks.toml", ModeInput::File),
    ("bin/run", ModeInput::Exec),
    ("docs", ModeInput::File),
    ("link", ModeInput::Symlink),
];

/// A store whose one changeset holds `entries`, and that changeset's id.
fn tree(entries: &[(&str, ModeInput)]) -> (tempfile::TempDir, LocalOps, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
    let (ops, _) = LocalOps::init(&root, "test").unwrap();
    let who = Identity::parse("agent/test").unwrap();
    let changes = (entries.iter())
        .map(|(path, mode)| {
            let mut change = ChangeInput::new((*path).to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(format!("{path}\n")));
            change.mode = Some(*mode);
            change
        })
        .collect();
    let id = ops
        .commit(&who, CommitReq::new(changes, "tree".to_owned()))
        .unwrap()
        .changeset;
    (dir, ops, id)
}

/// The inputs `patterns` declare.
fn inputs(patterns: &[&str]) -> Inputs {
    let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
    Inputs::parse(&patterns).unwrap()
}

/// The entries of `entries`' tree that `patterns` match, every one of them, in tree order; and
/// the same match over the list of its paths agrees.
#[track_caller]
fn matched(entries: &[(&str, ModeInput)], patterns: &[&str]) -> Vec<String> {
    let (_dir, ops, id) = tree(entries);
    let inputs = inputs(patterns);
    let found = resolve(&ops, &id, &inputs, usize::MAX).unwrap();
    assert!(!found.truncated, "{patterns:?}");
    let listed = resolve_paths(&inputs, entries.iter().map(|(path, _)| *path));
    assert_eq!(
        found.entries, listed,
        "{patterns:?}: the list of paths agrees"
    );
    found.entries
}

/// Every path of `entries`, in the order the fixture writes them: tree order.
fn all<'e>(entries: &[(&'e str, ModeInput)]) -> Vec<&'e str> {
    entries.iter().map(|(path, _)| *path).collect()
}

/// T1: literal files and trailing `**`, in tree order whatever order the patterns are in.
#[test]
fn t1_gives_its_pinned_set() {
    assert_eq!(
        matched(T1, &["web/**", "docs/**", "CHANGELOG.md"]),
        [
            "CHANGELOG.md",
            "docs/guide.md",
            "docs/status/M1/a.txt",
            "web/app/page.tsx",
            "web/package.json",
        ]
    );
    assert_eq!(matched(T1, &["docs/*"]), ["docs/guide.md"]);
    assert_eq!(
        matched(T1, &["crates/*/src/**", "nothing/**"]),
        ["crates/a/src/lib.rs"]
    );
}

/// T2: `*` is one segment at any place, `**` every depth.
#[test]
fn t2_gives_its_pinned_sets() {
    assert_eq!(
        matched(T2, &["a/*/k.rs", "*/k.rs"]),
        ["a/k.rs", "a/x/k.rs", "a/y/k.rs"]
    );
    assert_eq!(matched(T2, &["*"]), ["k.rs"]);
    assert_eq!(matched(T2, &["**"]), all(T2));
    assert_eq!(
        matched(T2, &["a/**"]),
        ["a/k.rs", "a/x/k.rs", "a/y/k.rs", "a/y/z/k.rs"]
    );
}

/// T3: a directory is no entry, so `docs/**` takes nothing of a file `docs` and `docs` takes
/// it; an executable and a symbolic link are entries.
#[test]
fn t3_gives_its_pinned_sets() {
    assert_eq!(matched(T3, &["docs/**"]), Vec::<String>::new());
    assert_eq!(matched(T3, &["docs"]), ["docs"]);
    assert_eq!(matched(T3, &[".swarmops/*"]), [".swarmops/checks.toml"]);
    assert_eq!(matched(T3, &["link", "bin/*"]), ["bin/run", "link"]);

    // A directory's own name matches nothing: `bin` is no entry, and neither is `.swarmops`.
    assert_eq!(matched(T3, &["bin", ".swarmops"]), Vec::<String>::new());
    assert_eq!(matched(T3, &["*"]), ["docs", "link"]);
}

/// No pattern is the whole tree: every entry of each of the three, in tree order.
#[test]
fn no_pattern_gives_every_entry() {
    for entries in [T1, T2, T3] {
        assert!(inputs(&[]).whole_tree());
        assert_eq!(matched(entries, &[]), all(entries));
    }
}

/// Tree order is directory by directory, a directory's entries by name, bytewise: a directory
/// `a` comes before `a-b` and `a.b`, with what is below it, whatever a path-wise sort would say.
#[test]
fn tree_order_is_by_name_bytes() {
    let entries: &[(&str, ModeInput)] = &[
        ("B", ModeInput::File),
        ("a/x", ModeInput::File),
        ("a/y/z", ModeInput::File),
        ("a-b", ModeInput::File),
        ("a.b", ModeInput::File),
        ("ab", ModeInput::File),
    ];
    assert_eq!(matched(entries, &[]), all(entries));
    assert_eq!(matched(entries, &["**"]), all(entries));
    assert_eq!(
        matched(entries, &["*", "a/*"]),
        ["B", "a/x", "a-b", "a.b", "ab"]
    );
}

/// The walk stops at its limit and says so: one below the matches is `truncated`, the matches
/// themselves are not.
#[test]
fn the_limit_stops_the_walk_and_says_so() {
    let (_dir, ops, id) = tree(T1);
    let inputs = inputs(&["web/**", "docs/**", "CHANGELOG.md"]);
    let cut = resolve(&ops, &id, &inputs, 4).unwrap();
    assert!(cut.truncated);
    assert_eq!(
        cut.entries,
        [
            "CHANGELOG.md",
            "docs/guide.md",
            "docs/status/M1/a.txt",
            "web/app/page.tsx"
        ]
    );
    let whole = resolve(&ops, &id, &inputs, 5).unwrap();
    assert!(!whole.truncated);
    assert_eq!(whole.entries.len(), 5);
    let none = resolve(&ops, &id, &inputs, 0).unwrap();
    assert!(none.truncated);
    assert_eq!(none.entries, Vec::<String>::new());

    // A tree with no match is not cut at a limit of none.
    let empty = resolve(&ops, &id, &self::inputs(&["nothing/**"]), 0).unwrap();
    assert!(!empty.truncated);
}

/// A directory no pattern can reach is never read; the whole tree reads them all.
#[test]
fn a_directory_no_pattern_reaches_is_not_read() {
    let (_dir, ops, id) = tree(T1);
    let counted = Counted::new(&ops);
    resolve(
        &counted,
        &id,
        &inputs(&["docs/*", "CHANGELOG.md"]),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(counted.read(), [None, Some("docs".to_owned())]);

    let counted = Counted::new(&ops);
    resolve(&counted, &id, &inputs(&["web/app/*"]), usize::MAX).unwrap();
    assert_eq!(
        counted.read(),
        [None, Some("web".to_owned()), Some("web/app".to_owned())]
    );

    let counted = Counted::new(&ops);
    resolve(&counted, &id, &inputs(&[]), usize::MAX).unwrap();
    assert_eq!(
        counted.read().len(),
        9,
        "the root and its eight directories"
    );
}

/// A directory of more entries than a page holds is read to its end.
#[test]
fn every_page_of_a_directory_is_read() {
    let names: Vec<String> = (0..1_205).map(|i| format!("many/f{i:04}")).collect();
    let mut entries: Vec<(&str, ModeInput)> = names
        .iter()
        .map(|name| (name.as_str(), ModeInput::File))
        .collect();
    entries.push(("z", ModeInput::File));
    let (_dir, ops, id) = tree(&entries);
    let counted = Counted::new(&ops);
    let found = resolve(&counted, &id, &inputs(&["many/*", "z"]), usize::MAX).unwrap();
    assert_eq!(found.entries.len(), 1_206);
    assert_eq!(found.entries, all(&entries));
    assert_eq!(
        counted.read().len(),
        3,
        "the root, and two pages of the directory"
    );
}

/// A store's error is the error: a changeset that is not there.
#[test]
fn a_store_error_is_the_error() {
    let (_dir, ops, _) = tree(T3);
    let missing = "0".repeat(64);
    let error = resolve(&ops, &missing, &inputs(&["**"]), 10).unwrap_err();
    assert_eq!(error.code, "E_REV_NOT_FOUND", "{error:?}");
}

/// A store that counts the directories read through it.
struct Counted<'s> {
    /// The store behind it.
    inner: &'s LocalOps,
    /// The `path` of each `read_tree`, in order.
    read: RefCell<Vec<Option<String>>>,
}

impl<'s> Counted<'s> {
    /// A counter over `inner`.
    fn new(inner: &'s LocalOps) -> Counted<'s> {
        Counted {
            inner,
            read: RefCell::new(Vec::new()),
        }
    }

    /// The directories read so far.
    fn read(&self) -> Vec<Option<String>> {
        self.read.borrow().clone()
    }
}

impl StoreOps for Counted<'_> {
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.inner.repo_info(req)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        self.inner.commit(who, req)
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        self.inner.land(who, req)
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        self.inner.rebase(who, req)
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        self.inner.resolve_rev(req)
    }

    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        self.inner.blob_put(who, req)
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        self.inner.blob_has(req)
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        self.inner.blob_get(req)
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.inner.read_file(req)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.read.borrow_mut().push(req.path.clone());
        self.inner.read_tree(req)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.inner.show(req)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.inner.log(req)
    }

    fn ancestry(&self, req: AncestryReq) -> Result<AncestryResp, OpError> {
        self.inner.ancestry(req)
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        self.inner.path_history(req)
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        self.inner.diff(req)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.inner.ref_list(req)
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.inner.ref_set(who, req)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.inner.ref_delete(who, req)
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        self.inner.op_log(req)
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        self.inner.undo(who, req)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.inner.verify(req)
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        self.inner.derive(req)
    }
}
