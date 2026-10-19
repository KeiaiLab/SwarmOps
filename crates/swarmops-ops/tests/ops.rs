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
        offset: 0,
        lines: None,
        max_bytes: DEFAULT_MAX_BYTES,
    }
}

fn ls(rev: &str, path: Option<&str>) -> ReadTreeReq {
    ReadTreeReq {
        rev: rev.into(),
        path: path.map(str::to_owned),
        depth: 1,
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
        paths: Vec::new(),
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
            offset: None,
            lines: None,
            next_offset: None,
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
            path: None,
            kind: EntryKindOut::Exec,
            size: Some(7),
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
    assert_eq!(err.details["scope"], "put", "one content's limit");
    assert_eq!(
        ops.repo_info(InfoReq {}).unwrap().changesets,
        0,
        "nothing was written"
    );
}

/// A commit's contents together are held to 1 GiB, measured by the sizes they declare before
/// any is read: three sparse client files of 512 MiB each, each within a put's limit, are
/// refused at the third (`E_TOO_LARGE`, scope `commit`, its `size` theirs together with it),
/// and nothing is written. The hint stores large contents first, to name them by id.
#[test]
fn a_commit_s_contents_past_1_gib_together_are_refused_before_any_is_read() {
    let (dir, _root, ops) = new_store();
    let limit: u64 = 512 * 1024 * 1024;
    let files: Vec<Utf8PathBuf> = (0..3)
        .map(|n| {
            let file = utf8(dir.path().join(format!("{n}.bin")));
            fs::File::create(&file).unwrap().set_len(limit).unwrap();
            file
        })
        .collect();
    let changes = (files.iter().enumerate())
        .map(|(n, file)| {
            put(
                &format!("big/{n}.bin"),
                ContentInput::File(file.to_string()),
                None,
            )
        })
        .collect();
    let err = expect_err(
        ops.commit(&agent(), commit_req(None, changes)),
        ErrorCode::TooLarge,
    );
    let hint = err.hint.clone().unwrap_or_default();
    assert_eq!(
        Value::Object(err.details),
        json!({
            "index": 2, "path": "big/2.bin", "file": files[2].as_str(),
            "size": 3 * limit, "limit": 2 * limit, "scope": "commit",
        })
    );
    assert!(hint.contains("swarmops blob put"), "{hint}");
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

/// One snapshot per request (operations spec §4, storage spec §8): every revision and ref one
/// request names is resolved in the same view, and that view is the request's alone.
mod snapshot {
    use super::*;

    /// A request never reads in a snapshot taken before its own write, nor in another request's:
    /// on one `LocalOps`, and through one request's record, a `show` of what a `commit` made, a
    /// `ref_list` after the `ref_set` that created `main`, and a `cat`, a `log` and an `info` of
    /// `main` after a `commit` that advanced it, each see what the write before it did.
    #[test]
    fn a_snapshot_is_not_reused_after_the_requests_own_write() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let resolved = Resolved::default();
        let request = ops.recording(&resolved);

        let c1 = (request.commit(&who, commit_req(None, vec![text("f", "1")])))
            .unwrap()
            .changeset;
        assert_eq!(request.show(show(&c1)).unwrap().id, c1);
        request.ref_set(&who, set("main", &c1, None, true)).unwrap();
        let refs = request.ref_list(RefListReq {}).unwrap().refs;
        let targets: Vec<(&str, &str)> = (refs.iter())
            .map(|r| (r.name.as_str(), r.target.as_str()))
            .collect();
        assert_eq!(targets, [("main", c1.as_str())]);

        let advancing = CommitReq {
            advance: Some("main".to_owned()),
            ..commit_req(Some(&c1), vec![text("f", "2")])
        };
        let c2 = request.commit(&who, advancing).unwrap().changeset;
        assert_eq!(request.read_file(cat("main", "f")).unwrap().content, "2");
        let log = request.log(log_req(None)).unwrap();
        assert_eq!(ids(&log), [c2.as_str(), c1.as_str()]);
        let main = request.repo_info(InfoReq {}).unwrap().main;
        assert_eq!(main.as_deref(), Some(c2.as_str()));
        assert_eq!(
            resolved.take()["main"],
            c2,
            "main as the last read resolved it"
        );
    }
}

/// `log` with `paths` and `path_history` (operations spec §1; M2 Task 15): the changesets whose
/// delta touches a path, by the chase of its last modifier, a page at a time.
mod path_history {
    use std::sync::Arc;

    use swarmops_api::{
        DEFAULT_PATH_HISTORY_LIMIT, Envelope, Meta, PathHistoryEntry, PathHistoryPage,
        PathHistoryReq,
    };
    use swarmops_testkit::fs::NoSyncFs;

    use super::*;

    /// A `path_history` request for `path` from `rev` (main without one), a page of the default
    /// size.
    fn history(rev: Option<&str>, path: &str) -> PathHistoryReq {
        PathHistoryReq {
            rev: rev.map(Rev::from),
            path: path.to_owned(),
            limit: DEFAULT_PATH_HISTORY_LIMIT,
            cursor: None,
        }
    }

    /// The changesets of `page`.
    fn changesets(page: &PathHistoryPage) -> Vec<&str> {
        page.entries
            .iter()
            .map(|entry| entry.changeset.as_str())
            .collect()
    }

    /// A first-parent chain of `ops`'s commits, one per step (`allow_empty`, so a step may change
    /// nothing), and `main` created on its head: the ids, the root first.
    fn chain(ops: &LocalOps, steps: Vec<Vec<ChangeInput>>) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for changes in steps {
            let req = CommitReq {
                allow_empty: true,
                ..commit_req(ids.last().map(String::as_str), changes)
            };
            ids.push(ops.commit(&agent(), req).unwrap().changeset);
        }
        let head = ids.last().unwrap();
        ops.ref_set(&agent(), set("main", head, None, true))
            .unwrap();
        ids
    }

    /// `log` with `paths` lists only the changesets whose delta has an entry at one of the paths
    /// or below it, newest first, each once: a file's, a directory's (with or without a trailing
    /// `/`), several at once, and none (an empty page, not an error). A page is `truncated`
    /// exactly when another such changeset follows, and its cursor resumes after it. A path that
    /// is not one is `E_INVALID_ARG`.
    #[test]
    fn log_with_paths_lists_only_the_changesets_that_touch_them() {
        let (_dir, _root, ops) = new_store();
        let c = chain(
            &ops,
            vec![
                vec![text("a", "1"), text("b", "1")],
                vec![text("b", "2")],
                vec![text("c/x", "1")],
                vec![text("a", "2")],
                vec![],
                vec![text("c/y", "1")],
                vec![delete("c/x"), delete("c/y")],
            ],
        );
        let log = |paths: &[&str], limit: usize, cursor: Option<&str>| {
            let req = LogReq {
                limit,
                cursor: cursor.map(str::to_owned),
                paths: paths.iter().map(|&p| p.to_owned()).collect(),
                ..log_req(None)
            };
            ops.log(req).unwrap()
        };
        let at =
            |indices: &[usize]| -> Vec<&str> { indices.iter().map(|&i| c[i].as_str()).collect() };
        assert_eq!(ids(&log(&["a"], 50, None)), at(&[3, 0]));
        assert_eq!(ids(&log(&["c"], 50, None)), at(&[6, 5, 2]));
        assert_eq!(ids(&log(&["c/"], 50, None)), at(&[6, 5, 2]));
        assert_eq!(ids(&log(&["a", "c/x"], 50, None)), at(&[6, 3, 2, 0]));
        assert_eq!(
            ids(&log(&["a", "a/"], 50, None)),
            at(&[3, 0]),
            "each changeset once"
        );
        let none = log(&["zz"], 50, None);
        assert_eq!(
            (none.entries.len(), none.truncated, none.cursor),
            (0, false, None)
        );

        let first = log(&["c"], 2, None);
        assert_eq!((ids(&first), first.truncated), (at(&[6, 5]), true));
        assert_eq!(first.cursor.as_deref(), Some(c[5].as_str()));
        let rest = log(&["c"], 2, first.cursor.as_deref());
        assert_eq!(
            (ids(&rest), rest.truncated, rest.cursor.as_deref()),
            (at(&[2]), false, None)
        );
        let exact = log(&["a"], 2, None);
        assert_eq!(
            (ids(&exact), exact.truncated),
            (at(&[3, 0]), false),
            "nothing older"
        );
        let after_root = log(&["a"], 2, Some(&c[0]));
        assert_eq!((after_root.entries.len(), after_root.truncated), (0, false));

        // From a revision, recorded as resolved.
        let resolved = Resolved::default();
        let from_c3 = LogReq {
            paths: vec!["b".to_owned()],
            ..log_req(Some(&c[3]))
        };
        let page = ops.recording(&resolved).log(from_c3).unwrap();
        assert_eq!(ids(&page), at(&[1, 0]));
        assert_eq!(
            resolved.take(),
            BTreeMap::from([(c[3].clone(), c[3].clone())])
        );

        for bad in ["a//b", "/", "", "./a"] {
            let req = LogReq {
                paths: vec!["a".to_owned(), bad.to_owned()],
                ..log_req(None)
            };
            let err = expect_err(ops.log(req), ErrorCode::InvalidArg);
            assert_eq!(err.details["path"], bad);
        }
    }

    /// A path's history, a page at a time: each entry what the path holds after its changeset
    /// (`put` with its kind and id, `delete` with neither) and who wrote it when; the cursor, the
    /// last entry's id, resumes from that changeset's first parent, and the last page is not
    /// `truncated`. A cursor at the root gives an empty page; one that is not a full id the store
    /// holds is `E_INVALID_ARG`. A directory's entries name its tree.
    #[test]
    fn history_pages_resume_after_the_cursor() {
        let (_dir, _root, ops) = new_store();
        let c = chain(
            &ops,
            vec![
                vec![text("p", "0"), text("q", "0")],
                vec![text("q", "1")],
                vec![text("p", "2")],
                vec![text("q", "3")],
                vec![delete("p")],
                vec![text("q", "5")],
                vec![put(
                    "p",
                    ContentInput::Inline("6".to_owned()),
                    Some(ModeInput::Exec),
                )],
                vec![text("dir/x", "7")],
            ],
        );
        let resolved = Resolved::default();
        let page = |cursor: Option<&str>| {
            let req = PathHistoryReq {
                limit: 2,
                cursor: cursor.map(str::to_owned),
                ..history(None, "p")
            };
            ops.recording(&resolved).path_history(req).unwrap()
        };
        let first = page(None);
        assert_eq!(first.path, "p");
        assert_eq!(changesets(&first), [c[6].as_str(), c[4].as_str()]);
        assert_eq!(
            (first.truncated, first.cursor.as_deref()),
            (true, Some(c[4].as_str()))
        );
        assert_eq!(
            first.entries[0],
            PathHistoryEntry {
                changeset: c[6].clone(),
                op: ChangeOp::Put,
                kind: Some(EntryKindOut::Exec),
                id: Some(blob(b"6")),
                author_agent: "agent/worker-17".to_owned(),
                created_at: first.entries[0].created_at,
                title: "a test commit".to_owned(),
            }
        );
        assert!(first.entries[0].created_at >= 1_760_000_000_000);
        let deleted = &first.entries[1];
        assert_eq!(
            (deleted.op, &deleted.kind, &deleted.id),
            (ChangeOp::Delete, &None, &None)
        );
        assert_eq!(
            resolved.take(),
            BTreeMap::from([("main".to_owned(), c[7].clone())])
        );

        let second = page(first.cursor.as_deref());
        assert_eq!(changesets(&second), [c[2].as_str(), c[0].as_str()]);
        assert_eq!((second.truncated, second.cursor), (false, None));
        assert_eq!(second.entries[1].kind, Some(EntryKindOut::File));
        assert_eq!(second.entries[1].id, Some(blob(b"0")));
        assert!(resolved.take().is_empty(), "a cursor resolves no revision");
        let after_root = page(Some(&c[0]));
        assert_eq!((after_root.entries.len(), after_root.truncated), (0, false));

        let dir = ops.path_history(history(None, "dir/")).unwrap();
        assert_eq!(dir.path, "dir");
        assert_eq!(changesets(&dir), [c[7].as_str()]);
        let tree = ops.read_tree(ls("main", None)).unwrap();
        let listed = tree
            .entries
            .iter()
            .find(|entry| entry.name == "dir")
            .unwrap();
        assert_eq!(
            (dir.entries[0].kind, dir.entries[0].id.as_deref()),
            (Some(EntryKindOut::Dir), Some(listed.id.as_str()))
        );

        let unknown = "0".repeat(64);
        for cursor in ["main", &c[0][..12], "zz", unknown.as_str()] {
            let req = PathHistoryReq {
                cursor: Some(cursor.to_owned()),
                ..history(None, "p")
            };
            let err = expect_err(ops.path_history(req), ErrorCode::InvalidArg);
            assert_eq!(err.details["cursor"], cursor);
        }
    }

    /// A path that no changeset on the first-parent chain ever touched is `E_PATH_NOT_FOUND`
    /// `{rev, path}`, its hint the listing of the directory it would be in, whether it never
    /// existed or exists only after the revision asked about; a path deleted since still has its
    /// history. A limit of 0 and a path that is not one are `E_INVALID_ARG`, an unknown revision
    /// `E_REV_NOT_FOUND`.
    #[test]
    fn a_path_that_never_existed_is_path_not_found() {
        let (_dir, _root, ops) = new_store();
        let c = chain(
            &ops,
            vec![
                vec![text("src/lib.rs", "1"), text("gone", "1")],
                vec![delete("gone")],
                vec![text("later", "1")],
            ],
        );
        let nope = expect_err(
            ops.path_history(history(None, "nope")),
            ErrorCode::PathNotFound,
        );
        assert_eq!(nope.details["rev"], "main");
        assert_eq!(nope.details["path"], "nope");
        assert_eq!(nope.hint.as_deref(), Some("swarmops ls main"));
        let below = expect_err(
            ops.path_history(history(None, "src/nope")),
            ErrorCode::PathNotFound,
        );
        assert_eq!(below.hint.as_deref(), Some("swarmops ls main:src"));
        let not_yet = expect_err(
            ops.path_history(history(Some(&c[1]), "later")),
            ErrorCode::PathNotFound,
        );
        assert_eq!(not_yet.details["rev"], c[1].as_str());

        let gone = ops.path_history(history(None, "gone")).unwrap();
        assert_eq!(changesets(&gone), [c[1].as_str(), c[0].as_str()]);
        assert_eq!(gone.entries[0].op, ChangeOp::Delete);

        let zero = PathHistoryReq {
            limit: 0,
            ..history(None, "gone")
        };
        expect_err(ops.path_history(zero), ErrorCode::InvalidArg);
        for bad in ["", "/", "a//b"] {
            let err = expect_err(ops.path_history(history(None, bad)), ErrorCode::InvalidArg);
            assert_eq!(err.details["path"], bad);
        }
        expect_err(
            ops.path_history(history(Some("release/none"), "gone")),
            ErrorCode::RevNotFound,
        );
    }

    /// Every page an agent gets by default stays under the operations spec's 64 KiB target (§9):
    /// over a path that 500 changesets changed, with 60-byte titles, the default `path_history`
    /// page and the default `log` page with that path, each in its envelope.
    #[test]
    fn a_default_history_page_stays_under_64_kib() {
        let (_dir, root, ops) = new_store();
        // Committed below the API, without syncs: what the pages hold is the subject here.
        let repo = LocalRepo::open_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
        let signature = Signature {
            name: b"agent/worker-17".to_vec(),
            email: b"worker-17@claude-code.swarmops.local".to_vec(),
            secs: 1_760_000_000,
            offset_secs: 0,
            negative_zero: false,
        };
        let mut head: Option<ChangesetId> = None;
        for i in 0..500_u32 {
            let content = i.to_string();
            let title = format!("{:x<55}{i:05}", "change the path, a title of 60 bytes ");
            assert_eq!(title.len(), 60);
            let outcome = repo
                .commit(CommitRequest {
                    parents: head.into_iter().collect(),
                    changes: vec![Change {
                        path: RepoPath::new("src/p").unwrap(),
                        op: swarmops_repo::ChangeOp::Put {
                            kind: LeafKind::File,
                            content: content.into_bytes(),
                        },
                    }],
                    author: signature.clone(),
                    committer: signature.clone(),
                    message: format!("{title}\n\nthe body is not in the page").into_bytes(),
                    metadata: vec![
                        ("agent".to_owned(), b"agent/worker-17".to_vec()),
                        ("run_id".to_owned(), b"run-17".to_vec()),
                    ],
                    allow_empty: false,
                    advance: None,
                })
                .unwrap();
            head = Some(outcome.id);
        }
        drop(repo);
        let head = head.unwrap().to_string();
        ops.ref_set(&agent(), set("main", &head, None, true))
            .unwrap();
        let meta = || Meta {
            repo: Some(root.to_string()),
            elapsed_ms: 1_000,
            resolved: BTreeMap::from([("main".to_owned(), head.clone())]),
            auth: "trust".to_owned(),
            recovered: None,
        };

        let page = ops.path_history(history(None, "src/p")).unwrap();
        assert_eq!(
            (page.entries.len(), page.truncated),
            (DEFAULT_PATH_HISTORY_LIMIT, true)
        );
        let bytes = serde_json::to_vec(&Envelope::ok("path_history", page, meta())).unwrap();
        assert!(bytes.len() <= 65_536, "path_history: {} bytes", bytes.len());

        let log = LogReq {
            paths: vec!["src".to_owned()],
            ..log_req(None)
        };
        let page = ops.log(log).unwrap();
        assert_eq!(
            (page.entries.len(), page.truncated),
            (DEFAULT_LOG_LIMIT, true)
        );
        let bytes = serde_json::to_vec(&Envelope::ok("log", page, meta())).unwrap();
        assert!(bytes.len() <= 65_536, "log --path: {} bytes", bytes.len());
    }
}

