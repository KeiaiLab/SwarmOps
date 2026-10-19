//! What the repository's tests build: a fresh store, and the changes and commit requests they
//! make on it.

use camino::Utf8PathBuf;
use swarmops_core::{BlobId, ChangesetId, LeafKind, ObjectKind, RepoPath, Signature, hash_object};
use swarmops_repo::{Change, ChangeOp, CommitRequest, LocalRepo};
use tempfile::TempDir;

/// A fresh store in a temp dir of its own; the directory lives as long as the `TempDir`.
pub fn new_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create(&root, "test").expect("create the store");
    (dir, root, repo)
}

/// The repo path `s`, which must be valid.
pub fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("a valid repo path")
}

/// The id of the blob that holds `content`.
pub fn blob(content: &[u8]) -> BlobId {
    BlobId(hash_object(ObjectKind::Blob, content))
}

/// A change that puts a regular file holding `content` at `p`.
pub fn put(p: &str, content: &[u8]) -> Change {
    put_as(p, LeafKind::File, content)
}

/// A change that puts a leaf of `kind` holding `content` at `p`.
pub fn put_as(p: &str, kind: LeafKind, content: &[u8]) -> Change {
    Change {
        path: path(p),
        op: ChangeOp::Put {
            kind,
            content: content.to_vec(),
        },
    }
}

/// A change that deletes `p`.
pub fn delete(p: &str) -> Change {
    Change {
        path: path(p),
        op: ChangeOp::Delete,
    }
}

/// A fixed signature, so that the same request always makes the same changeset.
pub fn signature() -> Signature {
    Signature {
        name: b"test/agent".to_vec(),
        email: b"agent@test.swarmops.local".to_vec(),
        secs: 1_760_000_000,
        offset_secs: 0,
        negative_zero: false,
    }
}

/// A request for `changes` on `parents` by [`signature`], with `message` and no metadata.
pub fn request(parents: &[ChangesetId], changes: Vec<Change>, message: &str) -> CommitRequest {
    CommitRequest {
        parents: parents.to_vec(),
        changes,
        author: signature(),
        committer: signature(),
        message: message.as_bytes().to_vec(),
        metadata: Vec::new(),
        allow_empty: false,
        advance: None,
    }
}

/// Commits `changes` on `parents` ([`request`], with the message "a test commit"); the new
/// changeset's id.
#[track_caller]
pub fn commit(repo: &LocalRepo, parents: &[ChangesetId], changes: Vec<Change>) -> ChangesetId {
    repo.commit(request(parents, changes, "a test commit"))
        .expect("commit")
        .id
}
