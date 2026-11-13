//! `swarmops serve --stdio '<store>'` (the native sync design §2): the upstream's side of native
//! sync, asked for through the key's forced command, answering the wire's request lines on stdin
//! with envelope lines on stdout. Each test writes a session's requests at once, closes stdin and
//! reads the answers; objects come from a second store, as a host's would, in the chunks the wire
//! carries them in.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use swarmops_api::{
    API_VERSION, ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, MAX_WIRE_LINE,
    RefListReq, Rev, ShowReq, StoreOps as _, WIRE_PROTOCOL,
};
use swarmops_core::{Changeset, DeltaOp, ObjectId, ObjectKind};
use swarmops_ops::LocalOps;
use swarmops_ops::serve::receive::MAX_PUSH_BYTES;
use swarmops_repo::LocalRepo;
use tempfile::TempDir;

use crate::support;

/// What OpenSSH names the client's command in.
const SSH_ORIGINAL_COMMAND: &str = "SSH_ORIGINAL_COMMAND";

/// The store every test serves.
const STORE: &str = "acme";

/// The agent that writes the source store and, through the wire, the served one.
const AGENT: &str = "agent/worker-1";

/// A caller who may read the served store and nothing more.
const READER: &str = "person/reader";

/// A caller who may push branches, and land nowhere.
const PUSHER: &str = "person/pusher";

/// A caller `access.toml` names and grants nothing.
const NOBODY: &str = "person/nobody";

/// The served store's grants.
const ACCESS: &str = r#"version = 1

[[grant]]
who = ["agent/*"]
push = ["**"]
force = ["feature/**"]
land = ["main"]

[[grant]]
who = ["person/reader"]
read = true

[[grant]]
who = ["person/pusher"]
push = ["feature/**"]

[[grant]]
who = ["person/nobody"]
"#;

/// [`ACCESS`] with [`PUSHER`] granted land on `main` too.
const ACCESS_PUSHER_LANDS: &str = r#"version = 1

[[grant]]
who = ["agent/*"]
push = ["**"]
land = ["main"]

[[grant]]
who = ["person/pusher"]
push = ["feature/**"]
land = ["main"]
"#;

/// A served root with one store, and a source store a host would send from.
struct Site {
    dir: TempDir,
    source: LocalOps,
}

impl Site {
    /// The root with [`STORE`], of `format` (`None`: the default), granted by [`ACCESS`], and
    /// an empty source store.
    fn new(format: Option<&str>) -> Site {
        let dir = TempDir::new().unwrap();
        let store = dir.path().join("root").join(STORE);
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        let path = store.to_str().unwrap();
        match format {
            Some(format) => support::run(&["init", "--format", format, path], &[]).ok(),
            None => support::run(&["init", path], &[]).ok(),
        };
        std::fs::write(store.join("access.toml"), ACCESS).unwrap();
        let source = utf8(dir.path().join("source"));
        let (source, _) = LocalOps::init(&source, "source").unwrap();
        Site { dir, source }
    }

    /// The served root.
    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    /// The served store's operations.
    fn served(&self) -> LocalOps {
        LocalOps::open(&utf8(self.root().join(STORE))).unwrap()
    }

    /// Commits a put of `path` holding `content` to the source store, on `parent`, advancing
    /// `advance`; the changeset's id.
    fn commit(
        &self,
        parent: Option<&str>,
        path: &str,
        content: &str,
        advance: Option<&str>,
    ) -> String {
        let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
        change.content = Some(ContentInput::Inline(content.to_owned()));
        let mut req = CommitReq::new(vec![change], format!("{path}: {content}"));
        req.parent = parent.map(Rev::from);
        req.advance = advance.map(str::to_owned);
        let who = Identity::parse(AGENT).unwrap();
        self.source.commit(&who, req).unwrap().changeset
    }

    /// The put of the source's changesets `ids`, parents first, with the blobs their deltas
    /// put: one chunk, and the request line that announces it and adopts them all.
    fn put(&self, ids: &[&str]) -> Vec<u8> {
        let repo = LocalRepo::open(&utf8(self.dir.path().join("source"))).unwrap();
        let object = |id: ObjectId| repo.object(id).unwrap().unwrap();

        // 1. The blobs first, then the changesets.
        let mut records = Vec::new();
        let mut changesets = Vec::new();
        for id in ids {
            let id: ObjectId = id.parse().unwrap();
            let (_, bytes) = object(id);
            for entry in Changeset::decode(&bytes).unwrap().delta {
                if let DeltaOp::Put { id: blob, .. } = entry.op {
                    records.push((ObjectKind::Blob, blob.0, object(blob.0).1));
                }
            }
            changesets.push((ObjectKind::Changeset, id, bytes));
        }
        records.extend(changesets);

        // 2. The line, then the chunk.
        let chunk = chunk(&records);
        let req = json!({"chunks": 1, "bytes": chunk.len(), "adopt": ids});
        let mut out = line("wire_put", &req);
        out.extend(chunk);
        out
    }