/// Blobs by id (operations spec §1's `blob_put` and `blob_has`, §0's `{blob: id}` content):
/// content stored first, outside any commit, then named by a commit's change list by its id.
mod blob {
    use swarmops_api::{BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp};

    use super::*;

    /// Stores `content` as [`agent`]: what `blob_put` returns.
    #[track_caller]
    fn blob_put(ops: &LocalOps, content: ContentInput) -> BlobPutResp {
        ops.blob_put(&agent(), BlobPutReq { content })
            .expect("blob_put")
    }

    /// A put of blob `id`, named by its id, as a regular file at `path`.
    fn named(path: &str, id: &str) -> ChangeInput {
        put(path, ContentInput::Blob(id.to_owned()), None)
    }

    /// `blob_put` stores content as a blob and says whether it placed it (`written`): from a
    /// client file, then the same bytes in base64, stored already. A commit names the blob by
    /// id, as a file and as an executable: it writes no blob, and reads back as the content.
    /// The changes made with the content itself give the same root. `blob_put` takes inline,
    /// base64 or file content, not `{"blob": id}`, which names content the store holds already.
    #[test]
    fn blob_put_then_commit_by_id() {
        let (dir, _root, ops) = new_store();
        let content = noise(4096);
        let file = utf8(dir.path().join("payload.bin"));
        fs::write(&file, &content).unwrap();
        let first = blob_put(&ops, ContentInput::File(file.to_string()));
        assert_eq!(
            first,
            BlobPutResp {
                blob: blob(&content),
                size: 4096,
                written: true,
            }
        );
        let again = blob_put(&ops, ContentInput::Base64(STANDARD.encode(&content)));
        assert_eq!(
            again,
            BlobPutResp {
                written: false,
                ..first.clone()
            }
        );

        let by_id = ops
            .commit(
                &agent(),
                commit_req(
                    None,
                    vec![
                        named("data/payload.bin", &first.blob),
                        put(
                            "tool",
                            ContentInput::Blob(first.blob.to_uppercase()),
                            Some(ModeInput::Exec),
                        ),
                    ],
                ),
            )
            .unwrap();
        assert_eq!((by_id.files_changed, by_id.blobs_written), (2, 0));
        let read = ops
            .read_file(cat(&by_id.changeset, "data/payload.bin"))
            .unwrap();
        assert_eq!(
            (read.blob.as_str(), read.size, read.kind),
            (first.blob.as_str(), 4096, FileKind::File)
        );
        assert_eq!(STANDARD.decode(&read.content).unwrap(), content);
        let tool = ops.read_file(cat(&by_id.changeset, "tool")).unwrap();
        assert_eq!((tool.blob, tool.kind), (first.blob.clone(), FileKind::Exec));
        let shown = ops.show(show(&by_id.changeset)).unwrap();
        assert_eq!(
            shown.changes[0],
            ChangeOut {
                path: "data/payload.bin".to_owned(),
                op: ChangeOp::Put,
                kind: Some(FileKind::File),
                blob: Some(first.blob.clone()),
            }
        );

        let by_content = ops
            .commit(
                &agent(),
                commit_req(
                    None,
                    vec![
                        put(
                            "data/payload.bin",
                            ContentInput::File(file.to_string()),
                            None,
                        ),
                        put(
                            "tool",
                            ContentInput::Base64(STANDARD.encode(&content)),
                            Some(ModeInput::Exec),
                        ),
                    ],
                ),
            )
            .unwrap();
        assert_eq!(by_content.root, by_id.root);

        let err = expect_err(
            ops.blob_put(
                &agent(),
                BlobPutReq {
                    content: ContentInput::Blob(first.blob.clone()),
                },
            ),
            ErrorCode::InvalidArg,
        );
        assert_eq!(err.details["blob"], first.blob.as_str(), "{err:?}");
    }

