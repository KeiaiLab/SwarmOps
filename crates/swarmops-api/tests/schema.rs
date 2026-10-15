//! The API contract: every type has a JSON Schema, error codes follow the operations spec, the
//! envelope has the spec's shape, and requests and responses keep their wire forms.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use swarmops_api::{
    API_VERSION, CatReq, ChangeInput, ChangeOpInput, ChangesetDetail, CommitReq, CommitResp,
    ContentEncoding, ContentInput, DEFAULT_LOG_LIMIT, DEFAULT_MAX_BYTES, EntryKindOut, Envelope,
    ErrorCode, FileContent, Identity, InitReq, LogPage, LogReq, LsReq, Meta, ModeInput, OpError,
    RefDeleteReq, RefListResp, RefSetReq, RefSetResp, RepoInfo, Rev, ShowReq, TreeListing,
    VerifyResp, WhoamiResp, schema_for_all,
};

/// Requests and the input types inside them: unknown fields are rejected.
const REQUESTS: &[&str] = &[
    "CatReq",
    "ChangeInput",
    "CommitReq",
    "Identity",
    "InitReq",
    "LogReq",
    "LsReq",
    "RefDeleteReq",
    "RefSetReq",
    "ShowReq",
];

/// Responses and the output types inside them: unknown fields are ignored.
const RESPONSES: &[&str] = &[
    "ChangeOut",
    "ChangesetDetail",
    "ChangesetSummary",
    "CommitResp",
    "FileContent",
    "LogPage",
    "Meta",
    "OpError",
    "RefListResp",
    "RefOut",
    "RefSetResp",
    "RepoInfo",
    "SignatureOut",
    "TreeEntryOut",
    "TreeListing",
    "VerifyResp",
    "WhoamiResp",
];

/// Enums and newtypes, which have no object fields of their own.
const SCALARS: &[&str] = &[
    "ChangeOpInput",
    "ContentEncoding",
    "ContentInput",
    "EntryKindOut",
    "ErrorCode",
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
    for name in REQUESTS {
        assert_eq!(all[*name]["type"], "object", "{name}");
        assert_eq!(
            all[*name]["additionalProperties"], false,
            "{name} must reject unknown fields"
        );
    }
    for name in RESPONSES {
        assert_eq!(all[*name]["type"], "object", "{name}");
        assert!(
            all[*name].get("additionalProperties").is_none(),
            "{name} must ignore unknown fields"
        );
    }
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
                details: Value::Null,
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

    let err = OpError::new(ErrorCode::CasMismatch, "main moved")
        .with_details(json!({ "actual": "c9f1" }))
        .with_hint("swarmops ref list");
    assert_eq!(err.details, json!({ "actual": "c9f1" }));
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
        .with_details(json!({ "actual": "d00d" }))
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
    // Without details or a hint, neither key appears.
    assert_eq!(
        serde_json::to_value(OpError::new(ErrorCode::RevNotFound, "no rev x")).unwrap(),
        json!({"code": "E_REV_NOT_FOUND", "message": "no rev x", "retryable": false, "exit": 6})
    );
    let text = serde_json::to_string(&envelope).unwrap();
    assert_eq!(
        serde_json::from_str::<Envelope<RefSetResp>>(&text).unwrap(),
        envelope
    );
}

fn with_unknown_field(mut value: Value) -> Value {
    value
        .as_object_mut()
        .expect("an object")
        .insert("zz_unknown".to_owned(), json!(1));
    value
}

fn assert_rejects_unknown_fields<T: DeserializeOwned>(name: &str, minimal: Value) {
    if let Err(e) = serde_json::from_value::<T>(minimal.clone()) {
        panic!("{name}: the minimal form must parse: {e}");
    }
    match serde_json::from_value::<T>(with_unknown_field(minimal)) {
        Ok(_) => panic!("{name} accepted an unknown field"),
        Err(e) => assert!(
            e.to_string().contains("unknown field `zz_unknown`"),
            "{name}: {e}"
        ),
    }
}

fn assert_ignores_unknown_fields<T: DeserializeOwned>(name: &str, minimal: Value) {
    if let Err(e) = serde_json::from_value::<T>(with_unknown_field(minimal)) {
        panic!("{name} must ignore unknown fields: {e}");
    }
}