    /// The answers of one session as `agent` on store `store` to `input`, and its exit code.
    fn session_on(&self, store: &str, agent: &str, input: Vec<u8>) -> (Vec<Value>, i32, String) {
        let root = self.root();
        let command = format!("swarmops serve --stdio '{store}'");
        let args = ["serve", "--root", root.to_str().unwrap(), "--as", agent];
        let mut child = support::swarmops(&args, &[(SSH_ORIGINAL_COMMAND, &command)])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            // A session that ends early stops reading: what is left is not its to read.
            let _ = stdin.write_all(&input);
        });
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let exit = output.status.code().unwrap_or(-1);
        (envelopes(&output.stdout), exit, stdout)
    }

    /// The answers of one session as `agent` on [`STORE`] to `input`.
    fn session(&self, agent: &str, input: Vec<u8>) -> Vec<Value> {
        self.session_on(STORE, agent, input).0
    }

    /// The served store's ref `name`'s target, if it exists.
    fn served_ref(&self, name: &str) -> Option<String> {
        let listed = self.served().ref_list(RefListReq::new(None)).unwrap();
        listed
            .refs
            .into_iter()
            .find(|r| r.name == name)
            .map(|r| r.target)
    }
}

/// `path` as a UTF-8 path.
fn utf8(path: PathBuf) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(path).unwrap()
}

/// One request line of `op` with `req`, in this build's API and protocol.
fn line(op: &str, req: &Value) -> Vec<u8> {
    raw(&json!({"op": op, "api": API_VERSION, "protocol": WIRE_PROTOCOL, "req": req}))
}

/// `request` as one line.
fn raw(request: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(request).unwrap();
    out.push(b'\n');
    out
}

/// The requests `lines`, one after the other.
fn lines(lines: &[Vec<u8>]) -> Vec<u8> {
    lines.concat()
}

/// One chunk of `records` as the native sync design §2.2 lays it out.
fn chunk(records: &[(ObjectKind, ObjectId, Vec<u8>)]) -> Vec<u8> {
    let body: Vec<u8> = (records.iter())
        .flat_map(|(kind, id, payload)| {
            let mut record = vec![kind.tag()];
            record.extend(id.0);
            record.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
            record.extend(payload);
            record
        })
        .collect();
    let mut out = b"SWCH\x01\0\0\0".to_vec();
    out.extend(u32::try_from(records.len()).unwrap().to_le_bytes());
    out.extend(u64::try_from(body.len()).unwrap().to_le_bytes());
    out.extend(body);
    out
}

/// The envelopes of `output`, the chunk bytes an answer announces skipped.
fn envelopes(mut output: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(end) = output.iter().position(|&b| b == b'\n') {
        let envelope: Value = serde_json::from_slice(&output[..end]).unwrap();
        let skip = envelope["result"]["bytes"].as_u64().unwrap_or(0);
        output = &output[end + 1 + usize::try_from(skip).unwrap()..];
        out.push(envelope);
    }
    out
}

/// Asserts `envelope` is a refusal with `code` and `reason`.
fn assert_refused(envelope: &Value, code: &str, reason: &str) {
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert_eq!(envelope["error"]["code"], code, "{envelope}");
    if !reason.is_empty() {
        assert_eq!(envelope["error"]["details"]["reason"], reason, "{envelope}");
    }
}

