//! Tags through the git crate (the annotated tags design, slice T-4): one intake (`take_tag`,
//! `take_tags`) that takes a git tag into a store tag, refusing a tag of a tag, of a tree or a
//! blob, of a commit that would export as another, and an annotated tag below FORMAT 3; the
//! export that writes a store tag back byte for byte; and the import into a `tags/` target.
//!
//! Every test owns a fresh store and, where git is written, a fresh `git::empty_repository()`,
//! read back through the import's reader (`GixSource`), which checks each object against its id.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ChangesetId;
use swarmops_git::{
    DivergedReason, ExportError, ExportOid, ExportReport, ExportRequest, GitOid, GitSink,
    GitSource, GixSink, GixSource, ImportError, ImportRequest, RawKind, RawTag, SignatureKind,
    TagError, TagRefusal, TagTaken, export, git_hash, import, register, take_tag, take_tags,
};
use swarmops_repo::{LocalRepo, RepoError, StoreFormat, TagKind, TagObject};
use swarmops_store::{ExternalEntry, Layout, SYSTEM_GIT};
use swarmops_testkit::fs::NoSyncFs;
use swarmops_testkit::git::{self, EmptyRepository};
use swarmops_testkit::repo::{commit, put};
use tempfile::TempDir;

use crate::support::MemSource;

/// Who makes the tags.
const ACTOR: &str = "test/tagger";

/// The store tag most tests take.
const V1: &str = "tags/v1";

/// A store of its own on a filesystem that never flushes, `ExportOid` registered.
struct Store {
    _dir: TempDir,
    root: Utf8PathBuf,
    repo: LocalRepo,
}

/// A store of `format`: FORMAT 3 is a FORMAT 2 one upgraded (no `init` makes FORMAT 3 yet).
fn store(format: StoreFormat) -> Store {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("a UTF-8 temp dir");
    let made = LocalRepo::create_with_fs_as(&root, "tags", StoreFormat::V2, Arc::new(NoSyncFs));
    let mut repo = made.expect("create the store");

    // 1. Upgraded, and opened again at its new format.
    if format == StoreFormat::V3 {
        assert!(repo.upgrade().expect("upgrade").upgraded);
        drop(repo);
        repo = LocalRepo::open_with_fs(&root, Arc::new(NoSyncFs)).expect("open the store");
    }
    assert_eq!(repo.format(), format);
    register(&mut repo).expect("register ExportOid");
    Store {
        _dir: dir,
        root,
        repo,
    }
}

/// A git history of one commit and the tags a test needs, held as bytes: put into a
/// [`MemSource`], or written into a bare repository.
#[derive(Default)]
struct History {
    objects: Vec<(RawKind, Vec<u8>)>,
}

impl History {
    /// Keeps `bytes` as an object of `kind`; its id.
    fn put(&mut self, kind: RawKind, bytes: Vec<u8>) -> GitOid {
        let oid = git_hash(kind, &bytes);
        self.objects.push((kind, bytes));
        oid
    }

    /// A tree of one file, `name` of `content`, with git's `mode`.
    fn tree(&mut self, mode: &str, name: &str, content: &[u8]) -> GitOid {
        let blob = self.put(RawKind::Blob, content.to_vec());
        let mut bytes = format!("{mode} {name}\0").into_bytes();
        bytes.extend_from_slice(&blob.0);
        self.put(RawKind::Tree, bytes)
    }

    /// A root commit of `tree`, as git writes one.
    fn commit(&mut self, tree: GitOid, message: &str) -> GitOid {
        let who = "A U Thor <a@example.com> 1700000000 +0000";
        let bytes = format!("tree {tree}\nauthor {who}\ncommitter {who}\n\n{message}\n");
        self.put(RawKind::Commit, bytes.into_bytes())
    }