    /// A change that names by id a blob the store lacks, a tree or a changeset is
    /// `E_INVALID_ARG` `{index, path, blob}`, whose hint stores the content first; one whose id
    /// is not 64 hex digits is refused with the list's other malformed changes. Either way the
    /// commit writes nothing, the put of new content beside it included.
    #[test]
    fn a_commit_naming_a_missing_blob_or_a_tree_is_an_invalid_argument() {
        let (_dir, _root, ops) = new_store();
        let c1 = commit(&ops, None, vec![text("a.txt", "a")]);
        let tree = ops.show(show(&c1)).unwrap().root;
        let missing = blob(b"never stored");
        for (id, what) in [
            (missing.as_str(), "a blob the store lacks"),
            (tree.as_str(), "a tree"),
            (c1.as_str(), "a changeset"),
        ] {
            let changes = vec![text("new.txt", "new"), named("b/named.bin", id)];
            let err = expect_err(
                ops.commit(&agent(), commit_req(Some(&c1), changes)),
                ErrorCode::InvalidArg,
            );
            assert_eq!(
                Value::Object(err.details),
                json!({"index": 1, "path": "b/named.bin", "blob": id}),
                "{what}"
            );
            assert!(
                err.message.starts_with("change 1 (\"b/named.bin\"): "),
                "{what}: {}",
                err.message
            );
            assert_eq!(
                err.hint.as_deref(),
                Some("swarmops blob put <file>"),
                "{what}"
            );
        }
        let long = format!("{missing}0");
        let not_hex = "g".repeat(64);
        for bad in ["abc", long.as_str(), not_hex.as_str()] {
            let changes = vec![text("new.txt", "new"), named("x", bad)];
            let err = expect_err(
                ops.commit(&agent(), commit_req(Some(&c1), changes)),
                ErrorCode::InvalidArg,
            );
            assert_eq!(
                Value::Object(err.details),
                json!({"index": 1, "path": "x", "blob": bad}),
                "{bad}"
            );
        }
        assert_eq!(ops.repo_info(InfoReq {}).unwrap().changesets, 1);
        let unwritten = BlobHasReq {
            ids: vec![blob(b"new")],
        };
        assert_eq!(
            ops.blob_has(unwritten).unwrap().present,
            Vec::<String>::new()
        );
    }

