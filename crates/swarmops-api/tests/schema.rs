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
    FileKind, Identity, IdentityInfo, InfoReq, InitReq, LogCutOut, LogPage, LogReq, Meta,
    ModeInput, OpError, ReadFileReq, ReadTreeReq, RecoveredOut, RefDeleteReq, RefListReq,
    RefListResp, RefSetReq, RefSetResp, RepoInfo, Rev, ShowReq, SignatureOut, Signatures, StoreOps,
    TreeEntryOut, TreeListing, VerifyFinding, VerifyReport, VerifyReq, schema_for_all,
};

/// Requests and the input types inside them.
const REQUESTS: &[&str] = &[
    "BlobHasReq",
    "BlobPutReq",
    "ChangeInput",
    "CommitReq",
    "DeriveReq",
    "DiffReq",
    "GitExportReq",
    "GitImportReq",
    "Identity",
    "InfoReq",
    "InitReq",
    "LandReq",
    "LineRange",
    "LogReq",
    "OpLogReq",
    "PathHistoryReq",
    "ReadFileReq",
    "ReadTreeReq",
    "RefDeleteReq",
    "RefListReq",
    "RefSetReq",
    "ResolutionInput",
    "ResolutionsInput",
    "ResolveRevReq",
    "ShowReq",
    "UndoReq",
    "VerifyReq",
];

/// Responses and the output types inside them.
const RESPONSES: &[&str] = &[
    "Advanced",
    "Attribution",
    "BlobHasResp",
    "BlobPutResp",
    "ChangeOut",
    "ChangesetDetail",
    "ChangesetSummary",
    "CommitResp",
    "ConflictReport",
    "DeriveResp",
    "Diff",
    "DiffHunk",
    "DiffLineOut",
    "Envelope",
    "FileConflictOut",
    "FileContent",
    "FileDiff",
    "GitExportResp",
    "GitImportResp",
    "HunkOut",
    "IdentityInfo",
    "LandResp",
    "LinesOut",
    "LogCutOut",
    "LogPage",
    "LossyCommitOut",
    "Meta",
    "OpChangesetOut",
    "OpError",
    "OpLogPage",
    "OperationOut",
    "PathHistoryEntry",
    "PathHistoryPage",
    "RecoveredOut",
    "RefListResp",
    "RefMoveOut",
    "RefOut",
    "RefSetResp",
    "RepoInfo",
    "ResolvedRev",
    "SignatureOut",
    "Signatures",
    "TreeEntryOut",
    "TreeListing",
    "UndoResp",
    "VerifyFinding",
    "VerifyReport",
];

/// Enums and newtypes, which have no object fields of their own.
const SCALARS: &[&str] = &[
    "ChangeOp",
    "ConflictKindOut",
    "ContentEncoding",
    "ContentInput",
    "DiffFormat",
    "DiffStatus",
    "EntryKindOut",
    "ErrorCode",
    "FileKind",
    "LandStatus",
    "LineOp",
    "LossyClass",
    "ModeInput",
    "OperationKind",
    "Rev",
    "RevKind",
    "Side",
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
    let _: fn(&S, swarmops_api::ResolveRevReq) -> Result<swarmops_api::ResolvedRev, OpError> =
        S::resolve_rev;
    let _: fn(&S, ReadFileReq) -> Result<FileContent, OpError> = S::read_file;
    let _: fn(&S, ReadTreeReq) -> Result<TreeListing, OpError> = S::read_tree;
    let _: fn(&S, ShowReq) -> Result<ChangesetDetail, OpError> = S::show;
    let _: fn(&S, LogReq) -> Result<LogPage, OpError> = S::log;
    let _: fn(&S, swarmops_api::PathHistoryReq) -> Result<swarmops_api::PathHistoryPage, OpError> =
        S::path_history;
    let _: fn(&S, swarmops_api::DiffReq) -> Result<swarmops_api::Diff, OpError> = S::diff;
    let _: fn(&S, RefListReq) -> Result<RefListResp, OpError> = S::ref_list;
    let _: fn(&S, &Identity, RefSetReq) -> Result<RefSetResp, OpError> = S::ref_set;
    let _: fn(&S, &Identity, RefDeleteReq) -> Result<RefSetResp, OpError> = S::ref_delete;
    let _: fn(&S, VerifyReq) -> Result<VerifyReport, OpError> = S::verify;
    let _: fn(
        &S,
        &Identity,
        swarmops_api::BlobPutReq,
    ) -> Result<swarmops_api::BlobPutResp, OpError> = S::blob_put;
    let _: fn(&S, swarmops_api::BlobHasReq) -> Result<swarmops_api::BlobHasResp, OpError> =
        S::blob_has;
    let _: fn(&S, swarmops_api::DeriveReq) -> Result<swarmops_api::DeriveResp, OpError> = S::derive;
}

#[test]
fn every_operation_takes_a_request_and_returns_a_response() {
    store_ops_signatures::<dyn StoreOps>();
}

/// Each code's string, exit code and retry advice, written out independently of the crate's
/// table: from the operations spec (§0 exit codes, §1 and §4 names and retryability) where it
/// names the code, from the M1 plan and rulings for `E_UNSUPPORTED_FS`, `E_BUSY` and
/// `E_INTERNAL`, from the M4 plan for `E_NOTHING_TO_UNDO` and `E_OP_NOT_FOUND`, and by the
/// `E_<NAME>` convention for `E_TYPE_CONFLICT`, `E_UNKNOWN_PARENT` and `E_IO`. A new variant does
/// not compile here until its row is added.
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
        ErrorCode::IoTransient => ("E_IO_TRANSIENT", 4, true),
        ErrorCode::Busy => ("E_BUSY", 4, true),
        ErrorCode::Internal => ("E_INTERNAL", 1, false),
        ErrorCode::Conflict => ("E_CONFLICT", 3, false),
        ErrorCode::CasRetryExhausted => ("E_CAS_RETRY_EXHAUSTED", 4, true),
        ErrorCode::NothingToLand => ("E_NOTHING_TO_LAND", 1, false),
        ErrorCode::NothingToUndo => ("E_NOTHING_TO_UNDO", 1, false),
        ErrorCode::OpNotFound => ("E_OP_NOT_FOUND", 6, false),
        ErrorCode::Auth => ("E_AUTH", 5, false),
        ErrorCode::PolicyDenied => ("E_POLICY_DENIED", 5, false),
        ErrorCode::MaintenanceBusy => ("E_MAINTENANCE_BUSY", 4, true),
        ErrorCode::GitNotARepo => ("E_GIT_NOT_A_REPO", 6, false),
        ErrorCode::GitRefNotFound => ("E_GIT_REF_NOT_FOUND", 6, false),
        ErrorCode::GitImportDiverged => ("E_GIT_IMPORT_DIVERGED", 3, false),
        ErrorCode::GitImportLossy => ("E_GIT_IMPORT_LOSSY", 1, false),
        ErrorCode::GitUnsupported => ("E_GIT_UNSUPPORTED", 1, false),
        ErrorCode::GitRead => ("E_GIT_READ", 1, false),
        ErrorCode::GitExportDiverged => ("E_GIT_EXPORT_DIVERGED", 3, false),
        ErrorCode::GitWrite => ("E_GIT_WRITE", 1, false),
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
        recovered: None,
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
        advanced: None,
        warnings: Vec::new(),
        replayed: false,
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
            "result": {
                "changeset": "c9f1", "root": "7a0b", "files_changed": 2, "blobs_written": 1,
                "replayed": false,
            },
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

