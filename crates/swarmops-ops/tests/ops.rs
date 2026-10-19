//! `LocalOps`, the operations over a store on the local filesystem: each `StoreOps` operation
//! on a fresh store, the identity and change-list rules, and the operations-spec error codes
//! that failures map to (operations spec §0, §1, §4, §5).
//!
//! Every test owns a fresh store in a temp dir of its own, so tests are independent and may
//! run in any order or in parallel. Nothing here spawns a process or touches the process
//! environment: identity resolution is handed its environment as a function.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use swarmops_api::{
    Attribution, ChangeInput, ChangeOp, ChangeOut, ChangesetSummary, CommitReq, ContentEncoding,
    ContentInput, DEFAULT_LOG_LIMIT, DEFAULT_MAX_BYTES, DEFAULT_MAX_CHANGES,
    DEFAULT_READ_TREE_LIMIT, EntryKindOut, ErrorCode, FileContent, FileKind, Identity,
    IdentityInfo, InfoReq, LogPage, LogReq, ModeInput, OpError, ReadFileReq, ReadTreeReq,
    RefDeleteReq, RefListReq, RefOut, RefSetReq, RefSetResp, RepoInfo, Rev, ShowReq, StoreOps,
    TreeEntryOut, VerifyReq,
};
use swarmops_core::{
    ChangesetId, LeafKind, ObjectId, ObjectKind, RepoPath, Signature, hash_object,
};
use swarmops_ops::identity::{
    AGENT_ENV, RUN_ID_ENV, identity_info, require_identity, resolve_identity_from,
};
use swarmops_ops::{LocalOps, Resolved};
use swarmops_repo::{Change, CommitRequest, LocalRepo};
use swarmops_store::{ChangesetMeta, Layout, MetaStore};
use tempfile::TempDir;

fn utf8(path: impl Into<PathBuf>) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(path.into()).expect("temp paths are UTF-8")
}

/// A fresh store named `acme` in a temp dir of its own; the directory lives as long as the
/// `TempDir`.
fn new_store() -> (TempDir, Utf8PathBuf, LocalOps) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = utf8(dir.path().join("store"));
    let (ops, _) = LocalOps::init(&root, "acme").expect("create the store");
    (dir, root, ops)
}

fn agent() -> Identity {
    Identity::parse("agent/worker-17").expect("a valid agent name")
}

fn put(path: &str, content: ContentInput, mode: Option<ModeInput>) -> ChangeInput {
    ChangeInput {
        path: path.to_owned(),
        op: ChangeOp::Put,
        content: Some(content),
        mode,
    }
}

fn text(path: &str, content: &str) -> ChangeInput {
    put(path, ContentInput::Inline(content.to_owned()), None)
}

fn delete(path: &str) -> ChangeInput {
    ChangeInput {
        path: path.to_owned(),
        op: ChangeOp::Delete,
        content: None,
        mode: None,
    }
}

fn commit_req(parent: Option<&str>, changes: Vec<ChangeInput>) -> CommitReq {
    CommitReq {
        parent: parent.map(Rev::from),
        changes,
        message: "a test commit".to_owned(),
        allow_empty: false,
        advance: None,
        idempotency_key: None,
    }
}

/// Commits `changes` on `parent` as [`agent`]; the new changeset's id.
#[track_caller]
fn commit(ops: &LocalOps, parent: Option<&str>, changes: Vec<ChangeInput>) -> String {
    ops.commit(&agent(), commit_req(parent, changes))
        .expect("commit")
        .changeset
}

fn cat(rev: &str, path: &str) -> ReadFileReq {
    ReadFileReq {
        rev: rev.into(),
        path: path.to_owned(),
        max_bytes: DEFAULT_MAX_BYTES,
    }
}

fn ls(rev: &str, path: Option<&str>) -> ReadTreeReq {
    ReadTreeReq {
        rev: rev.into(),
        path: path.map(str::to_owned),
        limit: DEFAULT_READ_TREE_LIMIT,
        cursor: None,
    }
}

fn show(rev: &str) -> ShowReq {
    ShowReq {
        rev: rev.into(),
        max_changes: DEFAULT_MAX_CHANGES,
    }
}

fn log_req(rev: Option<&str>) -> LogReq {
    LogReq {
        rev: rev.map(Rev::from),
        limit: DEFAULT_LOG_LIMIT,
        cursor: None,
    }
}

fn set(name: &str, new: &str, expected_old: Option<&str>, create: bool) -> RefSetReq {
    RefSetReq {
        name: name.to_owned(),
        new: new.into(),
        expected_old: expected_old.map(Rev::from),
        create,
    }
}

/// `word` as a hint writes it: as it is when it is made of `[A-Za-z0-9_./:@-]` only, else in
/// single quotes, each `'` in it written `'\''` (the crate's own rule, which its unit tests pin),
/// so that a temp dir holding a space, a `+` or a `~` still matches.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:@-".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The id of a blob holding `content`, as the API writes ids.
fn blob(content: &[u8]) -> String {
    hash_object(ObjectKind::Blob, content).to_hex()
}

fn ids(page: &LogPage) -> Vec<&str> {
    page.entries.iter().map(|entry| entry.id.as_str()).collect()
}

fn unix_secs() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

/// `len` bytes that look random and are not UTF-8 (the first is `0xff`).
fn noise(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut bytes: Vec<u8> = (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect();
    bytes[0] = 0xff;
    bytes
}

/// The error `result` holds, which must be `code`'s: its string, its exit code and retry
/// advice, and a message of one line.
#[track_caller]
fn expect_err<T: Debug>(result: Result<T, OpError>, code: ErrorCode) -> OpError {
    match result {
        Ok(value) => panic!("expected {}, got Ok({value:?})", code.as_str()),
        Err(error) => {
            assert_eq!(error.code, code.as_str(), "{error:?}");
            assert_eq!(
                (error.exit, error.retryable),
                (code.exit(), code.retryable()),
                "{error:?}"
            );
            assert!(
                !error.message.is_empty() && !error.message.contains(['\n', '\r']),
                "the message is one line: {error:?}"
            );
            assert!(
                !error
                    .hint
                    .as_deref()
                    .unwrap_or_default()
                    .contains(['\n', '\r']),
                "the hint is one line: {error:?}"
            );
            error
        }
    }
}

/// Two changeset records whose ids share the 12-digit prefix `abababababab`, inserted into
/// the store's metadata directly (they have no objects): their ids, as the API writes them.
fn insert_prefix_twins(root: &Utf8Path) -> [String; 2] {
    let twins = [1u8, 2].map(|last| {
        let mut id = [0xab; 32];
        id[31] = last;
        ChangesetId(ObjectId(id))
    });
    let meta = MetaStore::open(root).unwrap();
    let mut write = meta.write().unwrap();
    for (n, id) in (0u8..).zip(twins) {
        write
            .insert_changeset(ChangesetMeta {
                id,
                seq: 0,
                generation: 0,
                delta_hash: [n; 32],
                parents: Vec::new(),
            })
            .unwrap();
    }
    write.commit().unwrap();
    twins.map(|id| id.to_string())
}

#[test]
fn init_then_info_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = utf8(dir.path().join("store"));
    let (ops, info) = LocalOps::init(&root, "acme").unwrap();
    assert_eq!(info.name, "acme");
    assert_eq!(info.repo_id.len(), 32, "{info:?}");
    assert!(
        info.repo_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{info:?}"
    );
    assert_eq!(
        (info.main.as_deref(), info.store_version),
        (None, 1),
        "{info:?}"
    );
    assert_eq!((info.changesets, info.refs), (0, 0), "{info:?}");
    assert_eq!(ops.repo_info(InfoReq {}).unwrap(), info);
    assert_eq!(ops.store_path(), root);

    // The counts and `main` follow what the store holds.
    let c1 = commit(&ops, None, vec![text("README.md", "# acme\n")]);
    ops.ref_set(&agent(), set("main", &c1, None, true)).unwrap();
    let now = ops.repo_info(InfoReq {}).unwrap();
    assert_eq!(
        now,
        RepoInfo {
            main: Some(c1.clone()),
            changesets: 1,
            refs: 1,
            ..info.clone()
        }
    );

    // Opened again, the store reports the same.
    drop(ops);
    let reopened = LocalOps::open(&root).unwrap();
    assert_eq!(reopened.repo_info(InfoReq {}).unwrap(), now);
    assert_eq!(reopened.store_path(), root);

    // A second init of the store is refused, and the hint opens the one there.
    let err = expect_err(LocalOps::init(&root, "acme"), ErrorCode::ExistsAlready);
    let repo = shell_word(root.as_str());
    assert_eq!(err.hint, Some(format!("swarmops --repo {repo} info")));
    assert_eq!(
        reopened.repo_info(InfoReq {}).unwrap(),
        now,
        "nothing changed"
    );
}