/// `wire_hello` names the caller, the protocol both sides take, the store's format and the kinds
/// of object the wire carries.
#[test]
fn native_hello_names_the_caller_and_the_protocol() {
    let site = Site::new(None);
    let hello = json!({"protocol_min": 1, "protocol_max": 3, "object_kinds": ["blob", "changeset"],
                       "features": []});
    let mut request = json!({"op": "wire_hello", "api": API_VERSION, "protocol": 3, "req": hello});
    let answers = site.session(AGENT, raw(&request));
    let result = &answers[0]["result"];
    assert_eq!(answers[0]["op"], "wire_hello", "{}", answers[0]);
    assert_eq!(result["protocol"], WIRE_PROTOCOL, "{result}");
    assert_eq!(result["identity"], AGENT, "{result}");
    assert!(result["format"].as_u64().unwrap() >= 2, "{result}");
    assert_eq!(
        result["object_kinds"],
        json!(["blob", "changeset"]),
        "{result}"
    );

    // A reader is named as itself.
    request["protocol"] = WIRE_PROTOCOL.into();
    let answers = site.session(READER, raw(&request));
    assert_eq!(answers[0]["result"]["identity"], READER, "{}", answers[0]);
}

/// A client whose range starts past this server's protocol is refused, `protocol`, and the
/// session goes on.
#[test]
fn a_newer_protocol_is_refused_by_hello() {
    let site = Site::new(None);
    let hello = json!({"protocol_min": 2, "protocol_max": 3, "object_kinds": [], "features": []});
    let request = json!({"op": "wire_hello", "api": API_VERSION, "protocol": 3, "req": hello});
    let refs = line("wire_refs", &json!({}));
    let answers = site.session(AGENT, lines(&[raw(&request), refs]));
    assert_refused(&answers[0], "E_INVALID_ARG", "protocol");
    assert_eq!(answers[1]["ok"], true, "{}", answers[1]);
}

/// A request of another protocol than the server's is refused `protocol` before its body is
/// read: a body that is no request of its op is not what the refusal names.
#[test]
fn a_request_of_another_protocol_is_refused_before_its_body() {
    let site = Site::new(None);
    let request = json!({"op": "wire_refs", "api": API_VERSION, "protocol": 2,
                         "req": {"limit": "not a number"}});
    let answers = site.session(AGENT, raw(&request));
    assert_refused(&answers[0], "E_INVALID_ARG", "protocol");
}

/// A caller no grant names is refused before any request is read: one envelope line, the
/// session's, and `E_AUTH`'s exit.
#[test]
fn a_caller_without_read_is_refused_before_any_request() {
    let site = Site::new(None);
    let (answers, exit, _) =
        site.session_on(STORE, "person/stranger", line("wire_refs", &json!({})));
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["op"], "serve", "{}", answers[0]);
    assert_refused(&answers[0], "E_AUTH", "unknown_identity");
    assert_eq!(exit, 5);

    // A caller named and granted nothing reads nothing either.
    let (answers, _, _) = site.session_on(STORE, NOBODY, line("wire_refs", &json!({})));
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_refused(&answers[0], "E_POLICY_DENIED", "not_granted");
}

/// A FORMAT 1 store is refused before any request: `E_FORMAT_REQUIRED`, needs 2.
#[test]
fn a_format_1_store_is_refused_before_any_request() {
    let site = Site::new(Some("1"));
    let (answers, exit, _) = site.session_on(STORE, AGENT, line("wire_refs", &json!({})));
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_refused(&answers[0], "E_FORMAT_REQUIRED", "");
    assert_eq!(answers[0]["error"]["details"]["needs"], 2, "{}", answers[0]);
    assert_ne!(exit, 0);
}

/// `wire_refs` pages by name, and says whether `main` exists: an empty store lists nothing and
/// says so in `has_main`.
#[test]
fn refs_page_and_say_when_main_is_absent() {
    // 1. An empty store.
    let site = Site::new(None);
    let answers = site.session(AGENT, line("wire_refs", &json!({})));
    assert_eq!(answers[0]["result"], json!({"refs": [], "has_main": false}));

    // 2. main and two branches, a page of two, then the rest.
    let a = site.commit(None, "a", "a\n", Some("main"));
    let mut input = site.put(&[&a]);
    for name in ["main", "feature/x", "zeta"] {
        input.extend(line("wire_move", &json!({"name": name, "new": a})));
    }
    input.extend(line("wire_refs", &json!({"limit": 2})));
    input.extend(line("wire_refs", &json!({"limit": 2, "after": "main"})));
    let answers = site.session(AGENT, input);
    let first = &answers[4]["result"];
    let names = |page: &Value| {
        page["refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(first), [json!("feature/x"), json!("main")], "{first}");
    assert_eq!(first["next"], "main", "{first}");
    assert_eq!(first["has_main"], true, "{first}");
    let second = &answers[5]["result"];
    assert_eq!(names(second), [json!("zeta")], "{second}");
    assert!(second.get("next").is_none(), "{second}");
}

/// A put adopts the changesets with their ids, no ref moved; the land of the change lands it on
/// `main` under the change's key, `ss1-` and 60 hex digits.
#[test]
fn put_then_land_lands_with_the_change_key() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "main", "new": a})));
    input.extend(line(
        "wire_land",
        &json!({"target": "main", "base": a, "head": b}),
    ));
    let answers = site.session(AGENT, input);

    // 1. Adopted with their ids, then main created at a.
    assert_eq!(
        answers[0]["result"]["adopted"],
        json!([a, b]),
        "{}",
        answers[0]
    );
    assert_eq!(answers[1]["result"]["new"], a, "{}", answers[1]);

    // 2. Landed, under the change's key.
    let landed = &answers[2]["result"];
    assert_eq!(landed["status"], "landed", "{}", answers[2]);
    let landed = landed["landed"].as_str().unwrap();
    let shown = site.served().show(ShowReq::new(Rev::from(landed))).unwrap();
    let key = shown.land_key.unwrap();
    assert!(key.starts_with("ss1-") && key.len() == 64, "{key}");
    assert!(
        key.bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-' || b == b's'),
        "{key}"
    );
    assert_eq!(site.served_ref("main").as_deref(), Some(landed));
}

