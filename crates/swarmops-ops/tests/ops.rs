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
    RefDeleteReq, RefListReq, RefOut, RefSetReq, RefSetResp, Rev, ShowReq, StoreOps, TreeEntryOut,
    VerifyReq,
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

/// [`new_store`] of FORMAT 1: for a test of the names and revisions FORMAT 1 alone refuses.
fn v1_store() -> (TempDir, Utf8PathBuf, LocalOps) {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = utf8(dir.path().join("store"));
    let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).expect("create the store");
    (dir, root, ops)
}

fn agent() -> Identity {
    Identity::parse("agent/worker-17").expect("a valid agent name")
}

fn put(path: &str, content: ContentInput, mode: Option<ModeInput>) -> ChangeInput {
    let mut change_input = ChangeInput::new(path.to_owned(), ChangeOp::Put);
    change_input.content = Some(content);
    change_input.mode = mode;
    change_input
}

fn text(path: &str, content: &str) -> ChangeInput {
    put(path, ContentInput::Inline(content.to_owned()), None)
}

fn delete(path: &str) -> ChangeInput {
    ChangeInput::new(path.to_owned(), ChangeOp::Delete)
}

fn commit_req(parent: Option<&str>, changes: Vec<ChangeInput>) -> CommitReq {
    let mut commit_req = CommitReq::new(changes, "a test commit".to_owned());
    commit_req.parent = parent.map(Rev::from);
    commit_req
}

/// Commits `changes` on `parent` as [`agent`]; the new changeset's id.
#[track_caller]
fn commit(ops: &LocalOps, parent: Option<&str>, changes: Vec<ChangeInput>) -> String {
    ops.commit(&agent(), commit_req(parent, changes))
        .expect("commit")
        .changeset
}

fn cat(rev: &str, path: &str) -> ReadFileReq {
    let mut read_file_req = ReadFileReq::new(rev, path.to_owned());
    read_file_req.max_bytes = DEFAULT_MAX_BYTES;
    read_file_req
}

fn ls(rev: &str, path: Option<&str>) -> ReadTreeReq {
    let mut read_tree_req = ReadTreeReq::new(rev);
    read_tree_req.path = path.map(str::to_owned);
    read_tree_req.depth = 1;
    read_tree_req.limit = DEFAULT_READ_TREE_LIMIT;
    read_tree_req
}

fn show(rev: &str) -> ShowReq {
    let mut show_req = ShowReq::new(rev);
    show_req.max_changes = DEFAULT_MAX_CHANGES;
    show_req
}

fn log_req(rev: Option<&str>) -> LogReq {
    let mut log_req = LogReq::default();
    log_req.rev = rev.map(Rev::from);
    log_req.limit = DEFAULT_LOG_LIMIT;
    log_req.cursor = None;
    log_req.paths = Vec::new();
    log_req
}

fn set(name: &str, new: &str, expected_old: Option<&str>, create: bool) -> RefSetReq {
    let mut ref_set_req = RefSetReq::new(name.to_owned(), new);
    ref_set_req.expected_old = expected_old.map(Rev::from);
    ref_set_req.create = create;
    ref_set_req
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
        ChangesetId::new(ObjectId::new(id))
    });
    let meta = MetaStore::open(root).unwrap();
    let mut write = meta.write().unwrap();
    for (n, id) in (0u8..).zip(twins) {
        write
            .insert_changeset(ChangesetMeta::new(id, 0, 0, [n; 32], Vec::new()))
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
        (None, 2),
        "{info:?}"
    );
    assert_eq!((info.changesets, info.refs), (0, 0), "{info:?}");
    assert_eq!(ops.repo_info(InfoReq::default()).unwrap(), info);
    assert_eq!(ops.store_path(), root);

    // The counts and `main` follow what the store holds.
    let c1 = commit(&ops, None, vec![text("README.md", "# acme\n")]);
    ops.ref_set(&agent(), set("main", &c1, None, true)).unwrap();
    let now = ops.repo_info(InfoReq::default()).unwrap();
    let mut repo_info = info.clone();
    repo_info.main = Some(c1.clone());
    repo_info.changesets = 1;
    repo_info.refs = 1;
    assert_eq!(now, repo_info);

    // Opened again, the store reports the same.
    drop(ops);
    let reopened = LocalOps::open(&root).unwrap();
    assert_eq!(reopened.repo_info(InfoReq::default()).unwrap(), now);
    assert_eq!(reopened.store_path(), root);

    // A second init of the store is refused, and the hint opens the one there.
    let err = expect_err(LocalOps::init(&root, "acme"), ErrorCode::ExistsAlready);
    let repo = shell_word(root.as_str());
    assert_eq!(err.hint, Some(format!("swarmops --repo {repo} info")));
    assert_eq!(
        reopened.repo_info(InfoReq::default()).unwrap(),
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
            CommitReq::new(
                vec![
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
                "add the readme\n\nand a tool".to_owned(),
            ),
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
        FileContent::new(
            blob(b"# acme\n"),
            7,
            FileKind::File,
            ContentEncoding::Utf8,
            "# acme\n".to_owned(),
            false
        )
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
    let mut tool_entry = TreeEntryOut::new("tool".to_owned(), EntryKindOut::Exec, blob(tool));
    tool_entry.size = Some(7);
    assert_eq!(
        ops.read_tree(ls(c1, Some("bin"))).unwrap().entries,
        [tool_entry]
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
    let mut attribution = Attribution::new("agent/worker-17".to_owned());
    attribution.run_id = Some("run-3".to_owned());
    assert_eq!(detail.author, attribution);
    assert_eq!(detail.message, "add the readme\n\nand a tool");
    let change = |path: &str, kind: FileKind, content: &[u8]| {
        let mut change_out = ChangeOut::new(path.to_owned(), ChangeOp::Put);
        change_out.kind = Some(kind);
        change_out.blob = Some(blob(content));
        change_out
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
        ("agent/worker-17", "worker-17@agent.swarmops.local")
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
        [ChangesetSummary::new(
            c1.to_owned(),
            Vec::new(),
            "agent/worker-17".to_owned(),
            detail.created_at,
            "add the readme".to_owned()
        )]
    );
    assert_eq!((page.truncated, page.cursor), (false, None));
}

/// Once `main` names `c1`, a commit that names no parent is made on it.
fn commit_on_main_by_default(ops: &LocalOps, who: &Identity, c1: &str) {
    ops.ref_set(who, set("main", c1, None, true)).unwrap();
    let second = ops
        .commit(
            who,
            CommitReq::new(
                vec![text("README.md", "# acme, v2\n"), delete("docs/latest")],
                "update the readme".to_owned(),
            ),
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
    let mut readme = ChangeOut::new("README.md".to_owned(), ChangeOp::Put);
    readme.kind = Some(FileKind::File);
    readme.blob = Some(blob(b"# acme, v2\n"));
    assert_eq!(
        detail.changes,
        [
            readme,
            ChangeOut::new("docs/latest".to_owned(), ChangeOp::Delete),
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
        IdentityInfo::new("anonymous".to_owned())
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
    let mut expected = IdentityInfo::new("trust".to_owned());
    expected.agent = Some("agent/worker-1".to_owned());
    expected.run_id = Some("run-9".to_owned());
    assert_eq!(identity_info(Some(&flagged)), expected);

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
        "worker-1@agent.swarmops.local"
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
        let mut read_file_req = cat(&c, "big.bin");
        read_file_req.max_bytes = max_bytes;
        ops.read_file(read_file_req).unwrap()
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
        let mut read_file_req = cat(&c, path);
        read_file_req.max_bytes = max_bytes;
        ops.read_file(read_file_req).unwrap()
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
        let mut read_file_req = cat(&c, "noise.bin");
        read_file_req.max_bytes = max_bytes;
        let read = ops.read_file(read_file_req).unwrap();
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
        ops.repo_info(InfoReq::default()).unwrap().changesets,
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
        ops.repo_info(InfoReq::default()).unwrap().changesets,
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
    let resp = |old: Option<&str>, new: Option<&str>, already_applied: bool| {
        let mut ref_set_resp = RefSetResp::new("main".to_owned(), already_applied);
        ref_set_resp.old = old.map(str::to_owned);
        ref_set_resp.new = new.map(str::to_owned);
        ref_set_resp
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
        ops.ref_list(RefListReq::default()).unwrap().refs,
        [main_ref(c3.clone(), 3)]
    );

    // Exactly one of `expected_old` and `create`.
    for req in [
        set("main", &c1, Some(&c3), true),
        set("main", &c1, None, false),
    ] {
        expect_err(ops.ref_set(&who, req), ErrorCode::InvalidArg);
    }
    // Ref names are `main` or `release/<name>` in a FORMAT 1 store.
    let err = expect_err(
        ops.ref_set(&who, set("main/x", &c1, None, true)),
        ErrorCode::RefNameInvalid,
    );
    assert_eq!(err.details["name"], "main/x");
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
        ops.ref_delete(who, RefDeleteReq::new(name.to_owned(), expected))
    };
    let deleted = |name: &str, old: &str, already_applied: bool| {
        let mut ref_set_resp = RefSetResp::new(name.to_owned(), already_applied);
        ref_set_resp.old = Some(old.to_owned());
        ref_set_resp
    };
    let err = expect_err(delete("main", c1), ErrorCode::CasMismatch);
    assert_eq!(err.details["actual"], json!(c3));
    assert_eq!(
        err.message,
        format!("ref main now points at {c3}; re-check before deleting")
    );
    assert_eq!(err.hint, Some(format!("swarmops show {c3}")));
    assert_eq!(delete("main", c3).unwrap(), deleted("main", c3, false));
    let refs = ops.ref_list(RefListReq::default()).unwrap().refs;
    assert!(refs.is_empty(), "{refs:?}");
    // Deleting it again finds it gone, as asked: a replay, already applied, which answers as
    // the delete did. A ref that never existed is gone just the same.
    assert_eq!(delete("main", c3).unwrap(), deleted("main", c3, true));
    assert_eq!(
        delete("release/never", c1).unwrap(),
        deleted("release/never", c1, true)
    );
    // A name that cannot be a ref's is refused, not taken for a ref that is gone.
    let err = expect_err(delete("main/x", c1), ErrorCode::RefNameInvalid);
    assert_eq!(err.details["name"], "main/x");
    assert!(
        ops.ref_list(RefListReq::default()).unwrap().refs.is_empty(),
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
    let delete = |name: &str, expected: &str| RefDeleteReq::new(name.to_owned(), expected);
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
        ops.repo_info(InfoReq::default()).unwrap().main.as_deref(),
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
        let mut req = commit_req(parent, changes);
        req.advance = Some(name.to_owned());
        req
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
        let mut moved = Advanced::new("main".to_owned(), c2.clone(), false);
        moved.old = Some(c1.clone());
        assert_eq!(resp.advanced, Some(moved));
        assert_eq!(resp.warnings, Vec::<String>::new());
        let refs = ops.ref_list(RefListReq::default()).unwrap().refs;
        assert_eq!(refs, [main_ref(c2, 2)]);
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
        let main = ops.repo_info(InfoReq::default()).unwrap().main;
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
        let before = ops.repo_info(InfoReq::default()).unwrap();

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
            ops.repo_info(InfoReq::default()).unwrap(),
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
        let refs = ops.ref_list(RefListReq::default()).unwrap().refs;
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
        let bad = request.commit(&who, advancing(None, vec![text("x", "1")], "main/x"));
        let err = expect_err(bad, ErrorCode::RefNameInvalid);
        assert_eq!(err.details["name"], "main/x");
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 4);
    }

    /// The same advancing request made again in the second it signs in (a commit signs in whole
    /// seconds) is the same changeset, its move made already: a replay that says so in
    /// `advanced.already_applied`, and writes no frame.
    #[test]
    fn an_advancing_request_made_again_is_a_replay() {
        let who = agent();
        // Only two requests in the same second of the clock make the same changeset: a pair that
        // straddles a second runs again, on a fresh store.
        for _ in 0..10 {
            let (_dir, root, ops) = new_store();
            let c1 = commit(&ops, None, vec![text("f", "1")]);
            ops.ref_set(&who, set("main", &c1, None, true)).unwrap();
            let req = advancing(Some(&c1), vec![text("f", "2")], "main");
            let second = unix_secs();
            let first = ops.commit(&who, req.clone()).unwrap();
            let written = frames(&root);
            let again = ops.commit(&who, req);
            if unix_secs() != second {
                continue;
            }
            let again = again.unwrap();
            assert_eq!(again.changeset, first.changeset);
            let mut replay = Advanced::new("main".to_owned(), first.changeset, true);
            replay.old = Some(c1);
            assert_eq!(again.advanced, Some(replay));
            assert_eq!(again.warnings, Vec::<String>::new());
            assert_eq!(frames(&root), written, "no new frame");
            return;
        }
        panic!("ten pairs of requests each straddled a second of the clock");
    }

    /// An advancing commit's move is recorded for the agent that asked, as a `ref_set`'s is: the
    /// ref's reflog names that agent, beside the move another agent made before it.
    #[test]
    fn an_advancing_commit_s_move_names_its_agent() {
        let (_dir, root, ops) = new_store();
        let other = Identity::parse("agent/worker-3").unwrap();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        ops.ref_set(&other, set("main", &c1, None, true)).unwrap();
        let c2 = ops
            .commit(&agent(), advancing(Some(&c1), vec![text("f", "2")], "main"))
            .unwrap()
            .changeset;
        let meta = MetaStore::open(&root).unwrap();
        let moves = meta.read().unwrap().reflog("main", 10).unwrap();
        let hex = |id: Option<ChangesetId>| id.map(|id| id.to_string());
        let moves: Vec<(Option<String>, Option<String>, &str)> = moves
            .iter()
            .map(|entry| (hex(entry.old), hex(entry.new), entry.actor.as_str()))
            .collect();
        assert_eq!(
            moves,
            [
                (Some(c1.clone()), Some(c2), "agent/worker-17"),
                (None, Some(c1), "agent/worker-3"),
            ]
        );
    }
}

/// `commit` with an `idempotency_key`, `commit --key` (operations spec §4): a uuid v7 whose time
/// signs the commit and which the changeset records as its `commit_key` metadata, so that a
/// retry replays the first attempt instead of recording a second changeset.
mod commit_key {
    use swarmops_api::{Advanced, Signatures};

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
        let mut req = commit_req(parent, changes);
        req.idempotency_key = Some(key.to_owned());
        req
    }

    /// [`keyed`], also advancing `main`.
    fn keyed_advancing(parent: Option<&str>, changes: Vec<ChangeInput>, key: &str) -> CommitReq {
        let mut commit_req = keyed(parent, changes, key);
        commit_req.advance = Some("main".to_owned());
        commit_req
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
        let before = ops.repo_info(InfoReq::default()).unwrap().changesets;
        let key = key_at(KEY_MS, 1);
        let changes = || vec![text("f", "2"), text("g", "3")];

        // On main's head by default, which a commit without advance leaves where it is.
        let first = ops.commit(&who, keyed(None, changes(), &key)).unwrap();
        assert!(!first.replayed, "{first:?}");
        assert_eq!(first.blobs_written, 2);
        let records = journal_records(&root);
        let again = ops.commit(&who, keyed(None, changes(), &key)).unwrap();
        let mut replay = first.clone();
        replay.blobs_written = 0;
        replay.replayed = true;
        assert_eq!(again, replay);
        assert_eq!(journal_records(&root), records, "no frame");
        let after = ops.repo_info(InfoReq::default()).unwrap().changesets;
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
        let Signatures {
            author, committer, ..
        } = detail.signatures;
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
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 1);
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
        let mut moved = Advanced::new("main".to_owned(), c2.clone(), false);
        moved.old = Some(c1.clone());
        assert_eq!(first.advanced, Some(moved.clone()));
        let (info, records) = (
            ops.repo_info(InfoReq::default()).unwrap(),
            journal_records(&root),
        );

        let resolved = Resolved::default();
        let again = ops
            .recording(&resolved)
            .commit(&who, request(None))
            .unwrap();
        let mut replay = first;
        replay.blobs_written = 0;
        let mut advanced = moved;
        advanced.already_applied = true;
        replay.advanced = Some(advanced);
        replay.replayed = true;
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
            ops.repo_info(InfoReq::default()).unwrap(),
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
        let mut replay = first;
        replay.blobs_written = 0;
        replay.replayed = true;
        for parent in [Some(c2.as_str()), None] {
            let again = ops
                .commit(&who, keyed(parent, vec![text("f", "2")], &key))
                .unwrap();
            assert_eq!(again, replay, "parent {parent:?}");
        }
        assert_eq!(journal_records(&root), records, "no frame");
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 2);
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
        let mut req = commit_req(Some(&c2), vec![text("g", "3")]);
        req.advance = Some("main".to_owned());
        let c3 = ops.commit(&who, req).unwrap().changeset;
        let before = ops.repo_info(InfoReq::default()).unwrap();

        let request = keyed_advancing(Some(&c2), vec![text("f", "2")], &key);
        let err = expect_err(ops.commit(&who, request), ErrorCode::CasMismatch);
        assert_eq!(
            Value::Object(err.details.clone()),
            json!({"name": "main", "actual": c3, "committed": false})
        );
        assert_eq!(
            ops.repo_info(InfoReq::default()).unwrap(),
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
        let mut moved_again = first;
        moved_again.blobs_written = 0;
        assert_eq!(again, moved_again);
        let info = ops.repo_info(InfoReq::default()).unwrap();
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
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 4);
    }
}