#[test]
fn commit_from_inline_and_base64_contents_then_cat_ls_show_log() {
    let (_dir, _root, ops) = new_store();
    let who = agent().with_run_id("run-3");
    let tool: &[u8] = b"\x7fELF\x00\x01\xff";
    let before = unix_secs();
    let first = ops
        .commit(
            &who,
            CommitReq {
                parent: None,
                changes: vec![
                    text("README.md", "# acme\n"),
                    put(
                        "bin/tool",
                        ContentInput::Base64(STANDARD.encode(tool)),
                        Some(ModeInput::Exec),
                    ),
                    put(
                        "docs/latest",
                        ContentInput::Inline("../README.md".to_owned()),
                        Some(ModeInput::Symlink),
                    ),
                ],
                message: "add the readme\n\nand a tool".to_owned(),
                allow_empty: false,
                advance: None,
                idempotency_key: None,
            },
        )
        .unwrap();
    let after = unix_secs();
    assert_eq!(first.parent, None);
    assert_eq!((first.files_changed, first.blobs_written), (3, 3));
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    let c1 = first.changeset.as_str();
    for id in [c1, first.root.as_str()] {
        assert!(ObjectId::from_hex(id).is_ok() && id.len() == 64, "{id}");
        assert_eq!(id, id.to_ascii_lowercase());
    }

    // cat: text as UTF-8, binary as base64, each with its kind.
    assert_eq!(
        ops.read_file(cat(c1, "README.md")).unwrap(),
        FileContent {
            blob: blob(b"# acme\n"),
            size: 7,
            kind: FileKind::File,
            encoding: ContentEncoding::Utf8,
            content: "# acme\n".to_owned(),
            truncated: false,
        }
    );
    let bin = ops.read_file(cat(c1, "bin/tool")).unwrap();
    assert_eq!(
        (bin.kind, bin.encoding, bin.size, bin.truncated),
        (FileKind::Exec, ContentEncoding::Base64, 7, false)
    );
    assert_eq!(STANDARD.decode(&bin.content).unwrap(), tool);
    assert_eq!(bin.blob, blob(tool));
    let link = ops.read_file(cat(c1, "docs/latest")).unwrap();
    assert_eq!(
        (link.kind, link.content.as_str()),
        (FileKind::Symlink, "../README.md")
    );

    // ls: the root, then a directory.
    let listing = ops.read_tree(ls(c1, None)).unwrap();
    let names: Vec<(&str, EntryKindOut)> = listing
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("README.md", EntryKindOut::File),
            ("bin", EntryKindOut::Dir),
            ("docs", EntryKindOut::Dir)
        ]
    );
    assert_eq!(listing.entries[0].id, blob(b"# acme\n"));
    assert_eq!((listing.truncated, listing.cursor), (false, None));
    assert_eq!(
        ops.read_tree(ls(c1, Some("bin"))).unwrap().entries,
        [TreeEntryOut {
            name: "tool".to_owned(),
            kind: EntryKindOut::Exec,
            id: blob(tool),
        }]
    );

    show_and_log_the_first_commit(&ops, c1, &first.root, (before, after));
    commit_on_main_by_default(&ops, &who, c1);
}

/// The changeset `c1` of the test above, with root `root`, committed between `when.0` and
/// `when.1` (Unix seconds): `show` and `log` report it in full.
fn show_and_log_the_first_commit(ops: &LocalOps, c1: &str, root: &str, when: (i64, i64)) {
    let detail = ops.show(show(c1)).unwrap();
    assert_eq!((detail.id.as_str(), detail.seq), (c1, 0));
    assert_eq!((detail.parents.len(), detail.root.as_str()), (0, root));
    assert_eq!(
        detail.author,
        Attribution {
            agent: "agent/worker-17".to_owned(),
            run_id: Some("run-3".to_owned()),
            on_behalf_of: None,
        }
    );
    assert_eq!(detail.message, "add the readme\n\nand a tool");
    let change = |path: &str, kind: FileKind, content: &[u8]| ChangeOut {
        path: path.to_owned(),
        op: ChangeOp::Put,
        kind: Some(kind),
        blob: Some(blob(content)),
    };
    assert_eq!(
        detail.changes,
        [
            change("README.md", FileKind::File, b"# acme\n"),
            change("bin/tool", FileKind::Exec, b"\x7fELF\x00\x01\xff"),
            change("docs/latest", FileKind::Symlink, b"../README.md"),
        ]
    );
    assert_eq!((detail.files_changed, detail.changes_truncated), (3, false));
    assert_eq!(
        detail.metadata,
        BTreeMap::from([
            ("agent".to_owned(), "agent/worker-17".to_owned()),
            ("run_id".to_owned(), "run-3".to_owned()),
        ])
    );
    let signature = &detail.signatures.author;
    assert_eq!(
        (signature.name.as_str(), signature.email.as_str()),
        (
            "agent/worker-17",
            "worker-17@claude-code.swarmops.local"
        )
    );
    assert_eq!(signature.offset_secs, 0);
    assert!((when.0..=when.1).contains(&signature.secs), "{signature:?}");
    assert_eq!(&detail.signatures.committer, signature);
    assert_eq!(
        detail.created_at,
        u64::try_from(signature.secs).unwrap() * 1000
    );

    let page = ops.log(log_req(Some(c1))).unwrap();
    assert_eq!(
        page.entries,
        [ChangesetSummary {
            id: c1.to_owned(),
            parents: Vec::new(),
            author_agent: "agent/worker-17".to_owned(),
            created_at: detail.created_at,
            title: "add the readme".to_owned(),
        }]
    );
    assert_eq!((page.truncated, page.cursor), (false, None));
}

/// Once `main` names `c1`, a commit that names no parent is made on it.
fn commit_on_main_by_default(ops: &LocalOps, who: &Identity, c1: &str) {
    ops.ref_set(who, set("main", c1, None, true)).unwrap();
    let second = ops
        .commit(
            who,
            CommitReq {
                parent: None,
                changes: vec![text("README.md", "# acme, v2\n"), delete("docs/latest")],
                message: "update the readme".to_owned(),
                allow_empty: false,
                advance: None,
                idempotency_key: None,
            },
        )
        .unwrap();
    assert_eq!(second.parent.as_deref(), Some(c1));
    assert_eq!((second.files_changed, second.blobs_written), (2, 1));
    let c2 = second.changeset.as_str();
    assert_eq!(
        ops.read_file(cat(c2, "README.md")).unwrap().content,
        "# acme, v2\n"
    );
    assert_eq!(
        ops.read_file(cat(c1, "README.md")).unwrap().content,
        "# acme\n",
        "an older changeset reads as it was"
    );
    let detail = ops.show(show(c2)).unwrap();
    assert_eq!(detail.parents, [c1]);
    assert_eq!(
        detail.changes,
        [
            ChangeOut {
                path: "README.md".to_owned(),
                op: ChangeOp::Put,
                kind: Some(FileKind::File),
                blob: Some(blob(b"# acme, v2\n")),
            },
            ChangeOut {
                path: "docs/latest".to_owned(),
                op: ChangeOp::Delete,
                kind: None,
                blob: None,
            },
        ]
    );
    // `log` starts at main, which still names c1; from c2 it is newest first.
    assert_eq!(ids(&ops.log(log_req(None)).unwrap()), [c1]);
    assert_eq!(ids(&ops.log(log_req(Some(c2))).unwrap()), [c2, c1]);
    assert_eq!(
        ids(&ops.log(log_req(Some(&format!("{c2}~1")))).unwrap()),
        [c1]
    );
}

#[test]
fn commit_requires_identity() {
    let no_env = |_: &str| None::<OsString>;
    // Neither `--as` nor SWARMOPS_AGENT: the caller is anonymous, and a mutation refuses.
    assert_eq!(resolve_identity_from(None, no_env).unwrap(), None);
    let err = expect_err(require_identity(None), ErrorCode::IdentityRequired);
    assert_eq!(err.exit, 5);
    let hint = err.hint.unwrap();
    assert!(hint.contains("--as") && hint.contains(AGENT_ENV), "{hint}");
    assert_eq!(
        identity_info(None),
        IdentityInfo {
            agent: None,
            run_id: None,
            auth: "anonymous".to_owned(),
        }
    );

    // SWARMOPS_AGENT names the caller, SWARMOPS_RUN_ID its run, and `--as` wins over the
    // environment. Empty variables count as unset.
    let env = |name: &str| match name {
        AGENT_ENV => Some(OsString::from("human/purple")),
        RUN_ID_ENV => Some(OsString::from("run-9")),
        _ => None,
    };
    let from_env = resolve_identity_from(None, env).unwrap().unwrap();
    assert_eq!(
        (from_env.agent(), from_env.run_id()),
        ("human/purple", Some("run-9"))
    );
    let flagged = resolve_identity_from(Some("agent/worker-1"), env)
        .unwrap()
        .unwrap();
    assert_eq!(
        (flagged.agent(), flagged.run_id()),
        ("agent/worker-1", Some("run-9"))
    );
    let empty = |_: &str| Some(OsString::new());
    assert_eq!(resolve_identity_from(None, empty).unwrap(), None);
    assert_eq!(
        resolve_identity_from(Some("human/purple"), empty)
            .unwrap()
            .unwrap()
            .run_id(),
        None
    );
    assert_eq!(
        identity_info(Some(&flagged)),
        IdentityInfo {
            agent: Some("agent/worker-1".to_owned()),
            run_id: Some("run-9".to_owned()),
            auth: "trust".to_owned(),
        }
    );

    // A name that breaks the grammar is refused from either source, as is one that is not
    // UTF-8; the error names where the name came from.
    for (flag, source) in [(Some("Worker 1"), "--as"), (None, AGENT_ENV)] {
        let bad_env = |name: &str| (name == AGENT_ENV).then(|| OsString::from("no-slash"));
        let err = expect_err(resolve_identity_from(flag, bad_env), ErrorCode::InvalidArg);
        assert_eq!(err.details["source"], source, "{err:?}");
        assert!(err.message.starts_with(source), "{err:?}");
    }
    for name in [AGENT_ENV, RUN_ID_ENV] {
        let not_utf8 = |asked: &str| {
            (asked == name)
                .then(|| OsString::from_vec(vec![0xff]))
                .or_else(|| (asked == AGENT_ENV).then(|| OsString::from("human/purple")))
        };
        let err = expect_err(resolve_identity_from(None, not_utf8), ErrorCode::InvalidArg);
        assert_eq!(err.details["source"], name, "{err:?}");
    }

    // The identity a mutation requires is the one its changeset records.
    let (_dir, _root, ops) = new_store();
    let who = require_identity(Some(flagged)).unwrap();
    let c = ops
        .commit(&who, commit_req(None, vec![text("f", "1")]))
        .unwrap()
        .changeset;
    let detail = ops.show(show(&c)).unwrap();
    assert_eq!(detail.author.agent, "agent/worker-1");
    assert_eq!(detail.author.run_id.as_deref(), Some("run-9"));
    assert_eq!(
        detail.signatures.author.email,
        "worker-1@claude-code.swarmops.local"
    );
}

