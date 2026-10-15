//! What the repository's integration tests share: a fresh store, the changes and commit
//! requests they make, and, from the store's tests, the child-process runner ([`child`]) and
//! the `FsOps` doubles ([`fs`]). A module directory, so that Cargo does not take it for a test
//! target of its own.

#![allow(dead_code)] // Each test binary uses the part it needs.

#[path = "../../../swarmops-store/tests/support/child.rs"]
pub mod child;
#[path = "../../../swarmops-store/tests/support/fs.rs"]
pub mod fs;

use camino::Utf8PathBuf;
use swarmops_core::{BlobId, ChangesetId, EntryKind, ObjectKind, RepoPath, Signature, hash_object};
use swarmops_repo::{Change, ChangeOp, CommitRequest, LocalRepo};
use tempfile::TempDir;

/// A fresh store in a temp dir of its own; the directory lives as long as the `TempDir`.
pub fn new_repo() -> (TempDir, Utf8PathBuf, LocalRepo) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("temp dir is UTF-8");
    let repo = LocalRepo::create(&root, "test").expect("create the store");
    (dir, root, repo)
}

pub fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("a valid repo path")
}

pub fn blob(content: &[u8]) -> BlobId {
    BlobId(hash_object(ObjectKind::Blob, content))
}

pub fn put(p: &str, content: &[u8]) -> Change {
    put_as(p, EntryKind::File, content)
}

pub fn put_as(p: &str, kind: EntryKind, content: &[u8]) -> Change {
    Change {
        path: path(p),
        op: ChangeOp::Put {
            kind,
            content: content.to_vec(),
        },
    }
}

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