    /// `blob_has` gives the ids of those asked for that the store holds as blobs, in lowercase
    /// hex, in the order first given, each once: a tree, a changeset and an id the store lacks
    /// are not among them. An id that is not 64 hex digits is `E_INVALID_ARG` `{index, blob}`.
    #[test]
    fn blob_has_reports_the_present_blobs() {
        let (_dir, _root, ops) = new_store();
        let stored = blob_put(&ops, ContentInput::Inline("stored\n".to_owned())).blob;
        let c1 = commit(&ops, None, vec![text("a.txt", "a")]);
        let tree = ops.show(show(&c1)).unwrap().root;
        let committed = blob(b"a");
        let unknown = blob(b"never stored");
        let ask = |ids: &[&str]| {
            ops.blob_has(BlobHasReq {
                ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            })
        };
        let has = ask(&[
            &unknown,
            &stored.to_uppercase(),
            &tree,
            &c1,
            &committed,
            &stored,
        ])
        .unwrap();
        assert_eq!(
            has,
            BlobHasResp {
                present: vec![stored.clone(), committed],
            }
        );
        assert_eq!(ask(&[]).unwrap().present, Vec::<String>::new());
        let err = expect_err(ask(&[&stored, "not-an-id"]), ErrorCode::InvalidArg);
        assert_eq!(
            Value::Object(err.details),
            json!({"index": 1, "blob": "not-an-id"})
        );
    }

    /// `blob_put` is a mutation, as `commit` is (operations spec §5): it takes the caller's
    /// identity, which the CLI and the MCP server refuse to go without (`E_IDENTITY_REQUIRED`,
    /// their tests), and records nothing of it, a blob carrying no metadata. `blob_has` is a
    /// read, and takes none.
    #[test]
    fn blob_put_needs_an_identity() {
        let _: fn(&LocalOps, &Identity, BlobPutReq) -> Result<BlobPutResp, OpError> =
            <LocalOps as StoreOps>::blob_put;
        let _: fn(&LocalOps, BlobHasReq) -> Result<BlobHasResp, OpError> =
            <LocalOps as StoreOps>::blob_has;
        let err = expect_err(require_identity(None), ErrorCode::IdentityRequired);
        assert_eq!(err.exit, 5);
        // Two callers that store the same bytes store one blob: the first places it.
        let (_dir, _root, ops) = new_store();
        let content = || BlobPutReq {
            content: ContentInput::Inline("shared\n".to_owned()),
        };
        let one = Identity::parse("agent/worker-1").unwrap();
        let two = Identity::parse("agent/worker-2").unwrap();
        let first = ops.blob_put(&one, content()).unwrap();
        let second = ops.blob_put(&two, content()).unwrap();
        assert_eq!((first.written, second.written), (true, false));
        assert_eq!(first.blob, second.blob);
    }