    /// A faithful commit: one file of mode `100644`.
    fn faithful(&mut self) -> GitOid {
        let tree = self.tree("100644", "a.txt", b"a\n");
        self.commit(tree, "one")
    }

    /// A tag named `name` of `object`, of `kind`, as `git mktag` takes one, `message` last.
    fn tag(&mut self, object: GitOid, kind: RawKind, name: &str, message: &str) -> GitOid {
        let bytes = tag_bytes(object, kind, name, message);
        self.put(RawKind::Tag, bytes)
    }

    /// The bytes of the object `oid`.
    fn bytes(&self, oid: GitOid) -> Vec<u8> {
        let held = self
            .objects
            .iter()
            .find(|(kind, bytes)| git_hash(*kind, bytes) == oid);
        held.expect("the history holds it").1.clone()
    }

    /// Its objects, in memory.
    fn source(&self) -> MemSource {
        let mut source = MemSource::default();
        for (kind, bytes) in &self.objects {
            source.put(*kind, bytes.clone());
        }
        source
    }

    /// Its objects, written into a bare repository and made durable.
    fn repository(&self) -> EmptyRepository {
        let repository = git::empty_repository();
        let mut sink = gix(&repository.dir);
        let mut oids = Vec::new();
        for (kind, bytes) in &self.objects {
            oids.push(sink.write_object(*kind, bytes).expect("write"));
        }
        sink.sync(&oids).expect("sync");
        repository
    }
}

/// The bytes of a tag named `name` of `object`, of `kind`, with `message`.
fn tag_bytes(object: GitOid, kind: RawKind, name: &str, message: &str) -> Vec<u8> {
    let tagger = "A U Thor <a@example.com> 1700000000 +0000";
    format!("object {object}\ntype {kind}\ntag {name}\ntagger {tagger}\n\n{message}\n").into_bytes()
}

