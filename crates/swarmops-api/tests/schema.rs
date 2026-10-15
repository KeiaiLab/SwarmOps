//! The API contract: every type has a JSON Schema, error codes follow the operations spec, the
//! envelope has the spec's shape, and requests and responses keep their wire forms.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use swarmops_api::{
    API_VERSION, Attribution, ChangeInput, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary,
    CommitReq, CommitResp, ContentEncoding, ContentInput, DEFAULT_LOG_LIMIT, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_CHANGES, DEFAULT_READ_TREE_LIMIT, EntryKindOut, Envelope, ErrorCode, FileContent,
    FileKind, Identity, IdentityInfo, InfoReq, InitReq, LogPage, LogReq, Meta, ModeInput, OpError,
    ReadFileReq, ReadTreeReq, RefDeleteReq, RefListReq, RefListResp, RefSetReq, RefSetResp,
    RepoInfo, Rev, ShowReq, SignatureOut, Signatures, StoreOps, TreeEntryOut, TreeListing,
    VerifyReport, VerifyReq, schema_for_all,
};

/// Requests and the input types inside them.
const REQUESTS: &[&str] = &[
    "ChangeInput",
    "CommitReq",
    "Identity",
    "InfoReq",
    "InitReq",
    "LogReq",
    "ReadFileReq",
    "ReadTreeReq",
    "RefDeleteReq",
    "RefListReq",
    "RefSetReq",
    "ShowReq",
    "VerifyReq",
];

/// Responses and the output types inside them.
const RESPONSES: &[&str] = &[
    "Attribution",
    "ChangeOut",
    "ChangesetDetail",
    "ChangesetSummary",
    "CommitResp",
    "Envelope",
    "FileContent",
    "IdentityInfo",
    "LogPage",
    "Meta",
    "OpError",
    "RefListResp",
    "RefOut",
    "RefSetResp",
    "RepoInfo",
    "SignatureOut",
    "Signatures",
    "TreeEntryOut",
    "TreeListing",
    "VerifyReport",
];

/// Enums and newtypes, which have no object fields of their own.
const SCALARS: &[&str] = &[
    "ChangeOp",
    "ContentEncoding",
    "ContentInput",
    "EntryKindOut",
    "ErrorCode",
    "FileKind",
    "ModeInput",
    "Rev",
];

#[test]
fn every_request_and_response_has_a_json_schema() {
    let all = schema_for_all();
    let all = all
        .as_object()
        .expect("schema_for_all returns an object keyed by type name");
    let expected: BTreeSet<&str> = REQUESTS
        .iter()
        .chain(RESPONSES)
        .chain(SCALARS)
        .copied()
        .collect();
    let actual: BTreeSet<&str> = all.keys().map(String::as_str).collect();
    assert_eq!(actual, expected);

    for (name, schema) in all {
        assert_eq!(
            schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{name}"
        );
        assert_eq!(schema["title"], name.as_str(), "{name}");
    }
    // The operations spec (§0) forbids `deny_unknown_fields`: requests and responses alike accept
    // fields they do not know.
    for name in REQUESTS.iter().chain(RESPONSES) {
        assert_eq!(all[*name]["type"], "object", "{name}");
        assert!(
            all[*name].get("additionalProperties").is_none(),
            "{name} must ignore unknown fields"
        );
    }
}

#[test]
fn every_nested_type_is_registered_too() {
    let all = schema_for_all();
    let all = all.as_object().expect("an object keyed by type name");
    for (name, schema) in all {
        let nested = schema.get("$defs").and_then(Value::as_object);
        for inner in nested.into_iter().flat_map(serde_json::Map::keys) {
            assert!(
                all.contains_key(inner),
                "{name} uses {inner}, which schema_for_all does not register"
            );
        }
    }
}

#[test]
fn schema_descriptions_name_no_rust_items() {
    fn check(at: &str, value: &Value) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(text)) = map.get("description") {
                    assert!(!text.contains("::") && !text.contains("[`"), "{at}: {text}");
                }
                for (key, inner) in map {
                    check(&format!("{at}/{key}"), inner);
                }
            }
            Value::Array(items) => {
                for (i, inner) in items.iter().enumerate() {
                    check(&format!("{at}[{i}]"), inner);
                }
            }
            _ => {}
        }
    }
    check("", &schema_for_all());
}

