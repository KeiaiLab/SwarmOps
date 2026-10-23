//! M4's agent-facing contract, merging and landing: `land` and the conflict report it returns,
//! the operation log and `undo`, commit keys, and the five error codes they raise, in the shapes
//! the operations spec gives them (§0, §1, §3).

use std::collections::BTreeMap;
use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use swarmops_api::{
    Attribution, ChangesetDetail, CommitReq, CommitResp, ConflictKindOut, ConflictReport,
    DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_CHANGESETS, DEFAULT_MAX_CONFLICT_BYTES, DEFAULT_OP_LOG_LIMIT,
    ErrorCode, FileConflictOut, HunkOut, LandReq, LandResp, LandStatus, LinesOut,
    MAX_LAND_ATTEMPTS, OpChangesetOut, OpError, OpLogPage, OpLogReq, OperationKind, OperationOut,
    RefMoveOut, Rev, SignatureOut, Signatures, UndoReq, UndoResp, schema_for_all,
};

/// `value` serialized and read back: it must read as itself.
#[track_caller]
fn round_trip<T: serde::Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> String {
    let text = serde_json::to_string(value).unwrap();
    assert_eq!(&serde_json::from_str::<T>(&text).unwrap(), value, "{text}");
    text
}

/// The five codes M4 adds, each with its string, exit code and retry advice: `E_CONFLICT` takes
/// exit 3, the conflict exit, which had no code, and is not retryable (the agent resolves, then
/// lands again); a land that ran out of attempts can succeed when it is run again; the other
/// three are an error twice and a not-found. Each reaches `OpError` as it is, and `ErrorCode::ALL`
/// lists it once.
#[test]
fn the_m4_codes_have_their_exits() {
    for (code, name, exit, retryable) in [
        (ErrorCode::Conflict, "E_CONFLICT", 3, false),
        (
            ErrorCode::CasRetryExhausted,
            "E_CAS_RETRY_EXHAUSTED",
            4,
            true,
        ),
        (ErrorCode::NothingToLand, "E_NOTHING_TO_LAND", 1, false),
        (ErrorCode::NothingToUndo, "E_NOTHING_TO_UNDO", 1, false),
        (ErrorCode::OpNotFound, "E_OP_NOT_FOUND", 6, false),
    ] {
        assert_eq!(
            (code.as_str(), code.exit(), code.retryable()),
            (name, exit, retryable),
            "{code:?}"
        );
        assert_eq!(serde_json::to_value(code).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<ErrorCode>(json!(name)).unwrap(),
            code
        );
        let error = OpError::new(code, "m");
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            (name, exit, retryable)
        );
        let listed = ErrorCode::ALL.iter().filter(|&&listed| listed == code);
        assert_eq!(listed.count(), 1, "{name}");
    }
}

/// A land names only its `head`: it lands on `main`, tries to publish at most 8 times and keeps
/// a conflict report's texts within 32 KiB, and the options are absent. Written back, the
/// absent options are omitted. The schema says the defaults, and that `max_attempts` is 1 to 32.
#[test]
fn a_land_request_takes_its_defaults() {
    assert_eq!(
        (
            DEFAULT_MAX_ATTEMPTS,
            MAX_LAND_ATTEMPTS,
            DEFAULT_MAX_CONFLICT_BYTES
        ),
        (8, 32, 32_768)
    );
    let req: LandReq = serde_json::from_value(json!({"head": "x"})).unwrap();
    let mut expected = LandReq::new("x");
    expected.target = "main".to_owned();
    expected.max_attempts = 8;
    expected.max_conflict_bytes = 32_768;
    assert_eq!(req, expected);
    assert_eq!(
        serde_json::to_string(&req).unwrap(),
        r#"{"head":"x","target":"main","max_attempts":8,"max_conflict_bytes":32768}"#
    );
    let mut full = req;
    full.base = Some(Rev::from("b1"));
    full.message = Some("land it".to_owned());
    full.idempotency_key = Some("0192f0c4-7a1b-7c3d-8e4f-5a6b7c8d9e0f".to_owned());
    assert_eq!(
        round_trip(&full),
        concat!(
            r#"{"head":"x","base":"b1","target":"main","message":"land it","#,
            r#""idempotency_key":"0192f0c4-7a1b-7c3d-8e4f-5a6b7c8d9e0f","#,
            r#""max_attempts":8,"max_conflict_bytes":32768}"#
        )
    );

    let schema = &schema_for_all()["LandReq"];
    assert_eq!(schema["required"], json!(["head"]));
    let properties = &schema["properties"];
    assert_eq!(properties["target"]["default"], "main");
    let attempts = &properties["max_attempts"];
    assert_eq!(
        (
            &attempts["default"],
            &attempts["minimum"],
            &attempts["maximum"]
        ),
        (&json!(8), &json!(1), &json!(32))
    );
    assert_eq!(properties["max_conflict_bytes"]["default"], 32_768);
}