/// The same land again, in a later session, replays: nothing is written, the same changeset is
/// answered.
#[test]
fn a_land_again_replays() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let land = line(
        "wire_land",
        &json!({"target": "main", "base": a, "head": b}),
    );
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "main", "new": a})));
    input.extend(land.clone());
    let first = site.session(AGENT, input);
    let again = site.session(AGENT, land);
    assert_eq!(again[0]["result"]["status"], "replayed", "{}", again[0]);
    assert_eq!(again[0]["result"]["landed"], first[2]["result"]["landed"]);
}

/// A change that conflicts with what landed since its base is refused `E_CONFLICT` with the
/// report, and `main` does not move.
#[test]
fn a_land_conflict_returns_the_report_and_moves_nothing() {
    // 1. main at a, which holds x; two changes of x on a.
    let site = Site::new(None);
    let a = site.commit(None, "x", "0\n", Some("main"));
    let one = site.commit(Some(&a), "x", "1\n", None);
    let two = site.commit(Some(&a), "x", "2\n", None);
    let mut input = site.put(&[&a, &one, &two]);
    input.extend(line("wire_move", &json!({"name": "main", "new": a})));
    input.extend(line(
        "wire_land",
        &json!({"target": "main", "base": a, "head": one}),
    ));
    input.extend(line(
        "wire_land",
        &json!({"target": "main", "base": a, "head": two}),
    ));
    let answers = site.session(AGENT, input);

    // 2. The first lands, the second conflicts, main stays at the first.
    let landed = answers[2]["result"]["landed"].clone();
    assert_refused(&answers[3], "E_CONFLICT", "");
    let report = &answers[3]["error"]["details"]["report"];
    assert_eq!(report["files"][0]["path"], "x", "{}", answers[3]);
    assert_eq!(site.served_ref("main").map(Value::from), Some(landed));
}

/// Each land reads `access.toml` again: a caller without `land` is refused and nothing moves;
/// granted land since, the same request lands.
#[test]
fn land_needs_the_land_grant_each_attempt() {
    // 1. main at a, b put by the agent.
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "main", "new": a})));
    site.session(AGENT, input);

    // 2. The pusher may not land.
    let land = line(
        "wire_land",
        &json!({"target": "main", "base": a, "head": b}),
    );
    let answers = site.session(PUSHER, land.clone());
    assert_refused(&answers[0], "E_POLICY_DENIED", "not_granted");
    assert_eq!(site.served_ref("main").as_deref(), Some(a.as_str()));

    // 3. Granted, it lands.
    std::fs::write(
        site.root().join(STORE).join("access.toml"),
        ACCESS_PUSHER_LANDS,
    )
    .unwrap();
    let answers = site.session(PUSHER, land);
    assert_eq!(answers[0]["result"]["status"], "landed", "{}", answers[0]);
}