/// Pins every `StoreOps` method's name, request type and response type. Instantiated with
/// `S = dyn StoreOps`, it also proves the trait object-safe.
fn store_ops_signatures<S: StoreOps + ?Sized>() {
    let _: fn(&S, InfoReq) -> Result<RepoInfo, OpError> = S::repo_info;
    let _: fn(&S, &Identity, CommitReq) -> Result<CommitResp, OpError> = S::commit;
    let _: fn(&S, ReadFileReq) -> Result<FileContent, OpError> = S::read_file;
    let _: fn(&S, ReadTreeReq) -> Result<TreeListing, OpError> = S::read_tree;
    let _: fn(&S, ShowReq) -> Result<ChangesetDetail, OpError> = S::show;
    let _: fn(&S, LogReq) -> Result<LogPage, OpError> = S::log;
    let _: fn(&S, RefListReq) -> Result<RefListResp, OpError> = S::ref_list;
    let _: fn(&S, &Identity, RefSetReq) -> Result<RefSetResp, OpError> = S::ref_set;
    let _: fn(&S, &Identity, RefDeleteReq) -> Result<RefSetResp, OpError> = S::ref_delete;
    let _: fn(&S, VerifyReq) -> Result<VerifyReport, OpError> = S::verify;
}

#[test]
fn every_operation_takes_a_request_and_returns_a_response() {
    store_ops_signatures::<dyn StoreOps>();
}

/// Each code's string, exit code and retry advice, written out independently of the crate's
/// table: from the operations spec (§0 exit codes, §1 and §4 names and retryability) where it
/// names the code, from the M1 plan and rulings for `E_UNSUPPORTED_FS`, `E_BUSY` and
/// `E_INTERNAL`, and by the `E_<NAME>` convention for `E_TYPE_CONFLICT`, `E_UNKNOWN_PARENT` and
/// `E_IO`. A new variant does not compile here until its row is added.
fn spec_row(code: ErrorCode) -> (&'static str, i32, bool) {
    match code {
        ErrorCode::ExistsAlready => ("E_EXISTS", 1, false),
        ErrorCode::NotARepo => ("E_NOT_A_REPO", 6, false),
        ErrorCode::IdentityRequired => ("E_IDENTITY_REQUIRED", 5, false),
        ErrorCode::RevNotFound => ("E_REV_NOT_FOUND", 6, false),
        ErrorCode::AmbiguousId => ("E_AMBIGUOUS_ID", 1, false),
        ErrorCode::PathNotFound => ("E_PATH_NOT_FOUND", 6, false),
        ErrorCode::IsDir => ("E_IS_DIR", 1, false),
        ErrorCode::TooLarge => ("E_TOO_LARGE", 1, false),
        ErrorCode::EmptyCommit => ("E_EMPTY_COMMIT", 1, false),
        ErrorCode::TypeConflict => ("E_TYPE_CONFLICT", 1, false),
        ErrorCode::UnknownParent => ("E_UNKNOWN_PARENT", 6, false),
        ErrorCode::CasMismatch => ("E_CAS_MISMATCH", 4, true),
        ErrorCode::RefNotFound => ("E_REF_NOT_FOUND", 6, false),
        ErrorCode::RefNameInvalid => ("E_REF_NAME_INVALID", 1, false),
        ErrorCode::StoreCorrupt => ("E_STORE_CORRUPT", 1, false),
        ErrorCode::InvalidArg => ("E_INVALID_ARG", 2, false),
        ErrorCode::UnsupportedFs => ("E_UNSUPPORTED_FS", 1, false),
        ErrorCode::Io => ("E_IO", 1, false),
        ErrorCode::Busy => ("E_BUSY", 4, true),
        ErrorCode::Internal => ("E_INTERNAL", 1, false),
        ErrorCode::Auth => ("E_AUTH", 5, false),
        ErrorCode::PolicyDenied => ("E_POLICY_DENIED", 5, false),
        ErrorCode::MaintenanceBusy => ("E_MAINTENANCE_BUSY", 4, true),
    }
}