/// `land` (operations spec §3, the store-only block): a change committed on an older head,
/// merged three-way onto main and published there as one new changeset in one call; its
/// conflicts as `E_CONFLICT`, with the report in `details.report`; a keyed land run again a
/// replay; and `show` naming a land's lander, key and chain.
mod land {
    use swarmops_api::{
        ConflictKindOut, ConflictReport, DEFAULT_MAX_CONFLICT_BYTES, FileConflictOut, HunkOut,
        LandReq, LandResp, LandStatus, LineRange, LinesOut, OpLogReq, OperationKind,
    };

    use super::*;

    /// The agent whose commits main gains while a change waits to land.
    const OTHER: &str = "agent/worker-9";

    fn other() -> Identity {
        Identity::parse(OTHER).expect("a valid agent name")
    }

    /// A land of `head` onto main with the request's defaults, read as a client's arguments are.
    fn landing(head: &str) -> LandReq {
        serde_json::from_value(json!({"head": head})).expect("a land request")
    }

    /// Commits `changes` on `parent` as `who`, moving main to the changeset in the same
    /// transaction (creating main for a root commit); the changeset's id.
    #[track_caller]
    fn publish(
        ops: &LocalOps,
        who: &Identity,
        parent: Option<&str>,
        changes: Vec<ChangeInput>,
    ) -> String {
        let mut req = commit_req(parent, changes);
        req.advance = Some("main".to_owned());
        ops.commit(who, req).expect("publish").changeset
    }

    /// What main names.
    fn main_of(ops: &LocalOps) -> String {
        let refs = ops.ref_list(RefListReq::default()).unwrap().refs;
        refs.into_iter().find(|r| r.name == "main").unwrap().target
    }

    /// The newest operation's number: a request that writes nothing leaves it as it was.
    fn last_op(ops: &LocalOps) -> u64 {
        let mut req = OpLogReq::default();
        req.before = None;
        req.limit = 1;
        req.max_changesets = 1;
        ops.op_log(req).unwrap().ops[0].op
    }

    /// The report of the `E_CONFLICT` that `result` must be.
    #[track_caller]
    fn report_of(result: Result<LandResp, OpError>) -> (ConflictReport, OpError) {
        let error = expect_err(result, ErrorCode::Conflict);
        let report = serde_json::from_value(error.details["report"].clone())
            .unwrap_or_else(|e| panic!("details.report is a ConflictReport ({e}): {error:?}"));
        (report, error)
    }

    /// Lines `first` to `last` with their text.
    fn lines(first: u64, last: u64, text: &str) -> LinesOut {
        let mut lines_out = LinesOut::new(first, last);
        lines_out.text = Some(text.to_owned());
        lines_out
    }

    /// A land of a commit made on main's head is one call: one new changeset on main, whose
    /// parent is main's head and whose change is the commit's, in one transaction, which the
    /// operation log calls a land. The response says where it landed and what it squashed, and
    /// the request's record holds what `head` and the target named.
    #[test]
    fn land_publishes_a_commit_in_one_call() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let t0 = publish(&ops, &who, None, vec![text("a", "a\n")]);
        let head = commit(&ops, Some(&t0), vec![text("b", "b\n")]);
        let before = last_op(&ops);