#[test]
fn cat_truncates_to_max_bytes() {
    let (_dir, _root, ops) = new_store();
    let size = 3 * 1024 * 1024;
    let big = noise(size);
    let c = commit(
        &ops,
        None,
        vec![put(
            "big.bin",
            ContentInput::Base64(STANDARD.encode(&big)),
            None,
        )],
    );
    let read = |max_bytes: u64| {
        ops.read_file(ReadFileReq {
            max_bytes,
            ..cat(&c, "big.bin")
        })
        .unwrap()
    };

    let head = read(1024);
    assert!(head.truncated);
    assert_eq!(head.size, u64::try_from(size).unwrap());
    assert_eq!(head.blob, blob(&big));
    assert_eq!(head.encoding, ContentEncoding::Base64);
    assert!(
        head.content.len() <= STANDARD.encode([0_u8; 1024]).len(),
        "{}",
        head.content.len()
    );
    assert_eq!(STANDARD.decode(&head.content).unwrap(), &big[..1024]);

    // The default cap is 1 MiB; a cap at or past the size returns the whole blob.
    let default = ops.read_file(cat(&c, "big.bin")).unwrap();
    assert!(default.truncated);
    assert_eq!(
        STANDARD.decode(&default.content).unwrap(),
        &big[..1024 * 1024]
    );
    for max_bytes in [size, size + 1] {
        let whole = read(u64::try_from(max_bytes).unwrap());
        assert!(!whole.truncated);
        assert_eq!(STANDARD.decode(&whole.content).unwrap(), big);
    }
    let none = read(0);
    assert!(none.truncated);
    assert_eq!(none.content, "");
}

#[test]
fn cat_keeps_text_as_text_at_a_cut() {
    let (_dir, _root, ops) = new_store();
    let base64 =
        |path: &str, bytes: &[u8]| put(path, ContentInput::Base64(STANDARD.encode(bytes)), None);
    let c = commit(
        &ops,
        None,
        vec![
            text("emoji.txt", "a😀b"),
            text("only.txt", "😀"),
            base64("fake.bin", b"ab\xf0\x9f\xff\xff"),
            base64("tail.bin", b"ab\xf0\x9f"),
        ],
    );
    let cut = |path: &str, max_bytes: u64| {
        ops.read_file(ReadFileReq {
            max_bytes,
            ..cat(&c, path)
        })
        .unwrap()
    };

    // A cut inside the 4-byte emoji backs off to its start, up to 3 bytes short of the cap; the
    // file is still truncated, and its size is still the whole blob's.
    for (max_bytes, kept) in [
        (1, "a"),
        (2, "a"),
        (3, "a"),
        (4, "a"),
        (5, "a😀"),
        (6, "a😀b"),
    ] {
        let read = cut("emoji.txt", max_bytes);
        assert_eq!(
            (read.encoding, read.content.as_str()),
            (ContentEncoding::Utf8, kept),
            "max_bytes {max_bytes}"
        );
        assert_eq!(
            (read.size, read.truncated),
            (6, max_bytes < 6),
            "max_bytes {max_bytes}"
        );
    }
    let nothing_whole = cut("only.txt", 2);
    assert_eq!(
        (nothing_whole.encoding, nothing_whole.content.as_str()),
        (ContentEncoding::Utf8, "")
    );
    assert_eq!((nothing_whole.size, nothing_whole.truncated), (4, true));

    // Bytes that only look like the start of a character are not text: base64 of exactly the
    // cap's bytes. So is a blob that ends inside one, read whole.
    let fake = cut("fake.bin", 3);
    assert_eq!(fake.encoding, ContentEncoding::Base64);
    assert_eq!(STANDARD.decode(&fake.content).unwrap(), b"ab\xf0");
    let tail = cut("tail.bin", DEFAULT_MAX_BYTES);
    assert_eq!(
        (tail.encoding, tail.truncated),
        (ContentEncoding::Base64, false)
    );
    assert_eq!(STANDARD.decode(&tail.content).unwrap(), b"ab\xf0\x9f");

    // Random bytes cut anywhere are base64 of exactly the cap's bytes.
    let c = commit(&ops, Some(&c), vec![base64("noise.bin", &noise(64))]);
    for max_bytes in [1, 2, 3, 17, 63] {
        let read = ops
            .read_file(ReadFileReq {
                max_bytes,
                ..cat(&c, "noise.bin")
            })
            .unwrap();
        assert_eq!(read.encoding, ContentEncoding::Base64);
        let decoded = STANDARD.decode(&read.content).unwrap();
        assert_eq!(decoded, &noise(64)[..usize::try_from(max_bytes).unwrap()]);
    }
}

#[test]
fn puts_over_512_mib_are_refused_before_their_content_is_read() {
    let (dir, _root, ops) = new_store();
    let limit: u64 = 512 * 1024 * 1024;
    // A sparse file one byte over the limit: it takes no disk, and is refused by its length,
    // before a byte of it is read.
    let huge = utf8(dir.path().join("huge.bin"));
    fs::File::create(&huge).unwrap().set_len(limit + 1).unwrap();
    let err = expect_err(
        ops.commit(
            &agent(),
            commit_req(
                None,
                vec![
                    text("small", "fine"),
                    put("big/huge.bin", ContentInput::File(huge.to_string()), None),
                ],
            ),
        ),
        ErrorCode::TooLarge,
    );
    assert_eq!(err.exit, 1);
    assert_eq!(
        (&err.details["index"], &err.details["path"]),
        (&json!(1), &json!("big/huge.bin"))
    );
    assert_eq!(
        (&err.details["size"], &err.details["limit"]),
        (&json!(limit + 1), &json!(limit))
    );
    assert_eq!(err.details["file"], huge.as_str());
    assert_eq!(
        ops.repo_info(InfoReq {}).unwrap().changesets,
        0,
        "nothing was written"
    );
}

#[test]
fn cat_binary_content_is_base64() {
    let (_dir, _root, ops) = new_store();
    let bytes: &[u8] = &[0x00, 0x9f, 0x92, 0x96, 0xff, b'\n'];
    let text_content = "naïve — 東京\u{0}\n";
    let c = commit(
        &ops,
        None,
        vec![
            put(
                "blob.bin",
                ContentInput::Base64(STANDARD.encode(bytes)),
                None,
            ),
            text("text.txt", text_content),
        ],
    );

    let binary = ops.read_file(cat(&c, "blob.bin")).unwrap();
    assert_eq!(binary.encoding, ContentEncoding::Base64);
    assert_eq!(STANDARD.decode(&binary.content).unwrap(), bytes);
    assert_eq!((binary.size, binary.truncated), (6, false));
    assert_eq!(
        serde_json::to_value(&binary).unwrap()["encoding"],
        json!("base64")
    );

    // Bytes that are UTF-8 come back as text, whatever characters they hold.
    let utf8_text = ops.read_file(cat(&c, "text.txt")).unwrap();
    assert_eq!(utf8_text.encoding, ContentEncoding::Utf8);
    assert_eq!(utf8_text.content, text_content);
    assert_eq!(utf8_text.size, u64::try_from(text_content.len()).unwrap());
}

#[test]
fn ref_set_create_update_mismatch_and_already_applied() {
    let (_dir, _root, ops) = new_store();
    let who = agent();
    let c1 = commit(&ops, None, vec![text("f", "1")]);
    let c2 = commit(&ops, Some(&c1), vec![text("f", "2")]);
    let c3 = commit(&ops, Some(&c2), vec![text("f", "3")]);
    let resp = |old: Option<&str>, new: Option<&str>, already_applied: bool| RefSetResp {
        name: "main".to_owned(),
        old: old.map(str::to_owned),
        new: new.map(str::to_owned),
        already_applied,
        warnings: Vec::new(),
    };

    // Create, then the same create again: a replay, already applied.
    let created = ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
    assert_eq!(created, resp(None, Some(&c1), false));
    let replay = ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
    assert_eq!(replay, resp(None, Some(&c1), true));
    // Creating it at another changeset is a mismatch that names what the ref holds.
    let err = expect_err(
        ops.ref_set(&who, set("main", &c2, None, true)),
        ErrorCode::CasMismatch,
    );
    assert_eq!(err.details["actual"], json!(c1));
    assert_eq!(
        err.message,
        format!(
            "ref main exists already and points at {c1}: commit your change on top of it \
             (--parent {c1}), then retry with --expect {c1}; re-apply your edits to the content \
             at {c1} rather than re-sending your old files"
        )
    );
    assert_eq!(err.hint, Some(format!("swarmops show {c1}")));

    // Move with the target last seen, then replay the move.
    let moved = ops
        .ref_set(&who, set("main", &c2, Some(&c1), false))
        .unwrap();
    assert_eq!(moved, resp(Some(&c1), Some(&c2), false));
    let replay = ops
        .ref_set(&who, set("main", &c2, Some(&c1), false))
        .unwrap();
    assert_eq!(replay, resp(Some(&c1), Some(&c2), true));
    // A stale expectation is a retryable mismatch.
    let err = expect_err(
        ops.ref_set(&who, set("main", &c3, Some(&c1), false)),
        ErrorCode::CasMismatch,
    );
    assert_eq!((err.exit, err.retryable), (4, true));
    assert_eq!(err.details["actual"], json!(c2));
    assert_eq!(
        err.message,
        format!(
            "ref main moved to {c2}: commit your change on top of it (--parent {c2}), then \
             retry with --expect {c2}; re-apply your edits to the content at {c2} rather than \
             re-sending your old files"
        )
    );
    assert_eq!(err.hint, Some(format!("swarmops show {c2}")));
    // Both revisions resolve: a unique prefix and a ref name.
    let moved = ops
        .ref_set(&who, set("main", &c3[..12], Some("main"), false))
        .unwrap();
    assert_eq!(moved, resp(Some(&c2), Some(&c3), false));
    assert_eq!(
        ops.ref_list(RefListReq {}).unwrap().refs,
        [RefOut {
            name: "main".to_owned(),
            target: c3.clone(),
            version: 3,
        }]
    );

    // Exactly one of `expected_old` and `create`.
    for req in [
        set("main", &c1, Some(&c3), true),
        set("main", &c1, None, false),
    ] {
        expect_err(ops.ref_set(&who, req), ErrorCode::InvalidArg);
    }
    // Ref names are `main` or `release/<name>`.
    let err = expect_err(
        ops.ref_set(&who, set("feature/x", &c1, None, true)),
        ErrorCode::RefNameInvalid,
    );
    assert_eq!(err.details["name"], "feature/x");
    delete_refs_by_compare_and_swap(&ops, &who, [c1.as_str(), c3.as_str()]);
}