#[test]
fn error_codes_map_to_spec_strings_and_exits() {
    assert_eq!(ErrorCode::CasMismatch.as_str(), "E_CAS_MISMATCH");
    assert_eq!(ErrorCode::CasMismatch.exit(), 4);
    assert!(ErrorCode::CasMismatch.retryable());
    assert_eq!(ErrorCode::IdentityRequired.exit(), 5);
    assert_eq!(ErrorCode::RevNotFound.exit(), 6);
    assert_eq!(ErrorCode::InvalidArg.exit(), 2);
    assert_eq!(ErrorCode::Internal.as_str(), "E_INTERNAL");
    assert_eq!(ErrorCode::Internal.exit(), 1);
    assert!(!ErrorCode::Internal.retryable());

    let mut listed = BTreeSet::new();
    for &code in ErrorCode::ALL {
        let (name, exit, retryable) = spec_row(code);
        assert_eq!(
            (code.as_str(), code.exit(), code.retryable()),
            (name, exit, retryable),
            "{code:?}"
        );
        assert_eq!(serde_json::to_value(code).unwrap(), name, "{code:?}");
        assert_eq!(
            serde_json::from_value::<ErrorCode>(json!(name)).unwrap(),
            code
        );
        assert_eq!(
            OpError::new(code, "boom"),
            OpError {
                code: name.to_owned(),
                message: "boom".to_owned(),
                retryable,
                exit,
                details: Map::new(),
                hint: None,
            }
        );
        assert!(listed.insert(name), "{name} is listed twice");
    }

    // The schema enumerates the enum itself, so ALL cannot miss a variant.
    let schema = schema_for_all();
    let in_schema: BTreeSet<&str> = schema["ErrorCode"]["oneOf"]
        .as_array()
        .expect("ErrorCode's schema is a oneOf of its codes")
        .iter()
        .flat_map(|alt| match (&alt["const"], &alt["enum"]) {
            (Value::String(one), _) => vec![one.as_str()],
            (_, Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
            _ => panic!("unexpected ErrorCode schema alternative {alt}"),
        })
        .collect();
    assert_eq!(listed, in_schema);

    let actual = Map::from_iter([("actual".to_owned(), json!("c9f1"))]);
    let err = OpError::new(ErrorCode::CasMismatch, "main moved")
        .with_details(actual.clone())
        .with_hint("swarmops ref list");
    assert_eq!(err.details, actual);
    assert_eq!(err.hint.as_deref(), Some("swarmops ref list"));
    assert_eq!(err.to_string(), "E_CAS_MISMATCH: main moved");
}

fn meta() -> Meta {
    Meta {
        repo: None,
        elapsed_ms: 12,
        resolved: BTreeMap::from([("main".to_owned(), "c9f1".to_owned())]),
        auth: "trust".to_owned(),
    }
}

#[test]
fn envelope_serializes_without_nulls_for_success() {
    let result = CommitResp {
        changeset: "c9f1".to_owned(),
        parent: None,
        root: "7a0b".to_owned(),
        files_changed: 2,
        blobs_written: 1,
        warnings: Vec::new(),
    };
    let envelope = Envelope::ok("commit", result, meta());
    assert_eq!(envelope.api, API_VERSION);
    assert_eq!(API_VERSION, 1);

    assert_eq!(
        serde_json::to_value(&envelope).unwrap(),
        json!({
            "ok": true,
            "op": "commit",
            "api": 1,
            "result": {"changeset": "c9f1", "root": "7a0b", "files_changed": 2, "blobs_written": 1},
            "meta": {"elapsed_ms": 12, "resolved": {"main": "c9f1"}, "auth": "trust"},
        })
    );
    let text = serde_json::to_string(&envelope).unwrap();
    assert!(!text.contains("null"), "{text}");
    assert!(
        text.starts_with(r#"{"ok":true,"op":"commit","api":1,"result":{"changeset":"c9f1","#),
        "fields keep the spec's order: {text}"
    );
    assert_eq!(
        serde_json::from_str::<Envelope<CommitResp>>(&text).unwrap(),
        envelope
    );
}

#[test]
fn envelope_for_failure_carries_the_error_and_no_result() {
    let error = OpError::new(ErrorCode::CasMismatch, "ref main moved")
        .with_details(Map::from_iter([("actual".to_owned(), json!("d00d"))]))
        .with_hint("re-read the ref, then retry with --expect d00d");
    let envelope = Envelope::<RefSetResp>::err("ref_set", error, meta());
    assert_eq!(
        serde_json::to_value(&envelope).unwrap(),
        json!({
            "ok": false,
            "op": "ref_set",
            "api": 1,
            "error": {
                "code": "E_CAS_MISMATCH",
                "message": "ref main moved",
                "retryable": true,
                "exit": 4,
                "details": {"actual": "d00d"},
                "hint": "re-read the ref, then retry with --expect d00d",
            },
            "meta": {"elapsed_ms": 12, "resolved": {"main": "c9f1"}, "auth": "trust"},
        })
    );
    // Without details or a hint both keys are still written: details as `{}`, hint as null.
    let bare = OpError::new(ErrorCode::RevNotFound, "no rev x");
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"code":"E_REV_NOT_FOUND","message":"no rev x","retryable":false,"exit":6,"details":{},"hint":null}"#
    );
    let text = serde_json::to_string(&envelope).unwrap();
    assert_eq!(
        serde_json::from_str::<Envelope<RefSetResp>>(&text).unwrap(),
        envelope
    );
}