        let resolved = Resolved::default();
        let resp = ops.recording(&resolved).land(&who, landing(&head)).unwrap();
        let landed = resp.landed.clone();
        assert_eq!(
            resp,
            LandResp::new(
                LandStatus::Landed,
                landed.clone(),
                t0.clone(),
                "main".to_owned(),
                t0.clone(),
                1,
                false,
                1,
                vec![head.clone()]
            )
        );
        assert_eq!(landed.len(), 64, "a full id: {landed}");
        assert_eq!(
            resolved.take(),
            BTreeMap::from([
                (head.clone(), head.clone()),
                ("main".to_owned(), t0.clone())
            ])
        );
        assert_eq!(main_of(&ops), landed);
        assert_eq!(last_op(&ops), before + 1, "one transaction");
        let mut req = OpLogReq::default();
        req.before = None;
        req.limit = 1;
        req.max_changesets = 1;
        assert_eq!(ops.op_log(req).unwrap().ops[0].kind, OperationKind::Land);
        assert_eq!(ops.show(show(&landed)).unwrap().parents, [t0]);
        for (path, content) in [("a", "a\n"), ("b", "b\n")] {
            assert_eq!(ops.read_file(cat("main", path)).unwrap().content, content);
        }
    }

    /// A land whose change conflicts with what main gained since its base fails `E_CONFLICT`
    /// (exit 3, not retryable) and records nothing. `details.report` names the base, the change
    /// (`ours`) and main's head (`theirs`) by full id; each conflicting file, in path order, with
    /// its kind, the blob each side holds there (none for a side without the file), the hunks of
    /// a text both sides edited (each side's lines from 1, both ends included, and their texts),
    /// and the newest changeset on main since the base that changed it, with its agent; the
    /// kinds; and the paths both sides changed that merged cleanly. The hint reads the target's
    /// side, then names the land with its resolutions.
    #[test]
    fn a_conflicting_land_is_e_conflict_with_the_report() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let h0 = "h1\nh2\nh3\nh4\nh5\n";
        let files = vec![text("f", "1\n2\n3\n"), text("g", "g\n"), text("h", h0)];
        let t0 = publish(&ops, &who, None, files);
        let ours = vec![
            text("f", "1\nours\n3\n"),
            delete("g"),
            text("h", "H1\nh2\nh3\nh4\nh5\n"),
        ];
        let head = commit(&ops, Some(&t0), ours);
        let changed = vec![
            text("f", "1\ntheirs\n3\n"),
            text("g", "g2\n"),
            text("h", "h1\nh2\nh3\nh4\nH5\n"),
        ];
        let theirs = publish(&ops, &other(), Some(&t0), changed);
        let before = last_op(&ops);

        let (report, error) = report_of(ops.land(&who, landing(&head)));
        let blob_of = |content: &str| Some(blob(content.as_bytes()));
        let by_them = |path: &str, kind: ConflictKindOut, hunks: Vec<HunkOut>| {
            let mut file_conflict_out = FileConflictOut::new(path.to_owned(), kind);
            file_conflict_out.hunks = hunks;
            file_conflict_out.theirs_changeset = Some(theirs.clone());
            file_conflict_out.theirs_agent = Some(OTHER.to_owned());
            file_conflict_out
        };
        let hunk = HunkOut::new(
            0,
            lines(2, 2, "2\n"),
            lines(2, 2, "ours\n"),
            lines(2, 2, "theirs\n"),
        );
        let mut f = by_them("f", ConflictKindOut::Content, vec![hunk]);
        f.base_blob = blob_of("1\n2\n3\n");
        f.ours_blob = blob_of("1\nours\n3\n");
        f.theirs_blob = blob_of("1\ntheirs\n3\n");
        let mut g = by_them("g", ConflictKindOut::DeleteModify, Vec::new());
        g.base_blob = blob_of("g\n");
        g.theirs_blob = blob_of("g2\n");
        let mut expected = ConflictReport::new(
            t0.clone(),
            head.clone(),
            theirs.clone(),
            vec![f, g],
            vec![ConflictKindOut::Content, ConflictKindOut::DeleteModify],
            false,
        );
        expected.auto_merged = vec!["h".to_owned()];
        assert_eq!(report, expected);
        assert_eq!(error.details.len(), 1, "the report alone: {error:?}");
        assert_eq!(
            error.hint.as_deref(),
            Some(
                format!(
                    "swarmops cat {theirs}:f, then swarmops land {head} --resolve <file> with \
                     each path's resolution and against = {theirs}"
                )
                .as_str()
            )
        );
        assert!(error.message.contains(&theirs), "{error:?}");
        assert_eq!(main_of(&ops), theirs, "nothing recorded");
        assert_eq!(last_op(&ops), before);
    }

    /// The report holds hunk texts while their sum stays within `max_conflict_bytes`, a hunk's
    /// three sides together and in the report's order; past it, it leaves the texts out and
    /// says `truncated`, keeping every line position and blob id.
    #[test]
    fn a_report_past_its_budget_drops_texts_and_says_truncated() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let base = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
        let t0 = publish(&ops, &who, None, vec![text("f", base)]);
        let ours = "one\nTWO OURS\nthree\nfour\nfive\nSIX OURS\nseven\n";
        let head = commit(&ops, Some(&t0), vec![text("f", ours)]);
        let theirs = "one\nTWO THEIRS\nthree\nfour\nfive\nSIX THEIRS\nseven\n";
        publish(&ops, &other(), Some(&t0), vec![text("f", theirs)]);
        let report = |max_conflict_bytes| {
            let mut req = landing(&head);
            req.max_conflict_bytes = max_conflict_bytes;
            report_of(ops.land(&who, req)).0
        };
        // Each hunk's texts: 4 + 9 + 11 bytes.
        let hunk_bytes = 24;
        let full = report(DEFAULT_MAX_CONFLICT_BYTES);
        let texts = |report: &ConflictReport| -> Vec<bool> {
            let hunks = &report.files[0].hunks;
            let sides =
                |hunk: &HunkOut| [&hunk.base, &hunk.ours, &hunk.theirs].map(|s| s.text.is_some());
            hunks.iter().flat_map(sides).collect()
        };
        assert_eq!((texts(&full), full.truncated), (vec![true; 6], false));
        let without_texts = |mut report: ConflictReport| {
            for hunk in &mut report.files[0].hunks {
                for side in [&mut hunk.base, &mut hunk.ours, &mut hunk.theirs] {
                    side.text = None;
                }
            }
            report.truncated = false;
            report
        };
        for (max, kept) in [
            (16, [false; 6]),
            (0, [false; 6]),
            (hunk_bytes, [true, true, true, false, false, false]),
            (2 * hunk_bytes - 1, [true, true, true, false, false, false]),
            (2 * hunk_bytes, [true; 6]),
        ] {
            let cut = report(max);
            assert_eq!(texts(&cut), kept, "max_conflict_bytes {max}");
            assert_eq!(
                cut.truncated,
                kept.contains(&false),
                "max_conflict_bytes {max}"
            );
            assert_eq!(without_texts(cut), without_texts(full.clone()), "{max}");
        }
    }

    /// The bound a land's conflict report stays within, however large or many the conflicting
    /// files: whatever lines and files it leaves out, `truncated` says so.
    mod report_bound {
        use std::fmt::Write as _;

        use super::*;

        /// The operations spec's target for a default response (§9): 64 KiB.
        const RESPONSE_TARGET: usize = 64 * 1024;

        /// What the envelope around an error adds to it (`ok`, `op`, `api` and `meta`): less.
        const ENVELOPE_BYTES: usize = 1024;

        /// A file whose sides' edits meet at `n` places, as `side` holds it: its line `2i + 1`,
        /// from 1, is `side` and `i`, the line after it a `}` that no side changes.
        fn alternating(side: &str, n: u64) -> String {
            let mut content = String::new();
            for i in 0..n {
                writeln!(content, "{side}{i}\n}}").expect("a String takes it");
            }
            content
        }

        /// How many bytes `error` takes with its envelope.
        fn enveloped(error: &OpError) -> usize {
            serde_json::to_vec(error)
                .expect("an error serializes")
                .len()
                + ENVELOPE_BYTES
        }

        /// A land that conflicts on a file at two thousand places stays within the response
        /// target, whatever its `max_conflict_bytes`: the report carries the first hunks, at
        /// least one, and one spanning the rest, so that the lines of every place where the
        /// sides' edits meet are within a hunk it carries; and it says `truncated`.
        #[test]
        fn a_file_conflicting_at_many_places_stays_within_it() {
            const PLACES: u64 = 2_000;
            let (_dir, _root, ops) = new_store();
            let who = agent();
            let t0 = publish(&ops, &who, None, vec![text("f", &alternating("l", PLACES))]);
            let head = commit(&ops, Some(&t0), vec![text("f", &alternating("o", PLACES))]);
            let theirs = vec![text("f", &alternating("t", PLACES))];
            publish(&ops, &other(), Some(&t0), theirs);

            for max_conflict_bytes in [0, DEFAULT_MAX_CONFLICT_BYTES] {
                let mut req = landing(&head);
                req.max_conflict_bytes = max_conflict_bytes;
                let (report, error) = report_of(ops.land(&who, req));
                let size = enveloped(&error);
                assert!(
                    size < RESPONSE_TARGET,
                    "{size} bytes, max {max_conflict_bytes}"
                );
                assert!(report.truncated, "max {max_conflict_bytes}");
                let [file] = &report.files[..] else {
                    panic!("one file: {}", report.files.len());
                };
                let first = &file.hunks[0];
                let lines = |side: &LinesOut| (side.first, side.last);
                assert_eq!(
                    [&first.base, &first.ours, &first.theirs].map(lines),
                    [(1, 1); 3]
                );
                for at in 0..PLACES {
                    let line = 2 * at + 1;
                    let within = |hunk: &HunkOut| hunk.base.first <= line && line <= hunk.base.last;
                    assert!(file.hunks.iter().any(within), "base line {line} in no hunk");
                }
            }
        }

        /// A land that conflicts on four hundred files stays within the response target: the
        /// report lists the first ones, in path order, each with its hunk, and says
        /// `truncated`; the error's message counts them all.
        #[test]
        fn many_conflicting_files_stay_within_it() {
            const FILES: usize = 400;
            let (_dir, _root, ops) = new_store();
            let who = agent();
            let name = |i: usize| format!("src/module-{i:03}/lib.rs");
            let all = |content: &str| -> Vec<ChangeInput> {
                (0..FILES).map(|i| text(&name(i), content)).collect()
            };
            let t0 = publish(&ops, &who, None, all("base\n"));
            let head = commit(&ops, Some(&t0), all("ours\n"));
            publish(&ops, &other(), Some(&t0), all("theirs\n"));

            let (report, error) = report_of(ops.land(&who, landing(&head)));
            let size = enveloped(&error);
            assert!(size < RESPONSE_TARGET, "{size} bytes");
            assert!(report.truncated);
            let listed: Vec<&str> = report.files.iter().map(|file| file.path.as_str()).collect();
            assert!(listed.len() < FILES, "{} files listed", listed.len());
            let first: Vec<String> = (0..listed.len()).map(name).collect();
            assert_eq!(listed, first, "the first ones, in path order");
            for file in &report.files {
                let [hunk] = &file.hunks[..] else {
                    panic!("{}: {} hunks", file.path, file.hunks.len());
                };
                assert_eq!((hunk.base.first, hunk.base.last), (1, 1), "{}", file.path);
            }
            let counted = format!("{FILES} path(s)");
            assert!(error.message.contains(&counted), "{}", error.message);
        }
    }

    /// Each side of a hunk is the lines `read_file` takes as `lines` at that side's changeset
    /// (the report's `base`, `ours` or `theirs`): read there, they are the side's text. A side
    /// without lines there has `last` one less than `first`, and an empty text.
    #[test]
    fn hunk_sides_are_the_lines_read_file_takes() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let t0 = publish(&ops, &who, None, vec![text("f", "1\n2\n3\n4\n5\n6\n7\n")]);
        let ours = "1\n3\n4\n5\nours six\nours six again\n7\n";
        let head = commit(&ops, Some(&t0), vec![text("f", ours)]);
        let theirs = "1\ntheirs two\n3\n4\n5\ntheirs six\n7\n";
        publish(&ops, &other(), Some(&t0), vec![text("f", theirs)]);

        let (report, _) = report_of(ops.land(&who, landing(&head)));
        let [file] = &report.files[..] else {
            panic!("one file: {report:?}");
        };
        assert_eq!(file.hunks.len(), 2, "{report:?}");
        let mut empty = 0;
        for hunk in &file.hunks {
            for (side, at) in [
                (&hunk.base, &report.base),
                (&hunk.ours, &report.ours),
                (&hunk.theirs, &report.theirs),
            ] {
                let text = side.text.as_deref().expect("within the budget");
                if side.last + 1 == side.first {
                    assert_eq!(text, "", "{hunk:?}");
                    empty += 1;
                    continue;
                }
                let lines = LineRange::new(side.first, side.last);
                let mut read = cat(at, &file.path);
                read.lines = Some(lines);
                assert_eq!(ops.read_file(read).unwrap().content, text, "{hunk:?}");
            }
        }
        assert_eq!(empty, 1, "ours deleted line 2: {report:?}");
    }

    /// A land with nothing to add to the target fails `E_NOTHING_TO_LAND`, naming why: its head
    /// is on the target (`on_target`), or the target holds its change already (`no_changes`), as
    /// a land of the same change without a key, run again, finds. A target that does not exist
    /// is `E_REV_NOT_FOUND`, naming it.
    #[test]
    fn nothing_to_land_names_its_reason() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let t0 = publish(&ops, &who, None, vec![text("a", "a\n")]);
        let on_target = expect_err(ops.land(&who, landing(&t0)), ErrorCode::NothingToLand);
        assert_eq!(
            Value::Object(on_target.details),
            json!({"reason": "on_target"})
        );
        let head = commit(&ops, Some(&t0), vec![text("b", "b\n")]);
        let landed = ops.land(&who, landing(&head)).unwrap().landed;
        let before = last_op(&ops);
        let again = expect_err(ops.land(&who, landing(&head)), ErrorCode::NothingToLand);
        assert_eq!(
            Value::Object(again.details),
            json!({"reason": "no_changes"})
        );
        assert_eq!((main_of(&ops), last_op(&ops)), (landed, before));

        let mut elsewhere = landing(&head);
        elsewhere.target = "release/none".to_owned();
        let missing = expect_err(ops.land(&who, elsewhere), ErrorCode::RevNotFound);
        assert_eq!(missing.details["rev"], "release/none");
        assert_eq!(missing.hint.as_deref(), Some("swarmops ref list"));
    }

    /// A keyed land run again finds the changeset it landed on the target and replays it: the
    /// same response but `status`, nothing written. The key given for another head is
    /// `E_INVALID_ARG`, naming the key and the changeset it landed.
    #[test]
    fn a_keyed_land_run_again_is_a_replay() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let t0 = publish(&ops, &who, None, vec![text("a", "a\n")]);
        let head = commit(&ops, Some(&t0), vec![text("b", "b\n")]);
        let mut keyed = landing(&head);
        keyed.idempotency_key = Some("land-1".to_owned());
        let first = ops.land(&who, keyed.clone()).unwrap();
        let before = last_op(&ops);
        let again = ops.land(&who, keyed).unwrap();
        let mut land_resp = first.clone();
        land_resp.status = LandStatus::Replayed;
        assert_eq!(again, land_resp);
        assert_eq!(last_op(&ops), before, "a replay writes nothing");

        let another = commit(&ops, Some(&t0), vec![text("c", "c\n")]);
        let mut reused = landing(&another);
        reused.idempotency_key = Some("land-1".to_owned());
        let error = expect_err(ops.land(&who, reused), ErrorCode::InvalidArg);
        assert_eq!(
            Value::Object(error.details),
            json!({"key": "land-1", "landed": first.landed})
        );
    }

    /// `show` names who landed a changeset a land made (its `lander` and `lander_run_id`
    /// metadata), the land's key and the chain it squashed, oldest first; the author is the
    /// change's. A changeset no land made has none of the three, and its JSON leaves them out.
    #[test]
    fn show_names_a_lands_lander_key_and_chain() {
        let (_dir, _root, ops) = new_store();
        let writer = agent().with_run_id("run-w");
        let landing_agent = Identity::parse("agent/lander-1")
            .unwrap()
            .with_run_id("run-7");
        let t0 = publish(&ops, &writer, None, vec![text("a", "a\n")]);
        let c1 = ops
            .commit(&writer, commit_req(Some(&t0), vec![text("b", "b\n")]))
            .unwrap()
            .changeset;
        let c2 = ops
            .commit(&writer, commit_req(Some(&c1), vec![text("c", "c\n")]))
            .unwrap()
            .changeset;
        let mut req = landing(&c2);
        req.idempotency_key = Some("k-7".to_owned());
        req.message = Some("land b and c".to_owned());
        let landed = ops.land(&landing_agent, req).unwrap().landed;

        let shown = ops.show(show(&landed)).unwrap();
        let attribution = |agent: &str, run_id: &str| {
            let mut attribution = Attribution::new(agent.to_owned());
            attribution.run_id = Some(run_id.to_owned());
            attribution
        };
        assert_eq!(shown.author, attribution(writer.agent(), "run-w"));
        assert_eq!(shown.lander, Some(attribution("agent/lander-1", "run-7")));
        assert_eq!(shown.land_key.as_deref(), Some("k-7"));
        assert_eq!(shown.squashed_from, [c1.clone(), c2.clone()]);
        assert_eq!(shown.message, "land b and c");
        assert_eq!(shown.signatures.committer.name, "agent/lander-1");
        assert_eq!(shown.metadata["lander_run_id"], "run-7");

        let plain = ops.show(show(&c2)).unwrap();
        assert_eq!(
            (&plain.lander, &plain.land_key, &plain.squashed_from),
            (&None, &None, &Vec::new())
        );
        let json = serde_json::to_value(&plain).unwrap();
        for key in ["lander", "land_key", "squashed_from"] {
            assert!(json.get(key).is_none(), "{key}: {json}");
        }
        // A lander without a run id, and a land without a key.
        let c3 = commit(&ops, Some(&landed), vec![text("d", "d\n")]);
        let bare = ops.land(&agent(), landing(&c3)).unwrap().landed;
        let shown = ops.show(show(&bare)).unwrap();
        let by_agent = Attribution::new(agent().agent().to_owned());
        assert_eq!((shown.lander, shown.land_key), (Some(by_agent), None));
    }

    /// The key and the attempts are checked before anything is resolved: a key that is not 1 to
    /// 64 letters, digits and `-` is `E_INVALID_ARG` naming it, and so are `max_attempts` outside
    /// 1 to 32; within them, the land goes on (here to the head it cannot find).
    #[test]
    fn a_land_checks_its_key_and_attempts_first() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let long = "k".repeat(65);
        for key in ["", "has space", "a_b", "a.b", "ünï", long.as_str()] {
            let mut req = landing("release/none");
            req.idempotency_key = Some(key.to_owned());
            let error = expect_err(ops.land(&who, req), ErrorCode::InvalidArg);
            assert_eq!(
                Value::Object(error.details),
                json!({"idempotency_key": key})
            );
        }
        for attempts in [0, 33] {
            let mut req = landing("release/none");
            req.max_attempts = attempts;
            let error = expect_err(ops.land(&who, req), ErrorCode::InvalidArg);
            assert_eq!(
                Value::Object(error.details),
                json!({"max_attempts": attempts})
            );
        }
        let uuid = "0192e4f6-3c2a-7b1d-9e8f-0123456789ab";
        let id = "ab".repeat(32);
        let longest = "K-9".repeat(21) + "x";
        for key in ["k", "Key-1", uuid, id.as_str(), longest.as_str()] {
            for attempts in [1, 32] {
                let mut req = landing("release/none");
                req.idempotency_key = Some(key.to_owned());
                req.max_attempts = attempts;
                expect_err(ops.land(&who, req), ErrorCode::RevNotFound);
            }
        }
    }
    /// Resolutions through the operations (operations spec §3, resolution path (b)): a land's
    /// `resolutions` checked, their contents turned into bytes by a commit's rules, and a stale
    /// one named in the conflict report's `stale`.
    mod resolutions {
        use swarmops_api::{ResolutionInput, ResolutionsInput, Side};

        use super::*;

        /// A resolution with these fields.
        fn resolution(
            take: Option<Side>,
            content: Option<ContentInput>,
            mode: Option<ModeInput>,
            delete: bool,
        ) -> ResolutionInput {
            let mut resolution_input = ResolutionInput::default();
            resolution_input.take = take;
            resolution_input.content = content;
            resolution_input.mode = mode;
            resolution_input.delete = delete;
            resolution_input
        }

        /// A resolution that takes `side`.
        fn take(side: Side) -> ResolutionInput {
            resolution(Some(side), None, None, false)
        }

        /// A resolution that puts `text`, inline.
        fn inline(text: &str) -> ResolutionInput {
            let content = ContentInput::Inline(text.to_owned());
            resolution(None, Some(content), None, false)
        }

        /// A land of `head` with `files` resolved against `against`.
        fn resolving(head: &str, against: &str, files: Vec<(&str, ResolutionInput)>) -> LandReq {
            let files = (files.into_iter())
                .map(|(path, resolution)| (path.to_owned(), resolution))
                .collect();
            let mut land_req = landing(head);
            land_req.resolutions = Some(ResolutionsInput::new(against, files));
            land_req
        }

        /// A resolution takes exactly one of `take`, `content` and `delete`, a `mode` only with
        /// `content`, a repository path, and an id for a blob it names: each refusal is
        /// `E_INVALID_ARG` naming the path (and the blob), before anything is resolved (the head
        /// here is a ref that does not exist). A resolution of each form goes on, to the head.
        #[test]
        fn a_resolution_takes_exactly_one_of_take_content_delete() {
            let (_dir, _root, ops) = new_store();
            let who = agent();
            let against = "ab".repeat(32);
            let text = || Some(ContentInput::Inline("x\n".to_owned()));
            let refused = |files: Vec<(&str, ResolutionInput)>| {
                let req = resolving("release/none", &against, files);
                expect_err(ops.land(&who, req), ErrorCode::InvalidArg)
            };
            for (shape, why) in [
                (resolution(None, None, None, false), "none of the three"),
                (
                    resolution(Some(Side::Ours), None, None, true),
                    "take and delete",
                ),
                (
                    resolution(Some(Side::Theirs), text(), None, false),
                    "take and content",
                ),
                (resolution(None, text(), None, true), "content and delete"),
                (
                    resolution(Some(Side::Ours), text(), None, true),
                    "all three",
                ),
                (
                    resolution(Some(Side::Ours), None, Some(ModeInput::Exec), false),
                    "a mode with take",
                ),
                (
                    resolution(None, None, Some(ModeInput::File), true),
                    "a mode with delete",
                ),
            ] {
                let error = refused(vec![("src/f.rs", shape)]);
                assert_eq!(
                    Value::Object(error.details.clone()),
                    json!({"path": "src/f.rs"}),
                    "{why}: {error:?}"
                );
                assert!(error.message.contains("src/f.rs"), "{why}: {error:?}");
            }
            for bad in ["", "/abs", "a//b", "../x", "a/./b", "a/"] {
                let error = refused(vec![(bad, take(Side::Ours))]);
                assert_eq!(error.details["path"], bad, "{error:?}");
            }
            let blob = Some(ContentInput::Blob("not-an-id".to_owned()));
            let error = refused(vec![("f", resolution(None, blob, None, false))]);
            assert_eq!(
                Value::Object(error.details),
                json!({"path": "f", "blob": "not-an-id"})
            );

            let id = "cd".repeat(32);
            let blob = Some(ContentInput::Blob(id));
            let files = vec![
                ("a", take(Side::Ours)),
                ("b", take(Side::Theirs)),
                ("c", resolution(None, text(), Some(ModeInput::Exec), false)),
                ("d", resolution(None, blob, None, false)),
                ("e", resolution(None, None, None, true)),
            ];
            let req = resolving("release/none", &against, files);
            let missing = expect_err(ops.land(&who, req), ErrorCode::RevNotFound);
            assert_eq!(missing.details["rev"], "release/none");
        }

        /// A land resolved against its conflict report's `theirs` lands: a resolution of each
        /// form, its content by a commit's rules (inline, base64 with a mode). Made against a
        /// head of main that held a resolved path otherwise, it is `E_CONFLICT` again, whose
        /// report names the path in `stale`; `against` must be a changeset the store holds, and
        /// a client file a resolution names one it can read, which the error names.
        #[test]
        fn resolutions_land_and_a_stale_one_is_reported() {
            let (_dir, root, ops) = new_store();
            let who = agent();
            let base = vec![text("f", "1\n2\n3\n"), text("g", "1\n2\n3\n")];
            let t0 = publish(&ops, &who, None, base);
            let ours = vec![text("f", "1\nours\n3\n"), text("g", "1\nours\n3\n")];
            let head = commit(&ops, Some(&t0), ours);
            let theirs = vec![text("f", "1\ntheirs\n3\n"), text("g", "1\ntheirs\n3\n")];
            let x = publish(&ops, &other(), Some(&t0), theirs);
            let (report, _) = report_of(ops.land(&who, landing(&head)));
            assert_eq!(
                (report.theirs.as_str(), report.files.len()),
                (x.as_str(), 2)
            );
            assert_eq!(report.stale, Vec::<String>::new());

            // main moves on and changes g again: a resolution of g against x is stale.
            let y = publish(
                &ops,
                &other(),
                Some(&x),
                vec![text("g", "1\ntheirs\nthree\n")],
            );
            let before = last_op(&ops);
            let both = || {
                let encoded = STANDARD.encode("#!/bin/sh\n");
                let script = ContentInput::Base64(encoded);
                vec![
                    ("f", take(Side::Ours)),
                    ("g", inline("1\nboth\n3\n")),
                    (
                        "run.sh",
                        resolution(None, Some(script), Some(ModeInput::Exec), false),
                    ),
                ]
            };
            let (stale, error) = report_of(ops.land(&who, resolving(&head, &x, both())));
            assert_eq!(
                (stale.theirs.as_str(), stale.stale.clone()),
                (y.as_str(), vec!["g".to_owned()])
            );
            let files: Vec<&str> = stale.files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(files, ["g"], "f was resolved, g still conflicts: {stale:?}");
            assert!(error.message.contains("stale"), "{error:?}");
            assert_eq!(last_op(&ops), before, "nothing recorded");

            let unknown = "ef".repeat(32);
            let missing = expect_err(
                ops.land(&who, resolving(&head, &unknown, both())),
                ErrorCode::RevNotFound,
            );
            assert_eq!(missing.details["rev"], unknown.as_str());
            let absent = root.join("absent.txt").to_string();
            let file = resolution(None, Some(ContentInput::File(absent.clone())), None, false);
            let error = expect_err(
                ops.land(&who, resolving(&head, &y, vec![("g", file)])),
                ErrorCode::InvalidArg,
            );
            assert_eq!(
                Value::Object(error.details),
                json!({"path": "g", "file": absent})
            );

            let landed = ops.land(&who, resolving(&head, &y, both())).unwrap();
            assert_eq!(
                (landed.rebased_onto.as_str(), landed.files_changed),
                (y.as_str(), 3)
            );
            let read = |path: &str| ops.read_file(cat("main", path)).unwrap();
            assert_eq!(read("f").content, "1\nours\n3\n");
            assert_eq!(read("g").content, "1\nboth\n3\n");
            let script = read("run.sh");
            assert_eq!(
                (script.content.as_str(), script.kind),
                ("#!/bin/sh\n", FileKind::Exec)
            );
        }
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
    let (_dir, root, ops) = v1_store();
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
    let mut no_content = text("b", "");
    no_content.content = None;
    let mut delete_with_content = text("a/payload", "x");
    delete_with_content.op = ChangeOp::Delete;
    let mut delete_with_mode = delete("a/payload");
    delete_with_mode.mode = Some(ModeInput::Exec);
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
    assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 1);
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
    let mut req = commit_req(Some(&c1), vec![]);
    req.allow_empty = true;
    let empty = ops.commit(&agent(), req).unwrap();
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