#[test]
fn requests_reject_unknown_fields_and_responses_ignore_them() {
    assert_rejects_unknown_fields::<InitReq>("InitReq", json!({"path": "/srv/s", "name": "acme"}));
    assert_rejects_unknown_fields::<CommitReq>("CommitReq", json!({"message": "m", "changes": []}));
    assert_rejects_unknown_fields::<ChangeInput>(
        "ChangeInput",
        json!({"path": "a", "op": "delete"}),
    );
    assert_rejects_unknown_fields::<CatReq>("CatReq", json!({"rev": "main", "path": "a"}));
    assert_rejects_unknown_fields::<LsReq>("LsReq", json!({"rev": "main"}));
    assert_rejects_unknown_fields::<ShowReq>("ShowReq", json!({"rev": "main"}));
    assert_rejects_unknown_fields::<LogReq>("LogReq", json!({}));
    assert_rejects_unknown_fields::<RefSetReq>("RefSetReq", json!({"name": "main", "new": "c9f1"}));
    assert_rejects_unknown_fields::<RefDeleteReq>(
        "RefDeleteReq",
        json!({"name": "main", "expected_old": "c9f1"}),
    );
    assert_rejects_unknown_fields::<Identity>("Identity", json!({"agent": "human/purple"}));

    let signature = json!({"name": "human/purple", "email": "purple@human.swarmops.local", "secs": 0, "offset_secs": 0});
    assert_ignores_unknown_fields::<RepoInfo>(
        "RepoInfo",
        json!({"repo_id": "r", "name": "acme", "path": "/srv/s", "store_version": 1, "changesets": 0, "refs": 0}),
    );
    assert_ignores_unknown_fields::<WhoamiResp>("WhoamiResp", json!({"auth": "trust"}));
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
        json!({"entries": [{"name": "src", "kind": "dir", "id": "t", "zz_nested": 1}]}),
    );
    assert_ignores_unknown_fields::<ChangesetDetail>(
        "ChangesetDetail",
        json!({
            "id": "c", "seq": 0, "parents": [], "root": "t",
            "author": signature, "committer": signature, "created_at": 0, "message": "m",
            "changes": [{"path": "a", "op": "put", "kind": "file", "blob": "b"}, {"path": "b", "op": "delete"}],
            "metadata": {"agent": "human/purple"},
        }),
    );
    assert_ignores_unknown_fields::<LogPage>(
        "LogPage",
        json!({"entries": [{"id": "c", "parents": [], "author_agent": "human/purple", "created_at": 0, "title": "t"}]}),
    );
    assert_ignores_unknown_fields::<RefListResp>(
        "RefListResp",
        json!({"refs": [{"name": "main", "target": "c", "version": 1}]}),
    );
    assert_ignores_unknown_fields::<RefSetResp>(
        "RefSetResp",
        json!({"name": "main", "new": "c", "already_applied": false}),
    );
    assert_ignores_unknown_fields::<VerifyResp>(
        "VerifyResp",
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
        serde_json::from_value::<CatReq>(json!({"rev": "main", "path": "src/lib.rs"})).unwrap(),
        CatReq {
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
        op: ChangeOpInput::Put,
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
                    op: ChangeOpInput::Delete,
                    content: None,
                    mode: None,
                },
            ],
            allow_empty: false,
        }
    );
    // Serializing writes no nulls for the options left out.
    assert_eq!(
        serde_json::to_value(&commit.changes[3]).unwrap(),
        json!({"path": "old.txt", "op": "delete"})
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
        serde_json::to_value([ContentEncoding::Utf8, ContentEncoding::Base64]).unwrap(),
        json!(["utf8", "base64"])
    );
    for bad in [
        json!({"inline": "x", "base64": "eA=="}),
        json!({"blob": "c9f1"}),
        json!("x"),
    ] {
        assert!(
            serde_json::from_value::<ContentInput>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    assert!(serde_json::from_value::<ChangeOpInput>(json!("rename")).is_err());
    assert!(serde_json::from_value::<ModeInput>(json!("dir")).is_err());
}