#[test]
fn error_object_always_carries_details_and_hint() {
    // `details` reads as `{}` when it is missing or null.
    let minimal = json!({"code": "E_IO", "message": "m", "retryable": false, "exit": 1});
    let mut nulls = minimal.clone();
    nulls["details"] = Value::Null;
    nulls["hint"] = Value::Null;
    for wire in [minimal.clone(), nulls] {
        let err: OpError = serde_json::from_value(wire).unwrap();
        assert_eq!((err.details, err.hint), (Map::new(), None));
    }
    let mut listed = minimal;
    listed["details"] = json!([1]);
    assert!(serde_json::from_value::<OpError>(listed).is_err());

    // The schema says what is always written: `details` an object, `hint` a string or null.
    let schema = &schema_for_all()["OpError"];
    assert_eq!(schema["properties"]["details"]["type"], "object");
    assert_eq!(
        schema["properties"]["hint"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        schema["required"],
        json!(["code", "message", "retryable", "exit", "details", "hint"])
    );
}

fn with_unknown_field(mut value: Value) -> Value {
    value
        .as_object_mut()
        .expect("an object")
        .insert("zz_unknown".to_owned(), json!(1));
    value
}

/// `minimal` must parse, and parse to the same value with an extra unknown field.
fn assert_ignores_unknown_fields<T: DeserializeOwned + PartialEq + Debug>(
    name: &str,
    minimal: Value,
) {
    let plain: T = serde_json::from_value(minimal.clone())
        .unwrap_or_else(|e| panic!("{name}: the minimal form must parse: {e}"));
    let extended: T = serde_json::from_value(with_unknown_field(minimal))
        .unwrap_or_else(|e| panic!("{name} must ignore unknown fields: {e}"));
    assert_eq!(extended, plain, "{name}");
}

#[test]
fn requests_and_responses_ignore_unknown_fields() {
    assert_ignores_unknown_fields::<InitReq>("InitReq", json!({"path": "/srv/s", "name": "acme"}));
    assert_ignores_unknown_fields::<CommitReq>(
        "CommitReq",
        json!({"message": "m", "changes": [{"path": "a", "op": "delete", "zz_nested": 1}]}),
    );
    assert_ignores_unknown_fields::<ChangeInput>(
        "ChangeInput",
        json!({"path": "a", "op": "delete"}),
    );
    assert_ignores_unknown_fields::<ReadFileReq>(
        "ReadFileReq",
        json!({"rev": "main", "path": "a"}),
    );
    assert_ignores_unknown_fields::<ReadTreeReq>("ReadTreeReq", json!({"rev": "main"}));
    assert_ignores_unknown_fields::<ShowReq>("ShowReq", json!({"rev": "main"}));
    assert_ignores_unknown_fields::<LogReq>("LogReq", json!({}));
    assert_ignores_unknown_fields::<RefSetReq>("RefSetReq", json!({"name": "main", "new": "c9f1"}));
    assert_ignores_unknown_fields::<RefDeleteReq>(
        "RefDeleteReq",
        json!({"name": "main", "expected_old": "c9f1"}),
    );
    assert_ignores_unknown_fields::<Identity>("Identity", json!({"agent": "human/purple"}));
    assert_ignores_unknown_fields::<InfoReq>("InfoReq", json!({}));
    assert_ignores_unknown_fields::<RefListReq>("RefListReq", json!({}));
    assert_ignores_unknown_fields::<VerifyReq>("VerifyReq", json!({"deep": true}));

    let signature = json!({"name": "human/purple", "email": "purple@human.swarmops.local", "secs": 0, "offset_secs": 0});
    assert_ignores_unknown_fields::<RepoInfo>(
        "RepoInfo",
        json!({"repo_id": "r", "name": "acme", "store_version": 1, "changesets": 0, "refs": 0}),
    );
    assert_ignores_unknown_fields::<IdentityInfo>("IdentityInfo", json!({"auth": "trust"}));
    assert_ignores_unknown_fields::<CommitResp>(
        "CommitResp",
        json!({"changeset": "c", "root": "t", "files_changed": 1, "blobs_written": 1}),
    );
    assert_ignores_unknown_fields::<FileContent>(
        "FileContent",
        json!({"blob": "b", "size": 1, "kind": "file", "encoding": "utf8", "content": "x", "truncated": false}),
    );
    assert_ignores_unknown_fields::<TreeListing>(
        "TreeListing",
        json!({"entries": [{"name": "src", "kind": "dir", "id": "t", "zz_nested": 1}], "truncated": false}),
    );
    assert_ignores_unknown_fields::<ChangesetDetail>(
        "ChangesetDetail",
        json!({
            "id": "c", "seq": 0, "parents": [], "root": "t",
            "author": {"agent": "human/purple", "zz_nested": 1}, "created_at": 0, "message": "m",
            "changes": [{"path": "a", "op": "put", "kind": "file", "blob": "b"}, {"path": "b", "op": "delete"}],
            "files_changed": 2, "changes_truncated": false,
            "metadata": {"agent": "human/purple"},
            "signatures": {"author": signature, "committer": signature},
        }),
    );
    assert_ignores_unknown_fields::<LogPage>(
        "LogPage",
        json!({"entries": [{"id": "c", "parents": [], "author_agent": "human/purple", "created_at": 0, "title": "t"}], "truncated": false}),
    );
    assert_ignores_unknown_fields::<RefListResp>(
        "RefListResp",
        json!({"refs": [{"name": "main", "target": "c", "version": 1}]}),
    );
    assert_ignores_unknown_fields::<RefSetResp>(
        "RefSetResp",
        json!({"name": "main", "new": "c", "already_applied": false}),
    );
    assert_ignores_unknown_fields::<VerifyReport>(
        "VerifyReport",
        json!({"objects_checked": 0, "changesets_checked": 0, "problems": []}),
    );
    assert_ignores_unknown_fields::<Meta>(
        "Meta",
        json!({"elapsed_ms": 0, "resolved": {}, "auth": "trust"}),
    );
    // A code this client does not know still parses: `code` is a string, not the enum.
    assert_ignores_unknown_fields::<OpError>(
        "OpError",
        json!({"code": "E_FROM_THE_FUTURE", "message": "m", "retryable": false, "exit": 1}),
    );
    assert_ignores_unknown_fields::<Envelope<Value>>(
        "Envelope",
        json!({"ok": true, "op": "info", "api": 1, "result": {}, "meta": {"elapsed_ms": 0, "resolved": {}, "auth": "trust"}}),
    );
}

#[test]
fn omitted_request_fields_take_their_defaults() {
    assert_eq!(DEFAULT_MAX_BYTES, 1024 * 1024);
    assert_eq!(DEFAULT_LOG_LIMIT, 50);
    assert_eq!(
        serde_json::from_value::<InfoReq>(json!({})).unwrap(),
        InfoReq {}
    );
    assert_eq!(
        serde_json::from_value::<RefListReq>(json!({})).unwrap(),
        RefListReq {}
    );
    assert_eq!(
        serde_json::from_value::<VerifyReq>(json!({})).unwrap(),
        VerifyReq { deep: false }
    );
    assert_eq!(
        serde_json::from_value::<ReadFileReq>(json!({"rev": "main", "path": "src/lib.rs"}))
            .unwrap(),
        ReadFileReq {
            rev: Rev("main".to_owned()),
            path: "src/lib.rs".to_owned(),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    );
    assert_eq!(
        serde_json::from_value::<LogReq>(json!({})).unwrap(),
        LogReq {
            rev: None,
            limit: DEFAULT_LOG_LIMIT,
            cursor: None,
        }
    );
    assert_eq!(
        serde_json::from_value::<RefSetReq>(json!({"name": "main", "new": "c9f1"})).unwrap(),
        RefSetReq {
            name: "main".to_owned(),
            new: Rev("c9f1".to_owned()),
            expected_old: None,
            create: false,
        }
    );
    assert_eq!(
        serde_json::from_value::<CommitReq>(json!({"message": "m", "changes": []})).unwrap(),
        CommitReq {
            parent: None,
            message: "m".to_owned(),
            changes: Vec::new(),
            allow_empty: false,
        }
    );
}

#[test]
fn change_lists_and_enums_use_their_wire_forms() {
    let wire = json!({
        "message": "add files",
        "changes": [
            {"path": "a.txt", "op": "put", "content": {"inline": "hi\n"}},
            {"path": "bin/run", "op": "put", "content": {"base64": "AAE="}, "mode": "exec"},
            {"path": "link", "op": "put", "content": {"file": "./target.txt"}, "mode": "symlink"},
            {"path": "old.txt", "op": "delete"},
        ],
    });
    let commit: CommitReq = serde_json::from_value(wire).unwrap();
    let put = |path: &str, content: ContentInput, mode: Option<ModeInput>| ChangeInput {
        path: path.to_owned(),
        op: ChangeOp::Put,
        content: Some(content),
        mode,
    };
    assert_eq!(
        commit,
        CommitReq {
            parent: None,
            message: "add files".to_owned(),
            changes: vec![
                put("a.txt", ContentInput::Inline("hi\n".to_owned()), None),
                put(
                    "bin/run",
                    ContentInput::Base64("AAE=".to_owned()),
                    Some(ModeInput::Exec)
                ),
                put(
                    "link",
                    ContentInput::File("./target.txt".to_owned()),
                    Some(ModeInput::Symlink)
                ),
                ChangeInput {
                    path: "old.txt".to_owned(),
                    op: ChangeOp::Delete,
                    content: None,
                    mode: None,
                },
            ],
            allow_empty: false,
        }
    );
    // Serializing writes no nulls for the options left out, and keeps the spec's field order
    // (`{parent, changes}, message, allow_empty`).
    let rooted = CommitReq {
        parent: Some(Rev("main".to_owned())),
        changes: vec![commit.changes[3].clone()],
        message: "m".to_owned(),
        allow_empty: true,
    };
    assert_eq!(
        serde_json::to_string(&rooted).unwrap(),
        r#"{"parent":"main","changes":[{"path":"old.txt","op":"delete"}],"message":"m","allow_empty":true}"#
    );

    assert_eq!(
        serde_json::to_value(Rev("main~2".to_owned())).unwrap(),
        "main~2"
    );
    assert_eq!(
        serde_json::to_value([ModeInput::File, ModeInput::Exec, ModeInput::Symlink]).unwrap(),
        json!(["file", "exec", "symlink"])
    );
    assert_eq!(
        serde_json::to_value([
            EntryKindOut::File,
            EntryKindOut::Exec,
            EntryKindOut::Symlink,
            EntryKindOut::Dir
        ])
        .unwrap(),
        json!(["file", "exec", "symlink", "dir"])
    );
    assert_eq!(
        serde_json::to_value([FileKind::File, FileKind::Exec, FileKind::Symlink]).unwrap(),
        json!(["file", "exec", "symlink"])
    );
    assert_eq!(
        serde_json::to_value([ContentEncoding::Utf8, ContentEncoding::Base64]).unwrap(),
        json!(["utf8", "base64"])
    );
    for bad in [
        json!({"inline": "x", "base64": "eA=="}),
        json!({"carrier_pigeon": "c9f1"}),
        json!("x"),
    ] {
        assert!(
            serde_json::from_value::<ContentInput>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    assert!(serde_json::from_value::<ChangeOp>(json!("rename")).is_err());
    assert!(serde_json::from_value::<ModeInput>(json!("dir")).is_err());
    assert!(serde_json::from_value::<FileKind>(json!("dir")).is_err());
}

fn signature() -> SignatureOut {
    SignatureOut {
        name: "agent/worker-17".to_owned(),
        email: "worker-17@claude-code.swarmops.local".to_owned(),
        secs: 1_760_000_000,
        offset_secs: 0,
    }
}

#[test]
fn responses_follow_the_spec_shape_and_field_order() {
    let info = RepoInfo {
        repo_id: "r1".to_owned(),
        name: "acme".to_owned(),
        main: Some("c9f1".to_owned()),
        store_version: 1,
        changesets: 3,
        refs: 1,
    };
    assert_eq!(
        serde_json::to_string(&info).unwrap(),
        r#"{"repo_id":"r1","name":"acme","main":"c9f1","store_version":1,"changesets":3,"refs":1}"#
    );

    let detail = ChangesetDetail {
        id: "c9f1".to_owned(),
        seq: 7,
        parents: vec!["b0b0".to_owned()],
        root: "7a0b".to_owned(),
        author: Attribution {
            agent: "agent/worker-17".to_owned(),
            run_id: Some("run-3".to_owned()),
            on_behalf_of: None,
        },
        created_at: 1_760_000_000_000,
        message: "add files".to_owned(),
        changes: vec![
            ChangeOut {
                path: "a.txt".to_owned(),
                op: ChangeOp::Put,
                kind: Some(FileKind::File),
                blob: Some("b1".to_owned()),
            },
            ChangeOut {
                path: "old.txt".to_owned(),
                op: ChangeOp::Delete,
                kind: None,
                blob: None,
            },
        ],
        files_changed: 3,
        changes_truncated: true,
        metadata: BTreeMap::from([
            ("agent".to_owned(), "agent/worker-17".to_owned()),
            ("run_id".to_owned(), "run-3".to_owned()),
        ]),
        signatures: Signatures {
            author: signature(),
            committer: signature(),
        },
    };
    let text = serde_json::to_string(&detail).unwrap();
    let git = r#"{"name":"agent/worker-17","email":"worker-17@claude-code.swarmops.local","secs":1760000000,"offset_secs":0}"#;
    assert_eq!(
        text,
        [
            r#"{"id":"c9f1","seq":7,"parents":["b0b0"],"root":"7a0b","#,
            r#""author":{"agent":"agent/worker-17","run_id":"run-3"},"#,
            r#""created_at":1760000000000,"message":"add files","#,
            r#""changes":[{"path":"a.txt","op":"put","kind":"file","blob":"b1"},{"path":"old.txt","op":"delete"}],"#,
            r#""files_changed":3,"changes_truncated":true,"#,
            r#""metadata":{"agent":"agent/worker-17","run_id":"run-3"},"#,
            &format!(r#""signatures":{{"author":{git},"committer":{git}}}}}"#),
        ]
        .concat()
    );
    assert_eq!(
        serde_json::from_str::<ChangesetDetail>(&text).unwrap(),
        detail
    );
    // `created_at` is Unix milliseconds (operations spec §5), never negative.
    let all = schema_for_all();
    for name in ["ChangesetDetail", "ChangesetSummary"] {
        assert_eq!(
            all[name]["properties"]["created_at"]["format"], "uint64",
            "{name}"
        );
    }

    let file = FileContent {
        blob: "b1".to_owned(),
        size: 3,
        kind: FileKind::Exec,
        encoding: ContentEncoding::Utf8,
        content: "hi\n".to_owned(),
        truncated: false,
    };
    assert_eq!(
        serde_json::to_string(&file).unwrap(),
        r#"{"blob":"b1","size":3,"kind":"exec","encoding":"utf8","content":"hi\n","truncated":false}"#
    );
}

#[test]
fn ref_moves_carry_warnings_only_when_there_are_some() {
    let mut moved = RefSetResp {
        name: "main".to_owned(),
        old: Some("b0b0".to_owned()),
        new: Some("c9f1".to_owned()),
        already_applied: false,
        warnings: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&moved).unwrap(),
        r#"{"name":"main","old":"b0b0","new":"c9f1","already_applied":false}"#
    );
    moved.warnings.push("the ref move is committed".to_owned());
    let text = serde_json::to_string(&moved).unwrap();
    assert_eq!(
        text,
        r#"{"name":"main","old":"b0b0","new":"c9f1","already_applied":false,"warnings":["the ref move is committed"]}"#
    );
    assert_eq!(serde_json::from_str::<RefSetResp>(&text).unwrap(), moved);
}

#[test]
fn listings_page_with_truncated_and_cursor() {
    // Default pages within the operations spec's 64 KiB target for a default response (§9).
    assert_eq!(DEFAULT_READ_TREE_LIMIT, 500);
    assert_eq!(DEFAULT_MAX_CHANGES, 400);
    assert_eq!(DEFAULT_LOG_LIMIT, 50);
    // An empty page is a mistake: limits start at 1, and 0 is `E_INVALID_ARG`.
    let all = schema_for_all();
    for (name, field) in [
        ("ReadTreeReq", "limit"),
        ("LogReq", "limit"),
        ("ShowReq", "max_changes"),
    ] {
        assert_eq!(
            all[name]["properties"][field]["minimum"], 1,
            "{name}.{field}"
        );
    }
    assert_eq!(
        serde_json::from_value::<ReadTreeReq>(json!({"rev": "main"})).unwrap(),
        ReadTreeReq {
            rev: Rev("main".to_owned()),
            path: None,
            limit: DEFAULT_READ_TREE_LIMIT,
            cursor: None,
        }
    );
    assert_eq!(
        serde_json::from_value::<ReadTreeReq>(
            json!({"rev": "main", "path": "src", "limit": 2, "cursor": "b.rs"})
        )
        .unwrap(),
        ReadTreeReq {
            rev: Rev("main".to_owned()),
            path: Some("src".to_owned()),
            limit: 2,
            cursor: Some("b.rs".to_owned()),
        }
    );
    assert_eq!(
        serde_json::from_value::<ShowReq>(json!({"rev": "main"})).unwrap(),
        ShowReq {
            rev: Rev("main".to_owned()),
            max_changes: DEFAULT_MAX_CHANGES,
        }
    );
    assert_eq!(
        serde_json::from_value::<LogReq>(json!({"cursor": "c9f1"})).unwrap(),
        LogReq {
            rev: None,
            limit: DEFAULT_LOG_LIMIT,
            cursor: Some("c9f1".to_owned()),
        }
    );

    let entry = |name: &str| TreeEntryOut {
        name: name.to_owned(),
        kind: EntryKindOut::File,
        id: "b1".to_owned(),
    };
    let page = TreeListing {
        entries: vec![entry("a.rs"), entry("b.rs")],
        truncated: true,
        cursor: Some("b.rs".to_owned()),
    };
    assert_eq!(
        serde_json::to_string(&page).unwrap(),
        r#"{"entries":[{"name":"a.rs","kind":"file","id":"b1"},{"name":"b.rs","kind":"file","id":"b1"}],"truncated":true,"cursor":"b.rs"}"#
    );
    let last = TreeListing {
        entries: vec![entry("c.rs")],
        truncated: false,
        cursor: None,
    };
    assert_eq!(
        serde_json::to_string(&last).unwrap(),
        r#"{"entries":[{"name":"c.rs","kind":"file","id":"b1"}],"truncated":false}"#
    );

    let log = LogPage {
        entries: vec![ChangesetSummary {
            id: "c9f1".to_owned(),
            parents: vec!["b0b0".to_owned()],
            author_agent: "human/purple".to_owned(),
            created_at: 1_760_000_000_000,
            title: "add files".to_owned(),
        }],
        truncated: true,
        cursor: Some("c9f1".to_owned()),
    };
    assert_eq!(
        serde_json::to_string(&log).unwrap(),
        r#"{"entries":[{"id":"c9f1","parents":["b0b0"],"author_agent":"human/purple","created_at":1760000000000,"title":"add files"}],"truncated":true,"cursor":"c9f1"}"#
    );
}