/// `meta.recovered` is there only when a write repaired something: after `auth`, every count
/// written out, and `log_cut` only when the journal was cut. It reads back as written, and its
/// schema holds offsets, counts and a flag, no path.
#[test]
fn meta_reports_a_recovery_only_when_a_write_made_one() {
    let mut meta = meta();
    let plain = serde_json::to_string(&meta).unwrap();
    assert_eq!(
        plain,
        r#"{"elapsed_ms":12,"resolved":{"main":"c9f1"},"auth":"trust"}"#
    );
    let counts = r#""frames_rewritten":0,"off_bytes_dropped":0,"index_records_dropped":0,"views_rebuilt":false"#;
    meta.recovered = Some(RecoveredOut {
        frames_listed: 1,
        records_replayed: 1,
        ..RecoveredOut::default()
    });
    let listed = serde_json::to_string(&meta).unwrap();
    let want = format!(
        r#"{{"elapsed_ms":12,"resolved":{{"main":"c9f1"}},"auth":"trust","recovered":{{"frames_listed":1,{counts},"records_replayed":1,"refs_replayed":0}}}}"#
    );
    assert_eq!(listed, want);
    assert_eq!(serde_json::from_str::<Meta>(&listed).unwrap(), meta);
    meta.recovered = Some(RecoveredOut {
        log_cut: Some(LogCutOut { at: 257, bytes: 18 }),
        ..RecoveredOut::default()
    });
    let cut = serde_json::to_string(&meta).unwrap();
    assert!(
        cut.ends_with(&format!(
            r#""recovered":{{"log_cut":{{"at":257,"bytes":18}},"frames_listed":0,{counts},"records_replayed":0,"refs_replayed":0}}}}"#
        )),
        "{cut}"
    );
    assert_eq!(serde_json::from_str::<Meta>(&cut).unwrap(), meta);

    let all = schema_for_all();
    for name in ["RecoveredOut", "LogCutOut"] {
        let properties = all[name]["properties"].as_object().unwrap();
        for (field, schema) in properties {
            let kind = &schema["type"];
            let numeric = kind == "integer" && schema["format"] == "uint64";
            assert!(
                numeric || kind == "boolean" || field == "log_cut",
                "{name}.{field}: {schema}"
            );
        }
    }
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

/// Whether `value` meets `branch`, a schema of the keywords the `oneOf` branches in this file
/// use: `required`, `properties` whose schemas are a `const` or a `type`, and `not`. Any other
/// keyword fails the test, so that a branch cannot gain a rule this reading would skip.
fn meets(branch: &Value, value: &Value) -> bool {
    let fields = value.as_object().expect("an object to check");
    let keyword = |(name, rule): (&String, &Value)| match name.as_str() {
        "required" => rule
            .as_array()
            .expect("required is an array")
            .iter()
            .all(|key| fields.contains_key(key.as_str().expect("a key"))),
        "properties" => rule
            .as_object()
            .expect("properties is an object")
            .iter()
            .all(|(key, schema)| fields.get(key).is_none_or(|field| is(schema, field))),
        "not" => !meets(rule, value),
        other => panic!("a keyword this test does not read: {other}"),
    };
    branch
        .as_object()
        .expect("a branch is a schema object")
        .iter()
        .all(keyword)
}

/// Whether `field` is what `schema`, a `const` or a `type`, allows.
fn is(schema: &Value, field: &Value) -> bool {
    let keyword = |(name, rule): (&String, &Value)| match name.as_str() {
        "const" => field == rule,
        "type" => match rule.as_str().expect("a type name") {
            "string" => field.is_string(),
            "null" => field.is_null(),
            other => panic!("a type this test does not read: {other}"),
        },
        other => panic!("a keyword this test does not read: {other}"),
    };
    schema
        .as_object()
        .expect("a property schema is an object")
        .iter()
        .all(keyword)
}

/// The branches of `schema`'s `oneOf` that `value` meets, by their index.
fn branches_met(schema: &Value, value: &Value) -> Vec<usize> {
    let branches = schema["oneOf"].as_array().expect("a oneOf");
    (0..branches.len())
        .filter(|&i| meets(&branches[i], value))
        .collect()
}

/// `Envelope`'s schema says what its two constructors keep: exactly one of `result` (when
/// `ok`) and `error` (when not), the other omitted, and `error` never null. The wire does not
/// change: each constructor's envelope meets its own branch alone, and the shapes they never
/// write meet none.
#[test]
fn envelope_schema_says_exactly_one_outcome() {
    let schema = &schema_for_all()["Envelope"];
    assert_eq!(
        schema["oneOf"],
        json!([
            {"properties": {"ok": {"const": true}}, "required": ["result"],
             "not": {"required": ["error"]}},
            {"properties": {"ok": {"const": false}}, "required": ["error"],
             "not": {"required": ["result"]}},
        ])
    );
    assert_eq!(
        schema["properties"]["error"],
        json!({
            "$ref": "#/$defs/OpError",
            "description": "The failure; present exactly when not `ok`.",
        })
    );
    assert_eq!(schema["required"], json!(["ok", "op", "api", "meta"]));

    let bare = || OpError::new(ErrorCode::RevNotFound, "no rev x");
    let error = serde_json::to_value(bare()).unwrap();
    let ok = serde_json::to_value(Envelope::ok("info", json!({}), meta())).unwrap();
    let failed = serde_json::to_value(Envelope::<Value>::err("info", bare(), meta())).unwrap();
    assert_eq!(branches_met(schema, &ok), [0]);
    assert_eq!(branches_met(schema, &failed), [1]);
    for never in [
        json!({"ok": true, "result": {}, "error": error}),
        json!({"ok": false, "result": {}, "error": error}),
        json!({"ok": true}),
        json!({"ok": false}),
        json!({"ok": true, "error": error}),
        json!({"ok": false, "result": {}}),
    ] {
        assert_eq!(branches_met(schema, &never), [0_usize; 0], "{never}");
    }
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
    assert_ignores_unknown_fields::<swarmops_api::BlobPutReq>(
        "BlobPutReq",
        json!({"content": {"inline": "x"}}),
    );
    assert_ignores_unknown_fields::<swarmops_api::BlobHasReq>("BlobHasReq", json!({"ids": ["b1"]}));

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
    assert_ignores_unknown_fields::<swarmops_api::BlobPutResp>(
        "BlobPutResp",
        json!({"blob": "b1", "size": 1, "written": true}),
    );
    assert_ignores_unknown_fields::<swarmops_api::BlobHasResp>(
        "BlobHasResp",
        json!({"present": ["b1"]}),
    );
    assert_ignores_unknown_fields::<RefSetResp>(
        "RefSetResp",
        json!({"name": "main", "new": "c", "already_applied": false}),
    );
    assert_ignores_unknown_fields::<VerifyReport>(
        "VerifyReport",
        json!({"objects_checked": 0, "changesets_checked": 0, "problems": []}),
    );
    assert_ignores_unknown_fields::<VerifyFinding>(
        "VerifyFinding",
        json!({"kind": "missing_object", "object": "b1", "changeset": "c9f1"}),
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
            offset: 0,
            lines: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    );
    assert_eq!(
        serde_json::from_value::<LogReq>(json!({})).unwrap(),
        LogReq {
            rev: None,
            limit: DEFAULT_LOG_LIMIT,
            cursor: None,
            paths: Vec::new(),
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
            advance: None,
            idempotency_key: None,
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
            advance: None,
            idempotency_key: None,
        }
    );
    // Serializing writes no nulls for the options left out, and keeps the spec's field order
    // (`{parent, changes}, message, allow_empty`).
    let rooted = CommitReq {
        parent: Some(Rev("main".to_owned())),
        changes: vec![commit.changes[3].clone()],
        message: "m".to_owned(),
        allow_empty: true,
        advance: None,
        idempotency_key: None,
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

/// A commit's `advance` sits between `message` and `allow_empty`, as the operations spec's row
/// has it. A commit that advanced a ref says so in `advanced`, whose `old` is omitted for a root
/// commit's move, which created the ref; a commit that advanced none omits `advanced` (no null).
#[test]
fn a_commit_that_advances_a_ref_says_so() {
    let advancing = CommitReq {
        parent: Some(Rev("c9f0".to_owned())),
        changes: Vec::new(),
        message: "m".to_owned(),
        advance: Some("main".to_owned()),
        idempotency_key: None,
        allow_empty: true,
    };
    assert_eq!(
        serde_json::to_string(&advancing).unwrap(),
        r#"{"parent":"c9f0","changes":[],"message":"m","advance":"main","allow_empty":true}"#
    );
    let moved = |old: Option<&str>| swarmops_api::Advanced {
        name: "main".to_owned(),
        old: old.map(str::to_owned),
        new: "c9f1".to_owned(),
        already_applied: false,
    };
    let resp = |parent: Option<&str>, advanced| CommitResp {
        changeset: "c9f1".to_owned(),
        parent: parent.map(str::to_owned),
        root: "7a0b".to_owned(),
        files_changed: 1,
        blobs_written: 1,
        advanced,
        warnings: Vec::new(),
        replayed: false,
    };
    let cases = [
        (
            resp(Some("c9f0"), Some(moved(Some("c9f0")))),
            json!({"name": "main", "old": "c9f0", "new": "c9f1", "already_applied": false}),
        ),
        (
            resp(None, Some(moved(None))),
            json!({"name": "main", "new": "c9f1", "already_applied": false}),
        ),
    ];
    for (resp, advanced) in cases {
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["advanced"], advanced);
        assert_eq!(serde_json::from_value::<CommitResp>(value).unwrap(), resp);
    }
    let plain = serde_json::to_value(resp(Some("c9f0"), None)).unwrap();
    assert!(plain.get("advanced").is_none(), "{plain}");
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
        lander: None,
        created_at: 1_760_000_000_000,
        message: "add files".to_owned(),
        land_key: None,
        squashed_from: Vec::new(),
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
        offset: None,
        lines: None,
        next_offset: None,
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

/// `RefSetReq`'s schema says what `ref set` checks: exactly one of `expected_old` and
/// `create`. Of the five shapes a request can take, the two that the operation accepts (its
/// side is `ref_set_takes_exactly_one_of_expected_old_and_create` in the ops tests) meet one
/// branch each, and the three it refuses meet none: both, neither, and a null `expected_old`.
/// The wire does not change: each shape still reads as a `RefSetReq`.
#[test]
fn ref_set_schema_says_exactly_one_of_expected_old_and_create() {
    let schema = &schema_for_all()["RefSetReq"];
    assert_eq!(
        schema["oneOf"],
        json!([
            {"required": ["expected_old"],
             "properties": {"expected_old": {"type": "string"}, "create": {"const": false}}},
            {"required": ["create"],
             "properties": {"create": {"const": true}, "expected_old": {"type": "null"}}},
        ])
    );
    // The shape, what it adds to a name and a new target, the branches it meets, and whether
    // the operation takes it.
    for (shape, fields, met, accepted) in [
        (
            "{expected_old}",
            json!({"expected_old": "b0b0"}),
            vec![0],
            true,
        ),
        ("{create}", json!({"create": true}), vec![1], true),
        (
            "{expected_old, create}",
            json!({"expected_old": "b0b0", "create": true}),
            vec![],
            false,
        ),
        ("{}", json!({}), vec![], false),
        (
            "{expected_old: null}",
            json!({"expected_old": null}),
            vec![],
            false,
        ),
    ] {
        let mut req = json!({"name": "main", "new": "c9f1"});
        for (key, value) in fields.as_object().unwrap() {
            req[key] = value.clone();
        }
        assert_eq!(branches_met(schema, &req), met, "{shape}");
        assert_eq!(met.len() == 1, accepted, "{shape}");
        serde_json::from_value::<RefSetReq>(req).unwrap_or_else(|e| panic!("{shape}: {e}"));
    }
}

/// `verify`'s report carries `findings` beside `problems`, for a program to act on: a finding
/// requires only its `kind`, a string with no list of values, so that a client reads kinds
/// newer than its own; it has no message, since `problems` holds the line at the same index; a
/// report written before findings existed reads as having none; and a finding omits the keys
/// its kind does not use and calls its ref `ref`.
#[test]
fn verify_report_carries_findings_beside_problems() {
    let all = schema_for_all();
    let report = &all["VerifyReport"];
    assert_eq!(
        report["properties"]["findings"]["items"],
        json!({"$ref": "#/$defs/VerifyFinding"})
    );
    assert_eq!(report["properties"]["findings"]["type"], "array");
    assert_eq!(report["properties"]["findings"]["default"], json!([]));
    assert_eq!(
        report["required"],
        json!(["objects_checked", "changesets_checked", "problems"])
    );
    let finding = &all["VerifyFinding"];
    assert_eq!(finding["required"], json!(["kind"]));
    assert_eq!(finding["properties"]["kind"]["type"], "string");
    assert!(finding["properties"]["kind"].get("enum").is_none());
    let keys: BTreeSet<&str> = finding["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "kind",
            "object",
            "changeset",
            "parent",
            "seq",
            "offset",
            "ref",
            "cause",
            "system",
            "key",
            "derivation"
        ])
    );

    let before: VerifyReport = serde_json::from_value(
        json!({"objects_checked": 3, "changesets_checked": 1, "problems": []}),
    )
    .unwrap();
    assert_eq!(before.findings, []);
    let dangling = VerifyFinding {
        kind: "dangling_ref".to_owned(),
        object: None,
        changeset: None,
        parent: None,
        seq: None,
        offset: None,
        ref_name: Some("main".to_owned()),
        cause: None,
        system: None,
        key: None,
        derivation: None,
    };
    let report = VerifyReport {
        objects_checked: 3,
        changesets_checked: 1,
        problems: vec!["ref main names no changeset in the store".to_owned()],
        findings: vec![dangling],
    };
    let text = serde_json::to_string(&report).unwrap();
    assert_eq!(
        text,
        r#"{"objects_checked":3,"changesets_checked":1,"problems":["ref main names no changeset in the store"],"findings":[{"kind":"dangling_ref","ref":"main"}]}"#
    );
    assert_eq!(serde_json::from_str::<VerifyReport>(&text).unwrap(), report);
    let clean = VerifyReport {
        problems: Vec::new(),
        findings: Vec::new(),
        ..report
    };
    assert_eq!(
        serde_json::to_string(&clean).unwrap(),
        r#"{"objects_checked":3,"changesets_checked":1,"problems":[],"findings":[]}"#
    );
}

#[test]
fn listings_page_with_truncated_and_cursor() {
    // Default pages within the operations spec's 64 KiB target for a default response (§9).
    assert_eq!(DEFAULT_READ_TREE_LIMIT, 250);
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
            depth: 1,
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
            depth: 1,
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
            paths: Vec::new(),
        }
    );

    let entry = |name: &str| TreeEntryOut {
        name: name.to_owned(),
        path: None,
        kind: EntryKindOut::File,
        size: None,
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

/// `docs/architecture.md` says how many types `swarmops schema` prints, and the count is the
/// schema's: it drifted as types were added (41 for 42, then 44 for 45).
#[test]
fn the_architecture_overview_counts_the_schema_s_types() {
    const OVERVIEW: &str = include_str!("../../../docs/architecture.md");
    let text = OVERVIEW.split_whitespace().collect::<Vec<_>>().join(" ");
    let said = text
        .split_once("prints the JSON Schema of ")
        .and_then(|(_, rest)| rest.split_once(" types"))
        .and_then(|(count, _)| count.parse::<usize>().ok());
    let types = schema_for_all().as_object().map(Map::len);
    assert_eq!(
        said, types,
        "docs/architecture.md: `swarmops schema` prints the JSON Schema of <n> types"
    );
}

/// `path_history` and `log`'s `paths` (M2 Task 15).
mod path_history {
    use swarmops_api::{
        DEFAULT_PATH_HISTORY_LIMIT, PathHistoryEntry, PathHistoryPage, PathHistoryReq,
    };

    use super::*;

    /// A history request takes `main` and 50 entries by default, a limit of at least 1, and a
    /// cursor; `log`'s `paths` default to none and are omitted when empty. A page keeps the spec's
    /// field order, an entry after a delete has no kind and no id, and both ignore fields they do
    /// not know.
    #[test]
    fn history_requests_and_pages_keep_their_wire_form() {
        assert_eq!(DEFAULT_PATH_HISTORY_LIMIT, 50);
        let all = schema_for_all();
        assert_eq!(all["PathHistoryReq"]["properties"]["limit"]["minimum"], 1);
        assert_eq!(all["PathHistoryReq"]["required"], json!(["path"]));
        assert_eq!(
            serde_json::from_value::<PathHistoryReq>(json!({"path": "src"})).unwrap(),
            PathHistoryReq {
                rev: None,
                path: "src".to_owned(),
                limit: DEFAULT_PATH_HISTORY_LIMIT,
                cursor: None,
            }
        );
        let req = PathHistoryReq {
            rev: Some(Rev("main~1".to_owned())),
            path: "src/".to_owned(),
            limit: 2,
            cursor: Some("c9f1".to_owned()),
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"rev":"main~1","path":"src/","limit":2,"cursor":"c9f1"}"#
        );
        assert_ignores_unknown_fields::<PathHistoryReq>("PathHistoryReq", json!({"path": "a"}));

        let log = serde_json::from_value::<LogReq>(json!({"paths": ["src", "README.md"]})).unwrap();
        assert_eq!(log.paths, ["src", "README.md"]);
        let every = serde_json::to_value(LogReq {
            paths: Vec::new(),
            ..log.clone()
        })
        .unwrap();
        assert!(every.get("paths").is_none(), "{every}");
        assert_eq!(
            serde_json::to_value(&log).unwrap()["paths"],
            json!(["src", "README.md"])
        );
        assert!(
            all["LogReq"]["required"]
                .as_array()
                .is_none_or(|required| !required.contains(&json!("paths")))
        );

        let entry = |changeset: &str, put: bool| PathHistoryEntry {
            changeset: changeset.to_owned(),
            op: if put { ChangeOp::Put } else { ChangeOp::Delete },
            kind: put.then_some(EntryKindOut::Dir),
            id: put.then(|| "7a0b".to_owned()),
            author_agent: "human/purple".to_owned(),
            created_at: 1_760_000_000_000,
            title: "t".to_owned(),
        };
        let page = PathHistoryPage {
            path: "src".to_owned(),
            entries: vec![entry("c9f1", false), entry("b0b0", true)],
            truncated: true,
            cursor: Some("b0b0".to_owned()),
        };
        let text = serde_json::to_string(&page).unwrap();
        assert_eq!(
            text,
            [
                r#"{"path":"src","entries":["#,
                r#"{"changeset":"c9f1","op":"delete","author_agent":"human/purple","created_at":1760000000000,"title":"t"},"#,
                r#"{"changeset":"b0b0","op":"put","kind":"dir","id":"7a0b","author_agent":"human/purple","created_at":1760000000000,"title":"t"}"#,
                r#"],"truncated":true,"cursor":"b0b0"}"#,
            ]
            .concat()
        );
        assert_eq!(
            serde_json::from_str::<PathHistoryPage>(&text).unwrap(),
            page
        );
        assert_ignores_unknown_fields::<PathHistoryPage>(
            "PathHistoryPage",
            json!({"path": "a", "entries": [{"changeset": "c", "op": "put", "kind": "file", "id": "b", "author_agent": "human/purple", "created_at": 0, "title": "t", "zz_nested": 1}], "truncated": false}),
        );
        assert_eq!(
            all["PathHistoryEntry"]["properties"]["created_at"]["format"],
            "uint64"
        );
    }
}