    /// `blob_put`'s content is held to a put's limit, 512 MiB, measured before it is read: a
    /// sparse file one byte over it is `E_TOO_LARGE`, and nothing is stored.
    #[test]
    fn blob_put_refuses_more_than_512_mib_before_reading() {
        let (dir, _root, ops) = new_store();
        let limit: u64 = 512 * 1024 * 1024;
        let huge = utf8(dir.path().join("huge.bin"));
        fs::File::create(&huge).unwrap().set_len(limit + 1).unwrap();
        let err = expect_err(
            ops.blob_put(
                &agent(),
                BlobPutReq {
                    content: ContentInput::File(huge.to_string()),
                },
            ),
            ErrorCode::TooLarge,
        );
        assert_eq!(
            (
                &err.details["size"],
                &err.details["limit"],
                &err.details["file"]
            ),
            (&json!(limit + 1), &json!(limit), &json!(huge.as_str()))
        );
        assert_eq!(err.details["scope"], "put", "one content's limit");
    }
}

/// `read_file` from an offset or by lines, and `read_tree` to a depth with sizes (operations
/// spec §1).
mod spans {
    use std::fmt::Write as _;

    use swarmops_api::{Envelope, LineRange, MAX_TREE_DEPTH, Meta, TreeListing};

    use super::*;

    /// What a listing lists of each entry: its `path`, name, kind and size.
    type Row = (Option<String>, String, EntryKindOut, Option<u64>);

    /// `read_file` of `path` at `rev`, from byte `offset`, at most `max_bytes`.
    fn at(rev: &str, path: &str, offset: u64, max_bytes: u64) -> ReadFileReq {
        ReadFileReq {
            offset,
            max_bytes,
            ..cat(rev, path)
        }
    }

    /// `read_file` of lines `first` to `last` of `path` at `rev`, at most `max_bytes`.
    fn lines(rev: &str, path: &str, (first, last): (u64, u64), max_bytes: u64) -> ReadFileReq {
        ReadFileReq {
            lines: Some(LineRange { first, last }),
            max_bytes,
            ..cat(rev, path)
        }
    }

    /// The length of `text` in bytes.
    fn len(text: &str) -> u64 {
        u64::try_from(text.len()).unwrap()
    }

    /// The tree of the listing tests: `a/` (with `b.rs` and `c/d.md`), `a.txt`, `bin/run` (an
    /// executable) and `link` (a symlink to `a.txt`). By bytes `a.txt` sorts before `a/b.rs`, but
    /// in pre-order `a` and everything below it come first, as `a` sorts before `a.txt`. The
    /// changeset's id.
    fn listing_tree(ops: &LocalOps) -> String {
        let inline = |text: &str| ContentInput::Inline(text.to_owned());
        commit(
            ops,
            None,
            vec![
                text("a.txt", "12345"),
                text("a/b.rs", "fn b() {}\n"),
                text("a/c/d.md", "# d\n"),
                put("bin/run", inline("#!/bin/sh\n"), Some(ModeInput::Exec)),
                put("link", inline("a.txt"), Some(ModeInput::Symlink)),
            ],
        )
    }

    /// `listing`'s entries, each as a [`Row`].
    fn rows(listing: &TreeListing) -> Vec<Row> {
        (listing.entries.iter())
            .map(|entry| {
                (
                    entry.path.clone(),
                    entry.name.clone(),
                    entry.kind,
                    entry.size,
                )
            })
            .collect()
    }

    fn row(path: Option<&str>, name: &str, kind: EntryKindOut, size: Option<u64>) -> Row {
        (path.map(str::to_owned), name.to_owned(), kind, size)
    }

    /// Each entry's path below the listed directory: its `path`, or at depth 1 its name.
    fn paths(listing: &TreeListing) -> Vec<String> {
        (listing.entries.iter())
            .map(|entry| entry.path.clone().unwrap_or_else(|| entry.name.clone()))
            .collect()
    }

    /// 64 KiB of text read 16 KiB at a time, each page from the one before's `next_offset`: the
    /// pages are the file, in order; each but the last is truncated, and each but the first says
    /// where it starts.
    #[test]
    fn read_file_at_an_offset_pages_through_a_file() {
        let (_dir, _root, ops) = new_store();
        let mut file = String::new();
        for n in 0..4096 {
            writeln!(file, "line {n:010}").unwrap();
        }
        assert_eq!(file.len(), 64 << 10);
        let c = commit(&ops, None, vec![text("big.txt", &file)]);
        let mut pages = Vec::new();
        let mut offset = 0;
        loop {
            let page = ops.read_file(at(&c, "big.txt", offset, 16 << 10)).unwrap();
            assert_eq!(
                (page.encoding, page.size, page.offset, page.lines),
                (
                    ContentEncoding::Utf8,
                    64 << 10,
                    (offset > 0).then_some(offset),
                    None
                ),
                "offset {offset}"
            );
            pages.push(page.content.clone());
            let Some(next) = page.next_offset else {
                assert!(!page.truncated, "offset {offset}");
                break;
            };
            assert!(page.truncated, "offset {offset}");
            assert_eq!(next, offset + len(&page.content), "offset {offset}");
            offset = next;
        }
        assert_eq!(pages.len(), 4);
        assert!(pages.concat() == file, "the pages are the file");
    }

    /// Text of 1-, 2-, 3- and 4-byte characters paged by `next_offset` with caps of 4 bytes and
    /// more, which fall inside characters: every page is text, at most 3 bytes short of its cap
    /// until the last, and the pages are the file.
    #[test]
    fn paging_by_next_offset_keeps_text_as_text() {
        let (_dir, _root, ops) = new_store();
        let file = "aé東😀".repeat(100);
        let c = commit(&ops, None, vec![text("chars.txt", &file)]);
        for max_bytes in [4, 5, 6, 7, 64, 333] {
            let mut pages = Vec::new();
            let mut offset = 0;
            loop {
                let page = ops
                    .read_file(at(&c, "chars.txt", offset, max_bytes))
                    .unwrap();
                let what = format!("cap {max_bytes} at {offset}");
                assert_eq!(page.encoding, ContentEncoding::Utf8, "{what}");
                let taken = len(&page.content);
                assert!(taken <= max_bytes, "{what}: {taken} bytes");
                pages.push(page.content);
                let Some(next) = page.next_offset else {
                    break;
                };
                assert!(taken + 3 >= max_bytes, "{what}: {taken} bytes");
                assert_eq!(next, offset + taken, "{what}");
                offset = next;
            }
            assert!(pages.concat() == file, "cap {max_bytes}");
        }
    }