/// The operation's side of `RefSetReq`'s schema (the API's
/// `ref_set_schema_says_exactly_one_of_expected_old_and_create`): of the five shapes a request
/// can take, as JSON, `ref set` takes the two that meet one branch of its `oneOf`, an
/// `expected_old` and a `create`, and refuses both, neither, and a null `expected_old` with
/// `E_INVALID_ARG`, before it resolves anything.
#[test]
fn ref_set_takes_exactly_one_of_expected_old_and_create() {
    let (_dir, _root, ops) = new_store();
    let who = agent();
    let c1 = commit(&ops, None, vec![text("f", "1")]);
    let c2 = commit(&ops, Some(&c1), vec![text("f", "2")]);
    let request = |fields: Value| -> RefSetReq {
        let mut req = json!({"name": "main", "new": c1});
        for (key, value) in fields.as_object().unwrap() {
            req[key] = value.clone();
        }
        serde_json::from_value(req).expect("each shape reads as a request")
    };
    for (shape, fields) in [
        (
            "{expected_old, create}",
            json!({"expected_old": c1, "create": true}),
        ),
        ("{}", json!({})),
        ("{expected_old: null}", json!({"expected_old": null})),
    ] {
        let err = expect_err(ops.ref_set(&who, request(fields)), ErrorCode::InvalidArg);
        assert!(err.message.contains("exactly one"), "{shape}: {err:?}");
    }
    let created = ops.ref_set(&who, request(json!({"create": true}))).unwrap();
    assert!(!created.already_applied, "{created:?}");
    let mut moved = request(json!({"expected_old": c1}));
    moved.new = Rev::from(c2.as_str());
    assert!(!ops.ref_set(&who, moved).unwrap().already_applied);
}

/// `main` names `c3`, and `c1` is an older changeset: delete it by compare-and-swap.
fn delete_refs_by_compare_and_swap(ops: &LocalOps, who: &Identity, [c1, c3]: [&str; 2]) {
    let delete = |name: &str, expected: &str| {
        ops.ref_delete(
            who,
            RefDeleteReq {
                name: name.to_owned(),
                expected_old: expected.into(),
            },
        )
    };
    let deleted = |name: &str, old: &str, already_applied: bool| RefSetResp {
        name: name.to_owned(),
        old: Some(old.to_owned()),
        new: None,
        already_applied,
        warnings: Vec::new(),
    };
    let err = expect_err(delete("main", c1), ErrorCode::CasMismatch);
    assert_eq!(err.details["actual"], json!(c3));
    assert_eq!(
        err.message,
        format!("ref main now points at {c3}; re-check before deleting")
    );
    assert_eq!(err.hint, Some(format!("swarmops show {c3}")));
    assert_eq!(delete("main", c3).unwrap(), deleted("main", c3, false));
    let refs = ops.ref_list(RefListReq {}).unwrap().refs;
    assert!(refs.is_empty(), "{refs:?}");
    // Deleting it again finds it gone, as asked: a replay, already applied, which answers as
    // the delete did. A ref that never existed is gone just the same.
    assert_eq!(delete("main", c3).unwrap(), deleted("main", c3, true));
    assert_eq!(
        delete("release/never", c1).unwrap(),
        deleted("release/never", c1, true)
    );
    // A name that cannot be a ref's is refused, not taken for a ref that is gone.
    let err = expect_err(delete("feature/x", c1), ErrorCode::RefNameInvalid);
    assert_eq!(err.details["name"], "feature/x");
    assert!(
        ops.ref_list(RefListReq {}).unwrap().refs.is_empty(),
        "nothing was created"
    );
    // Moving a ref that does not exist is a mismatch with no actual target.
    let err = expect_err(
        ops.ref_set(who, set("main", c1, Some(c3), false)),
        ErrorCode::CasMismatch,
    );
    assert_eq!(err.details["actual"], Value::Null);
    assert_eq!(err.hint.as_deref(), Some("swarmops ref list"));
}

#[test]
fn cas_mismatch_hints_never_overwrite_the_winner() {
    let (_dir, _root, ops) = new_store();
    let who = agent();
    let c1 = commit(&ops, None, vec![text("f", "1")]);
    let c2 = commit(&ops, Some(&c1), vec![text("f", "2")]);
    let c3 = commit(&ops, Some(&c2), vec![text("f", "3")]);
    ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
    // The winner moves main on; then each kind of loser comes along.
    ops.ref_set(&who, set("main", &c2, Some(&c1), false))
        .unwrap();
    let delete = |name: &str, expected: &str| RefDeleteReq {
        name: name.to_owned(),
        expected_old: expected.into(),
    };
    let lost = [
        (ops.ref_set(&who, set("main", &c3, None, true)), Some(&c2)),
        (
            ops.ref_set(&who, set("main", &c3, Some(&c1), false)),
            Some(&c2),
        ),
        (ops.ref_delete(&who, delete("main", &c1)), Some(&c2)),
        (
            ops.ref_set(&who, set("release/1", &c3, Some(&c1), false)),
            None,
        ),
    ];
    for (result, actual) in lost {
        let err = expect_err(result, ErrorCode::CasMismatch);
        assert_eq!(err.details["actual"], json!(actual), "{err:?}");
        // The hint only reads: running it cannot undo the winner's move.
        let hint = err.hint.clone().unwrap();
        let reads = match actual {
            Some(actual) => hint == format!("swarmops show {actual}"),
            None => hint == "swarmops ref list",
        };
        assert!(reads, "{err:?}");
        for forbidden in ["--expect", "--create", "ref set", "ref delete"] {
            assert!(!hint.contains(forbidden), "{err:?}");
        }
    }
    assert_eq!(
        ops.repo_info(InfoReq {}).unwrap().main.as_deref(),
        Some(c2.as_str())
    );
}

/// `commit` with `advance`: the changeset and its ref's move in one call and one transaction
/// (operations spec §1, §4).
mod advance {
    use swarmops_api::Advanced;

    use super::*;

    /// A request for `changes` on `parent` ([`commit_req`]) that advances ref `name`.
    fn advancing(parent: Option<&str>, changes: Vec<ChangeInput>, name: &str) -> CommitReq {
        CommitReq {
            advance: Some(name.to_owned()),
            ..commit_req(parent, changes)
        }
    }

    /// The journal frames of the store at `root`, each as the kinds of its records.
    fn frames(root: &Utf8Path) -> Vec<Vec<&'static str>> {
        let meta = MetaStore::open(root).unwrap();
        let entries = meta.read().unwrap().journal_after(None, 1_000).unwrap();
        let mut frames: Vec<(u64, Vec<&'static str>)> = Vec::new();
        for entry in entries {
            match frames.last_mut() {
                Some((seq, kinds)) if *seq == entry.seq => kinds.push(entry.kind),
                _ => frames.push((entry.seq, vec![entry.kind])),
            }
        }
        frames.into_iter().map(|(_, kinds)| kinds).collect()
    }

    /// A commit with `advance` moves the ref in the same call and the same journal frame as its
    /// changeset, and says so in `advanced`; `ref_list` shows the move.
    #[test]
    fn commit_with_advance_moves_the_ref_in_one_call() {
        let (_dir, root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let before = frames(&root);

        let resp = ops
            .commit(&who, advancing(Some(&c1), vec![text("f", "2")], "main"))
            .unwrap();
        let c2 = resp.changeset.clone();
        assert_eq!(resp.parent.as_deref(), Some(c1.as_str()));
        let moved = Advanced {
            name: "main".to_owned(),
            old: Some(c1.clone()),
            new: c2.clone(),
            already_applied: false,
        };
        assert_eq!(resp.advanced, Some(moved));
        assert_eq!(resp.warnings, Vec::<String>::new());
        let refs = ops.ref_list(RefListReq {}).unwrap().refs;
        let main = RefOut {
            name: "main".to_owned(),
            target: c2,
            version: 2,
        };
        assert_eq!(refs, [main]);
        let after = frames(&root);
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(
            after[before.len()..],
            [vec!["changeset", "ref_move"]],
            "one frame"
        );

        // A commit without `advance` still moves no ref, and reports no move.
        let plain = ops
            .commit(&who, commit_req(None, vec![text("f", "3")]))
            .unwrap();
        assert_eq!(plain.advanced, None);
        let main = ops.repo_info(InfoReq {}).unwrap().main;
        assert_eq!(main.as_deref(), Some(resp.changeset.as_str()));
    }

    /// A commit with `advance` whose ref moved since its parent was read records nothing:
    /// `E_CAS_MISMATCH` (exit 4, retryable) with what the ref names and `committed: false`, a
    /// message that says to commit again on top of it, `--parent <actual> --advance main`, and a
    /// hint that only reads. The store's changesets and main are as they were.
    #[test]
    fn commit_with_advance_that_loses_is_cas_mismatch_and_records_nothing() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        // Another agent publishes c2 on c1 first.
        let won = ops
            .commit(&who, advancing(Some(&c1), vec![text("f", "2")], "main"))
            .unwrap();
        let c2 = won.changeset;
        let before = ops.repo_info(InfoReq {}).unwrap();

        let lost = ops.commit(&who, advancing(Some(&c1), vec![text("f", "3")], "main"));
        let err = expect_err(lost, ErrorCode::CasMismatch);
        assert_eq!((err.exit, err.retryable), (4, true));
        assert_eq!(
            Value::Object(err.details.clone()),
            json!({"name": "main", "actual": c2, "committed": false})
        );
        let advice = format!("--parent {c2} --advance main");
        assert!(err.message.contains(&advice), "{err:?}");
        assert!(err.message.contains("recorded nothing"), "{err:?}");
        let hint = err.hint.clone().unwrap();
        assert_eq!(hint, format!("swarmops show {c2}"), "the hint only reads");
        assert_eq!(
            ops.repo_info(InfoReq {}).unwrap(),
            before,
            "nothing recorded"
        );
    }