fn gix(path: &Path) -> GixSink {
    GixSink::open(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

/// `export`, which must succeed.
#[track_caller]
fn exported(repo: &LocalRepo, sink: &mut dyn GitSink, req: &ExportRequest) -> ExportReport {
    export(repo, sink, req).unwrap_or_else(|error| panic!("export: {error}"))
}

/// `take_tag`, which must create the tag.
#[track_caller]
fn taken(repo: &LocalRepo, source: &dyn GitSource, name: &str, oid: GitOid) {
    let taken = take_tag(repo, source, name, oid, ACTOR);
    assert!(matches!(taken, Ok(TagTaken::Created)), "{taken:?}");
}

/// What the store's tag `name` names and carries; `None` for no tag.
fn carried(repo: &LocalRepo, name: &str) -> Option<(ChangesetId, TagKind)> {
    let tag = repo.snapshot().expect("a snapshot").tag(name).expect("tag");
    tag.map(|tag| (tag.target, tag.kind))
}

/// The bytes of the blob an annotated store tag `name` carries.
fn object_of(repo: &LocalRepo, name: &str) -> Vec<u8> {
    let Some((_, TagKind::Annotated { blob })) = carried(repo, name) else {
        panic!("{name} carries no object");
    };
    repo.snapshot().unwrap().blob(blob).unwrap()
}

/// The object `oid` of the repository at `dir`, read and checked against its id.
fn read_back(dir: &Path, oid: GitOid) -> (RawKind, Vec<u8>) {
    let source = GixSource::open(dir).expect("open the repository");
    source
        .read(oid)
        .unwrap_or_else(|error| panic!("read {oid}: {error}"))
}

/// Whether `taken` is the refusal `reason` (`details.reason`'s word).
fn refused(taken: &Result<TagTaken, TagError>, reason: &str) -> bool {
    matches!(taken, Err(TagError::Refused(refusal)) if refusal.as_str() == reason)
}

/// Every file of the store at `root`, by its path in the store, with its bytes.
fn files(root: &Utf8Path) -> Files {
    let mut files = BTreeMap::new();
    let mut dirs = vec![root.as_std_path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = path.strip_prefix(root).unwrap().to_path_buf();
            files.insert(name, fs::read(&path).unwrap());
        }
    }
    files
}

/// The paths whose bytes differ between `before` and `after`, or that only one holds.
fn changed(before: &Files, after: &Files) -> Vec<PathBuf> {
    let paths = before.keys().chain(after.keys());
    let mut changed: Vec<PathBuf> = paths
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect();
    changed.dedup();
    changed
}

/// Every file of a store, by its path in the store, with its bytes.
type Files = BTreeMap<PathBuf, Vec<u8>>;

/// How many object files the store at `root` holds.
fn objects(root: &Utf8Path) -> usize {
    (files(root).keys())
        .filter(|path| path.starts_with("objects") && !path.starts_with("objects/tmp"))
        .count()
}

/// The length of the store's journal.
fn journal_len(store: &Store) -> u64 {
    let log = Layout::with_format(&store.root, store.repo.format()).journal_log();
    fs::metadata(log).expect("the journal").len()
}

/// An annotated store tag exports as git's own tag object, byte for byte, under its own id:
/// `refs/tags/v1` names the tag, which names the commit the tag's target exports to.
#[test]
fn an_annotated_tag_exports_byte_for_byte() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    taken(&store.repo, &history.source(), V1, tag);
    assert_eq!(object_of(&store.repo, V1), history.bytes(tag));

    // 1. Exported into an empty repository: the tag object, then the ref naming it.
    let destination = git::empty_repository();
    let req = ExportRequest::tag(V1, "v1");
    let report = exported(&store.repo, &mut gix(&destination.dir), &req);
    assert_eq!((report.head_oid, report.tag_oid), (commit, Some(tag)));
    assert!(report.branch_moved);

    // 2. Read back: the same bytes under the same id, the ref's file naming it.
    assert_eq!(gix(&destination.dir).tag("v1"), Ok(Some(tag)));
    let file = fs::read_to_string(destination.dir.join("refs/tags/v1")).unwrap();
    assert_eq!(file, format!("{tag}\n"));
    assert_eq!(
        read_back(&destination.dir, tag),
        (RawKind::Tag, history.bytes(tag))
    );
    assert_eq!(read_back(&destination.dir, commit).0, RawKind::Commit);
}

/// A lightweight store tag of a FORMAT 3 store exports as today: the ref names the commit, no
/// tag object is written.
#[test]
fn a_lightweight_tag_exports_as_today() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    taken(&store.repo, &history.source(), V1, commit);
    assert!(matches!(
        carried(&store.repo, V1),
        Some((_, TagKind::Lightweight))
    ));

    let destination = git::empty_repository();
    let req = ExportRequest::tag(V1, "v1");
    let report = exported(&store.repo, &mut gix(&destination.dir), &req);
    assert_eq!((report.head_oid, report.tag_oid), (commit, None));
    assert_eq!(gix(&destination.dir).tag("v1"), Ok(Some(commit)));
    assert_eq!(report.objects_written, 3, "a blob, a tree and a commit");
}

/// A signed tag keeps its signature block: taken, held and exported byte for byte, its kind
/// named, nothing verified.
#[test]
fn a_signed_tag_round_trips_unverified() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    let signed = "release 1\n-----BEGIN PGP SIGNATURE-----\n\niQEzBAABCAAdFiEE\n=abcd\n\
                  -----END PGP SIGNATURE-----";
    let tag = history.tag(commit, RawKind::Commit, "v1", signed);
    taken(&store.repo, &history.source(), V1, tag);

    let destination = git::empty_repository();
    exported(
        &store.repo,
        &mut gix(&destination.dir),
        &ExportRequest::tag(V1, "v1"),
    );
    let (_, bytes) = read_back(&destination.dir, tag);
    assert_eq!(bytes, history.bytes(tag));
    assert_eq!(RawTag::signature(&bytes), Some(SignatureKind::Pgp));
}