/// An upload needs `push` or `land` on some ref: a reader's `wire_has` is refused, and its
/// `wire_put` too, before a chunk is read, which ends the session.
#[test]
fn an_upload_needs_push_or_land() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let has = line("wire_has", &json!({"ids": [a]}));

    // 1. A reader: refused, and the put ends the session.
    let mut input = has.clone();
    input.extend(site.put(&[&a]));
    input.extend(has.clone());
    let (answers, exit, _) = site.session_on(STORE, READER, input);
    assert_refused(&answers[0], "E_POLICY_DENIED", "not_granted");
    assert_refused(&answers[1], "E_POLICY_DENIED", "not_granted");
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert_eq!(exit, 5);

    // 2. A pusher of branches alone uploads.
    let mut input = has.clone();
    input.extend(site.put(&[&a]));
    input.extend(has);
    let answers = site.session(PUSHER, input);
    assert_eq!(
        answers[0]["result"]["held"],
        json!([false]),
        "{}",
        answers[0]
    );
    assert_eq!(
        answers[2]["result"]["held"],
        json!([true]),
        "{}",
        answers[2]
    );
}

/// A put whose bytes miss their id writes nothing: refused `object_id`, the session ends, and
/// the store holds no object of it.
#[test]
fn a_put_with_a_bad_object_writes_nothing() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let id: ObjectId = a.parse().unwrap();
    let forged = chunk(&[(ObjectKind::Changeset, id, b"not the changeset".to_vec())]);
    let req = json!({"chunks": 1, "bytes": forged.len(), "adopt": [a]});
    let mut input = line("wire_put", &req);
    input.extend(forged);
    input.extend(line("wire_refs", &json!({})));
    let (answers, exit, _) = site.session_on(STORE, AGENT, input);
    assert_refused(&answers[0], "E_INVALID_ARG", "object_id");
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(exit, 2);
    let repo = LocalRepo::open(&utf8(site.root().join(STORE))).unwrap();
    assert!(!repo.contains_object(id).unwrap());
}

/// A put that announces more than [`MAX_PUSH_BYTES`] is refused before a byte of it is read.
#[test]
fn a_put_past_max_push_bytes_is_refused_before_reading() {
    let site = Site::new(None);
    let req = json!({"chunks": 1, "bytes": MAX_PUSH_BYTES + 1, "adopt": []});
    let (answers, exit, _) = site.session_on(STORE, AGENT, line("wire_put", &req));
    assert_refused(&answers[0], "E_INVALID_ARG", "too_large");
    assert_eq!(
        answers[0]["error"]["details"]["limit"], MAX_PUSH_BYTES,
        "{}",
        answers[0]
    );
    assert_eq!(exit, 2);
}

/// `wire_list` takes only the head of a ref `wire_refs` lists now: another changeset, held or
/// not, is `not_a_ref_head`.
#[test]
fn a_list_of_a_want_that_is_no_ref_head_is_refused() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "main", "new": b})));
    input.extend(line("wire_list", &json!({"want": a})));
    input.extend(line("wire_list", &json!({"want": b})));
    let answers = site.session(AGENT, input);
    assert_refused(&answers[2], "E_REV_NOT_FOUND", "not_a_ref_head");
    assert_eq!(
        answers[3]["result"]["changesets"],
        json!([b, a]),
        "{}",
        answers[3]
    );
}

/// A move creates a branch; a tag is created and never moved (`E_TAG_IMMUTABLE`).
#[test]
fn move_creates_a_branch_and_refuses_a_tag_move() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "feature/x", "new": a})));
    input.extend(line(
        "wire_move",
        &json!({"name": "feature/x", "old": a, "new": b}),
    ));
    input.extend(line("wire_move", &json!({"name": "tags/v1", "new": a})));
    input.extend(line(
        "wire_move",
        &json!({"name": "tags/v1", "old": a, "new": b}),
    ));
    let answers = site.session(AGENT, input);
    assert_eq!(answers[1]["result"], json!({"name": "feature/x", "new": a}));
    assert_eq!(answers[2]["result"]["new"], b, "{}", answers[2]);
    assert_eq!(answers[3]["result"]["new"], a, "{}", answers[3]);
    assert_refused(&answers[4], "E_TAG_IMMUTABLE", "");
    assert_eq!(site.served_ref("tags/v1").as_deref(), Some(a.as_str()));
}

/// `main` moves only forward along first parents: back is `E_REF_PROTECTED`, and it stays.
#[test]
fn move_of_main_backwards_is_refused() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "main", "new": b})));
    input.extend(line(
        "wire_move",
        &json!({"name": "main", "old": b, "new": a}),
    ));
    let answers = site.session(AGENT, input);
    assert_refused(&answers[2], "E_REF_PROTECTED", "");
    assert_eq!(site.served_ref("main").as_deref(), Some(b.as_str()));
}