    /// Without `parent`, a commit with `advance` builds on that ref's head, not main's, and the
    /// envelope's `meta.resolved` records the ref under its name; a ref that does not exist makes
    /// the commit a root commit that creates it.
    #[test]
    fn commit_with_advance_and_no_parent_builds_on_that_ref() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let r1 = commit(&ops, Some(&c1), vec![text("r", "1")]);
        ops.ref_set(&who, set("release/r1", &r1, None, true))
            .unwrap();
        let resolved = Resolved::default();
        let request = ops.recording(&resolved);

        let on_r1 = request
            .commit(&who, advancing(None, vec![text("r", "2")], "release/r1"))
            .unwrap();
        assert_eq!(on_r1.parent.as_deref(), Some(r1.as_str()), "r1's head");
        assert_eq!(
            resolved.take(),
            BTreeMap::from([("release/r1".to_owned(), r1.clone())])
        );
        let moved = on_r1.advanced.unwrap();
        assert_eq!(
            (moved.old.as_deref(), moved.new.as_str()),
            (Some(r1.as_str()), on_r1.changeset.as_str())
        );

        let fresh = request
            .commit(&who, advancing(None, vec![text("n", "1")], "release/new"))
            .unwrap();
        assert_eq!(fresh.parent, None, "a root commit");
        assert!(resolved.take().is_empty(), "nothing resolved");
        let created = fresh.advanced.unwrap();
        assert_eq!((created.old, created.new), (None, fresh.changeset.clone()));
        let refs = ops.ref_list(RefListReq {}).unwrap().refs;
        let names: Vec<(&str, &str)> = (refs.iter())
            .map(|r| (r.name.as_str(), r.target.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("main", c1.as_str()),
                ("release/new", fresh.changeset.as_str()),
                ("release/r1", on_r1.changeset.as_str()),
            ]
        );

        // A bad ref name is refused before anything is resolved or written.
        let bad = request.commit(&who, advancing(None, vec![text("x", "1")], "feature/x"));
        let err = expect_err(bad, ErrorCode::RefNameInvalid);
        assert_eq!(err.details["name"], "feature/x");
        assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 4);
    }
}

/// `commit` with an `idempotency_key`, `commit --key` (operations spec §4): a uuid v7 whose time
/// signs the commit and which the changeset records as its `commit_key` metadata, so that a
/// retry replays the first attempt instead of recording a second changeset.
mod commit_key {
    use swarmops_api::{Advanced, CommitResp, Signatures};

    use super::*;

    /// A key's time, in Unix milliseconds: 2001-09-09T01:46:40.999Z, far from the clock a test
    /// runs at, with milliseconds that a signature's whole seconds drop.
    const KEY_MS: u64 = 1_000_000_000_999;

    /// A uuid v7 (RFC 9562) of time `ms`, in Unix milliseconds, whose random bits come from
    /// `seed`: 48 bits of time, the version 7, 12 bits, the variant `10`, 62 bits.
    fn key_at(ms: u64, seed: u64) -> String {
        let rand_b = (seed & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
        let hex = format!("{ms:012x}7{:03x}{rand_b:016x}", seed & 0xfff);
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    }

    /// [`commit_req`] with `key` as its `idempotency_key`.
    fn keyed(parent: Option<&str>, changes: Vec<ChangeInput>, key: &str) -> CommitReq {
        CommitReq {
            idempotency_key: Some(key.to_owned()),
            ..commit_req(parent, changes)
        }
    }

    /// [`keyed`], also advancing `main`.
    fn keyed_advancing(parent: Option<&str>, changes: Vec<ChangeInput>, key: &str) -> CommitReq {
        CommitReq {
            advance: Some("main".to_owned()),
            ..keyed(parent, changes, key)
        }
    }

    /// How many records the journal of the store at `root` holds: a request that writes no
    /// frame adds none.
    fn journal_records(root: &Utf8Path) -> usize {
        let meta = MetaStore::open(root).unwrap();
        meta.read()
            .unwrap()
            .journal_after(None, 1_000)
            .unwrap()
            .len()
    }

    /// The same keyed request twice, on the same parent, is one changeset: the second answers
    /// `replayed` with the first's response but for its `blobs_written`, 0, and writes no frame;
    /// `repo_info.changesets` grew by one. Given its parent by its full id, the request is the
    /// same changeset again.
    #[test]
    fn commit_with_a_key_twice_is_one_changeset() {
        let (_dir, root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let before = ops.repo_info(InfoReq {}).unwrap().changesets;
        let key = key_at(KEY_MS, 1);
        let changes = || vec![text("f", "2"), text("g", "3")];

        // On main's head by default, which a commit without advance leaves where it is.
        let first = ops.commit(&who, keyed(None, changes(), &key)).unwrap();
        assert!(!first.replayed, "{first:?}");
        assert_eq!(first.blobs_written, 2);
        let records = journal_records(&root);
        let again = ops.commit(&who, keyed(None, changes(), &key)).unwrap();
        let replay = CommitResp {
            blobs_written: 0,
            replayed: true,
            ..first.clone()
        };
        assert_eq!(again, replay);
        assert_eq!(journal_records(&root), records, "no frame");
        let after = ops.repo_info(InfoReq {}).unwrap().changesets;
        assert_eq!(after, before + 1);

        let pinned = ops.commit(&who, keyed(Some(&c1), changes(), &key)).unwrap();
        assert_eq!(pinned, replay);
    }

    /// A keyed commit signs at its key's time, to the second: `show`'s `created_at` is the key's
    /// milliseconds rounded down to a whole second, and both signatures hold those seconds, not
    /// the clock's, which an unkeyed commit signs at. (Through the binary, `SWARMOPS_TEST_NOW`
    /// does not move it either: `commit_with_a_key_twice_through_the_binary`.)
    #[test]
    fn a_commit_key_pins_the_signature_time() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let key = key_at(KEY_MS, 2);
        let cs = ops
            .commit(&who, keyed(None, vec![text("f", "1")], &key))
            .unwrap()
            .changeset;
        let detail = ops.show(show(&cs)).unwrap();
        assert_eq!(detail.created_at, 1_000_000_000_000);
        let Signatures { author, committer } = detail.signatures;
        assert_eq!(
            (author.secs, committer.secs),
            (1_000_000_000, 1_000_000_000)
        );

        let now = unix_secs();
        let unkeyed = commit(&ops, Some(&cs), vec![text("f", "2")]);
        let committer = ops.show(show(&unkeyed)).unwrap().signatures.committer;
        assert!(committer.secs >= now, "the clock's time: {committer:?}");
    }

    /// A key that is not a uuid v7 is `E_INVALID_ARG` naming it in `details.idempotency_key`,
    /// before anything is resolved or written: a v4 uuid and a v8 one, variants other than RFC
    /// 9562's, a uuid without its hyphens or with one more digit, a digit that is not hex, a word
    /// and nothing.
    #[test]
    fn a_commit_key_must_be_a_uuid_v7() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let v7 = key_at(KEY_MS, 3);
        let at = |index: usize, digit: &str| {
            let mut key = v7.clone();
            key.replace_range(index..=index, digit);
            key
        };
        let refused = [
            at(14, "4"),
            at(14, "8"),
            at(19, "c"),
            at(19, "7"),
            v7.replace('-', ""),
            format!("{v7}0"),
            at(35, "g"),
            "retry-1".to_owned(),
            String::new(),
        ];
        for key in refused {
            let resolved = Resolved::default();
            let request = keyed(Some("main"), vec![text("f", "2")], &key);
            let result = ops.recording(&resolved).commit(&who, request);
            let err = expect_err(result, ErrorCode::InvalidArg);
            assert_eq!(err.details["idempotency_key"], key.as_str(), "{err:?}");
            assert!(resolved.take().is_empty(), "{key:?}: nothing resolved");
        }
        assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 1);
    }

    /// A keyed commit records its key as `commit_key` metadata, beside `agent` and `run_id`, in
    /// lowercase also when it was given in uppercase, which is the same key: a retry with it in
    /// lowercase is a replay.
    #[test]
    fn a_keyed_commit_records_its_key() {
        let (_dir, _root, ops) = new_store();
        let who = agent().with_run_id("run-3");
        // Its variant digit is b, so that the uppercase key has a B there.
        let key = key_at(KEY_MS, 0x3000_0000_0000_0004);
        let shouted = key.to_uppercase();
        assert_ne!(shouted, key);
        let first = ops
            .commit(&who, keyed(None, vec![text("f", "1")], &shouted))
            .unwrap();
        let metadata = ops.show(show(&first.changeset)).unwrap().metadata;
        let expected = BTreeMap::from([
            ("agent".to_owned(), "agent/worker-17".to_owned()),
            ("commit_key".to_owned(), key.clone()),
            ("run_id".to_owned(), "run-3".to_owned()),
        ]);
        assert_eq!(metadata, expected);
        let again = ops
            .commit(&who, keyed(None, vec![text("f", "1")], &key))
            .unwrap();
        assert!(again.replayed, "{again:?}");
        assert_eq!(again.changeset, first.changeset);
    }

    /// An advancing commit retried with its key and no parent finds the ref's head carrying the
    /// key, and replays it: the first attempt's changeset, parent and move, `replayed`,
    /// `advanced.already_applied`, no second changeset and no frame, with `meta.resolved`
    /// naming the ref's head as the request resolved it. Retried with the first attempt's parent
    /// given, it is the same replay.
    #[test]
    fn an_advancing_commit_retried_with_its_key_is_a_replay() {
        let (_dir, root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let key = key_at(KEY_MS, 5);
        let request = |parent: Option<&str>| keyed_advancing(parent, vec![text("f", "2")], &key);
        let first = ops.commit(&who, request(None)).unwrap();
        let c2 = first.changeset.clone();
        let moved = Advanced {
            name: "main".to_owned(),
            old: Some(c1.clone()),
            new: c2.clone(),
            already_applied: false,
        };
        assert_eq!(first.advanced, Some(moved.clone()));
        let (info, records) = (ops.repo_info(InfoReq {}).unwrap(), journal_records(&root));

        let resolved = Resolved::default();
        let again = ops
            .recording(&resolved)
            .commit(&who, request(None))
            .unwrap();
        let replay = CommitResp {
            blobs_written: 0,
            advanced: Some(Advanced {
                already_applied: true,
                ..moved
            }),
            replayed: true,
            ..first
        };
        assert_eq!(again, replay);
        let main_at_c2 = BTreeMap::from([("main".to_owned(), c2.clone())]);
        assert_eq!(resolved.take(), main_at_c2);
        assert_eq!(
            resolved.take_recovered(),
            None,
            "its write repaired nothing"
        );
        let pinned = ops.commit(&who, request(Some(&c1))).unwrap();
        assert_eq!(pinned, replay);
        assert_eq!(
            ops.repo_info(InfoReq {}).unwrap(),
            info,
            "no second changeset"
        );
        assert_eq!(journal_records(&root), records, "no frame");
    }

    /// The changeset a keyed commit would build on is its replay when it carries the key, so it
    /// is not made again on top of itself: given as `parent`, or as `main`'s head when the
    /// request names none, after the first attempt was published by a `ref set` of its own.
    #[test]
    fn a_keyed_commit_on_its_own_changeset_is_a_replay() {
        let (_dir, root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let key = key_at(KEY_MS, 6);
        let first = ops
            .commit(&who, keyed(None, vec![text("f", "2")], &key))
            .unwrap();
        let c2 = first.changeset.clone();
        ops.ref_set(&who, set("main", &c2, Some(&c1), false))
            .unwrap();
        let records = journal_records(&root);
        let replay = CommitResp {
            blobs_written: 0,
            replayed: true,
            ..first
        };
        for parent in [Some(c2.as_str()), None] {
            let again = ops
                .commit(&who, keyed(parent, vec![text("f", "2")], &key))
                .unwrap();
            assert_eq!(again, replay, "parent {parent:?}");
        }
        assert_eq!(journal_records(&root), records, "no frame");
        assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 2);
    }

    /// With `advance`, a keyed commit whose parent is its own changeset replays only while the
    /// ref names it: once the ref has moved on, it is `E_CAS_MISMATCH` naming what the ref holds,
    /// `committed: false`, as the commit itself would be, and nothing is recorded.
    #[test]
    fn a_keyed_advance_on_its_changeset_after_the_ref_moved_is_a_cas_mismatch() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let key = key_at(KEY_MS, 7);
        let c2 = ops
            .commit(&who, keyed_advancing(None, vec![text("f", "2")], &key))
            .unwrap()
            .changeset;
        let c3 = ops
            .commit(
                &who,
                CommitReq {
                    advance: Some("main".to_owned()),
                    ..commit_req(Some(&c2), vec![text("g", "3")])
                },
            )
            .unwrap()
            .changeset;
        let before = ops.repo_info(InfoReq {}).unwrap();

        let request = keyed_advancing(Some(&c2), vec![text("f", "2")], &key);
        let err = expect_err(ops.commit(&who, request), ErrorCode::CasMismatch);
        assert_eq!(
            Value::Object(err.details.clone()),
            json!({"name": "main", "actual": c3, "committed": false})
        );
        assert_eq!(
            ops.repo_info(InfoReq {}).unwrap(),
            before,
            "nothing recorded"
        );
    }

    /// `replayed` says the request wrote nothing: a keyed retry whose changeset the store holds
    /// already, but whose ref was moved back off it in between (as an undo does), moves the ref
    /// to it again, in a frame of its own, and so answers as the first attempt did, with
    /// `replayed` and `already_applied` false; it wrote no blob, and no second changeset.
    #[test]
    fn a_keyed_retry_that_moves_its_ref_again_is_no_replay() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let key = key_at(KEY_MS, 9);
        let request = || keyed_advancing(Some(&c1), vec![text("f", "2")], &key);
        let first = ops.commit(&who, request()).unwrap();
        let c2 = first.changeset.clone();
        ops.ref_set(&who, set("main", &c1, Some(&c2), false))
            .unwrap();

        let again = ops.commit(&who, request()).unwrap();
        let moved_again = CommitResp {
            blobs_written: 0,
            ..first
        };
        assert_eq!(again, moved_again);
        let info = ops.repo_info(InfoReq {}).unwrap();
        assert_eq!((info.changesets, info.main), (2, Some(c2)));
    }

    /// What `commit --key` leaves open (known limitations): a keyed commit that names no parent
    /// builds on `main`'s head as it is when it runs, so a retry after `main` moved to another
    /// changeset commits again, on the new head. Given its parent by its full id, the retry is
    /// the first attempt's changeset.
    #[test]
    fn a_keyed_retry_after_main_moved_commits_again_unless_its_parent_is_pinned() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let key = key_at(KEY_MS, 8);
        let first = ops
            .commit(&who, keyed(None, vec![text("f", "2")], &key))
            .unwrap();
        // Another agent's changeset becomes main's head in between.
        let other = commit(&ops, Some(&c1), vec![text("g", "x")]);
        ops.ref_set(&who, set("main", &other, Some(&c1), false))
            .unwrap();

        let again = ops
            .commit(&who, keyed(None, vec![text("f", "2")], &key))
            .unwrap();
        assert!(!again.replayed, "{again:?}");
        assert_ne!(again.changeset, first.changeset);
        assert_eq!(again.parent.as_deref(), Some(other.as_str()));
        let pinned = ops
            .commit(&who, keyed(Some(&c1), vec![text("f", "2")], &key))
            .unwrap();
        assert!(pinned.replayed, "{pinned:?}");
        assert_eq!(pinned.changeset, first.changeset);
        assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 4);
    }
}