/// A land's resolutions (operations spec §3, resolution path (b)): `against`, and by path one of
/// `take` (`ours` or `theirs`), `content` with an optional `mode`, and `delete`, each written
/// only when given (`delete` only when true); a land without them omits `resolutions`. A
/// conflict report names its stale paths in `stale`, omitted when there is none. The schema lists
/// the three new types, and a resolution's description says the rule its type cannot, exactly
/// one of the three, which keeps its schema a plain object.
#[test]
fn a_land_request_carries_its_resolutions() {
    use swarmops_api::{ContentInput, ModeInput, ResolutionInput, ResolutionsInput, Side};

    let text = concat!(
        r#"{"head":"h7","target":"main","max_attempts":8,"max_conflict_bytes":32768,"#,
        r#""resolutions":{"against":"m9","files":{"a":{"take":"ours"},"b":{"take":"theirs"},"#,
        r#""c":{"content":{"inline":"c\n"},"mode":"exec"},"d":{"delete":true}}}}"#
    );
    let req: LandReq = serde_json::from_str(text).unwrap();
    let only = |take, content, mode, delete| {
        let mut resolution = ResolutionInput::default();
        resolution.take = take;
        resolution.content = content;
        resolution.mode = mode;
        resolution.delete = delete;
        resolution
    };
    let expected = ResolutionsInput::new(
        Rev::from("m9"),
        BTreeMap::from([
            ("a".to_owned(), only(Some(Side::Ours), None, None, false)),
            ("b".to_owned(), only(Some(Side::Theirs), None, None, false)),
            (
                "c".to_owned(),
                only(
                    None,
                    Some(ContentInput::Inline("c\n".to_owned())),
                    Some(ModeInput::Exec),
                    false,
                ),
            ),
            ("d".to_owned(), only(None, None, None, true)),
        ]),
    );
    assert_eq!(req.resolutions.as_ref(), Some(&expected));
    assert_eq!(round_trip(&req), text);
    let plain: LandReq = serde_json::from_value(json!({"head": "h7"})).unwrap();
    assert_eq!(plain.resolutions, None);
    assert!(!round_trip(&plain).contains("resolutions"));

    let all = schema_for_all();
    for name in ["ResolutionsInput", "ResolutionInput", "Side"] {
        assert!(all.get(name).is_some(), "{name} in the schema");
    }
    assert!(all["LandReq"]["properties"].get("resolutions").is_some());
    let resolution = all["ResolutionInput"].as_object().unwrap();
    assert_eq!(resolution["type"], "object");
    assert!(!resolution.contains_key("oneOf") && !resolution.contains_key("anyOf"));
    let description = resolution["description"].as_str().unwrap();
    assert!(description.contains("exactly one"), "{description}");
    assert_eq!(resolution["properties"]["delete"]["type"], "boolean");
    assert_eq!(resolution.get("required"), None, "every field is optional");
    assert!(all["ConflictReport"]["properties"].get("stale").is_some());

    let mut report = ConflictReport::new("b1", "h7", "m9", Vec::new(), Vec::new(), false);
    report.stale = vec!["src/lib.rs".to_owned()];
    assert_eq!(
        round_trip(&report),
        r#"{"base":"b1","ours":"h7","theirs":"m9","files":[],"kinds":[],"stale":["src/lib.rs"],"truncated":false}"#
    );
}