/// X3: a store tag exported into a bare repository and imported from there into a fresh store's
/// `tags/` target comes back with the same bytes, and exports again under the same id.
#[test]
fn an_import_and_export_round_trip_a_tag() {
    let first = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    taken(&first.repo, &history.source(), V1, tag);
    let between = git::empty_repository();
    let req = ExportRequest::tag(V1, "v1");
    exported(&first.repo, &mut gix(&between.dir), &req);

    // 1. Imported by the tag's id into a fresh store: the tag, annotated, its bytes kept.
    let second = store(StoreFormat::V3);
    let source = GixSource::open(&between.dir).expect("open");
    let into = ImportRequest::new(tag.to_string(), V1, ACTOR);
    let report = import(&second.repo, &source, &into).expect("import");
    assert!(report.ref_moved);
    assert_eq!((report.git_head, report.imported), (commit, 1));
    assert_eq!(object_of(&second.repo, V1), history.bytes(tag));

    // 2. Run again: nothing moves.
    let again = import(&second.repo, &source, &into).expect("import again");
    assert!(!again.ref_moved);

    // 3. Exported from the second store: the same tag under the same id.
    let last = git::empty_repository();
    let report = exported(&second.repo, &mut gix(&last.dir), &req);
    assert_eq!(report.tag_oid, Some(tag));
    assert_eq!(read_back(&last.dir, tag).1, history.bytes(tag));
}

/// G5's intake: a tag whose `type` is `tag` is refused (`tag_of_tag`), by `take_tag` and by an
/// import into a `tags/` target, and no tag is made.
#[test]
fn a_tag_of_a_tag_is_refused_by_name() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    let inner = history.tag(commit, RawKind::Commit, "v0", "inner");
    let outer = history.tag(inner, RawKind::Tag, "v1", "outer");

    let taken = take_tag(&store.repo, &history.source(), V1, outer, ACTOR);
    assert!(refused(&taken, "tag_of_tag"), "{taken:?}");
    assert!(matches!(taken, Err(TagError::Refused(TagRefusal::Nested))));

    // The import into a `tags/` target refuses it the same way.
    let into = ImportRequest::new(outer.to_string(), V1, ACTOR);
    let error = import(&store.repo, &history.source(), &into).unwrap_err();
    assert!(
        matches!(
            &error,
            ImportError::Tag {
                refusal: TagRefusal::Nested,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(carried(&store.repo, V1), None);
}

/// G6's intake: a tag of a tree or of a blob, and a lightweight tag naming one, are refused
/// (`tag_target_kind`): a store tag names a changeset.
#[test]
fn a_tag_of_a_tree_or_blob_is_refused_by_kind() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let tree = history.tree("100644", "a.txt", b"a\n");
    let blob = git_hash(RawKind::Blob, b"a\n");
    let of_tree = history.tag(tree, RawKind::Tree, "v1", "a tree");
    let of_blob = history.tag(blob, RawKind::Blob, "v1", "a blob");
    let source = history.source();

    for (oid, kind) in [
        (of_tree, RawKind::Tree),
        (of_blob, RawKind::Blob),
        (tree, RawKind::Tree),
        (blob, RawKind::Blob),
    ] {
        let taken = take_tag(&store.repo, &source, V1, oid, ACTOR);
        assert!(refused(&taken, "tag_target_kind"), "{taken:?}");
        let Err(TagError::Refused(TagRefusal::TargetKind(named))) = taken else {
            panic!("{taken:?}");
        };
        assert_eq!(named, kind);
    }
    assert_eq!(carried(&store.repo, V1), None);
}

/// G7's intake: a tag of a commit the import takes only with a loss (a file of mode `100664`)
/// is refused (`tag_target_lossy`): its `object` line could never be exported again.
#[test]
fn a_tag_of_a_lossy_commit_is_refused() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let tree = history.tree("100664", "a.txt", b"a\n");
    let commit = history.commit(tree, "lossy");
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    let source = history.source();

    for oid in [tag, commit] {
        let taken = take_tag(&store.repo, &source, V1, oid, ACTOR);
        assert!(refused(&taken, "tag_target_lossy"), "{taken:?}");
    }
    assert_eq!(carried(&store.repo, V1), None);
}