/// A blob named by id is `{"blob": "<id>"}` content, beside `inline`, `base64` and `file`, and
/// `blob_put` and `blob_has` keep their wire forms: `blob_put` takes any content and answers
/// with the blob's id, size and whether it wrote it; `blob_has` takes ids and gives the present
/// ones.
#[test]
fn blobs_by_id_use_their_wire_forms() {
    use swarmops_api::{BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp};

    let id = "ab".repeat(32);
    let change: ChangeInput =
        serde_json::from_value(json!({"path": "a.bin", "op": "put", "content": {"blob": id}}))
            .unwrap();
    assert_eq!(change.content, Some(ContentInput::Blob(id.clone())));
    assert_eq!(
        serde_json::to_value(&change).unwrap(),
        json!({"path": "a.bin", "op": "put", "content": {"blob": id}})
    );
    let put: BlobPutReq = serde_json::from_value(json!({"content": {"base64": "AAE="}})).unwrap();
    assert_eq!(put.content, ContentInput::Base64("AAE=".to_owned()));
    let stored = BlobPutResp {
        blob: id.clone(),
        size: 2,
        written: false,
    };
    assert_eq!(
        serde_json::to_string(&stored).unwrap(),
        format!(r#"{{"blob":"{id}","size":2,"written":false}}"#)
    );
    let has: BlobHasReq = serde_json::from_value(json!({"ids": [id]})).unwrap();
    assert_eq!(has.ids, vec![id.clone()]);
    let present = BlobHasResp {
        present: vec![id.clone()],
    };
    assert_eq!(
        serde_json::to_string(&present).unwrap(),
        format!(r#"{{"present":["{id}"]}}"#)
    );
}

/// `read_file` at an offset or by lines, and `read_tree` to a depth with sizes (operations spec
/// §1): their defaults, their bounds in the schema, and their wire forms.
mod spans {
    use swarmops_api::{LineRange, MAX_TREE_DEPTH};

    use super::*;

    #[test]
    fn offsets_lines_and_depths_take_their_defaults_and_bounds() {
        assert_eq!(MAX_TREE_DEPTH, 64);
        let all = schema_for_all();
        assert_eq!(all["ReadFileReq"]["properties"]["offset"]["default"], 0);
        let depth = &all["ReadTreeReq"]["properties"]["depth"];
        assert_eq!(
            (&depth["default"], &depth["minimum"], &depth["maximum"]),
            (&json!(1), &json!(1), &json!(MAX_TREE_DEPTH))
        );
        for field in ["first", "last"] {
            assert_eq!(
                all["LineRange"]["properties"][field]["minimum"], 1,
                "{field}"
            );
        }
        let by_lines = json!({"rev": "main", "path": "a", "lines": {"first": 3, "last": 5}});
        let req: ReadFileReq = serde_json::from_value(by_lines).unwrap();
        assert_eq!(
            (req.offset, req.lines),
            (0, Some(LineRange { first: 3, last: 5 }))
        );
        let req: ReadTreeReq = serde_json::from_value(json!({"rev": "main"})).unwrap();
        assert_eq!(req.depth, 1);
    }

    #[test]
    fn a_part_of_a_file_and_an_entry_below_say_where_they_are() {
        let part = FileContent {
            blob: "b1".to_owned(),
            size: 30,
            kind: FileKind::File,
            encoding: ContentEncoding::Utf8,
            content: "x\n".to_owned(),
            truncated: true,
            offset: Some(10),
            lines: Some(LineRange { first: 3, last: 5 }),
            next_offset: Some(12),
        };
        assert_eq!(
            serde_json::to_string(&part).unwrap(),
            r#"{"blob":"b1","size":30,"kind":"file","encoding":"utf8","content":"x\n","truncated":true,"offset":10,"lines":{"first":3,"last":5},"next_offset":12}"#
        );
        let below = TreeEntryOut {
            name: "b.rs".to_owned(),
            path: Some("a/b.rs".to_owned()),
            kind: EntryKindOut::File,
            size: Some(10),
            id: "b1".to_owned(),
        };
        assert_eq!(
            serde_json::to_string(&below).unwrap(),
            r#"{"name":"b.rs","path":"a/b.rs","kind":"file","size":10,"id":"b1"}"#
        );
    }
}

/// `derive`'s request and response (operations spec §1): `after` and `limit`, 1,000 frames by
/// default, in; the counts and `next` out, `next` omitted when there is none.
mod derive {
    use swarmops_api::{DEFAULT_DERIVE_LIMIT, DeriveReq, DeriveResp};

    use super::*;

    #[test]
    fn derive_takes_after_and_limit_and_gives_counts_and_next() {
        assert_eq!(DEFAULT_DERIVE_LIMIT, 1000);
        let defaults: DeriveReq = serde_json::from_value(json!({})).unwrap();
        let limit = DEFAULT_DERIVE_LIMIT;
        assert_eq!(defaults, DeriveReq { after: None, limit });
        assert_ignores_unknown_fields::<DeriveReq>("DeriveReq", json!({"after": 7}));
        assert_ignores_unknown_fields::<DeriveResp>(
            "DeriveResp",
            json!({"derived": 1, "cached": 2, "next": 7}),
        );
        let req = DeriveReq {
            after: None,
            limit: 5,
        };
        assert_eq!(serde_json::to_value(&req).unwrap(), json!({"limit": 5}));
        let resp = DeriveResp {
            derived: 1,
            cached: 2,
            next: None,
        };
        let printed = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            printed, r#"{"derived":1,"cached":2}"#,
            "in this order, next omitted"
        );
    }
}

/// `diff`'s request and page on the wire (operations spec §1).
mod diff {
    use swarmops_api::{
        DEFAULT_DIFF_CONTEXT, DEFAULT_DIFF_LIMIT, DEFAULT_DIFF_MAX_BYTES, Diff, DiffFormat,
        DiffHunk, DiffLineOut, DiffReq, DiffStatus, FileDiff, LineOp,
    };

    use super::*;

    /// A request needs only `from` and `to`: the rest default to `names`, three lines of
    /// context, 500 files and 32 KiB, and an unknown field is ignored. It writes its fields in
    /// the spec's order, without nulls or an empty `paths`; its limits start at 1; and its
    /// format and a page's statuses and line operations have their wire names.
    #[test]
    fn diff_requests_keep_their_defaults_and_wire_names() {
        assert_eq!(
            (
                DEFAULT_DIFF_LIMIT,
                DEFAULT_DIFF_MAX_BYTES,
                DEFAULT_DIFF_CONTEXT
            ),
            (500, 32_768, 3)
        );
        let minimal = json!({"from": "main~1", "to": "main"});
        let req: DiffReq = serde_json::from_value(minimal.clone()).unwrap();
        assert_eq!(
            req,
            DiffReq {
                from: Rev::from("main~1"),
                to: Rev::from("main"),
                paths: Vec::new(),
                format: DiffFormat::Names,
                context: DEFAULT_DIFF_CONTEXT,
                limit: DEFAULT_DIFF_LIMIT,
                cursor: None,
                max_bytes: DEFAULT_DIFF_MAX_BYTES,
            }
        );
        assert_ignores_unknown_fields::<DiffReq>("DiffReq", minimal);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"from":"main~1","to":"main","format":"names","context":3,"limit":500,"max_bytes":32768}"#
        );
        let full = DiffReq {
            paths: vec!["src".to_owned()],
            format: DiffFormat::Unified,
            cursor: Some("src/a.rs".to_owned()),
            ..req
        };
        assert_eq!(
            serde_json::to_string(&full).unwrap(),
            r#"{"from":"main~1","to":"main","paths":["src"],"format":"unified","context":3,"limit":500,"cursor":"src/a.rs","max_bytes":32768}"#
        );
        let all = schema_for_all();
        let schema = &all["DiffReq"];
        assert_eq!(schema["required"], json!(["from", "to"]));
        for field in ["limit", "max_bytes"] {
            assert_eq!(schema["properties"][field]["minimum"], 1, "{field}");
        }
        assert_eq!(
            serde_json::to_value([DiffFormat::Names, DiffFormat::Stat, DiffFormat::Unified])
                .unwrap(),
            json!(["names", "stat", "unified"])
        );
        assert_eq!(
            serde_json::to_value([
                DiffStatus::Added,
                DiffStatus::Deleted,
                DiffStatus::Modified,
                DiffStatus::TypeChanged
            ])
            .unwrap(),
            json!(["added", "deleted", "modified", "type_changed"])
        );
        assert_eq!(
            serde_json::to_value([LineOp::Context, LineOp::Add, LineOp::Remove]).unwrap(),
            json!(["context", "add", "remove"])
        );
        assert!(serde_json::from_value::<DiffFormat>(json!("patch")).is_err());
    }

    /// A page: each file writes only the fields its format gives (a `names` file its path and
    /// status, a `unified` one everything), in the spec's order, without nulls; the page reads
    /// back as written, and ignores fields it does not know, a file's too.
    #[test]
    fn diff_pages_write_what_each_format_gives() {
        let named = FileDiff {
            path: "a.txt".to_owned(),
            status: DiffStatus::Deleted,
            old_kind: None,
            new_kind: None,
            old_blob: None,
            new_blob: None,
            binary: None,
            insertions: None,
            deletions: None,
            hunks: None,
            hunks_truncated: None,
        };
        let line = |op, text: &str| DiffLineOut {
            op,
            text: text.to_owned(),
        };
        let unified = FileDiff {
            path: "b.txt".to_owned(),
            status: DiffStatus::Modified,
            old_kind: Some(FileKind::File),
            new_kind: Some(FileKind::Exec),
            old_blob: Some("b0".to_owned()),
            new_blob: Some("b1".to_owned()),
            binary: Some(false),
            insertions: Some(1),
            deletions: Some(1),
            hunks: Some(vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![line(LineOp::Remove, "x\n"), line(LineOp::Add, "y")],
            }]),
            hunks_truncated: Some(true),
        };
        let page = Diff {
            from: "c0".to_owned(),
            to: "c1".to_owned(),
            files: vec![named, unified],
            truncated: true,
            cursor: Some("b.txt".to_owned()),
        };
        let text = serde_json::to_string(&page).unwrap();
        assert_eq!(
            text,
            [
                r#"{"from":"c0","to":"c1","files":[{"path":"a.txt","status":"deleted"},"#,
                r#"{"path":"b.txt","status":"modified","old_kind":"file","new_kind":"exec","#,
                r#""old_blob":"b0","new_blob":"b1","binary":false,"insertions":1,"deletions":1,"#,
                r#""hunks":[{"old_start":1,"old_lines":1,"new_start":1,"new_lines":1,"lines":"#,
                r#"[{"op":"remove","text":"x\n"},{"op":"add","text":"y"}]}],"#,
                r#""hunks_truncated":true}],"truncated":true,"cursor":"b.txt"}"#,
            ]
            .concat()
        );
        assert_eq!(serde_json::from_str::<Diff>(&text).unwrap(), page);
        assert_ignores_unknown_fields::<Diff>(
            "Diff",
            json!({"from": "c0", "to": "c1", "files": [{"path": "a", "status": "added", "zz_nested": 1}], "truncated": false}),
        );
    }
}