    /// An offset inside a character starts with bytes that are not text: base64 of exactly the
    /// bytes from there, cut at exactly the cap. An offset at a character's start is text again.
    #[test]
    fn an_offset_inside_a_character_returns_base64() {
        let (_dir, _root, ops) = new_store();
        // The emoji is bytes 1 to 4.
        let file = "a😀b";
        let c = commit(&ops, None, vec![text("emoji.txt", file)]);
        for offset in [2_u64, 3, 4] {
            let read = (ops.read_file(at(&c, "emoji.txt", offset, DEFAULT_MAX_BYTES))).unwrap();
            assert_eq!(read.encoding, ContentEncoding::Base64, "offset {offset}");
            let from = usize::try_from(offset).unwrap();
            assert_eq!(
                STANDARD.decode(&read.content).unwrap(),
                &file.as_bytes()[from..]
            );
            assert_eq!(
                (read.offset, read.truncated, read.next_offset),
                (Some(offset), false, None)
            );
        }
        let read = |offset: u64, max_bytes: u64| {
            let read = ops
                .read_file(at(&c, "emoji.txt", offset, max_bytes))
                .unwrap();
            (read.encoding, read.content, read.next_offset)
        };
        let text = |content: &str| (ContentEncoding::Utf8, content.to_owned());
        let (encoding, content, next) = read(1, DEFAULT_MAX_BYTES);
        assert_eq!(((encoding, content), next), (text("😀b"), None));
        let (encoding, content, next) = read(5, DEFAULT_MAX_BYTES);
        assert_eq!(((encoding, content), next), (text("b"), None));
        // A cut inside the emoji backs the text off to it, where the next page starts.
        let (encoding, content, next) = read(0, 3);
        assert_eq!(((encoding, content), next), (text("a"), Some(1)));
        let (encoding, content, next) = read(2, 2);
        assert_eq!((encoding, next), (ContentEncoding::Base64, Some(4)));
        assert_eq!(STANDARD.decode(content).unwrap(), &file.as_bytes()[2..4]);
    }

    /// Lines 3 to 5 of ten, a range whose end is past the last line, a range past the end, a
    /// range cut by `max_bytes`, a file whose last line has no newline, an empty file, and a line
    /// of bytes that are not text: what exists of each range, where it starts, which lines it
    /// holds, and whether it was cut.
    #[test]
    fn read_file_lines_returns_the_lines_that_exist() {
        let (_dir, _root, ops) = new_store();
        let mut ten = String::new();
        for n in 1..=10 {
            writeln!(ten, "line {n}").unwrap();
        }
        let binary: &[u8] = b"\xff\n\x00\xfe\n\x01";
        let c = commit(
            &ops,
            None,
            vec![
                text("ten.txt", &ten),
                text("open.txt", "a\nbb\nccc"),
                put(
                    "bin.dat",
                    ContentInput::Base64(STANDARD.encode(binary)),
                    None,
                ),
                text("empty.txt", ""),
            ],
        );
        // A read of lines `range`: its content, offset, lines, truncated and next_offset.
        let read = |path: &str, range: (u64, u64), max_bytes: u64| {
            let read = ops.read_file(lines(&c, path, range, max_bytes)).unwrap();
            let shape = (read.offset, read.lines, read.truncated, read.next_offset);
            (read.content, shape)
        };
        let held = |first, last| Some(LineRange { first, last });
        let all = DEFAULT_MAX_BYTES;
        assert_eq!(
            read("ten.txt", (3, 5), all),
            (
                "line 3\nline 4\nline 5\n".to_owned(),
                (Some(14), held(3, 5), false, None)
            )
        );
        assert_eq!(
            read("ten.txt", (1, 1), all),
            ("line 1\n".to_owned(), (None, held(1, 1), false, None))
        );
        assert_eq!(
            read("ten.txt", (9, 12), all),
            (
                "line 9\nline 10\n".to_owned(),
                (Some(56), held(9, 10), false, None)
            )
        );
        assert_eq!(
            read("ten.txt", (11, 12), all),
            (String::new(), (Some(len(&ten)), None, false, None))
        );
        assert_eq!(
            read("ten.txt", (3, 5), 10),
            (
                "line 3\nlin".to_owned(),
                (Some(14), held(3, 5), true, Some(24))
            )
        );
        assert_eq!(
            read("open.txt", (2, 3), all),
            ("bb\nccc".to_owned(), (Some(2), held(2, 3), false, None))
        );
        assert_eq!(
            read("open.txt", (3, 3), all),
            ("ccc".to_owned(), (Some(5), held(3, 3), false, None))
        );
        assert_eq!(
            read("open.txt", (4, 9), all),
            (String::new(), (Some(8), None, false, None))
        );
        assert_eq!(
            read("empty.txt", (1, 1), all),
            (String::new(), (None, None, false, None))
        );
        let second = ops.read_file(lines(&c, "bin.dat", (2, 2), all)).unwrap();
        assert_eq!(
            (second.encoding, second.offset, second.lines),
            (ContentEncoding::Base64, Some(2), held(2, 2))
        );
        assert_eq!(STANDARD.decode(&second.content).unwrap(), b"\x00\xfe\n");
    }

    /// An offset at or past the end returns empty content, not truncated, saying where it
    /// starts, whatever the cap; an empty file is empty from its start. Short of the end, a cap
    /// of 0 cuts all the same: nothing returned, and more to read from there.
    #[test]
    fn an_offset_past_the_end_is_empty() {
        let (_dir, _root, ops) = new_store();
        let c = commit(
            &ops,
            None,
            vec![text("f.txt", "hello\n"), text("empty.txt", "")],
        );
        for offset in [6, 7, 1 << 40, u64::MAX] {
            for max_bytes in [0, 1, DEFAULT_MAX_BYTES] {
                let read = ops.read_file(at(&c, "f.txt", offset, max_bytes)).unwrap();
                let what = format!("offset {offset}, max_bytes {max_bytes}");
                assert_eq!(
                    (read.content.as_str(), read.encoding, read.truncated),
                    ("", ContentEncoding::Utf8, false),
                    "{what}"
                );
                assert_eq!(
                    (read.offset, read.next_offset, read.size),
                    (Some(offset), None, 6),
                    "{what}"
                );
            }
        }
        let empty = ops.read_file(cat(&c, "empty.txt")).unwrap();
        assert_eq!(
            (
                empty.content.as_str(),
                empty.truncated,
                empty.offset,
                empty.size
            ),
            ("", false, None, 0)
        );
        let none = ops.read_file(at(&c, "f.txt", 5, 0)).unwrap();
        assert_eq!(
            (none.content.as_str(), none.truncated, none.next_offset),
            ("", true, Some(5))
        );
    }