/// Decision 7: `git import` of an annotated tag into a FORMAT 2 store's `tags/` target peels it,
/// stores a lightweight tag as today, and says so (`annotated_tag_peeled`).
#[test]
fn import_to_a_format_2_tag_peels_with_a_warning() {
    let store = store(StoreFormat::V2);
    let mut history = History::default();
    let commit = history.faithful();
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    let repository = history.repository();
    let source = GixSource::open(&repository.dir).expect("open");

    let into = ImportRequest::new(tag.to_string(), V1, ACTOR);
    let report = import(&store.repo, &source, &into).expect("import");
    assert!(report.ref_moved);
    assert_eq!(report.git_head, commit);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("annotated_tag_peeled")),
        "{:?}",
        report.warnings
    );
    let held = store.repo.get_ref(V1).unwrap().expect("the tag");
    assert_eq!(held.target, report.head);
}

/// A binding whose object names another commit than its target exports to is refused as a
/// damaged derivation (`E_STORE_CORRUPT`, `details.derivation` `tag_object`): the tag is not
/// written.
#[test]
fn a_stale_tag_object_is_not_exported() {
    let store = store(StoreFormat::V3);
    let target = commit(&store.repo, &[], vec![put("a.txt", b"a\n")]);
    let elsewhere: GitOid = "07".repeat(20).parse().expect("an id");
    let bytes = tag_bytes(elsewhere, RawKind::Commit, "v1", "stale");
    let made = store
        .repo
        .tag_create(V1, target, TagObject::Annotated(bytes), ACTOR);
    made.expect("tag_create");

    let destination = git::empty_repository();
    let req = ExportRequest::tag(V1, "v1");
    let error = export(&store.repo, &mut gix(&destination.dir), &req).unwrap_err();
    assert!(
        matches!(
            &error,
            ExportError::Repo(RepoError::DerivedCorrupt { name, changeset, .. })
                if name == "tag_object" && *changeset == target
        ),
        "{error:?}"
    );
    assert_eq!(gix(&destination.dir).tag("v1"), Ok(None));
}

/// `take_tags` run again finds each tag made (`Unchanged`) and writes no file; a tag taken again
/// with another object is immutable (X1).
#[test]
fn take_tags_run_again_writes_nothing() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    let one = history.tag(commit, RawKind::Commit, "v1", "one");
    let three = history.tag(commit, RawKind::Commit, "v3", "three");
    let source = history.source();
    let batch = [
        ("tags/v1".to_owned(), one),
        ("tags/v2".to_owned(), commit),
        ("tags/v3".to_owned(), three),
    ];

    // 1. Each created, then each found made, no file changed.
    let first = take_tags(&store.repo, &source, &batch, ACTOR).expect("take_tags");
    assert!(
        first.iter().all(|t| matches!(t, Ok(TagTaken::Created))),
        "{first:?}"
    );
    let before = files(&store.root);
    let again = take_tags(&store.repo, &source, &batch, ACTOR).expect("take_tags");
    assert!(
        again.iter().all(|t| matches!(t, Ok(TagTaken::Unchanged))),
        "{again:?}"
    );
    assert_eq!(changed(&before, &files(&store.root)), [] as [PathBuf; 0]);

    // 2. Another object at the same target: immutable, the tag as it was.
    let moved = take_tag(&store.repo, &source, "tags/v1", commit, ACTOR);
    assert!(
        matches!(&moved, Err(TagError::Immutable(name)) if name == "tags/v1"),
        "{moved:?}"
    );
    assert_eq!(object_of(&store.repo, "tags/v1"), history.bytes(one));
}