/// `resolve_rev` takes a revision and answers with the full id of the changeset it names and its
/// kind, `ref` or `changeset` in snake case; both ignore fields they do not know, as every request
/// and response does.
#[test]
fn resolve_rev_uses_its_wire_forms() {
    use swarmops_api::{ResolveRevReq, ResolvedRev, RevKind};

    assert_ignores_unknown_fields::<ResolveRevReq>("ResolveRevReq", json!({"rev": "main~1"}));
    assert_ignores_unknown_fields::<ResolvedRev>("ResolvedRev", json!({"id": "c", "kind": "ref"}));
    let resolved = ResolvedRev {
        id: "c".to_owned(),
        kind: RevKind::Changeset,
    };
    assert_eq!(
        serde_json::to_value(&resolved).unwrap(),
        json!({"id": "c", "kind": "changeset"})
    );
    assert_eq!(serde_json::to_value(RevKind::Ref).unwrap(), "ref");
}

/// `git import`'s contract (M3 Decisions 7, 13 and 14): a request with its defaults, the
/// classes of content an import may take lossily, the response, and six error codes. The
/// operation runs on the client, as `init` does, so it is no `StoreOps` method.
mod git_import {
    use serde_json::json;
    use swarmops_api::{
        ErrorCode, GitImportReq, GitImportResp, LossyClass, LossyCommitOut, OpError, schema_for_all,
    };