/// `main` is never forced or deleted through the wire, whatever the grants.
#[test]
fn main_is_never_forced_or_deleted_through_the_wire() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let mut input = site.put(&[&a, &b]);
    input.extend(line("wire_move", &json!({"name": "main", "new": b})));
    input.extend(line(
        "wire_move",
        &json!({"name": "main", "old": b, "new": a, "mode": "force"}),
    ));
    input.extend(line("wire_move", &json!({"name": "main", "old": b})));
    let answers = site.session(AGENT, input);
    assert_refused(&answers[2], "E_REF_PROTECTED", "");
    assert_refused(&answers[3], "E_REF_PROTECTED", "");
    assert_eq!(site.served_ref("main").as_deref(), Some(b.as_str()));
}

/// A move to a changeset the store does not hold is refused, and nothing moves.
#[test]
fn move_to_a_changeset_the_store_lacks_is_refused() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let answers = site.session(
        AGENT,
        line("wire_move", &json!({"name": "feature/x", "new": a})),
    );
    assert_refused(&answers[0], "E_REV_NOT_FOUND", "not_held");
    assert_eq!(site.served_ref("feature/x"), None);
}

/// A move whose `old` is not what the ref holds is `E_CAS_MISMATCH`, and nothing moves.
#[test]
fn move_with_a_stale_old_is_refused_and_moves_nothing() {
    let site = Site::new(None);
    let a = site.commit(None, "a", "a\n", Some("main"));
    let b = site.commit(Some(&a), "b", "b\n", None);
    let c = site.commit(Some(&a), "c", "c\n", None);
    let mut input = site.put(&[&a, &b, &c]);
    input.extend(line("wire_move", &json!({"name": "feature/x", "new": b})));
    input.extend(line(
        "wire_move",
        &json!({"name": "feature/x", "old": a, "new": c, "mode": "force"}),
    ));
    input.extend(line(
        "wire_move",
        &json!({"name": "feature/y", "old": a, "new": c}),
    ));
    let answers = site.session(AGENT, input);
    assert_refused(&answers[2], "E_CAS_MISMATCH", "");
    assert_refused(&answers[3], "E_CAS_MISMATCH", "");
    assert_eq!(site.served_ref("feature/x").as_deref(), Some(b.as_str()));
    assert_eq!(site.served_ref("feature/y"), None);
}

/// A request line past `MAX_WIRE_LINE` bytes is refused, and the session ends there.
#[test]
fn a_request_line_past_its_bound_is_refused() {
    let site = Site::new(None);
    let mut input = vec![b' '; MAX_WIRE_LINE + 1];
    input.push(b'\n');
    input.extend(line("wire_refs", &json!({})));
    let (answers, exit, _) = site.session_on(STORE, AGENT, input);
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_refused(&answers[0], "E_INVALID_ARG", "line_too_long");
    assert_eq!(exit, 2);
}

/// An op this server does not answer, `wire_facts` among them, is `unknown_op`; the session goes
/// on.
#[test]
fn an_unknown_op_is_refused() {
    let site = Site::new(None);
    let input = lines(&[
        line("wire_facts", &json!({})),
        line("wire_nothing", &json!({})),
        line("wire_refs", &json!({})),
    ]);
    let answers = site.session(AGENT, input);
    assert_refused(&answers[0], "E_INVALID_ARG", "unknown_op");
    assert_refused(&answers[1], "E_INVALID_ARG", "unknown_op");
    assert_eq!(answers[2]["ok"], true, "{}", answers[2]);
}

/// No answer names a host path: a store that is not there, a refused caller, a refused request.
#[test]
fn no_answer_names_a_host_path() {
    let site = Site::new(None);
    let root = site.dir.path().to_str().unwrap().to_owned();
    let refs = line("wire_refs", &json!({}));
    let runs = [
        site.session_on("missing", AGENT, refs.clone()),
        site.session_on(STORE, "person/stranger", refs.clone()),
        site.session_on(STORE, AGENT, line("wire_get", &json!({"under": "main"}))),
        site.session_on(
            STORE,
            AGENT,
            line(
                "wire_move",
                &json!({"name": "main", "old": "ab".repeat(32)}),
            ),
        ),
    ];
    for (answers, _, stdout) in runs {
        assert_eq!(answers[0]["ok"], false, "{stdout}");
        assert!(!stdout.contains(&root), "{stdout}");
    }
}