#[test]
fn hints_quote_what_they_interpolate() {
    let (dir, root, ops) = new_store();
    let c = commit(
        &ops,
        None,
        vec![
            text("my dir/f", "1"),
            text("odd\nname/f", "2"),
            text("it's/f", "3"),
        ],
    );
    let hint_of = |result: Result<FileContent, OpError>, code: ErrorCode| {
        expect_err(result, code).hint.unwrap()
    };
    // A word with characters a shell would split or expand comes in single quotes; a line
    // break in it is escaped, so the hint stays one line.
    let is_dir = |path: &str| hint_of(ops.read_file(cat(&c, path)), ErrorCode::IsDir);
    assert_eq!(is_dir("my dir"), format!("swarmops ls '{c}:my dir'"));
    assert_eq!(is_dir("odd\nname"), format!("swarmops ls '{c}:odd\\nname'"));
    assert_eq!(is_dir("it's"), format!("swarmops ls '{c}:it'\\''s'"));
    let missing = hint_of(
        ops.read_file(cat(&c, "my dir/no file")),
        ErrorCode::PathNotFound,
    );
    assert_eq!(missing, format!("swarmops ls '{c}:my dir'"));
    let not_a_dir = expect_err(
        ops.read_tree(ls(&c, Some("my dir/f"))),
        ErrorCode::PathNotFound,
    );
    assert_eq!(not_a_dir.hint, Some(format!("swarmops cat '{c}:my dir/f'")));
    // A plain word stays as it is.
    assert_eq!(
        expect_err(ops.read_file(cat(&c, "nope")), ErrorCode::PathNotFound).hint,
        Some(format!("swarmops ls {c}"))
    );

    // Store paths too.
    let spaced = utf8(dir.path().join("my store"));
    let (_spaced_ops, _) = LocalOps::init(&spaced, "spaced").unwrap();
    let err = expect_err(LocalOps::init(&spaced, "spaced"), ErrorCode::ExistsAlready);
    let quoted = shell_word(spaced.as_str());
    assert!(
        quoted.starts_with('\''),
        "a path with a space is quoted: {quoted}"
    );
    assert_eq!(err.hint, Some(format!("swarmops --repo {quoted} info")));
    let nowhere = utf8(dir.path().join("no store"));
    let err = expect_err(LocalOps::open(&nowhere), ErrorCode::NotARepo);
    let quoted = shell_word(nowhere.as_str());
    assert!(
        quoted.starts_with('\''),
        "a path with a space is quoted: {quoted}"
    );
    assert_eq!(err.hint, Some(format!("swarmops init {quoted}")));
    assert_eq!(
        expect_err(LocalOps::init(&root, "acme"), ErrorCode::ExistsAlready).hint,
        Some(format!(
            "swarmops --repo {} info",
            shell_word(root.as_str())
        ))
    );
}