    use super::assert_ignores_unknown_fields;

    /// A request names only the repository: it imports what `HEAD` reaches into `main`, and
    /// takes nothing lossily. The schema gives the same defaults, and the other fields are taken
    /// as given and written back in the operations spec's order.
    #[test]
    fn a_git_import_request_takes_its_defaults() {
        assert_eq!(
            serde_json::from_value::<GitImportReq>(json!({"git_path": "x"})).unwrap(),
            GitImportReq {
                git_path: "x".to_owned(),
                git_ref: "HEAD".to_owned(),
                target_ref: "main".to_owned(),
                lossy: Vec::new(),
            }
        );
        assert!(serde_json::from_value::<GitImportReq>(json!({"git_ref": "HEAD"})).is_err());
        let schema = &schema_for_all()["GitImportReq"];
        assert_eq!(schema["required"], json!(["git_path"]));
        let properties = &schema["properties"];
        assert_eq!(properties["git_ref"]["default"], "HEAD");
        assert_eq!(properties["target_ref"]["default"], "main");
        assert_eq!(properties["lossy"]["default"], json!([]));
        assert_eq!(
            properties["lossy"]["items"],
            json!({"$ref": "#/$defs/LossyClass"})
        );

        let wire = r#"{"git_path":"../example","git_ref":"refs/heads/stable","target_ref":"release/example","lossy":["gitlinks","tree_order"]}"#;
        let req: GitImportReq = serde_json::from_str(wire).unwrap();
        assert_eq!(
            req,
            GitImportReq {
                git_path: "../example".to_owned(),
                git_ref: "refs/heads/stable".to_owned(),
                target_ref: "release/example".to_owned(),
                lossy: vec![LossyClass::Gitlinks, LossyClass::TreeOrder],
            }
        );
        assert_eq!(serde_json::to_string(&req).unwrap(), wire);
        assert_ignores_unknown_fields::<GitImportReq>("GitImportReq", json!({"git_path": "x"}));
    }