/// A store whose `meta/lock` is a FIFO, as a restored archive can hold, would block every write
/// in its open of the lock: a commit fails at once instead, with `E_STORE_CORRUPT` naming the
/// lock file relative to the store (no host path) and the hint to run verify, which names it as
/// damaged metadata. Reads take no lock, and go on. Once the FIFO is removed, the next commit
/// makes a new lock file and lands. The commit runs on a thread with a deadline, so that a
/// regression fails instead of hanging the test.
#[test]
fn a_fifo_at_meta_lock_fails_a_commit_as_corrupt_and_verify_names_it() {
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let (_dir, root, ops) = new_store();
    let c1 = commit(&ops, None, vec![text("f", "1")]);
    let lock = Layout::new(root.clone()).meta_lock();
    fs::remove_file(&lock).unwrap();
    let made = Command::new("mkfifo").arg(&lock).status();
    assert!(made.expect("run mkfifo").success(), "mkfifo");

    let (tx, rx) = mpsc::channel();
    let (at, parent) = (root.clone(), c1.clone());
    thread::spawn(move || {
        let ops = LocalOps::open(&at).expect("open the store");
        let _ = tx.send(ops.commit(&agent(), commit_req(Some(&parent), vec![text("f", "2")])));
    });
    let committed = rx.recv_timeout(Duration::from_secs(10));
    let err = expect_err(
        committed.expect("the commit blocked on the FIFO at meta/lock"),
        ErrorCode::StoreCorrupt,
    );
    assert_eq!(
        Value::Object(err.details.clone()),
        json!({"fs_op": "open", "store_file": "meta/lock"})
    );
    assert_eq!(
        err.message,
        "i/o error: open meta/lock: lock is not a regular file"
    );
    assert_eq!(err.hint.as_deref(), Some("swarmops verify"));
    assert_eq!(ops.read_file(cat(&c1, "f")).unwrap().content, "1");

    let mut verify_req = VerifyReq::default();
    verify_req.deep = false;
    let report = ops.verify(verify_req).unwrap();
    assert_eq!(
        report.problems,
        [
            "damaged metadata: meta/lock is not a regular file: every write fails on it until it \
          is removed, and the next write makes a new one"
        ]
    );
    assert_eq!(
        serde_json::to_value(&report.findings).unwrap(),
        json!([{"kind": "meta_corrupt"}])
    );

    fs::remove_file(&lock).unwrap();
    commit(&ops, Some(&c1), vec![text("f", "2")]);
    assert!(fs::symlink_metadata(&lock).unwrap().is_file());
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
    let mut zero_ls = ls(&c, None);
    zero_ls.limit = 0;
    let mut zero_log = log_req(Some(&c));
    zero_log.limit = 0;
    let mut zero_show = show(&c);
    zero_show.max_changes = 0;
    expect_err(ops.read_tree(zero_ls), ErrorCode::InvalidArg);
    expect_err(ops.log(zero_log), ErrorCode::InvalidArg);
    expect_err(ops.show(zero_show), ErrorCode::InvalidArg);
    // A log cursor is a changeset id the store holds.
    let unknown = "0".repeat(64);
    for cursor in ["main", &c[..12], "zz", unknown.as_str()] {
        let mut req = log_req(None);
        req.cursor = Some(cursor.to_owned());
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
        let mut read_tree_req = ls(&head, None);
        read_tree_req.limit = 2;
        read_tree_req.cursor = cursor.clone();
        let page = ops.read_tree(read_tree_req).unwrap();
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
    let mut after_e = ls(&head, None);
    after_e.cursor = Some("e".to_owned());
    let past_the_end = ops.read_tree(after_e).unwrap();
    assert!(past_the_end.entries.is_empty(), "{past_the_end:?}");

    // log, two changesets a page, newest first; each cursor is the page's last id.
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let mut req = log_req(Some(&head));
        req.limit = 2;
        req.cursor = cursor.clone();
        let page = ops.log(req).unwrap();
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
    let mut req = log_req(Some(&chain[3]));
    req.limit = 2;
    let two = ops.log(req).unwrap();
    assert_eq!(
        (two.entries.len(), two.truncated, two.cursor),
        (2, false, None)
    );
    let mut root_cursor = log_req(None);
    root_cursor.cursor = Some(chain[4].clone());
    let empty = ops.log(root_cursor).unwrap();
    assert_eq!((empty.entries.len(), empty.truncated), (0, false));

    // show caps `changes` and still counts them all.
    let mut show_req = show(&chain[4]);
    show_req.max_changes = 2;
    let capped = ops.show(show_req).unwrap();
    let paths: Vec<&str> = capped.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a", "b"]);
    assert_eq!((capped.files_changed, capped.changes_truncated), (5, true));
    let mut req = show(&chain[4]);
    req.max_changes = 5;
    let exact = ops.show(req).unwrap();
    assert_eq!((exact.changes.len(), exact.changes_truncated), (5, false));
}

#[test]
fn show_and_log_decode_what_native_commits_do_not_write() {
    let (_dir, root, ops) = new_store();
    // A changeset made below the API: no agent metadata, a value and message bytes that are
    // not UTF-8, a signature with a UTC offset.
    let repo = LocalRepo::open(&root).unwrap();
    let signature = Signature::new(
        b"Ada \xffLovelace".to_vec(),
        b"ada@example.com".to_vec(),
        1_760_000_000,
        3600,
        false,
    )
    .unwrap();
    let mut committer = signature.clone();
    committer.secs = 1_760_000_123;
    let outcome = repo
        .commit(CommitRequest::new(
            Vec::new(),
            vec![Change::new(
                RepoPath::new("f").unwrap(),
                swarmops_repo::ChangeOp::Put {
                    kind: LeafKind::File,
                    content: b"x".to_vec(),
                },
            )],
            signature.clone(),
            committer,
            b"caf\xe9\nsecond line".to_vec(),
            vec![
                ("blob".to_owned(), vec![0xff, 0x00]),
                ("note".to_owned(), b"hi".to_vec()),
            ],
            false,
            None,
        ))
        .unwrap();
    let id = outcome.id.to_string();

    let detail = ops.show(show(&id)).unwrap();
    assert_eq!(
        detail.author,
        Attribution::new("unknown/unknown".to_owned())
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
        let mut verify_req = VerifyReq::default();
        verify_req.deep = deep;
        let report = ops.verify(verify_req).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!(report.changesets_checked, 1);
        assert!(report.objects_checked >= 3, "{report:?}");
    }

    // A blob gone from the store: verify says so, and reading it is E_STORE_CORRUPT.
    let gone = hash_object(ObjectKind::Blob, b"content");
    fs::remove_file(Layout::new(root.clone()).object_path(gone)).unwrap();
    let mut req = VerifyReq::default();
    req.deep = false;
    let report = ops.verify(req).unwrap();
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
    let mut req = VerifyReq::default();
    req.deep = true;
    assert_eq!(ops.verify(req).unwrap().findings, []);

    let gone = hash_object(ObjectKind::Blob, b"content");
    fs::remove_file(Layout::new(root.clone()).object_path(gone)).unwrap();
    for deep in [false, true] {
        let mut verify_req = VerifyReq::default();
        verify_req.deep = deep;
        let report = ops.verify(verify_req).unwrap();
        assert_eq!(report.problems.len(), 1, "{report:?}");
        assert_eq!(
            serde_json::to_value(&report.findings).unwrap(),
            json!([{"kind": "missing_object", "object": gone.to_hex(), "changeset": c}]),
            "deep {deep}"
        );
    }
}

/// With problems of two kinds, `findings[i]` is the finding of `problems[i]`, one for one and in
/// the same order: a blob gone from the store and one whose file is damaged (which a deep
/// verify reads) each name their object in the line and in the finding at its index.
#[test]
fn verify_findings_stand_beside_their_problems_one_for_one() {
    let (_dir, root, ops) = new_store();
    let c = commit(&ops, None, vec![text("f", "content"), text("g", "other")]);
    let layout = Layout::new(root.clone());
    let gone = hash_object(ObjectKind::Blob, b"content");
    fs::remove_file(layout.object_path(gone)).unwrap();
    let damaged = hash_object(ObjectKind::Blob, b"other");
    let mut bytes = fs::read(layout.object_path(damaged)).unwrap();
    *bytes.last_mut().unwrap() ^= 0x01;
    fs::write(layout.object_path(damaged), bytes).unwrap();

    let mut req = VerifyReq::default();
    req.deep = true;
    let report = ops.verify(req).unwrap();
    assert_eq!(report.problems.len(), 2, "{report:?}");
    assert_eq!(report.findings.len(), report.problems.len(), "{report:?}");
    for (problem, finding) in report.problems.iter().zip(&report.findings) {
        let (kind, object, changeset) = if problem.ends_with("is corrupt") {
            ("corrupt_object", damaged, None)
        } else {
            ("missing_object", gone, Some(c.as_str()))
        };
        assert!(problem.contains(&object.to_hex()), "{problem}");
        assert_eq!(finding.kind, kind, "{problem}: {finding:?}");
        assert_eq!(
            finding.object.as_deref(),
            Some(object.to_hex().as_str()),
            "{problem}"
        );
        assert_eq!(finding.changeset.as_deref(), changeset, "{problem}");
    }
    assert_ne!(report.findings[0].kind, report.findings[1].kind);
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
        let refs = request.ref_list(RefListReq::default()).unwrap().refs;
        let targets: Vec<(&str, &str)> = (refs.iter())
            .map(|r| (r.name.as_str(), r.target.as_str()))
            .collect();
        assert_eq!(targets, [("main", c1.as_str())]);

        let mut advancing = commit_req(Some(&c1), vec![text("f", "2")]);
        advancing.advance = Some("main".to_owned());
        let c2 = request.commit(&who, advancing).unwrap().changeset;
        assert_eq!(request.read_file(cat("main", "f")).unwrap().content, "2");
        let log = request.log(log_req(None)).unwrap();
        assert_eq!(ids(&log), [c2.as_str(), c1.as_str()]);
        let main = request.repo_info(InfoReq::default()).unwrap().main;
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
        let mut path_history_req = PathHistoryReq::new(path.to_owned());
        path_history_req.rev = rev.map(Rev::from);
        path_history_req.limit = DEFAULT_PATH_HISTORY_LIMIT;
        path_history_req
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
            let mut req = commit_req(ids.last().map(String::as_str), changes);
            req.allow_empty = true;
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
            let mut req = log_req(None);
            req.limit = limit;
            req.cursor = cursor.map(str::to_owned);
            req.paths = paths.iter().map(|&p| p.to_owned()).collect();
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
        let mut from_c3 = log_req(Some(&c[3]));
        from_c3.paths = vec!["b".to_owned()];
        let page = ops.recording(&resolved).log(from_c3).unwrap();
        assert_eq!(ids(&page), at(&[1, 0]));
        assert_eq!(
            resolved.take(),
            BTreeMap::from([(c[3].clone(), c[3].clone())])
        );

        for bad in ["a//b", "/", "", "./a"] {
            let mut req = log_req(None);
            req.paths = vec!["a".to_owned(), bad.to_owned()];
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
            let mut req = history(None, "p");
            req.limit = 2;
            req.cursor = cursor.map(str::to_owned);
            ops.recording(&resolved).path_history(req).unwrap()
        };
        let first = page(None);
        assert_eq!(first.path, "p");
        assert_eq!(changesets(&first), [c[6].as_str(), c[4].as_str()]);
        assert_eq!(
            (first.truncated, first.cursor.as_deref()),
            (true, Some(c[4].as_str()))
        );
        let mut path_history_entry = PathHistoryEntry::new(
            c[6].clone(),
            ChangeOp::Put,
            "agent/worker-17".to_owned(),
            first.entries[0].created_at,
            "a test commit".to_owned(),
        );
        path_history_entry.kind = Some(EntryKindOut::Exec);
        path_history_entry.id = Some(blob(b"6"));
        assert_eq!(first.entries[0], path_history_entry);
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
            let mut req = history(None, "p");
            req.cursor = Some(cursor.to_owned());
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

        let mut zero = history(None, "gone");
        zero.limit = 0;
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
        let signature = Signature::new(
            b"agent/worker-17".to_vec(),
            b"worker-17@agent.swarmops.local".to_vec(),
            1_760_000_000,
            0,
            false,
        )
        .unwrap();
        let mut head: Option<ChangesetId> = None;
        for i in 0..500_u32 {
            let content = i.to_string();
            let title = format!("{:x<55}{i:05}", "change the path, a title of 60 bytes ");
            assert_eq!(title.len(), 60);
            let outcome = repo
                .commit(CommitRequest::new(
                    head.into_iter().collect(),
                    vec![Change::new(
                        RepoPath::new("src/p").unwrap(),
                        swarmops_repo::ChangeOp::Put {
                            kind: LeafKind::File,
                            content: content.into_bytes(),
                        },
                    )],
                    signature.clone(),
                    signature.clone(),
                    format!("{title}\n\nthe body is not in the page").into_bytes(),
                    vec![
                        ("agent".to_owned(), b"agent/worker-17".to_vec()),
                        ("run_id".to_owned(), b"run-17".to_vec()),
                    ],
                    false,
                    None,
                ))
                .unwrap();
            head = Some(outcome.id);
        }
        drop(repo);
        let head = head.unwrap().to_string();
        ops.ref_set(&agent(), set("main", &head, None, true))
            .unwrap();
        let meta = || {
            let mut meta = Meta::new(
                1_000,
                BTreeMap::from([("main".to_owned(), head.clone())]),
                "trust".to_owned(),
            );
            meta.repo = Some(root.to_string());
            meta
        };

        let page = ops.path_history(history(None, "src/p")).unwrap();
        assert_eq!(
            (page.entries.len(), page.truncated),
            (DEFAULT_PATH_HISTORY_LIMIT, true)
        );
        let bytes = serde_json::to_vec(&Envelope::ok("path_history", page, meta())).unwrap();
        assert!(bytes.len() <= 65_536, "path_history: {} bytes", bytes.len());

        let mut log = log_req(None);
        log.paths = vec!["src".to_owned()];
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
        ops.blob_put(&agent(), BlobPutReq::new(content))
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
        assert_eq!(first, BlobPutResp::new(blob(&content), 4096, true));
        let again = blob_put(&ops, ContentInput::Base64(STANDARD.encode(&content)));
        let mut blob_put_resp = first.clone();
        blob_put_resp.written = false;
        assert_eq!(again, blob_put_resp);

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
        let mut change_out = ChangeOut::new("data/payload.bin".to_owned(), ChangeOp::Put);
        change_out.kind = Some(FileKind::File);
        change_out.blob = Some(first.blob.clone());
        assert_eq!(shown.changes[0], change_out);

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
                BlobPutReq::new(ContentInput::Blob(first.blob.clone())),
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
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 1);
        let unwritten = BlobHasReq::new(vec![blob(b"new")]);
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
            ops.blob_has(BlobHasReq::new(
                ids.iter().map(|id| (*id).to_owned()).collect(),
            ))
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
        assert_eq!(has, BlobHasResp::new(vec![stored.clone(), committed]));
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
        let content = || BlobPutReq::new(ContentInput::Inline("shared\n".to_owned()));
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
                BlobPutReq::new(ContentInput::File(huge.to_string())),
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
        let mut read_file_req = cat(rev, path);
        read_file_req.offset = offset;
        read_file_req.max_bytes = max_bytes;
        read_file_req
    }

    /// `read_file` of lines `first` to `last` of `path` at `rev`, at most `max_bytes`.
    fn lines(rev: &str, path: &str, (first, last): (u64, u64), max_bytes: u64) -> ReadFileReq {
        let mut read_file_req = cat(rev, path);
        read_file_req.lines = Some(LineRange::new(first, last));
        read_file_req.max_bytes = max_bytes;
        read_file_req
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
        let held = |first, last| Some(LineRange::new(first, last));
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
        let mut both = lines(&c, "dir/f", (1, 1), all);
        both.offset = 1;
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
            let mut req = ls(&c, None);
            req.depth = depth;
            let err = expect_err(ops.read_tree(req), ErrorCode::InvalidArg);
            assert_eq!(err.details["depth"], depth);
        }
        let mut deepest = ls(&c, None);
        deepest.depth = MAX_TREE_DEPTH;
        assert_eq!(paths(&ops.read_tree(deepest).unwrap()), ["dir", "dir/f"]);
        for cursor in ["", "/dir", "dir/", "dir//f", "..", "a/./b"] {
            let mut req = ls(&c, None);
            req.cursor = Some(cursor.to_owned());
            let err = expect_err(ops.read_tree(req), ErrorCode::InvalidArg);
            assert_eq!(err.details["cursor"], cursor);
        }

        // Before the revision: one the store does not hold changes nothing.
        let unknown = "0".repeat(64);
        let mut req = ls(&unknown, None);
        req.depth = 0;
        expect_err(ops.read_tree(req), ErrorCode::InvalidArg);
        let mut req = lines(&unknown, "dir/f", (1, 1), all);
        req.offset = 1;
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
            let mut req = ls(&c, path);
            req.depth = depth;
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
        let mut req = ls(&c, Some("a"));
        req.depth = 2;
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
            let mut req = ls(&c, None);
            req.depth = depth;
            req.limit = limit;
            req.cursor = cursor.map(str::to_owned);
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
            let mut req = ls(&c, Some(dir));
            req.depth = depth;
            req.limit = limit;
            let listing = ops.read_tree(req).unwrap();
            assert!(listing.truncated, "{dir}: a page of {limit}");
            let mut meta = Meta::new(
                12_345,
                BTreeMap::from([("main".to_owned(), c.clone())]),
                "trust".to_owned(),
            );
            meta.repo = Some(root.to_string());
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
        let mut derive_req = DeriveReq::default();
        derive_req.after = after;
        derive_req.limit = limit;
        derive_req
    }

    fn resp(derived: u64, cached: u64, next: Option<u64>) -> DeriveResp {
        let mut derive_resp = DeriveResp::new(derived, cached);
        derive_resp.next = next;
        derive_resp
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

/// `diff` (operations spec §1): the files that differ between two revisions, a page at a time,
/// by name, with line counts or with unified hunks.
mod diff {
    use std::fmt::Write as _;
    use std::sync::Arc;

    use swarmops_api::{
        DEFAULT_DIFF_CONTEXT, DEFAULT_DIFF_LIMIT, DEFAULT_DIFF_MAX_BYTES, Diff, DiffFormat,
        DiffReq, Envelope, FileDiff, Meta,
    };
    use swarmops_testkit::fs::NoSyncFs;
    use swarmops_testkit::repo::{put as repo_put, request};

    use super::*;

    /// A diff of `from` to `to` in `format`, every other field at its default.
    fn diff_req(from: &str, to: &str, format: DiffFormat) -> DiffReq {
        let mut diff_req = DiffReq::new(from, to);
        diff_req.format = format;
        diff_req.context = DEFAULT_DIFF_CONTEXT;
        diff_req.limit = DEFAULT_DIFF_LIMIT;
        diff_req.max_bytes = DEFAULT_DIFF_MAX_BYTES;
        diff_req
    }

    /// The paths of a page's files, in its order.
    fn paths_of(page: &Diff) -> Vec<String> {
        page.files.iter().map(|file| file.path.clone()).collect()
    }

    /// How many bytes `files` take as the JSON array they print as.
    fn json_len(files: &[FileDiff]) -> usize {
        serde_json::to_string(files).unwrap().len()
    }

    /// A symlink put at `path`, to `target`.
    fn symlink(path: &str, target: &str) -> ChangeInput {
        put(
            path,
            ContentInput::Inline(target.to_owned()),
            Some(ModeInput::Symlink),
        )
    }

    /// A pair of changesets with each status among the files between them: a text file
    /// modified, an exec bit turned on, a symlink pointed elsewhere, a file deleted, one added,
    /// and a file that became a symlink. Each format says exactly what it promises, in path
    /// order: `names` the path and status; `stat` also whether the file is binary and the lines
    /// inserted and deleted; `unified` also the kinds, the blobs and the hunks, with three lines
    /// of context, git's header numbers and every line's break.
    #[test]
    fn diff_formats_names_stat_and_unified() {
        let (_dir, _root, ops) = new_store();
        let script = "#!/bin/sh\necho hi\n";
        let c1 = commit(
            &ops,
            None,
            vec![
                text("README.md", "# acme\n\nline 3\nline 4\n"),
                text("bin/run", script),
                symlink("latest", "README.md"),
                text("notes.txt", "kept\n"),
                text("version", "1\n"),
            ],
        );
        let c2 = commit(
            &ops,
            Some(&c1),
            vec![
                text("README.md", "# acme\n\nline three\nline 4\n"),
                put(
                    "bin/run",
                    ContentInput::Inline(script.to_owned()),
                    Some(ModeInput::Exec),
                ),
                symlink("latest", "src/lib.rs"),
                delete("notes.txt"),
                text("src/lib.rs", "pub fn f() {}\n"),
                symlink("version", "README.md"),
            ],
        );
        let diff = |format| serde_json::to_value(ops.diff(diff_req(&c1, &c2, format)).unwrap());
        let status = |path: &str, status: &str| json!({"path": path, "status": status});
        assert_eq!(
            diff(DiffFormat::Names).unwrap(),
            json!({
                "from": c1,
                "to": c2,
                "files": [
                    status("README.md", "modified"),
                    status("bin/run", "modified"),
                    status("latest", "modified"),
                    status("notes.txt", "deleted"),
                    status("src/lib.rs", "added"),
                    status("version", "type_changed"),
                ],
                "truncated": false,
            })
        );
        let counted = |path: &str, status: &str, insertions: u64, deletions: u64| {
            json!({
                "path": path, "status": status, "binary": false,
                "insertions": insertions, "deletions": deletions,
            })
        };
        assert_eq!(
            diff(DiffFormat::Stat).unwrap()["files"],
            json!([
                counted("README.md", "modified", 1, 1),
                counted("bin/run", "modified", 0, 0),
                counted("latest", "modified", 1, 1),
                counted("notes.txt", "deleted", 0, 1),
                counted("src/lib.rs", "added", 1, 0),
                counted("version", "type_changed", 1, 1),
            ])
        );
        let unified = diff(DiffFormat::Unified).unwrap();
        assert_eq!(
            (&unified["from"], &unified["to"], &unified["truncated"]),
            (&json!(c1), &json!(c2), &json!(false))
        );
        assert!(unified.get("cursor").is_none(), "{unified}");
        assert_eq!(unified["files"], unified_files(script));
    }

    /// `diff_formats_names_stat_and_unified`'s files in `unified`.
    fn unified_files(script: &str) -> Value {
        let line = |op: &str, text: &str| json!({"op": op, "text": text});
        let hunk = |old: [u64; 2], new: [u64; 2], lines: Vec<Value>| {
            json!({
                "old_start": old[0], "old_lines": old[1],
                "new_start": new[0], "new_lines": new[1],
                "lines": lines,
            })
        };
        let replaced = |old: &str, new: &str| {
            hunk([1, 1], [1, 1], vec![line("remove", old), line("add", new)])
        };
        json!([
            {
                "path": "README.md", "status": "modified",
                "old_kind": "file", "new_kind": "file",
                "old_blob": blob(b"# acme\n\nline 3\nline 4\n"),
                "new_blob": blob(b"# acme\n\nline three\nline 4\n"),
                "binary": false, "insertions": 1, "deletions": 1,
                "hunks": [hunk([1, 4], [1, 4], vec![
                    line("context", "# acme\n"),
                    line("context", "\n"),
                    line("remove", "line 3\n"),
                    line("add", "line three\n"),
                    line("context", "line 4\n"),
                ])],
            },
            {
                "path": "bin/run", "status": "modified",
                "old_kind": "file", "new_kind": "exec",
                "old_blob": blob(script.as_bytes()), "new_blob": blob(script.as_bytes()),
                "binary": false, "insertions": 0, "deletions": 0, "hunks": [],
            },
            {
                "path": "latest", "status": "modified",
                "old_kind": "symlink", "new_kind": "symlink",
                "old_blob": blob(b"README.md"), "new_blob": blob(b"src/lib.rs"),
                "binary": false, "insertions": 1, "deletions": 1,
                "hunks": [replaced("README.md", "src/lib.rs")],
            },
            {
                "path": "notes.txt", "status": "deleted",
                "old_kind": "file", "old_blob": blob(b"kept\n"),
                "binary": false, "insertions": 0, "deletions": 1,
                "hunks": [hunk([1, 1], [0, 0], vec![line("remove", "kept\n")])],
            },
            {
                "path": "src/lib.rs", "status": "added",
                "new_kind": "file", "new_blob": blob(b"pub fn f() {}\n"),
                "binary": false, "insertions": 1, "deletions": 0,
                "hunks": [hunk([0, 0], [1, 1], vec![line("add", "pub fn f() {}\n")])],
            },
            {
                "path": "version", "status": "type_changed",
                "old_kind": "file", "new_kind": "symlink",
                "old_blob": blob(b"1\n"), "new_blob": blob(b"README.md"),
                "binary": false, "insertions": 1, "deletions": 1,
                "hunks": [replaced("1\n", "README.md")],
            },
        ])
    }

    /// A page holds at most `limit` files, in path order (by bytes: `b-x` before `b/c`), and the
    /// next starts after the `cursor` the page before returned, in every format; the pages
    /// together are the whole diff. A cursor need not be a path of the diff. `paths` keeps the
    /// files at or below some paths (one trailing `/` taken, a file's path taking that file),
    /// and pages within them.
    #[test]
    fn diff_pages_by_limit_and_resumes_after_the_cursor() {
        let (_dir, _root, ops) = new_store();
        let c1 = commit(&ops, None, vec![text("keep", "k\n")]);
        let added = ["z", "f/g/h", "e", "b/d", "b/c", "b-x", "a"];
        let c2 = commit(
            &ops,
            Some(&c1),
            added.iter().map(|path| text(path, path)).collect(),
        );
        let page = |format, limit: usize, cursor: Option<&str>, paths: &[&str]| {
            let mut req = diff_req(&c1, &c2, format);
            req.limit = limit;
            req.cursor = cursor.map(str::to_owned);
            req.paths = paths.iter().map(|path| (*path).to_owned()).collect();
            ops.diff(req).unwrap()
        };
        let whole = page(DiffFormat::Names, 500, None, &[]);
        assert_eq!(
            paths_of(&whole),
            ["a", "b-x", "b/c", "b/d", "e", "f/g/h", "z"]
        );
        assert!(!whole.truncated && whole.cursor.is_none(), "{whole:?}");
        for format in [DiffFormat::Names, DiffFormat::Stat, DiffFormat::Unified] {
            let (mut cursor, mut pages, mut files) = (None, Vec::new(), Vec::new());
            // A page per file at most: a diff that pages on past its end fails, never hangs.
            for _ in 0..added.len() {
                let next = page(format, 3, cursor.as_deref(), &[]);
                pages.push(paths_of(&next));
                let last = next.files.last().map(|file| file.path.clone());
                files.extend(next.files);
                if !next.truncated {
                    assert_eq!(next.cursor, None, "{format:?}");
                    break;
                }
                assert_eq!(
                    next.cursor, last,
                    "{format:?}: the cursor is the page's last path"
                );
                cursor = next.cursor;
            }
            assert_eq!(
                pages,
                [
                    vec!["a", "b-x", "b/c"],
                    vec!["b/d", "e", "f/g/h"],
                    vec!["z"]
                ],
                "{format:?}"
            );
            assert_eq!(
                files,
                page(format, 500, None, &[]).files,
                "{format:?}: the pages make the whole diff"
            );
        }
        let after = |cursor: &str| paths_of(&page(DiffFormat::Names, 500, Some(cursor), &[]));
        assert_eq!(after("c"), ["e", "f/g/h", "z"]);
        let past = page(DiffFormat::Names, 500, Some("zz"), &[]);
        assert!(past.files.is_empty() && !past.truncated && past.cursor.is_none());

        let within = |paths: &[&str]| paths_of(&page(DiffFormat::Names, 500, None, paths));
        assert_eq!(within(&["b/"]), ["b/c", "b/d"]);
        assert_eq!(within(&["z", "b"]), ["b/c", "b/d", "z"]);
        assert_eq!(within(&["f/g/h"]), ["f/g/h"]);
        assert_eq!(within(&["nope"]), Vec::<String>::new());
        let first = page(DiffFormat::Names, 1, None, &["b", "z"]);
        assert_eq!(paths_of(&first), ["b/c"]);
        assert_eq!(first.cursor.as_deref(), Some("b/c"));
        let rest = page(DiffFormat::Names, 2, Some("b/c"), &["b", "z"]);
        assert_eq!(
            (paths_of(&rest), rest.truncated),
            (vec!["b/d".to_owned(), "z".to_owned()], false)
        );
    }

    /// `big.txt` before (`long` false) or after: 16 blocks, each a line that changes (short,
    /// then 4 KiB long) and ten that do not, so that each change is a hunk of its own.
    fn big(long: bool) -> String {
        let mut text = String::new();
        for n in 0..16 {
            if long {
                writeln!(text, "{n:02}{}", "y".repeat(4_093)).unwrap();
            } else {
                writeln!(text, "short {n:02}").unwrap();
            }
            for k in 0..10 {
                writeln!(text, "keep {n:02}.{k}").unwrap();
            }
        }
        text
    }

    /// The 4 KiB of file `f<i>`: four lines of 1 KiB.
    fn four_kib(i: usize) -> String {
        let mut text = String::new();
        for k in 0..4 {
            writeln!(text, "{i:02}-{k}{}", "x".repeat(1_019)).unwrap();
        }
        text
    }

    /// A `unified` page's files take at most `max_bytes` bytes as JSON: of 20 added files of
    /// 4 KiB each, a 10 KiB page holds 2 and is truncated, where a third would not have fitted.
    /// A page's first file always comes: one with 64 KiB of changes keeps the hunks that fit, in
    /// order, marked `hunks_truncated`, and still counts every line it changes; with 1 byte, the
    /// first file comes with no hunk at all. `names` and `stat` pages are held by `limit` alone.
    #[test]
    fn a_unified_page_stops_at_max_bytes() {
        let (_dir, _root, ops) = new_store();
        let c1 = commit(&ops, None, vec![text("big.txt", &big(false))]);
        let mut changes = vec![text("big.txt", &big(true))];
        changes.extend((0..20).map(|i| text(&format!("f{i:02}"), &four_kib(i))));
        let c2 = commit(&ops, Some(&c1), changes);
        let unified = |max_bytes: u64, cursor: Option<&str>| {
            let mut req = diff_req(&c1, &c2, DiffFormat::Unified);
            req.max_bytes = max_bytes;
            req.cursor = cursor.map(str::to_owned);
            ops.diff(req).unwrap()
        };
        let mut req = diff_req(&c1, &c2, DiffFormat::Unified);
        req.paths = vec!["big.txt".to_owned()];
        req.max_bytes = u64::MAX;
        let whole = ops.diff(req).unwrap();
        let all_hunks = whole.files[0].hunks.clone().unwrap();
        assert_eq!(
            (all_hunks.len(), whole.files[0].hunks_truncated),
            (16, None)
        );

        // big.txt alone passes 10 KiB: it keeps the first hunks that fit, and the page ends.
        let first = unified(10_240, None);
        let [cut] = &first.files[..] else {
            panic!("one file: {:?}", paths_of(&first));
        };
        assert_eq!(
            (
                cut.path.as_str(),
                cut.hunks_truncated,
                cut.insertions,
                cut.deletions
            ),
            ("big.txt", Some(true), Some(16), Some(16))
        );
        assert_eq!(cut.hunks.as_deref(), Some(&all_hunks[..2]));
        assert!(json_len(&first.files) <= 10_240);
        let mut one_more = cut.clone();
        one_more.hunks.as_mut().unwrap().push(all_hunks[2].clone());
        assert!(json_len(&[one_more]) > 10_240, "a third hunk would not fit");
        assert_eq!(
            (first.truncated, first.cursor.as_deref()),
            (true, Some("big.txt"))
        );

        // Then two of the 4 KiB files to a page, each page within 10 KiB.
        let mut cursor = first.cursor;
        for i in (0..20).step_by(2) {
            let page = unified(10_240, cursor.as_deref());
            assert_eq!(
                paths_of(&page),
                [format!("f{i:02}"), format!("f{:02}", i + 1)]
            );
            assert!(page.files.iter().all(|file| file.hunks_truncated.is_none()));
            assert!(json_len(&page.files) <= 10_240, "f{i:02}");
            if i + 2 < 20 {
                let next = unified(10_240, page.cursor.as_deref());
                let three = [page.files.clone(), vec![next.files[0].clone()]].concat();
                assert!(
                    json_len(&three) > 10_240,
                    "f{i:02}: a third file would not fit"
                );
            }
            assert_eq!(page.truncated, i + 2 < 20);
            cursor = page.cursor;
        }
        assert_eq!(cursor, None);

        // With room for nothing, the first file still comes, without a hunk.
        let tiny = unified(1, None);
        let [only] = &tiny.files[..] else {
            panic!("one file: {:?}", paths_of(&tiny));
        };
        assert_eq!(
            (only.path.as_str(), only.hunks.clone(), only.hunks_truncated),
            ("big.txt", Some(Vec::new()), Some(true))
        );
        assert!(tiny.truncated);
        for format in [DiffFormat::Names, DiffFormat::Stat] {
            let mut req = diff_req(&c1, &c2, format);
            req.max_bytes = 1;
            assert_eq!(ops.diff(req).unwrap().files.len(), 21, "{format:?}");
        }
    }

    /// A `unified` page counts its files' bytes exactly, as the JSON array they print as: two
    /// files that fill it to the byte both come, and with a byte less the second waits for the
    /// next page. It stops at the first file that does not fit, even where a later, smaller one
    /// would: no file is passed over. A first file cut to fit keeps exactly the hunks that do.
    #[test]
    fn a_unified_page_counts_its_bytes_exactly() {
        let (_dir, _root, ops) = new_store();
        let c1 = commit(&ops, None, vec![text("big.txt", &big(false))]);
        let eight_kib = [four_kib(2), four_kib(3)].concat();
        let changes = vec![
            text("big.txt", &big(true)),
            text("x1", &four_kib(1)),
            text("x2", &eight_kib),
            text("x3", "small\n"),
        ];
        let c2 = commit(&ops, Some(&c1), changes);
        let page = |paths: &[&str], max_bytes: usize| {
            let mut req = diff_req(&c1, &c2, DiffFormat::Unified);
            req.paths = paths.iter().map(|path| (*path).to_owned()).collect();
            req.max_bytes = u64::try_from(max_bytes).unwrap();
            ops.diff(req).unwrap()
        };
        // A file's own length as JSON.
        let len = |file: &FileDiff| json_len(std::slice::from_ref(file)) - 2;
        let xs = ["x1", "x2", "x3"];
        let whole = page(&xs, usize::MAX);
        let [x1, x2, x3] = &whole.files[..] else {
            panic!("three files: {:?}", paths_of(&whole));
        };
        assert!(len(x2) > len(x3));
        let both = 2 + len(x1) + 1 + len(x2);
        assert_eq!(paths_of(&page(&xs, both)), ["x1", "x2"]);
        let one_short = page(&xs, both - 1);
        assert_eq!(paths_of(&one_short), ["x1"]);
        assert_eq!(one_short.cursor.as_deref(), Some("x1"));
        let room_for_x3 = page(&xs, 2 + len(x1) + 1 + len(x3));
        assert_eq!(paths_of(&room_for_x3), ["x1"], "x2 is not passed over");

        let all_hunks = page(&["big.txt"], usize::MAX).files[0]
            .hunks
            .clone()
            .unwrap();
        let mut two = page(&["big.txt"], usize::MAX).files[0].clone();
        two.hunks = Some(all_hunks[..2].to_vec());
        two.hunks_truncated = Some(true);
        let kept = |max_bytes| {
            page(&["big.txt"], max_bytes).files[0]
                .hunks
                .clone()
                .unwrap()
        };
        assert_eq!(kept(2 + len(&two)), all_hunks[..2]);
        assert_eq!(kept(2 + len(&two) - 1), all_hunks[..1]);
    }

    /// Binary files get no line counts and no hunks (`binary: true`), in `stat` and in
    /// `unified`, which still gives their kinds and blobs: a NUL in the first 8,000 bytes, or
    /// bytes that are not UTF-8 (Latin-1 text among them), on either side (text that becomes
    /// binary, and binary that becomes text), added or modified.
    /// A NUL past the first 8,000 bytes of UTF-8 is text.
    #[test]
    fn binary_and_non_utf8_files_have_no_hunks() {
        let (_dir, _root, ops) = new_store();
        let bytes = |path: &str, content: &[u8]| {
            put(path, ContentInput::Base64(STANDARD.encode(content)), None)
        };
        let late_nul = |tail: &[u8]| [&[b'x'; 8_000][..], b"\0\n", tail].concat();
        let c1 = commit(
            &ops,
            None,
            vec![
                bytes("becomes_binary.txt", b"text\n"),
                bytes("becomes_text.txt", b"\xff\n"),
                bytes("late_nul.txt", &late_nul(b"one\n")),
                bytes("latin1.txt", b"caf\xe9\n"),
                bytes("nul.bin", b"a\0b\n"),
            ],
        );
        let c2 = commit(
            &ops,
            Some(&c1),
            vec![
                bytes("added.bin", b"\0\x01"),
                bytes("becomes_binary.txt", b"\xff\xfe\n"),
                bytes("becomes_text.txt", b"text\n"),
                bytes("late_nul.txt", &late_nul(b"two\n")),
                bytes("latin1.txt", b"caf\xe9!\n"),
                bytes("nul.bin", b"a\0c\n"),
            ],
        );
        let stat = ops.diff(diff_req(&c1, &c2, DiffFormat::Stat)).unwrap();
        let binary =
            |path: &str, status: &str| json!({"path": path, "status": status, "binary": true});
        assert_eq!(
            serde_json::to_value(&stat.files).unwrap(),
            json!([
                binary("added.bin", "added"),
                binary("becomes_binary.txt", "modified"),
                binary("becomes_text.txt", "modified"),
                {"path": "late_nul.txt", "status": "modified", "binary": false,
                 "insertions": 1, "deletions": 1},
                binary("latin1.txt", "modified"),
                binary("nul.bin", "modified"),
            ])
        );
        let unified = ops.diff(diff_req(&c1, &c2, DiffFormat::Unified)).unwrap();
        assert_eq!(unified.files.len(), 6);
        for file in &unified.files {
            assert!(
                file.new_kind.is_some() && file.new_blob.is_some(),
                "{file:?}"
            );
            if file.path == "late_nul.txt" {
                assert_eq!(file.hunks.as_ref().map(Vec::len), Some(1), "{file:?}");
                continue;
            }
            assert_eq!(
                (file.binary, file.insertions, file.deletions, &file.hunks),
                (Some(true), None, None, &None),
                "{file:?}"
            );
        }
    }

    /// A text file past the line diff's bound gets no line diff: `stat` and `unified` say it is
    /// not binary, give no line counts and mark its hunks cut (`hunks_truncated`), `unified` with
    /// no hunk, and still give its kinds and blobs. Here two files of 200,000 lines (past the
    /// bound by their lines alone): one with a single line changed, one with every third line.
    #[test]
    fn a_text_file_past_the_line_diff_bound_has_no_hunks() {
        let (_dir, _root, ops) = new_store();
        let numbered = |edited: fn(usize) -> bool| -> String {
            (0..200_000)
                .map(|n| {
                    if edited(n) {
                        format!("edited {n}\n")
                    } else {
                        format!("line {n}\n")
                    }
                })
                .collect()
        };
        let base = numbered(|_| false);
        let c1 = commit(
            &ops,
            None,
            vec![text("every_third.txt", &base), text("one.txt", &base)],
        );
        let c2 = commit(
            &ops,
            Some(&c1),
            vec![
                text("every_third.txt", &numbered(|n| n % 3 == 0)),
                text("one.txt", &numbered(|n| n == 100_000)),
            ],
        );
        let stat = |paths: &[&str]| {
            let mut req = diff_req(&c1, &c2, DiffFormat::Stat);
            req.paths = paths.iter().map(|&path| path.to_owned()).collect();
            serde_json::to_value(ops.diff(req).unwrap().files).unwrap()
        };
        let cut = |path: &str| {
            json!({
                "path": path, "status": "modified", "binary": false, "hunks_truncated": true,
            })
        };
        assert_eq!(stat(&["one.txt"]), json!([cut("one.txt")]));
        assert_eq!(stat(&[]), json!([cut("every_third.txt"), cut("one.txt")]));
        let unified = ops.diff(diff_req(&c1, &c2, DiffFormat::Unified)).unwrap();
        assert_eq!(paths_of(&unified), ["every_third.txt", "one.txt"]);
        for file in &unified.files {
            assert_eq!(
                (file.binary, file.insertions, file.deletions),
                (Some(false), None, None),
                "{}",
                file.path
            );
            // Lengths, not hunks, which a failure would print whole.
            assert_eq!(
                (file.hunks.as_ref().map(Vec::len), file.hunks_truncated),
                (Some(0), Some(true)),
                "{}",
                file.path
            );
            assert!(
                file.new_kind.is_some() && file.new_blob.is_some() && file.old_blob.is_some(),
                "{}",
                file.path
            );
        }
    }

    /// The line diff's bound is `MAX_DIFF_LINES` lines a side: a text file of that many gets its
    /// line counts, and with one line more, none (`hunks_truncated`).
    #[test]
    fn a_text_file_at_the_line_diff_bound_has_its_line_counts() {
        use swarmops_repo::merge::line_diff::MAX_DIFF_LINES;

        let (_dir, _root, ops) = new_store();
        let numbered = |count: usize, edited: usize| -> String {
            (0..count)
                .map(|n| {
                    if n == edited {
                        "edited\n".to_owned()
                    } else {
                        format!("{n}\n")
                    }
                })
                .collect()
        };
        let (at, past) = (MAX_DIFF_LINES, MAX_DIFF_LINES + 1);
        let c1 = commit(
            &ops,
            None,
            vec![
                text("at.txt", &numbered(at, usize::MAX)),
                text("past.txt", &numbered(past, usize::MAX)),
            ],
        );
        let c2 = commit(
            &ops,
            Some(&c1),
            vec![
                text("at.txt", &numbered(at, 7)),
                text("past.txt", &numbered(past, 7)),
            ],
        );
        let stat = ops.diff(diff_req(&c1, &c2, DiffFormat::Stat)).unwrap();
        assert_eq!(
            serde_json::to_value(&stat.files).unwrap(),
            json!([
                {"path": "at.txt", "status": "modified", "binary": false,
                 "insertions": 1, "deletions": 1},
                {"path": "past.txt", "status": "modified", "binary": false,
                 "hunks_truncated": true},
            ])
        );
    }

    /// A default page stays under the operations spec's 64 KiB target for a response (§9) in
    /// every format: 5,000 changed files with 40-byte paths, each format's first page printed in
    /// the envelope the CLI prints it in.
    #[test]
    fn a_default_diff_page_stays_under_64_kib() {
        let (_dir, root, ops) = new_store();
        drop(ops);
        // Nothing here outlives the test, so the setup's 10,000 blobs skip their syncs.
        let repo = LocalRepo::open_with_fs(&root, Arc::new(NoSyncFs)).unwrap();
        let paths: Vec<String> = (0..5_000)
            .map(|i| format!("src/mod{:02}/{i:04}-{}.txt", i / 100, "a".repeat(21)))
            .collect();
        assert!(paths.iter().all(|path| path.len() == 40));
        let all = |content: &[u8]| paths.iter().map(|path| repo_put(path, content)).collect();
        let c1 = repo
            .commit(request(&[], all(b"one\ntwo\nthree\n"), "c1"))
            .unwrap()
            .id;
        let c2 = repo
            .commit(request(&[c1], all(b"one\n2\nthree\n"), "c2"))
            .unwrap()
            .id;
        drop(repo);
        let ops = LocalOps::open(&root).unwrap();
        let (c1, c2) = (c1.to_string(), c2.to_string());
        for format in [DiffFormat::Names, DiffFormat::Stat, DiffFormat::Unified] {
            let page = ops.diff(diff_req(&c1, &c2, format)).unwrap();
            assert!(page.truncated, "{format:?}");
            let mut meta = Meta::new(
                1_000,
                BTreeMap::from([(c1.clone(), c1.clone()), (c2.clone(), c2.clone())]),
                "trust".to_owned(),
            );
            meta.repo = Some(root.to_string());
            let printed = serde_json::to_string(&Envelope::ok("diff", page, meta)).unwrap();
            assert!(
                printed.len() <= 65_536,
                "{format:?}: {} bytes",
                printed.len()
            );
        }
    }

    /// `diff`'s errors: a revision that names nothing, on either side, is `E_REV_NOT_FOUND`, a
    /// prefix of two changesets `E_AMBIGUOUS_ID`, and a string that is no revision
    /// `E_INVALID_ARG`, as for every operation; and `E_INVALID_ARG` for a `limit` or a
    /// `max_bytes` of 0, a malformed path and a malformed cursor, each named in `details`,
    /// checked before any revision is resolved.
    #[test]
    fn diff_errors_map_to_codes() {
        let (_dir, root, ops) = v1_store();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        let unknown = "0".repeat(64);
        for (from, to) in [
            (unknown.as_str(), c1.as_str()),
            (c1.as_str(), unknown.as_str()),
        ] {
            let err = expect_err(
                ops.diff(diff_req(from, to, DiffFormat::Names)),
                ErrorCode::RevNotFound,
            );
            assert_eq!(err.details["rev"], unknown.as_str());
        }
        let err = expect_err(
            ops.diff(diff_req(&c1, "HEAD", DiffFormat::Names)),
            ErrorCode::InvalidArg,
        );
        assert_eq!(err.details["rev"], "HEAD");
        insert_prefix_twins(&root);
        let err = expect_err(
            ops.diff(diff_req("abababababab", &c1, DiffFormat::Names)),
            ErrorCode::AmbiguousId,
        );
        assert_eq!(err.details["rev"], "abababababab");

        // The request's own rules, checked first: with revisions that name nothing too.
        for (from, to) in [
            (c1.as_str(), c1.as_str()),
            (unknown.as_str(), unknown.as_str()),
        ] {
            let req = || diff_req(from, to, DiffFormat::Unified);
            let mut zero_limit = req();
            zero_limit.limit = 0;
            let err = expect_err(ops.diff(zero_limit), ErrorCode::InvalidArg);
            assert_eq!(err.details["limit"], 0);
            let mut zero_bytes = req();
            zero_bytes.max_bytes = 0;
            let err = expect_err(ops.diff(zero_bytes), ErrorCode::InvalidArg);
            assert_eq!(err.details["max_bytes"], 0);
            for path in ["", "/", "/f", "a//b", "a//"] {
                let paths = vec!["f".to_owned(), path.to_owned()];
                let mut bad_path = req();
                bad_path.paths = paths;
                let err = expect_err(ops.diff(bad_path), ErrorCode::InvalidArg);
                assert_eq!(err.details["path"], path);
            }
            for cursor in ["", "/f", "a//b", "f/"] {
                let mut req = req();
                req.cursor = Some(cursor.to_owned());
                let err = expect_err(ops.diff(req), ErrorCode::InvalidArg);
                assert_eq!(err.details["cursor"], cursor);
            }
        }
    }
}

/// `resolve_rev` (M2 Task 14).
mod resolve_rev {
    use swarmops_api::{ResolveRevReq, ResolvedRev, RevKind};

    use super::*;

    fn resolve(ops: &LocalOps, rev: &str) -> Result<ResolvedRev, OpError> {
        ops.resolve_rev(ResolveRevReq::new(rev))
    }

    fn named(id: &str, kind: RevKind) -> ResolvedRev {
        ResolvedRev::new(id.to_owned(), kind)
    }

    /// `resolve_rev` names the full id of the changeset a revision names, resolved as every
    /// operation resolves one, and its kind: `ref` for exactly a ref's name, `changeset` for a
    /// full id (of either case), a prefix, or a `~N` walk (a ref's name with `~0` too). A revision
    /// that names nothing is `E_REV_NOT_FOUND`, a prefix of two changesets `E_AMBIGUOUS_ID`, and
    /// a string that is no revision `E_INVALID_ARG`, each with the revision in `details.rev`.
    /// Made through a request's `recording`, the revision is in that request's `Resolved`.
    #[test]
    fn resolve_rev_names_the_kind() {
        let (_dir, root, ops) = v1_store();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        let c2 = commit(&ops, Some(&c1), vec![text("f", "2")]);
        ops.ref_set(&agent(), set("main", &c2, None, true)).unwrap();
        ops.ref_set(&agent(), set("release/1.0", &c1, None, true))
            .unwrap();

        assert_eq!(resolve(&ops, "main").unwrap(), named(&c2, RevKind::Ref));
        assert_eq!(
            resolve(&ops, "release/1.0").unwrap(),
            named(&c1, RevKind::Ref)
        );
        for (rev, id) in [
            ("main~1", &c1),
            ("main~0", &c2),
            ("release/1.0~0", &c1),
            (c1.as_str(), &c1),
            (&c2.to_uppercase(), &c2),
            (&c2[..12], &c2),
            (&format!("{}~1", &c2[..20]), &c1),
        ] {
            assert_eq!(
                resolve(&ops, rev).unwrap(),
                named(id, RevKind::Changeset),
                "{rev}"
            );
        }

        let unknown = "0".repeat(64);
        for rev in ["release/2.0", "main~2", unknown.as_str(), &unknown[..12]] {
            let err = expect_err(resolve(&ops, rev), ErrorCode::RevNotFound);
            assert_eq!(err.details["rev"], rev);
        }
        for rev in ["", "HEAD", "main^", "feature/x", &c1[..11]] {
            let err = expect_err(resolve(&ops, rev), ErrorCode::InvalidArg);
            assert_eq!(err.details["rev"], rev);
        }
        insert_prefix_twins(&root);
        let err = expect_err(resolve(&ops, "abababababab"), ErrorCode::AmbiguousId);
        assert_eq!(err.details["rev"], "abababababab");

        let resolved = Resolved::default();
        let request = ops.recording(&resolved);
        let req = ResolveRevReq::new("main~1");
        assert_eq!(
            request.resolve_rev(req).unwrap(),
            named(&c1, RevKind::Changeset)
        );
        assert_eq!(
            resolved.take(),
            BTreeMap::from([("main~1".to_owned(), c1.clone())])
        );
    }
}

/// `<store>/authors.toml` (operations spec §5): the email new changesets are signed with.
mod authors {
    use swarmops_api::LandReq;

    use super::*;

    const NATIVE: &str = "worker-17@agent.swarmops.local";
    const LINKED: &str = "123456+keiailab-bot@users.noreply.github.com";

    fn write_authors(root: &Utf8Path, body: &str) {
        fs::write(root.join("authors.toml"), body).expect("write authors.toml");
    }

    /// An `authors.toml` giving the `agent` family `email`.
    fn file_for_agent(email: &str) -> String {
        format!("version = 1\n\n[[author]]\nfamily = \"agent\"\nemail = \"{email}\"\n")
    }

    /// The author signature `show` reports for `rev`.
    fn email_of(ops: &LocalOps, rev: &str) -> String {
        ops.show(show(rev)).unwrap().signatures.author.email
    }

    #[test]
    fn without_authors_toml_signatures_are_unchanged() {
        let (_dir, _root, ops) = new_store();
        let c = commit(&ops, None, vec![text("f", "1")]);

        assert_eq!(email_of(&ops, &c), NATIVE);
    }

    #[test]
    fn authors_toml_sets_the_email_of_new_commits() {
        let (_dir, root, ops) = new_store();
        write_authors(&root, &file_for_agent(LINKED));
        let c = commit(&ops, None, vec![text("f", "1")]);

        let signatures = ops.show(show(&c)).unwrap().signatures;
        assert_eq!(signatures.author.email, LINKED);
        assert_eq!(signatures.author.name, "agent/worker-17");
        assert_eq!(signatures.committer.email, LINKED);
    }

    #[test]
    fn a_name_in_authors_toml_replaces_the_agent_name() {
        let (_dir, root, ops) = new_store();
        let body = format!("{}name = \"KeiaiLab Bot\"\n", file_for_agent(LINKED));
        write_authors(&root, &body);
        let c = commit(&ops, None, vec![text("f", "1")]);

        let author = ops.show(show(&c)).unwrap().signatures.author;
        assert_eq!(
            (author.name.as_str(), author.email.as_str()),
            ("KeiaiLab Bot", LINKED)
        );
    }

    #[test]
    fn an_entry_for_another_family_changes_nothing() {
        let (_dir, root, ops) = new_store();
        write_authors(
            &root,
            &file_for_agent(LINKED).replace("\"agent\"", "\"claude\""),
        );
        let c = commit(&ops, None, vec![text("f", "1")]);

        assert_eq!(email_of(&ops, &c), NATIVE);
    }

    #[test]
    fn authors_toml_sets_the_lander_of_new_lands() {
        let (_dir, root, ops) = new_store();
        let writer = agent();
        let lander = Identity::parse("agent/lander-1").unwrap();
        let mut req = commit_req(None, vec![text("a", "a\n")]);
        req.advance = Some("main".to_owned());
        let t0 = ops.commit(&writer, req).unwrap().changeset;
        let c1 = commit(&ops, Some(&t0), vec![text("b", "b\n")]);
        write_authors(&root, &file_for_agent(LINKED));

        let req: LandReq = serde_json::from_value(json!({"head": c1})).unwrap();
        let merged = ops.land(&lander, req).unwrap().landed;

        let signatures = ops.show(show(&merged)).unwrap().signatures;
        assert_eq!(signatures.committer.email, LINKED);
        assert_eq!(signatures.committer.name, "agent/lander-1");
        // The author is the change's, signed before the file existed.
        assert_eq!(signatures.author.email, NATIVE);
    }

    #[test]
    fn old_changesets_export_as_before() {
        let (_dir, root, ops) = new_store();
        let old = commit(&ops, None, vec![text("f", "1")]);
        write_authors(&root, &file_for_agent(LINKED));
        let new = commit(&ops, Some(&old), vec![text("f", "2")]);

        // The old changeset keeps its id and its signature; the new one has the file's.
        assert_eq!(email_of(&ops, &old), NATIVE);
        assert_eq!(email_of(&ops, &new), LINKED);
        assert_ne!(old, new);
    }

    #[test]
    fn authors_toml_with_an_angle_bracket_is_store_corrupt() {
        for bad in [
            "a<b@x.org",
            "a>b@x.org",
            "a\\nb@x.org",
            "a\\u0000b@x.org",
            "",
        ] {
            let (_dir, root, ops) = new_store();
            write_authors(&root, &file_for_agent(bad));
            let err = expect_err(
                ops.commit(&agent(), commit_req(None, vec![text("f", "1")])),
                ErrorCode::StoreCorrupt,
            );
            assert_eq!(err.details["store_file"], "authors.toml", "{bad:?}");
        }
    }

    #[test]
    fn a_file_that_does_not_parse_is_store_corrupt() {
        for body in ["not toml [", "version = 2\n", "version = 1\nextra = 1\n"] {
            let (_dir, root, ops) = new_store();
            write_authors(&root, body);
            let err = expect_err(
                ops.commit(&agent(), commit_req(None, vec![text("f", "1")])),
                ErrorCode::StoreCorrupt,
            );
            assert_eq!(err.details["store_file"], "authors.toml", "{body:?}");
        }
    }

    #[test]
    fn two_entries_for_one_family_is_store_corrupt() {
        let (_dir, root, ops) = new_store();
        let one = file_for_agent(LINKED);
        let two = one.replace("version = 1\n", "");
        write_authors(&root, &format!("{one}{two}"));

        let err = expect_err(
            ops.commit(&agent(), commit_req(None, vec![text("f", "1")])),
            ErrorCode::StoreCorrupt,
        );
        assert_eq!(err.details["store_file"], "authors.toml");
    }

    #[test]
    fn authors_toml_edited_between_commits_applies_to_the_next() {
        let (_dir, root, ops) = new_store();
        let c1 = commit(&ops, None, vec![text("f", "1")]);

        write_authors(&root, &file_for_agent(LINKED));
        let c2 = commit(&ops, Some(&c1), vec![text("f", "2")]);

        write_authors(&root, &file_for_agent("second@x.org"));
        let c3 = commit(&ops, Some(&c2), vec![text("f", "3")]);

        fs::remove_file(root.join("authors.toml")).unwrap();
        let c4 = commit(&ops, Some(&c3), vec![text("f", "4")]);

        let emails: Vec<String> = [&c1, &c2, &c3, &c4].map(|c| email_of(&ops, c)).into();
        assert_eq!(emails, [NATIVE, LINKED, "second@x.org", NATIVE]);
    }
}

/// `main` as `ref list` reports it: pointing at `target`, at `version`.
fn main_ref(target: String, version: u64) -> RefOut {
    let mut main = RefOut::new("main", target, version);
    main.kind = Some(swarmops_api::RefKind::Main);
    main
}

/// The store formats (the M6 design §2.5, §4): what a FORMAT 1 store does with the names only
/// FORMAT 2 takes, the kinds `ref list` shows, and the format `init` makes.
mod formats {
    use super::*;
    use swarmops_api::RefKind;

    /// A branch or a tag on a FORMAT 1 store is `E_FORMAT_REQUIRED`, naming the format that
    /// takes it and the store's, with the upgrade as the hint; a name no format takes is
    /// `E_REF_NAME_INVALID` as before. Nothing moves.
    #[test]
    fn v2_names_on_a_v1_store_are_format_required() {
        let (_dir, _root, ops) = v1_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);

        for name in ["feature/x", "tags/v1"] {
            let err = expect_err(
                ops.ref_set(&who, set(name, &c1, None, true)),
                ErrorCode::FormatRequired,
            );
            assert_eq!(
                Value::Object(err.details.clone()),
                json!({"name": name, "needs": 2, "store": 1}),
                "{name}"
            );
            assert_eq!(err.hint.as_deref(), Some("swarmops upgrade"), "{name}");
        }
        expect_err(
            ops.ref_set(&who, set("main/x", &c1, None, true)),
            ErrorCode::RefNameInvalid,
        );
        assert_eq!(ops.ref_list(RefListReq::default()).unwrap().refs, []);
    }

    /// A remote-tracking ref is refused to `ref set`, whatever the store's format: only a fetch
    /// moves one (`reason: "reserved"`).
    #[test]
    fn remote_tracking_refs_are_reserved() {
        let (_dir, _root, ops) = new_store();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        let err = expect_err(
            ops.ref_set(&agent(), set("remote/origin/main", &c1, None, true)),
            ErrorCode::RefNameInvalid,
        );
        assert_eq!(err.details["reason"], json!("reserved"));
    }

    /// `ref list --prefix` lists the refs whose names start with it, each with its kind.
    #[test]
    fn ref_list_prefix_filters_and_kinds_show() {
        let (_dir, _root, ops) = new_store();
        let who = agent();
        let c1 = commit(&ops, None, vec![text("f", "1")]);
        for name in ["main", "release/1.0", "release/2.0"] {
            ops.ref_set(&who, set(name, &c1, None, true)).unwrap();
        }
        let listed = |prefix: Option<&str>| -> Vec<(String, Option<RefKind>)> {
            let req = RefListReq::new(prefix.map(str::to_owned));
            (ops.ref_list(req).unwrap().refs.into_iter())
                .map(|out| (out.name, out.kind))
                .collect()
        };

        let release = Some(RefKind::Release);
        assert_eq!(
            listed(Some("release/")),
            [
                ("release/1.0".to_owned(), release),
                ("release/2.0".to_owned(), release)
            ]
        );
        assert_eq!(listed(Some("tags/")), []);
        let all = listed(None);
        assert_eq!(all.len(), 3, "{all:?}");
        assert_eq!(all[0], ("main".to_owned(), Some(RefKind::Main)));
        assert_eq!(listed(Some("")), all);
    }

    /// `info` reports the format the store has, which `init` chose.
    #[test]
    fn info_reports_the_store_version() {
        let dir = tempfile::tempdir().unwrap();
        let root = utf8(dir.path().join("store"));
        let (ops, info) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
        assert_eq!(info.store_version, 1);
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().store_version, 1);
        let format = fs::read(root.join("FORMAT")).unwrap();
        assert_eq!(format, b"swarmops-store 1\n");
    }

    /// A format this build does not create is `E_INVALID_ARG`, naming the ones it does (1 and
    /// 2), and nothing is made; `init` without a format makes FORMAT 2.
    #[test]
    fn init_makes_format_2_by_default_and_refuses_others() {
        let dir = tempfile::tempdir().unwrap();
        let root = utf8(dir.path().join("store"));
        for format in [3, 0] {
            let err = expect_err(
                LocalOps::init_as(&root, "acme", Some(format)),
                ErrorCode::InvalidArg,
            );
            assert_eq!(
                Value::Object(err.details.clone()),
                json!({"format": format, "formats": [1, 2]}),
                "{format}"
            );
            assert!(!fs::exists(&root).unwrap(), "{format}: nothing was made");
        }

        // No format named: this build's default.
        let (ops, info) = LocalOps::init(&root, "acme").unwrap();
        assert_eq!(info.store_version, 2);
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().store_version, 2);
        let format = fs::read(root.join("FORMAT")).unwrap();
        assert_eq!(format, b"swarmops-store 2\n");
    }
}