    /// `offset` and `lines` together, lines from 0 or a range that ends before it starts, a
    /// `depth` of 0 or past 64, and a cursor that is no repository path: `E_INVALID_ARG`, which
    /// names what broke the rule, before the revision is resolved.
    #[test]
    fn spans_depths_and_cursors_that_break_their_rules_are_invalid_args() {
        let (_dir, _root, ops) = new_store();
        let c = commit(&ops, None, vec![text("dir/f", "1\n2\n")]);
        let all = DEFAULT_MAX_BYTES;
        let both = ReadFileReq {
            offset: 1,
            ..lines(&c, "dir/f", (1, 1), all)
        };
        let err = expect_err(ops.read_file(both), ErrorCode::InvalidArg);
        assert_eq!(
            (&err.details["offset"], &err.details["lines"]),
            (&json!(1), &json!({"first": 1, "last": 1}))
        );
        for (first, last) in [(0, 1), (0, 0), (3, 2)] {
            let bad = lines(&c, "dir/f", (first, last), all);
            let err = expect_err(ops.read_file(bad), ErrorCode::InvalidArg);
            assert_eq!(err.details["lines"], json!({"first": first, "last": last}));
        }
        // An offset of 0 is the default, not a choice: lines go with it.
        let second = ops.read_file(lines(&c, "dir/f", (2, 2), all)).unwrap();
        assert_eq!(second.content, "2\n");

        for depth in [0, MAX_TREE_DEPTH + 1, u32::MAX] {
            let req = ReadTreeReq {
                depth,
                ..ls(&c, None)
            };
            let err = expect_err(ops.read_tree(req), ErrorCode::InvalidArg);
            assert_eq!(err.details["depth"], depth);
        }
        let deepest = ReadTreeReq {
            depth: MAX_TREE_DEPTH,
            ..ls(&c, None)
        };
        assert_eq!(paths(&ops.read_tree(deepest).unwrap()), ["dir", "dir/f"]);
        for cursor in ["", "/dir", "dir/", "dir//f", "..", "a/./b"] {
            let req = ReadTreeReq {
                cursor: Some(cursor.to_owned()),
                ..ls(&c, None)
            };
            let err = expect_err(ops.read_tree(req), ErrorCode::InvalidArg);
            assert_eq!(err.details["cursor"], cursor);
        }

        // Before the revision: one the store does not hold changes nothing.
        let unknown = "0".repeat(64);
        let req = ReadTreeReq {
            depth: 0,
            ..ls(&unknown, None)
        };
        expect_err(ops.read_tree(req), ErrorCode::InvalidArg);
        let req = ReadFileReq {
            offset: 1,
            ..lines(&unknown, "dir/f", (1, 1), all)
        };
        expect_err(ops.read_file(req), ErrorCode::InvalidArg);
    }

    /// `depth` lists the directories below the listed one too, in pre-order (a directory, then
    /// its entries, each directory's by name bytes), each entry with its `path` below the listed
    /// directory and each leaf with its size; depth 1 lists the directory's own entries as
    /// before, with sizes and without `path`.
    #[test]
    fn read_tree_depth_lists_in_pre_order_with_sizes() {
        use EntryKindOut::{Dir, Exec, File, Symlink};

        let (_dir, _root, ops) = new_store();
        let c = listing_tree(&ops);
        let listed = |path: Option<&str>, depth: u32| {
            let req = ReadTreeReq {
                depth,
                ..ls(&c, path)
            };
            rows(&ops.read_tree(req).unwrap())
        };
        let deep = [
            row(Some("a"), "a", Dir, None),
            row(Some("a/b.rs"), "b.rs", File, Some(10)),
            row(Some("a/c"), "c", Dir, None),
            row(Some("a/c/d.md"), "d.md", File, Some(4)),
            row(Some("a.txt"), "a.txt", File, Some(5)),
            row(Some("bin"), "bin", Dir, None),
            row(Some("bin/run"), "run", Exec, Some(10)),
            row(Some("link"), "link", Symlink, Some(5)),
        ];
        assert_eq!(listed(None, 3), deep);
        assert_eq!(
            listed(None, MAX_TREE_DEPTH),
            deep,
            "no deeper than the tree"
        );
        let two: Vec<Row> = (deep.iter())
            .filter(|row| row.0.as_deref() != Some("a/c/d.md"))
            .cloned()
            .collect();
        assert_eq!(listed(None, 2), two);
        assert_eq!(
            listed(None, 1),
            [
                row(None, "a", Dir, None),
                row(None, "a.txt", File, Some(5)),
                row(None, "bin", Dir, None),
                row(None, "link", Symlink, Some(5)),
            ]
        );
        assert_eq!(
            listed(Some("a"), 2),
            [
                row(Some("b.rs"), "b.rs", File, Some(10)),
                row(Some("c"), "c", Dir, None),
                row(Some("c/d.md"), "d.md", File, Some(4)),
            ]
        );
        // Each id is the entry's blob or tree, as depth 1 gives it.
        let req = ReadTreeReq {
            depth: 2,
            ..ls(&c, Some("a"))
        };
        let page = ops.read_tree(req).unwrap();
        let flat = ops.read_tree(ls(&c, Some("a"))).unwrap();
        assert_eq!(page.entries[0].id, blob(b"fn b() {}\n"));
        assert_eq!(page.entries[1].id, flat.entries[1].id);
        assert_eq!((page.truncated, page.cursor), (false, None));
    }