    /// The six classes have one spelling each, the one `git.lossy` metadata and the CLI's
    /// `--lossy` use: snake case, as `as_str` gives it, in `ALL`'s order; the schema lists the
    /// same six, each with what an import does to it.
    #[test]
    fn lossy_classes_are_snake_case() {
        let names = [
            "gitlinks",
            "paths",
            "modes",
            "tree_order",
            "empty_trees",
            "nonstandard",
        ];
        assert_eq!(serde_json::to_value(LossyClass::ALL).unwrap(), json!(names));
        for (class, name) in LossyClass::ALL.into_iter().zip(names) {
            assert_eq!(class.as_str(), name);
            assert_eq!(
                serde_json::from_value::<LossyClass>(json!(name)).unwrap(),
                class
            );
        }
        for wrong in [
            "TreeOrder",
            "treeOrder",
            "tree-order",
            "empty_tree",
            "all",
            "",
        ] {
            assert!(
                serde_json::from_value::<LossyClass>(json!(wrong)).is_err(),
                "{wrong}"
            );
        }
        let schema = &schema_for_all()["LossyClass"];
        let listed: Vec<&str> = schema["oneOf"]
            .as_array()
            .expect("LossyClass's schema is a oneOf of its classes")
            .iter()
            .map(|class| {
                assert!(class["description"].is_string(), "{class}");
                class["const"].as_str().expect("one class a branch")
            })
            .collect();
        assert_eq!(listed, names);
    }

