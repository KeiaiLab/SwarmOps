//! `blob_get` (M5; operations spec §1): blobs by id in one snapshot, up to a byte budget.

use camino::Utf8PathBuf;
use swarmops_api::{
    BlobGetReq, ChangeInput, ChangeOp, CommitReq, ContentEncoding, ContentInput,
    DEFAULT_BLOB_GET_MAX_BYTES, ErrorCode, Identity, ShowReq, StoreOps,
};
use swarmops_core::{ObjectKind, hash_object};
use swarmops_ops::LocalOps;
use tempfile::TempDir;

/// A fresh store in a temp dir of its own.
fn new_store() -> (TempDir, LocalOps) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8 temp path");
    let (ops, _) = LocalOps::init(&root, "acme").expect("create the store");
    (dir, ops)
}

/// Commits `files` on a root commit; the changeset's id.
fn commit(ops: &LocalOps, files: &[(&str, ContentInput)]) -> String {
    let who = Identity::parse("agent/worker-17").expect("a valid agent name");
    let changes = files
        .iter()
        .map(|(path, content)| ChangeInput {
            path: (*path).to_owned(),
            op: ChangeOp::Put,
            content: Some(content.clone()),
            mode: None,
        })
        .collect();
    let req = CommitReq {
        parent: None,
        changes,
        message: "a test commit".to_owned(),
        allow_empty: false,
        advance: None,
        idempotency_key: None,
    };

    ops.commit(&who, req).expect("commit").changeset
}

/// The id a blob of `bytes` is stored under.
fn blob_id(bytes: &[u8]) -> String {
    hash_object(ObjectKind::Blob, bytes).to_string()
}

fn get(ids: &[String], max_bytes: u64) -> BlobGetReq {
    BlobGetReq {
        ids: ids.to_vec(),
        max_bytes,
    }
}

#[test]
fn blob_get_returns_blobs_and_lists_missing() {
    let (_dir, ops) = new_store();
    commit(
        &ops,
        &[
            ("a.txt", ContentInput::Inline("hello".to_owned())),
            ("b.bin", ContentInput::Base64("/wD/".to_owned())),
        ],
    );
    let absent = blob_id(b"never stored");
    let ids = vec![
        blob_id(b"hello"),
        absent.clone(),
        blob_id(&[0xff, 0x00, 0xff]),
    ];

    let got = ops
        .blob_get(get(&ids, DEFAULT_BLOB_GET_MAX_BYTES))
        .expect("blob_get");

    assert!(!got.truncated);
    assert_eq!(got.missing, vec![absent]);
    assert_eq!(got.blobs.len(), 2);
    // In the order asked, each encoded as read_file encodes a file.
    assert_eq!(got.blobs[0].id, ids[0]);
    assert_eq!(got.blobs[0].size, 5);
    assert_eq!(got.blobs[0].encoding, ContentEncoding::Utf8);
    assert_eq!(got.blobs[0].content, "hello");
    assert_eq!(got.blobs[1].id, ids[2]);
    assert_eq!(got.blobs[1].size, 3);
    assert_eq!(got.blobs[1].encoding, ContentEncoding::Base64);
    assert_eq!(got.blobs[1].content, "/wD/");
}

#[test]
fn blob_get_stops_at_max_bytes_and_says_truncated() {
    let (_dir, ops) = new_store();
    commit(
        &ops,
        &[
            ("a", ContentInput::Inline("aaaa".to_owned())),
            ("b", ContentInput::Inline("bbbb".to_owned())),
            ("c", ContentInput::Inline("cccc".to_owned())),
        ],
    );
    let ids = vec![blob_id(b"aaaa"), blob_id(b"bbbb"), blob_id(b"cccc")];

    // 1. Room for two whole blobs: the third is left for the next request.
    let got = ops.blob_get(get(&ids, 8)).expect("blob_get");
    assert!(got.truncated);
    assert_eq!(got.blobs.len(), 2);
    assert_eq!(got.blobs[1].id, ids[1]);
    assert_eq!(got.missing, Vec::<String>::new());

    // 2. A blob larger than the budget still comes alone, so a batch always makes progress.
    let got = ops.blob_get(get(&ids, 1)).expect("blob_get");
    assert!(got.truncated);
    assert_eq!(got.blobs.len(), 1);
    assert_eq!(got.blobs[0].content, "aaaa");

    // 3. The rest, asked for again, is whole.
    let got = ops.blob_get(get(&ids[2..], 8)).expect("blob_get");
    assert!(!got.truncated);
    assert_eq!(got.blobs[0].content, "cccc");
}

#[test]
fn blob_get_of_a_tree_id_is_missing_not_a_blob() {
    let (_dir, ops) = new_store();
    let cs = commit(&ops, &[("a", ContentInput::Inline("x".to_owned()))]);
    let root = ops
        .show(ShowReq {
            rev: cs.as_str().into(),
            max_changes: 10,
        })
        .expect("show")
        .root;

    let got = ops
        .blob_get(get(std::slice::from_ref(&root), DEFAULT_BLOB_GET_MAX_BYTES))
        .expect("blob_get");

    assert_eq!(got.blobs.len(), 0);
    assert_eq!(got.missing, vec![root]);
    assert!(!got.truncated);
}

#[test]
fn blob_get_refuses_a_bad_id() {
    let (_dir, ops) = new_store();
    let good = blob_id(b"x");

    for bad in ["", "abc", &"g".repeat(64), &good[..63]] {
        let err = ops
            .blob_get(get(&[good.clone(), bad.to_owned()], 1024))
            .expect_err("a bad id");
        assert_eq!(err.code, ErrorCode::InvalidArg.as_str(), "{bad:?}: {err:?}");
    }
}