#[test]
fn resolve_errors_map_to_codes() {
    let (_dir, root, ops) = new_store();
    // Before main exists, `main` (log's default) names nothing.
    let err = expect_err(ops.log(log_req(None)), ErrorCode::RevNotFound);
    assert_eq!(err.details["rev"], "main");
    assert_eq!(err.hint.as_deref(), Some("swarmops ref list"));

    let c1 = commit(&ops, None, vec![text("f", "1")]);
    // An id or a prefix of no changeset, and a `~N` past the root, name nothing.
    let unknown = "0".repeat(64);
    let past_root = format!("{c1}~1");
    for rev in [unknown.as_str(), &unknown[..12], past_root.as_str()] {
        let err = expect_err(ops.show(show(rev)), ErrorCode::RevNotFound);
        assert_eq!(err.details["rev"], rev);
        assert_eq!(err.exit, 6);
    }
    // Every operation that takes a revision resolves it the same way.
    expect_err(ops.read_file(cat(&unknown, "f")), ErrorCode::RevNotFound);
    expect_err(ops.read_tree(ls(&unknown, None)), ErrorCode::RevNotFound);
    expect_err(ops.log(log_req(Some(&unknown))), ErrorCode::RevNotFound);
    expect_err(
        ops.commit(&agent(), commit_req(Some(&unknown), vec![text("f", "2")])),
        ErrorCode::RevNotFound,
    );
    expect_err(
        ops.ref_set(&agent(), set("main", &unknown, None, true)),
        ErrorCode::RevNotFound,
    );

    // A string that is not a revision at all is a malformed argument.
    for rev in ["", "HEAD", "main^", "feature/x", &c1[..11]] {
        let err = expect_err(ops.show(show(rev)), ErrorCode::InvalidArg);
        assert_eq!(err.details["rev"], rev);
    }

    // A prefix that two changesets share is ambiguous; these two differ in the last digit
    // only, so their full ids tell them apart (but the records have no objects, as in a
    // damaged store).
    let twins = insert_prefix_twins(&root);
    let err = expect_err(ops.show(show("abababababab")), ErrorCode::AmbiguousId);
    assert_eq!(err.details["rev"], "abababababab");
    assert_eq!(err.exit, 1);
    assert!(err.hint.is_some());
    let err = expect_err(ops.show(show(&twins[0])), ErrorCode::StoreCorrupt);
    assert_eq!(err.hint.as_deref(), Some("swarmops verify"));
    // A unique prefix of the real changeset still resolves.
    assert_eq!(ops.show(show(&c1[..12])).unwrap().id, c1);
}

#[test]
fn resolved_revisions_are_reported_once() {
    let (_dir, _root, ops) = new_store();
    let resolved = Resolved::default();
    let request = ops.recording(&resolved);
    let c1 = request
        .commit(&agent(), commit_req(None, vec![text("f", "1")]))
        .unwrap()
        .changeset;
    // A root commit resolves nothing.
    assert_eq!(resolved.take(), BTreeMap::new());
    request
        .ref_set(&agent(), set("main", &c1[..12], None, true))
        .unwrap();
    assert_eq!(
        resolved.take(),
        BTreeMap::from([(c1[..12].to_owned(), c1.clone())])
    );
    request.read_file(cat("main", "f")).unwrap();
    assert_eq!(
        resolved.take(),
        BTreeMap::from([("main".to_owned(), c1.clone())])
    );
    assert!(resolved.take().is_empty(), "taking empties it");

    // A commit with no parent, and a log with no revision, resolve `main`.
    let c2 = request
        .commit(&agent(), commit_req(None, vec![text("f", "2")]))
        .unwrap()
        .changeset;
    assert_eq!(
        resolved.take(),
        BTreeMap::from([("main".to_owned(), c1.clone())])
    );
    request.log(log_req(None)).unwrap();
    request.show(show(&format!("{c2}~1"))).unwrap();
    assert_eq!(
        resolved.take(),
        BTreeMap::from([
            ("main".to_owned(), c1.clone()),
            (format!("{c2}~1"), c1.clone()),
        ])
    );

    // Each request records into its own record only, and an operation called on `LocalOps`
    // itself into none.
    let other = Resolved::default();
    ops.recording(&other).show(show(&c2)).unwrap();
    ops.read_file(cat("main", "f")).unwrap();
    assert_eq!(other.take(), BTreeMap::from([(c2.clone(), c2.clone())]));
    assert!(resolved.take().is_empty());
}

#[test]
fn open_on_non_repo_is_e_not_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let base = utf8(dir.path());
    let file = base.join("file");
    fs::write(&file, b"not a store").unwrap();
    // A store that lost its REPO file is not one either.
    let (_store_dir, broken, ops) = new_store();
    drop(ops);
    fs::remove_file(Layout::new(broken.clone()).repo_file()).unwrap();

    // An empty directory and a missing one hold no store: the hint creates one there.
    for path in [base.clone(), base.join("missing")] {
        let err = expect_err(LocalOps::open(&path), ErrorCode::NotARepo);
        assert_eq!(err.exit, 6);
        let init = format!("swarmops init {}", shell_word(path.as_str()));
        assert_eq!(err.hint, Some(init), "{err:?}");
    }
    assert!(!base.join("missing").exists(), "open creates nothing");
    // Neither a file nor that broken store is a store, and init cannot help there.
    for path in [file, broken] {
        let err = expect_err(LocalOps::open(&path), ErrorCode::NotARepo);
        assert_eq!(err.hint, None, "{err:?}");
    }
}

#[test]
fn init_refuses_mnt_path() {
    // WSL mounts Windows drives under /mnt/; the refusal is by path, so /mnt need not exist.
    let path = Utf8PathBuf::from(format!("/mnt/swarmops-test-{}/store", std::process::id()));
    let err = expect_err(LocalOps::init(&path, "acme"), ErrorCode::UnsupportedFs);
    assert_eq!(err.exit, 1);
    assert_eq!(err.details["path"], path.as_str());
    assert!(err.hint.is_some());
    assert!(!path.exists(), "nothing was created");
}

#[test]
fn change_lists_are_checked_and_their_contents_resolved() {
    let (dir, _root, ops) = new_store();
    // A file's content comes from the client's filesystem.
    let payload = utf8(dir.path().join("payload.bin"));
    fs::write(&payload, b"\xff from disk").unwrap();
    let c1 = commit(
        &ops,
        None,
        vec![put(
            "a/payload",
            ContentInput::File(payload.to_string()),
            None,
        )],
    );
    let read = ops.read_file(cat(&c1, "a/payload")).unwrap();
    assert_eq!(STANDARD.decode(read.content).unwrap(), b"\xff from disk");

    // Each of these is refused before anything is written, naming the change.
    let missing = utf8(dir.path().join("missing.bin"));
    let refused = |changes: Vec<ChangeInput>| {
        expect_err(
            ops.commit(&agent(), commit_req(Some(&c1), changes)),
            ErrorCode::InvalidArg,
        )
    };
    let err = refused(vec![
        text("ok", "1"),
        put("b", ContentInput::File(missing.to_string()), None),
    ]);
    assert_eq!(
        (
            &err.details["index"],
            &err.details["path"],
            &err.details["file"]
        ),
        (&json!(1), &json!("b"), &json!(missing.as_str()))
    );
    for bad in ["not base64!", "AAE", "AA=E"] {
        let err = refused(vec![put("b", ContentInput::Base64(bad.to_owned()), None)]);
        assert_eq!(err.details["path"], "b", "{bad}");
    }
    let no_content = ChangeInput {
        content: None,
        ..text("b", "")
    };
    let delete_with_content = ChangeInput {
        op: ChangeOp::Delete,
        ..text("a/payload", "x")
    };
    let delete_with_mode = ChangeInput {
        mode: Some(ModeInput::Exec),
        ..delete("a/payload")
    };
    for change in [no_content, delete_with_content, delete_with_mode] {
        let err = refused(vec![change.clone()]);
        assert_eq!(err.details["path"], change.path.as_str(), "{change:?}");
    }
    let err = refused(vec![text("b", "1"), text("c", "2"), text("b", "3")]);
    assert_eq!(err.details["path"], "b");
    for path in [
        "",
        "/abs",
        "a//b",
        "a/./b",
        "a/../b",
        "trailing/",
        "nul\0byte",
    ] {
        let err = refused(vec![text(path, "x")]);
        assert_eq!(err.details["path"], path);
    }
    assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 1);
}

#[test]
fn commit_errors_map_to_codes() {
    let (_dir, _root, ops) = new_store();
    let c1 = commit(&ops, None, vec![text("dir/f", "1"), text("file", "x")]);
    let commit_on_c1 =
        |changes: Vec<ChangeInput>| ops.commit(&agent(), commit_req(Some(&c1), changes));

    // Nothing to record: no changes, or only puts of what is there already.
    for changes in [vec![], vec![text("file", "x")]] {
        let err = expect_err(commit_on_c1(changes), ErrorCode::EmptyCommit);
        assert!(err.hint.unwrap().contains("--allow-empty"));
    }
    // ...unless an empty changeset is asked for: it keeps its parent's root.
    let empty = ops
        .commit(
            &agent(),
            CommitReq {
                allow_empty: true,
                ..commit_req(Some(&c1), vec![])
            },
        )
        .unwrap();
    assert_eq!((empty.files_changed, empty.blobs_written), (0, 0));
    assert_eq!(empty.root, ops.show(show(&c1)).unwrap().root);

    // A delete of no file; a put below a file, onto a directory, or a delete of one.
    let err = expect_err(commit_on_c1(vec![delete("nope")]), ErrorCode::PathNotFound);
    assert_eq!(err.details["path"], "nope");
    for change in [text("file/below", "x"), text("dir", "x"), delete("dir")] {
        let err = expect_err(commit_on_c1(vec![change.clone()]), ErrorCode::TypeConflict);
        assert_eq!(err.details["path"], change.path.as_str());
    }
    // A path with a line break still gets a one-line message (`expect_err` checks).
    let c2 = commit(&ops, Some(&c1), vec![text("odd\nname", "x")]);
    let err = expect_err(
        ops.commit(
            &agent(),
            commit_req(Some(&c2), vec![text("odd\nname/x", "y")]),
        ),
        ErrorCode::TypeConflict,
    );
    assert_eq!(err.details["path"], "odd\nname/x");
    // A parent the revision names must exist: `main` does not yet.
    expect_err(
        ops.commit(&agent(), commit_req(Some("main"), vec![text("g", "1")])),
        ErrorCode::RevNotFound,
    );
}