    /// The tree above at depth 3, paged 1, 2 and 3 entries at a time from each page's cursor:
    /// the pages are the listing, each cursor its page's last path. A cursor resumes after the
    /// path it names in pre-order, whether or not an entry is there: below a directory it names;
    /// at the next entry past a name that is not there; after a file, whatever it names below
    /// it; and past the depth listed, at the next entry the listing holds.
    #[test]
    fn read_tree_depth_pages_resume_after_the_cursor() {
        let (_dir, _root, ops) = new_store();
        let c = listing_tree(&ops);
        let page = |depth: u32, limit: usize, cursor: Option<&str>| {
            let req = ReadTreeReq {
                depth,
                limit,
                cursor: cursor.map(str::to_owned),
                ..ls(&c, None)
            };
            ops.read_tree(req).unwrap()
        };
        let all = paths(&page(3, DEFAULT_READ_TREE_LIMIT, None));
        assert_eq!(
            all,
            [
                "a", "a/b.rs", "a/c", "a/c/d.md", "a.txt", "bin", "bin/run", "link"
            ]
        );
        for limit in [1, 2, 3] {
            let mut seen = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let got = page(3, limit, cursor.as_deref());
                let names = paths(&got);
                assert!(names.len() <= limit, "limit {limit}");
                assert_eq!(
                    got.cursor.as_ref(),
                    got.truncated.then(|| names.last().unwrap()),
                    "limit {limit}"
                );
                seen.extend(names);
                if !got.truncated {
                    break;
                }
                cursor = got.cursor;
            }
            assert_eq!(seen, all, "limit {limit}");
        }
        let after =
            |depth: u32, cursor: &str| paths(&page(depth, DEFAULT_READ_TREE_LIMIT, Some(cursor)));
        assert_eq!(after(3, "a"), all[1..]);
        assert_eq!(after(3, "a/bb"), all[2..]);
        assert_eq!(after(3, "a/zz"), all[4..]);
        assert_eq!(after(3, "a.txt/x"), all[5..]);
        assert_eq!(after(3, "zzz"), Vec::<String>::new());
        assert_eq!(after(1, "a/c/d.md"), ["a.txt", "bin", "link"]);
        assert_eq!(after(1, "a.txt"), ["bin", "link"]);
        let last = page(3, DEFAULT_READ_TREE_LIMIT, Some("link"));
        assert_eq!(
            (last.entries.len(), last.truncated, last.cursor),
            (0, false, None)
        );
    }

    /// The default `read_tree` page as the CLI prints it, in its envelope: 2,000 entries of
    /// 30-byte names, files of 12,345 bytes, listed at depth 1 (a directory of the files) and at
    /// depth 3 (two directories down, where each file's `path` is three names long). At both
    /// depths it stays within 64 KiB, and the default is the most entries, in fifties, that do:
    /// 50 more pass 64 KiB at depth 3.
    #[test]
    fn a_default_ls_page_stays_under_64_kib() {
        let (_dir, root, ops) = new_store();
        let name = |n: usize| format!("{n:030}");
        let body = "x".repeat(12_345);
        let mut changes = Vec::new();
        for n in 0..2000 {
            changes.push(text(&format!("wide/{}", name(n)), &body));
            let deep = format!("deep/{}/{}/{}", name(0), name(1), name(n));
            changes.push(text(&deep, &body));
        }
        let c = commit(&ops, None, changes);
        let printed = |dir: &str, depth: u32, limit: usize| {
            let req = ReadTreeReq {
                depth,
                limit,
                ..ls(&c, Some(dir))
            };
            let listing = ops.read_tree(req).unwrap();
            assert!(listing.truncated, "{dir}: a page of {limit}");
            let meta = Meta {
                repo: Some(root.to_string()),
                elapsed_ms: 12_345,
                resolved: BTreeMap::from([("main".to_owned(), c.clone())]),
                auth: "trust".to_owned(),
                recovered: None,
            };
            // The CLI prints the envelope compact, then a newline.
            serde_json::to_string(&Envelope::ok("read_tree", listing, meta))
                .unwrap()
                .len()
                + 1
        };
        let flat = printed("wide", 1, DEFAULT_READ_TREE_LIMIT);
        let deep = printed("deep", 3, DEFAULT_READ_TREE_LIMIT);
        assert!(
            flat <= 65_536 && deep <= 65_536,
            "{DEFAULT_READ_TREE_LIMIT} entries: {flat} bytes at depth 1, {deep} at depth 3"
        );
        let more = DEFAULT_READ_TREE_LIMIT + 50;
        let past = printed("deep", 3, more);
        assert!(
            past > 65_536,
            "{more} entries at depth 3 print in {past} bytes: the default could be larger"
        );
    }
}

/// `derive` (operations spec §1): the eager derived values of the changesets the journal records
/// after a frame, how many it derived and found stored, and where to go on.
mod derive {
    use swarmops_api::{DEFAULT_DERIVE_LIMIT, DeriveReq, DeriveResp};

    use super::*;

    fn req(after: Option<u64>, limit: usize) -> DeriveReq {
        DeriveReq { after, limit }
    }

    fn resp(derived: u64, cached: u64, next: Option<u64>) -> DeriveResp {
        DeriveResp {
            derived,
            cached,
            next,
        }
    }

    /// Over three commits (frames 0 to 2), the first call derives their skip lists and says to
    /// go on after frame 2; from there nothing is left and `next` is omitted; from the first
    /// frame the three are found stored. `limit` bounds the frames a call reads, and a `limit`
    /// of 0 reads none and gives the journal's last frame. It needs no identity and resolves no
    /// revision. A request of `{}` reads 1,000 frames from the first, and a response without a
    /// `next` omits it.
    #[test]
    fn derive_reports_what_it_derived() {
        let (_dir, root, ops) = new_store();
        assert_eq!(
            ops.derive(req(None, DEFAULT_DERIVE_LIMIT)).unwrap(),
            resp(0, 0, None)
        );
        let c1 = commit(&ops, None, vec![text("a.txt", "1")]);
        let c2 = commit(&ops, Some(&c1), vec![text("a.txt", "2")]);
        let c3 = commit(&ops, Some(&c2), vec![text("a.txt", "3")]);

        let first = ops.derive(req(None, DEFAULT_DERIVE_LIMIT)).unwrap();
        assert_eq!(first, resp(3, 0, Some(2)));
        for id in [&c1, &c2, &c3] {
            let list = format!(
                "meta/derived/first_parent_skip_list/1/{}/{}",
                &id[..2],
                &id[2..]
            );
            assert!(root.join(&list).is_file(), "{list}");
        }
        assert_eq!(
            ops.derive(req(Some(2), DEFAULT_DERIVE_LIMIT)).unwrap(),
            resp(0, 0, None)
        );
        assert_eq!(
            ops.derive(req(None, DEFAULT_DERIVE_LIMIT)).unwrap(),
            resp(0, 3, Some(2))
        );
        assert_eq!(ops.derive(req(Some(0), 1)).unwrap(), resp(0, 1, Some(1)));
        assert_eq!(ops.derive(req(None, 0)).unwrap(), resp(0, 0, Some(2)));

        let resolved = Resolved::default();
        let recorded = ops.recording(&resolved).derive(req(None, 1)).unwrap();
        assert_eq!(recorded, resp(0, 1, Some(0)));
        assert_eq!(resolved.take(), BTreeMap::new(), "no revision resolved");

        let wire = serde_json::to_value(resp(0, 0, None)).unwrap();
        assert_eq!(wire, json!({"derived": 0, "cached": 0}));
        let wire = serde_json::to_value(first).unwrap();
        assert_eq!(wire, json!({"derived": 3, "cached": 0, "next": 2}));
        let defaults: DeriveReq = serde_json::from_value(json!({})).unwrap();
        assert_eq!(defaults, req(None, 1000));
    }
}