/// H8: intake accepts exactly what export writes back. An external id entry someone planted maps
/// the tagged commit to a changeset that exports another commit: the tag is refused
/// (`tag_target_lossy`), and neither its ref nor its fact is written.
#[test]
fn a_tag_is_taken_only_if_export_returns_its_object() {
    let store = store(StoreFormat::V3);
    let other = commit(&store.repo, &[], vec![put("b.txt", b"b\n")]);
    let mut history = History::default();
    let commit = history.faithful();
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    let planted = [(commit.0.to_vec(), ExternalEntry::new(other, true))];
    store
        .repo
        .record_external_ids(SYSTEM_GIT, &planted)
        .expect("plant");
    let exported_as = store.repo.derived::<ExportOid>(other, &()).unwrap();
    assert_ne!(exported_as.git_oid(), commit);

    let taken = take_tag(&store.repo, &history.source(), V1, tag, ACTOR);
    assert!(refused(&taken, "tag_target_lossy"), "{taken:?}");
    assert!(store.repo.get_ref(V1).unwrap().is_none());
    assert_eq!(carried(&store.repo, V1), None);
}

/// S1, X2: a FORMAT 2 store refuses an annotated tag (`E_FORMAT_REQUIRED`, `needs` 3, name
/// `tag_object`) before any import or blob put: no object, no frame.
#[test]
fn a_format_2_refusal_writes_no_object() {
    let store = store(StoreFormat::V2);
    let mut history = History::default();
    let commit = history.faithful();
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    let (held, journal) = (objects(&store.root), journal_len(&store));

    let taken = take_tag(&store.repo, &history.source(), V1, tag, ACTOR);
    assert!(
        matches!(
            &taken,
            Err(TagError::Format { name, needs: 3, store: 2 }) if name == "tag_object"
        ),
        "{taken:?}"
    );
    assert_eq!((objects(&store.root), journal_len(&store)), (held, journal));
    assert!(store.repo.get_ref(V1).unwrap().is_none());
}

/// An export run again writes nothing for an annotated tag (the same object: unchanged); another
/// object under the name is `tag_exists`, unless forced.
#[test]
fn an_annotated_tag_exported_again_writes_nothing() {
    let store = store(StoreFormat::V3);
    let mut history = History::default();
    let commit = history.faithful();
    let tag = history.tag(commit, RawKind::Commit, "v1", "release 1");
    let source = history.source();
    taken(&store.repo, &source, V1, tag);
    taken(&store.repo, &source, "tags/light", commit);
    let destination = git::empty_repository();
    let req = ExportRequest::tag(V1, "v1");
    exported(&store.repo, &mut gix(&destination.dir), &req);

    // 1. Again: up to date, nothing written.
    let again = exported(&store.repo, &mut gix(&destination.dir), &req);
    assert_eq!(
        (
            again.branch_moved,
            again.objects_written,
            again.objects_checked
        ),
        (false, 0, 0)
    );
    assert_eq!(again.tag_oid, Some(tag));

    // 2. The lightweight tag of the same commit into the same name: refused, then forced.
    let mut other = ExportRequest::tag("tags/light", "v1");
    let error = export(&store.repo, &mut gix(&destination.dir), &other).unwrap_err();
    assert!(
        matches!(
            error,
            ExportError::Diverged { reason: DivergedReason::TagExists, git_head, .. }
                if git_head == tag
        ),
        "{error:?}"
    );
    assert_eq!(gix(&destination.dir).tag("v1"), Ok(Some(tag)));
    other.force = true;
    let forced = exported(&store.repo, &mut gix(&destination.dir), &other);
    assert!(forced.branch_moved);
    assert_eq!(gix(&destination.dir).tag("v1"), Ok(Some(commit)));
}