    /// `E_GIT_IMPORT_DIVERGED` exits 3, the conflict exit; a repository or a ref that is not
    /// there exits 6, as the store's own not-founds do; the rest exit 1. None is retryable:
    /// running the same import again meets the same source.
    #[test]
    fn the_git_error_codes_have_their_exits() {
        for (code, name, exit) in [
            (ErrorCode::GitNotARepo, "E_GIT_NOT_A_REPO", 6),
            (ErrorCode::GitRefNotFound, "E_GIT_REF_NOT_FOUND", 6),
            (ErrorCode::GitImportDiverged, "E_GIT_IMPORT_DIVERGED", 3),
            (ErrorCode::GitImportLossy, "E_GIT_IMPORT_LOSSY", 1),
            (ErrorCode::GitUnsupported, "E_GIT_UNSUPPORTED", 1),
            (ErrorCode::GitRead, "E_GIT_READ", 1),
        ] {
            assert_eq!(
                (code.as_str(), code.exit(), code.retryable()),
                (name, exit, false),
                "{code:?}"
            );
            assert!(ErrorCode::ALL.contains(&code), "{name}");
            assert_eq!(serde_json::to_value(code).unwrap(), name);
            let error = OpError::new(code, "m");
            assert_eq!(
                (error.code.as_str(), error.exit, error.retryable),
                (name, exit, false)
            );
        }
    }

    /// The response follows the operations spec's field order; `old_head` is omitted when the
    /// import created the target, and `warnings` when there are none, while `lossy` and
    /// `lossy_total` are always written. A lossy commit names the git commit, its changeset and
    /// its classes.
    #[test]
    fn a_git_import_response_follows_the_spec_shape() {
        let created = GitImportResp {
            imported: 5,
            skipped: 0,
            head_changeset: "c9f1".to_owned(),
            git_head_sha: "4b82".to_owned(),
            target_ref: "main".to_owned(),
            ref_moved: true,
            old_head: None,
            lossy: Vec::new(),
            lossy_total: 0,
            warnings: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&created).unwrap(),
            r#"{"imported":5,"skipped":0,"head_changeset":"c9f1","git_head_sha":"4b82","target_ref":"main","ref_moved":true,"lossy":[],"lossy_total":0}"#
        );
        let again = GitImportResp {
            imported: 3,
            skipped: 1,
            old_head: Some("b0b0".to_owned()),
            lossy: vec![LossyCommitOut {
                commit: "4b82".to_owned(),
                changeset: "c9f1".to_owned(),
                classes: vec![LossyClass::EmptyTrees, LossyClass::Gitlinks],
            }],
            lossy_total: 1,
            warnings: vec!["the ref move is committed".to_owned()],
            ..created
        };
        let text = serde_json::to_string(&again).unwrap();
        assert_eq!(
            text,
            [
                r#"{"imported":3,"skipped":1,"head_changeset":"c9f1","git_head_sha":"4b82","#,
                r#""target_ref":"main","ref_moved":true,"old_head":"b0b0","#,
                r#""lossy":[{"commit":"4b82","changeset":"c9f1","classes":["empty_trees","gitlinks"]}],"#,
                r#""lossy_total":1,"warnings":["the ref move is committed"]}"#,
            ]
            .concat()
        );
        assert_eq!(serde_json::from_str::<GitImportResp>(&text).unwrap(), again);