/// An operation log names nothing: the 50 newest operations, at most 20 changesets listed for
/// each. Its `limit` is at least 1, as every page's is. An undo names its operation.
#[test]
fn an_op_log_request_takes_its_defaults() {
    assert_eq!((DEFAULT_OP_LOG_LIMIT, DEFAULT_MAX_CHANGESETS), (50, 20));
    let mut expected = OpLogReq::default();
    expected.limit = 50;
    expected.max_changesets = 20;
    assert_eq!(
        serde_json::from_value::<OpLogReq>(json!({})).unwrap(),
        expected
    );
    let mut page = OpLogReq::default();
    page.before = Some(31);
    page.limit = 5;
    page.max_changesets = 0;
    assert_eq!(
        round_trip(&page),
        r#"{"before":31,"limit":5,"max_changesets":0}"#
    );
    let all = schema_for_all();
    assert_eq!(all["OpLogReq"]["properties"]["limit"]["minimum"], 1);
    assert!(all["OpLogReq"].get("required").is_none());

    assert_eq!(all["UndoReq"]["required"], json!(["op"]));
    assert!(serde_json::from_value::<UndoReq>(json!({})).is_err());
    assert_eq!(round_trip(&UndoReq::new(41)), r#"{"op":41}"#);
}

/// Requests and responses alike read as before with a field they do not know (operations spec
/// §0: no `deny_unknown_fields`).
#[test]
fn the_m4_requests_ignore_fields_they_do_not_know() {
    #[track_caller]
    fn ignores<T: DeserializeOwned + PartialEq + Debug>(minimal: Value) {
        let plain: T = serde_json::from_value(minimal.clone()).unwrap();
        let mut extended = minimal;
        extended["zz_unknown"] = json!(1);
        assert_eq!(serde_json::from_value::<T>(extended).unwrap(), plain);
    }
    ignores::<LandReq>(json!({"head": "x", "base": "b1", "idempotency_key": "k-1"}));
    ignores::<OpLogReq>(json!({"before": 7}));
    ignores::<UndoReq>(json!({"op": 7}));
    ignores::<CommitReq>(json!({"message": "m", "changes": [], "idempotency_key": "k"}));

    ignores::<LandResp>(json!({
        "status": "landed", "landed": "l1", "rebased_onto": "m9", "target": "main", "base": "b1",
        "attempts": 1, "relocated": false, "files_changed": 1, "squashed_from": ["h7"],
    }));
    ignores::<ConflictReport>(json!({
        "base": "b1", "ours": "h7", "theirs": "m9", "kinds": ["binary"], "auto_merged": [],
        "files": [{"path": "a.bin", "kind": "binary", "zz_nested": 1}], "truncated": false,
    }));
    ignores::<OpLogPage>(json!({
        "ops": [{"op": 0, "kind": "commit", "changesets": [], "changesets_total": 0,
                 "ref_moves": [], "zz_nested": 1}],
        "truncated": false,
    }));
    ignores::<UndoResp>(json!({"undone": 4, "ref_moves": [], "already_applied": true}));
}

/// The three new requests are tools of `swarmops mcp` once their operations exist, and a tool's
/// input schema must be a plain object (the rule M1.6's MCP guard enforces): no combinator at the
/// top, which the Claude API refuses.
#[test]
fn the_m4_request_schemas_are_plain_objects() {
    let all = schema_for_all();
    for name in ["LandReq", "OpLogReq", "UndoReq"] {
        let schema = all[name].as_object().expect("a schema object");
        assert_eq!(schema["type"], "object", "{name}");
        for combinator in ["oneOf", "anyOf", "allOf", "not", "if", "dependentSchemas"] {
            assert!(
                !schema.contains_key(combinator),
                "{name} has {combinator} at the top"
            );
        }
    }
}

/// A commit response written before `replayed` existed (M1.6's) reads as not replayed. The field
/// is written always, after `warnings`, as the operations spec's row has it.
#[test]
fn a_commit_response_without_replayed_reads_as_false() {
    let advanced =
        r#""advanced":{"name":"main","old":"b0b0","new":"c9f1","already_applied":false}"#;
    let before = format!(
        r#"{{"changeset":"c9f1","parent":"b0b0","root":"7a0b","files_changed":1,"blobs_written":1,{advanced}}}"#
    );
    let read: CommitResp = serde_json::from_str(&before).unwrap();
    assert!(!read.replayed, "{read:?}");
    let now = before.replace(
        &format!("{advanced}}}"),
        &format!(r#"{advanced},"replayed":false}}"#),
    );
    assert_eq!(round_trip(&read), now);

    let mut replayed = read;
    replayed.replayed = true;
    replayed.warnings = vec!["the commit is durable".to_owned()];
    assert!(
        round_trip(&replayed).ends_with(r#""warnings":["the commit is durable"],"replayed":true}"#)
    );
    let schema = &schema_for_all()["CommitResp"];
    assert_eq!(schema["properties"]["replayed"]["default"], false);
}

/// A commit's `idempotency_key` sits between `advance` and `allow_empty`, as the operations
/// spec's row has it, and is omitted when absent.
#[test]
fn a_commit_request_carries_its_key_after_advance() {
    let key = "0192f0c4-7a1b-7c3d-8e4f-5a6b7c8d9e0f";
    let mut keyed = CommitReq::new(Vec::new(), "m");
    keyed.parent = Some(Rev::from("c9f0"));
    keyed.advance = Some("main".to_owned());
    keyed.idempotency_key = Some(key.to_owned());
    keyed.allow_empty = true;
    assert_eq!(
        round_trip(&keyed),
        format!(
            r#"{{"parent":"c9f0","changes":[],"message":"m","advance":"main","idempotency_key":"{key}","allow_empty":true}}"#
        )
    );
    let mut plain = keyed;
    plain.idempotency_key = None;
    assert!(!round_trip(&plain).contains("idempotency_key"));
}

fn signature() -> SignatureOut {
    SignatureOut::new(
        "agent/worker-17",
        "worker-17@agent.swarmops.local",
        1_760_000_000,
        0,
    )
}

/// `show` names who landed a changeset, the land's key and the changesets it squashed only for a
/// changeset a land made: for any other, the three are omitted. They sit where the operations
/// spec's row puts them: `lander` after `author`, `land_key` and `squashed_from` after `message`.
#[test]
fn show_names_a_land_only_for_a_changeset_a_land_made() {
    let mut detail = ChangesetDetail::new(
        "c9f1",
        7,
        vec!["m9".to_owned()],
        "7a0b",
        Attribution::new("agent/worker-17"),
        0,
        "m",
        Vec::new(),
        0,
        false,
        BTreeMap::new(),
        Signatures::new(signature(), signature()),
    );
    let plain = round_trip(&detail);
    for key in ["lander", "land_key", "squashed_from"] {
        assert!(!plain.contains(key), "{key}: {plain}");
    }

    let mut by = Attribution::new("agent/lander-1");
    by.run_id = Some("run-9".to_owned());
    detail.lander = Some(by);
    detail.land_key = Some("k-1".to_owned());
    detail.squashed_from = vec!["h6".to_owned(), "h7".to_owned()];
    let landed = round_trip(&detail);
    let fields = concat!(
        r#""author":{"agent":"agent/worker-17"},"#,
        r#""lander":{"agent":"agent/lander-1","run_id":"run-9"},"#,
        r#""created_at":0,"message":"m","land_key":"k-1","squashed_from":["h6","h7"],"#,
        r#""changes":[]"#
    );
    assert!(landed.contains(fields), "{landed}");
}

/// A land's response, in the operations spec's order: `auto_merged` and `warnings` are omitted
/// when empty, everything else is written; `status` is `landed` or `replayed`.
#[test]
fn a_land_response_has_the_spec_shape() {
    let mut resp = LandResp::new(
        LandStatus::Landed,
        "l1",
        "m9",
        "main",
        "b1",
        1,
        false,
        2,
        vec!["h6".to_owned(), "h7".to_owned()],
    );
    assert_eq!(
        round_trip(&resp),
        concat!(
            r#"{"status":"landed","landed":"l1","rebased_onto":"m9","target":"main","base":"b1","#,
            r#""attempts":1,"relocated":false,"files_changed":2,"squashed_from":["h6","h7"]}"#
        )
    );
    resp.status = LandStatus::Replayed;
    resp.auto_merged = vec!["src/other.rs".to_owned()];
    resp.warnings = vec!["the land is durable".to_owned()];
    let text = round_trip(&resp);
    assert!(text.starts_with(r#"{"status":"replayed","#), "{text}");
    assert!(
        text.ends_with(
            r#""squashed_from":["h6","h7"],"auto_merged":["src/other.rs"],"warnings":["the land is durable"]}"#
        ),
        "{text}"
    );
}

/// A side of a hunk: lines `first` to `last`, numbered from 1, both included.
fn lines(first: u64, last: u64, text: &str) -> LinesOut {
    let mut lines = LinesOut::new(first, last);
    lines.text = Some(text.to_owned());
    lines
}

/// The report `E_CONFLICT` carries in `details.report` (operations spec §3): each file with its
/// kind, the sides' blobs, its hunks and who changed it on the target (per file, decision 6);
/// `kinds` names each kind in `files` once. A side with no lines in a hunk has `last` one less
/// than `first`. Past `max_conflict_bytes` the texts are omitted and `truncated` says so, the
/// positions kept. Absent blobs, attribution and hunks are omitted.
#[test]
fn a_conflict_report_has_the_spec_shape() {
    let mut content = FileConflictOut::new("src/lib.rs", ConflictKindOut::Content);
    content.base_blob = Some("e1".to_owned());
    content.ours_blob = Some("e2".to_owned());
    content.theirs_blob = Some("e3".to_owned());
    content.hunks = vec![HunkOut::new(
        0,
        lines(11, 12, "a\nb\n"),
        lines(11, 13, "a\nB\nc\n"),
        lines(11, 10, ""),
    )];
    content.theirs_changeset = Some("m9".to_owned());
    content.theirs_agent = Some("agent/worker-3".to_owned());
    let mut deleted = FileConflictOut::new("docs/x.md", ConflictKindOut::DeleteModify);
    deleted.base_blob = Some("e4".to_owned());
    deleted.theirs_blob = Some("e5".to_owned());
    let mut report = ConflictReport::new(
        "b1",
        "h7",
        "m9",
        vec![content, deleted],
        vec![ConflictKindOut::Content, ConflictKindOut::DeleteModify],
        false,
    );
    report.auto_merged = vec!["src/other.rs".to_owned()];
    let hunk = concat!(
        r#"{"id":0,"base":{"first":11,"last":12,"text":"a\nb\n"},"#,
        r#""ours":{"first":11,"last":13,"text":"a\nB\nc\n"},"#,
        r#""theirs":{"first":11,"last":10,"text":""}}"#
    );
    let files = format!(
        concat!(
            r#""files":[{{"path":"src/lib.rs","kind":"content","base_blob":"e1","ours_blob":"e2","#,
            r#""theirs_blob":"e3","hunks":[{hunk}],"theirs_changeset":"m9","#,
            r#""theirs_agent":"agent/worker-3"}},"#,
            r#"{{"path":"docs/x.md","kind":"delete_modify","base_blob":"e4","theirs_blob":"e5"}}],"#
        ),
        hunk = hunk
    );
    assert_eq!(
        round_trip(&report),
        format!(
            r#"{{"base":"b1","ours":"h7","theirs":"m9",{files}"kinds":["content","delete_modify"],"auto_merged":["src/other.rs"],"truncated":false}}"#
        )
    );

    let hunk = &mut report.files[0].hunks[0];
    for side in [&mut hunk.base, &mut hunk.ours, &mut hunk.theirs] {
        side.text = None;
    }
    report.truncated = true;
    let cut = round_trip(&report);
    assert!(
        cut.contains(r#""hunks":[{"id":0,"base":{"first":11,"last":12},"ours":{"first":11,"last":13},"theirs":{"first":11,"last":10}}]"#),
        "{cut}"
    );
    assert!(cut.ends_with(r#""truncated":true}"#), "{cut}");

    assert_eq!(
        serde_json::to_value([
            ConflictKindOut::Content,
            ConflictKindOut::Binary,
            ConflictKindOut::DeleteModify,
            ConflictKindOut::ModifyDelete,
            ConflictKindOut::AddAdd,
            ConflictKindOut::Mode,
            ConflictKindOut::TypeChange,
        ])
        .unwrap(),
        json!([
            "content",
            "binary",
            "delete_modify",
            "modify_delete",
            "add_add",
            "mode",
            "type_change"
        ])
    );
}

/// The operation log, newest first: each operation with its number, kind, changesets (with the
/// land's key when there is one) and ref moves; a move that created a ref has no `old`, one that
/// deleted it no `new`. An undo that restored refs names its own operation; one that found them
/// restored already wrote nothing, so it has no `op`.
#[test]
fn an_op_log_page_and_an_undo_have_the_spec_shape() {
    let moved = |old: Option<&str>, new: Option<&str>| {
        let mut moved = RefMoveOut::new("main", "agent/worker-17", 1_760_000_000);
        moved.old = old.map(str::to_owned);
        moved.new = new.map(str::to_owned);
        moved
    };
    let mut landed =
        OpChangesetOut::new("l1", "agent/worker-3", 1_760_000_000_000, "fix the parser");
    landed.land_key = Some("k-1".to_owned());
    let page = OpLogPage::new(
        vec![OperationOut::new(
            41,
            OperationKind::Land,
            vec![landed],
            1,
            vec![moved(Some("m9"), Some("l1"))],
        )],
        true,
    );
    let actor = r#""actor":"agent/worker-17","at":1760000000"#;
    assert_eq!(
        round_trip(&page),
        format!(
            concat!(
                r#"{{"ops":[{{"op":41,"kind":"land","changesets":[{{"id":"l1","#,
                r#""agent":"agent/worker-3","created_at":1760000000000,"#,
                r#""title":"fix the parser","land_key":"k-1"}}],"changesets_total":1,"#,
                r#""ref_moves":[{{"name":"main","old":"m9","new":"l1",{actor}}}]}}],"#,
                r#""truncated":true}}"#
            ),
            actor = actor
        )
    );
    assert_eq!(
        round_trip(&moved(None, Some("c1"))),
        format!(r#"{{"name":"main","new":"c1",{actor}}}"#)
    );
    assert_eq!(
        round_trip(&moved(Some("c1"), None)),
        format!(r#"{{"name":"main","old":"c1",{actor}}}"#)
    );
    assert_eq!(
        serde_json::to_value([
            OperationKind::Commit,
            OperationKind::Publish,
            OperationKind::Land,
            OperationKind::RefMove,
            OperationKind::Compaction,
            OperationKind::Mixed,
        ])
        .unwrap(),
        json!([
            "commit",
            "publish",
            "land",
            "ref_move",
            "compaction",
            "mixed"
        ])
    );

    let mut undo = UndoResp::new(41, vec![moved(Some("l1"), Some("m9"))], false);
    undo.op = Some(42);
    undo.warnings = vec!["the undo is durable".to_owned()];
    assert_eq!(
        round_trip(&undo),
        format!(
            r#"{{"undone":41,"op":42,"ref_moves":[{{"name":"main","old":"l1","new":"m9",{actor}}}],"already_applied":false,"warnings":["the undo is durable"]}}"#
        )
    );
    let replay = UndoResp::new(41, Vec::new(), true);
    assert_eq!(
        round_trip(&replay),
        r#"{"undone":41,"ref_moves":[],"already_applied":true}"#
    );
}
