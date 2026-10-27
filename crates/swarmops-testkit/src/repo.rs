//! What the repository's tests build: a fresh store, and the changes and commit requests they
//! make on it.

use camino::Utf8PathBuf;
use swarmops_core::{BlobId, ChangesetId, LeafKind, ObjectKind, RepoPath, Signature};
use swarmops_repo::{Change, ChangeOp, CommitRequest, LocalRepo};
use swarmops_store::{DEFAULT_INIT_FORMAT, StoreFormat};
use tempfile::TempDir;

/// A fresh store in a temp dir of its own; the directory lives as long as the `TempDir`.
pub fn new_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    new_repo_as(DEFAULT_INIT_FORMAT)
}

/// [`new_repo`] of a store of `format`: for a test of one format's files.
pub fn new_repo_as(format: StoreFormat) -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create_as(&root, "test", format).expect("create the store");
    (dir, root, repo)
}

/// The repo path `s`, which must be valid.
pub fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("a valid repo path")
}

/// The id of the blob that holds `content`.
pub fn blob(content: &[u8]) -> BlobId {
    BlobId::new(swarmops_core::hash_object(ObjectKind::Blob, content))
}

/// A change that puts a regular file holding `content` at `p`.
pub fn put(p: &str, content: &[u8]) -> Change {
    put_as(p, LeafKind::File, content)
}

/// A change that puts a leaf of `kind` holding `content` at `p`.
pub fn put_as(p: &str, kind: LeafKind, content: &[u8]) -> Change {
    Change::new(
        path(p),
        ChangeOp::Put {
            kind,
            content: content.to_vec(),
        },
    )
}

/// A change that puts blob `id`, which the store must hold already, as a regular file at `p`:
/// named by its id, so that the commit neither reads its content nor writes it again
/// ([`ChangeOp::PutBlob`]).
pub fn put_blob(p: &str, id: BlobId) -> Change {
    Change::new(
        path(p),
        ChangeOp::PutBlob {
            kind: LeafKind::File,
            id,
        },
    )
}

/// A change that deletes `p`.
pub fn delete(p: &str) -> Change {
    Change::new(path(p), ChangeOp::Delete)
}

/// A fixed signature, so that the same request always makes the same changeset.
pub fn signature() -> Signature {
    Signature::new(
        b"test/agent".to_vec(),
        b"agent@test.swarmops.local".to_vec(),
        1_760_000_000,
        0,
        false,
    )
    .expect("a zero offset is a valid signature")
}

/// A request for `changes` on `parents` by [`signature`], with `message` and no metadata.
pub fn request(parents: &[ChangesetId], changes: Vec<Change>, message: &str) -> CommitRequest {
    CommitRequest::new(
        parents.to_vec(),
        changes,
        signature(),
        signature(),
        message.as_bytes().to_vec(),
        Vec::new(),
        false,
        None,
    )
}

/// Commits `changes` on `parents` ([`request`], with the message "a test commit"); the new
/// changeset's id.
#[track_caller]
pub fn commit(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    repo.commit(request(parents, changes, "a test commit"))
        .expect("commit")
        .id
}