        let all = schema_for_all();
        assert_eq!(
            all["GitImportResp"]["required"],
            json!([
                "imported",
                "skipped",
                "head_changeset",
                "git_head_sha",
                "target_ref",
                "ref_moved",
                "lossy",
                "lossy_total"
            ])
        );
        assert_eq!(
            all["LossyCommitOut"]["required"],
            json!(["commit", "changeset", "classes"])
        );
        assert_ignores_unknown_fields::<GitImportResp>(
            "GitImportResp",
            json!({
                "imported": 0, "skipped": 1, "head_changeset": "c9f1", "git_head_sha": "4b82",
                "target_ref": "main", "ref_moved": false, "old_head": "c9f1",
                "lossy": [{"commit": "4b82", "changeset": "c9f1", "classes": ["paths"], "zz_nested": 1}],
                "lossy_total": 1,
            }),
        );
        assert_ignores_unknown_fields::<LossyCommitOut>(
            "LossyCommitOut",
            json!({"commit": "4b82", "changeset": "c9f1", "classes": []}),
        );
    }
}

/// `git export`'s contract (M8 Decisions 1, 8 to 11): a request with its defaults, the
/// response, and two error codes. The operation runs on the client, as `init` and `git import`
/// do, so it is no `StoreOps` method.
mod git_export {
    use serde_json::json;
    use swarmops_api::{ErrorCode, GitExportReq, GitExportResp, OpError, schema_for_all};

    use super::assert_ignores_unknown_fields;

    /// A request names only the repository: it exports `main` into the branch `main`, and a
    /// diverged branch stops it. The schema gives the same defaults, the other fields are taken
    /// as given and written back in the operations spec's order, and `git_path` says that it is
    /// read and written on the caller's host.
    #[test]
    fn a_git_export_request_takes_its_defaults() {
        assert_eq!(
            serde_json::from_value::<GitExportReq>(json!({"git_path": "x"})).unwrap(),
            GitExportReq {
                git_path: "x".to_owned(),
                source_ref: "main".to_owned(),
                branch: "main".to_owned(),
                force: false,
            }
        );
        assert!(serde_json::from_value::<GitExportReq>(json!({"branch": "main"})).is_err());
        let schema = &schema_for_all()["GitExportReq"];
        assert_eq!(schema["required"], json!(["git_path"]));
        let properties = &schema["properties"];
        assert_eq!(properties["source_ref"]["default"], "main");
        assert_eq!(properties["branch"]["default"], "main");
        assert_eq!(properties["force"]["default"], false);
        let path = properties["git_path"]["description"].as_str().unwrap();
        assert!(path.contains("caller's host"), "{path}");

        let wire = r#"{"git_path":"../example","source_ref":"release/example","branch":"stable","force":true}"#;
        let req: GitExportReq = serde_json::from_str(wire).unwrap();
        assert_eq!(
            req,
            GitExportReq {
                git_path: "../example".to_owned(),
                source_ref: "release/example".to_owned(),
                branch: "stable".to_owned(),
                force: true,
            }
        );
        assert_eq!(serde_json::to_string(&req).unwrap(), wire);
        assert_ignores_unknown_fields::<GitExportReq>("GitExportReq", json!({"git_path": "x"}));
    }

    /// `E_GIT_EXPORT_DIVERGED` exits 3, the conflict exit, as `E_GIT_IMPORT_DIVERGED` does;
    /// `E_GIT_WRITE` exits 1. Neither is retryable: running the same export again meets the
    /// same branch, and a hint says what to do first.
    #[test]
    fn the_git_export_codes_have_their_exits() {
        for (code, name, exit) in [
            (ErrorCode::GitExportDiverged, "E_GIT_EXPORT_DIVERGED", 3),
            (ErrorCode::GitWrite, "E_GIT_WRITE", 1),
        ] {
            assert_eq!(
                (code.as_str(), code.exit(), code.retryable()),
                (name, exit, false),
                "{code:?}"
            );
            assert!(ErrorCode::ALL.contains(&code), "{name}");
            assert_eq!(serde_json::to_value(code).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<ErrorCode>(json!(name)).unwrap(),
                code
            );
            let error = OpError::new(code, "m");
            assert_eq!(
                (error.code.as_str(), error.exit, error.retryable),
                (name, exit, false)
            );
        }
    }

    /// The response follows the operations spec's field order; `old_sha` is omitted when the
    /// export created the branch, and `warnings` when there are none, while the counts and
    /// flags are always written.
    #[test]
    fn a_git_export_response_omits_what_it_lacks() {
        let created = GitExportResp {
            exported: 5,
            objects_written: 17,
            head_changeset: "c9f1".to_owned(),
            head_sha: "4b82".to_owned(),
            branch: "main".to_owned(),
            old_sha: None,
            branch_moved: true,
            incremental: false,
            rewritten: 0,
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&created).unwrap();
        assert!(value.get("old_sha").is_none(), "{value}");
        assert!(value.get("warnings").is_none(), "{value}");
        assert_eq!(
            serde_json::to_string(&created).unwrap(),
            [
                r#"{"exported":5,"objects_written":17,"head_changeset":"c9f1","#,
                r#""head_sha":"4b82","branch":"main","branch_moved":true,"#,
                r#""incremental":false,"rewritten":0}"#,
            ]
            .concat()
        );
        let again = GitExportResp {
            exported: 1,
            objects_written: 3,
            old_sha: Some("b0b0".to_owned()),
            incremental: true,
            rewritten: 2,
            warnings: vec!["2 exported commits are new commits".to_owned()],
            ..created
        };
        let text = serde_json::to_string(&again).unwrap();
        assert_eq!(
            text,
            [
                r#"{"exported":1,"objects_written":3,"head_changeset":"c9f1","#,
                r#""head_sha":"4b82","branch":"main","old_sha":"b0b0","branch_moved":true,"#,
                r#""incremental":true,"rewritten":2,"#,
                r#""warnings":["2 exported commits are new commits"]}"#,
            ]
            .concat()
        );
        assert_eq!(serde_json::from_str::<GitExportResp>(&text).unwrap(), again);

        assert_eq!(
            schema_for_all()["GitExportResp"]["required"],
            json!([
                "exported",
                "objects_written",
                "head_changeset",
                "head_sha",
                "branch",
                "branch_moved",
                "incremental",
                "rewritten"
            ])
        );
        assert_ignores_unknown_fields::<GitExportResp>(
            "GitExportResp",
            json!({
                "exported": 0, "objects_written": 0, "head_changeset": "c9f1",
                "head_sha": "4b82", "branch": "main", "branch_moved": false,
                "incremental": true, "rewritten": 0,
            }),
        );
    }
}