#[test]
fn read_errors_map_to_codes() {
    let (_dir, _root, ops) = new_store();
    let c = commit(&ops, None, vec![text("dir/f", "1"), text("file", "x")]);

    let err = expect_err(ops.read_file(cat(&c, "dir")), ErrorCode::IsDir);
    assert_eq!(
        (&err.details["rev"], &err.details["path"]),
        (&json!(c), &json!("dir"))
    );
    assert_eq!(err.hint, Some(format!("swarmops ls {c}:dir")));
    for path in ["nope", "dir/nope", "file/below"] {
        let err = expect_err(ops.read_file(cat(&c, path)), ErrorCode::PathNotFound);
        assert_eq!(err.details["path"], path);
    }
    for path in ["", "/dir/f", "dir//f"] {
        expect_err(ops.read_file(cat(&c, path)), ErrorCode::InvalidArg);
    }

    // `ls` of a file, or of nothing.
    let err = expect_err(ops.read_tree(ls(&c, Some("file"))), ErrorCode::PathNotFound);
    assert_eq!(err.hint, Some(format!("swarmops cat {c}:file")));
    expect_err(ops.read_tree(ls(&c, Some("nope"))), ErrorCode::PathNotFound);
    // An empty path is the root, and one trailing `/` is taken (`ls main:dir/`); two are not.
    let listing = |path: Option<&str>| ops.read_tree(ls(&c, path)).unwrap();
    assert_eq!(listing(Some("")), listing(None));
    assert_eq!(listing(Some("/")), listing(None));
    assert_eq!(listing(Some("dir/")), listing(Some("dir")));
    let err = expect_err(ops.read_tree(ls(&c, Some("dir//"))), ErrorCode::InvalidArg);
    assert_eq!(err.details["path"], "dir//");

    // Limits start at 1.
    let zero_ls = ReadTreeReq {
        limit: 0,
        ..ls(&c, None)
    };
    let zero_log = LogReq {
        limit: 0,
        ..log_req(Some(&c))
    };
    let zero_show = ShowReq {
        max_changes: 0,
        ..show(&c)
    };
    expect_err(ops.read_tree(zero_ls), ErrorCode::InvalidArg);
    expect_err(ops.log(zero_log), ErrorCode::InvalidArg);
    expect_err(ops.show(zero_show), ErrorCode::InvalidArg);
    // A log cursor is a changeset id the store holds.
    let unknown = "0".repeat(64);
    for cursor in ["main", &c[..12], "zz", unknown.as_str()] {
        let req = LogReq {
            cursor: Some(cursor.to_owned()),
            ..log_req(None)
        };
        let err = expect_err(ops.log(req), ErrorCode::InvalidArg);
        assert_eq!(err.details["cursor"], cursor);
    }
}

#[test]
fn listings_page_with_cursors() {
    let (_dir, _root, ops) = new_store();
    let files = ["a", "b", "c", "d", "e"].map(|name| text(name, name));
    let mut chain = vec![commit(&ops, None, files.to_vec())];
    for n in 0..4 {
        let next = commit(
            &ops,
            chain.last().map(String::as_str),
            vec![text("a", &n.to_string())],
        );
        chain.push(next);
    }
    let head = chain.last().unwrap().clone();

    // ls, two entries a page; each cursor is the page's last name.
    let mut pages = Vec::new();
    let mut cursor = None;
    loop {
        let page = ops
            .read_tree(ReadTreeReq {
                limit: 2,
                cursor: cursor.clone(),
                ..ls(&head, None)
            })
            .unwrap();
        let names: Vec<String> = page.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            page.cursor.as_ref(),
            page.truncated.then(|| names.last().unwrap())
        );
        pages.push(names);
        if !page.truncated {
            break;
        }
        cursor = page.cursor;
    }
    assert_eq!(pages, [vec!["a", "b"], vec!["c", "d"], vec!["e"]]);
    let after_e = ReadTreeReq {
        cursor: Some("e".to_owned()),
        ..ls(&head, None)
    };
    let past_the_end = ops.read_tree(after_e).unwrap();
    assert!(past_the_end.entries.is_empty(), "{past_the_end:?}");

    // log, two changesets a page, newest first; each cursor is the page's last id.
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = ops
            .log(LogReq {
                limit: 2,
                cursor: cursor.clone(),
                ..log_req(Some(&head))
            })
            .unwrap();
        seen.extend(page.entries.iter().map(|e| e.id.clone()));
        assert_eq!(
            page.cursor.as_ref(),
            page.truncated.then(|| seen.last().unwrap())
        );
        if !page.truncated {
            break;
        }
        cursor = page.cursor;
    }
    chain.reverse();
    assert_eq!(seen, chain);
    // A page that ends at the root is the last, and a cursor at the root gives an empty page.
    let two = ops
        .log(LogReq {
            limit: 2,
            ..log_req(Some(&chain[3]))
        })
        .unwrap();
    assert_eq!(
        (two.entries.len(), two.truncated, two.cursor),
        (2, false, None)
    );
    let root_cursor = LogReq {
        cursor: Some(chain[4].clone()),
        ..log_req(None)
    };
    let empty = ops.log(root_cursor).unwrap();
    assert_eq!((empty.entries.len(), empty.truncated), (0, false));

    // show caps `changes` and still counts them all.
    let capped = ops
        .show(ShowReq {
            max_changes: 2,
            ..show(&chain[4])
        })
        .unwrap();
    let paths: Vec<&str> = capped.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a", "b"]);
    assert_eq!((capped.files_changed, capped.changes_truncated), (5, true));
    let exact = ops
        .show(ShowReq {
            max_changes: 5,
            ..show(&chain[4])
        })
        .unwrap();
    assert_eq!((exact.changes.len(), exact.changes_truncated), (5, false));
}

#[test]
fn show_and_log_decode_what_native_commits_do_not_write() {
    let (_dir, root, ops) = new_store();
    // A changeset made below the API: no agent metadata, a value and message bytes that are
    // not UTF-8, a signature with a UTC offset.
    let repo = LocalRepo::open(&root).unwrap();
    let signature = Signature {
        name: b"Ada \xffLovelace".to_vec(),
        email: b"ada@example.com".to_vec(),
        secs: 1_760_000_000,
        offset_secs: 3600,
        negative_zero: false,
    };
    let outcome = repo
        .commit(CommitRequest {
            parents: Vec::new(),
            changes: vec![Change {
                path: RepoPath::new("f").unwrap(),
                op: swarmops_repo::ChangeOp::Put {
                    kind: LeafKind::File,
                    content: b"x".to_vec(),
                },
            }],
            author: signature.clone(),
            committer: Signature {
                secs: 1_760_000_123,
                ..signature
            },
            message: b"caf\xe9\nsecond line".to_vec(),
            metadata: vec![
                ("blob".to_owned(), vec![0xff, 0x00]),
                ("note".to_owned(), b"hi".to_vec()),
            ],
            allow_empty: false,
            advance: None,
        })
        .unwrap();
    let id = outcome.id.to_string();

    let detail = ops.show(show(&id)).unwrap();
    assert_eq!(
        detail.author,
        Attribution {
            agent: "unknown/unknown".to_owned(),
            run_id: None,
            on_behalf_of: None,
        }
    );
    assert_eq!(
        detail.metadata,
        BTreeMap::from([
            ("blob:b64".to_owned(), "/wA=".to_owned()),
            ("note".to_owned(), "hi".to_owned()),
        ])
    );
    assert_eq!(detail.message, "caf\u{fffd}\nsecond line");
    assert_eq!(detail.signatures.author.name, "Ada \u{fffd}Lovelace");
    assert_eq!(detail.signatures.author.offset_secs, 3600);
    assert_eq!(detail.signatures.committer.secs, 1_760_000_123);
    assert_eq!(detail.created_at, 1_760_000_123_000);

    let page = ops.log(log_req(Some(&id))).unwrap();
    assert_eq!(page.entries[0].author_agent, "unknown/unknown");
    assert_eq!(page.entries[0].title, "caf\u{fffd}");
}

#[test]
fn verify_reports_problems_as_lines() {
    let (_dir, root, ops) = new_store();
    let c = commit(&ops, None, vec![text("f", "content")]);
    for deep in [false, true] {
        let report = ops.verify(VerifyReq { deep }).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!(report.changesets_checked, 1);
        assert!(report.objects_checked >= 3, "{report:?}");
    }

    // A blob gone from the store: verify says so, and reading it is E_STORE_CORRUPT.
    let gone = hash_object(ObjectKind::Blob, b"content");
    fs::remove_file(Layout::new(root.clone()).object_path(gone)).unwrap();
    let report = ops.verify(VerifyReq { deep: false }).unwrap();
    assert_eq!(
        report.problems,
        [format!(
            "object {gone} is missing; changeset {c} references it"
        )]
    );
    let err = expect_err(ops.read_file(cat(&c, "f")), ErrorCode::StoreCorrupt);
    assert_eq!(err.details["object"], gone.to_hex());
}

/// `verify` reports each problem as a finding beside its line, at the same index: a blob gone
/// from the store is a `missing_object` that names it and the changeset that references it, in
/// a structural verify and a deep one alike; a clean store has none.
#[test]
fn verify_reports_each_problem_as_a_finding() {
    let (_dir, root, ops) = new_store();
    let c = commit(&ops, None, vec![text("f", "content")]);
    assert_eq!(ops.verify(VerifyReq { deep: true }).unwrap().findings, []);

    let gone = hash_object(ObjectKind::Blob, b"content");
    fs::remove_file(Layout::new(root.clone()).object_path(gone)).unwrap();
    for deep in [false, true] {
        let report = ops.verify(VerifyReq { deep }).unwrap();
        assert_eq!(report.problems.len(), 1, "{report:?}");
        assert_eq!(
            serde_json::to_value(&report.findings).unwrap(),
            json!([{"kind": "missing_object", "object": gone.to_hex(), "changeset": c}]),
            "deep {deep}"
        );
    }
}
