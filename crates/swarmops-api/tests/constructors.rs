//! The constructors and `Default`s of the operation types fill each field they do not take as
//! serde fills a field that JSON omits: the two ways of building a value agree.

use std::collections::BTreeMap;
use std::fmt::Debug;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use swarmops_api::{
    Advanced, Attribution, BlobGetReq, BlobGetResp, BlobHasReq, BlobHasResp, BlobOut, BlobPutReq,
    BlobPutResp, BranchUpdate, ChangeInput, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary,
    CloneReq, CloneResp, CommitReq, CommitResp, ConflictKindOut, ConflictReport, ContentEncoding,
    ContentInput, DeriveReq, DeriveResp, Diff, DiffHunk, DiffLineOut, DiffReq, DiffStatus,
    DropReason, DroppedChange, EntryKindOut, ErrorCode, FetchReq, FetchResp, FileConflictOut,
    FileContent, FileDiff, FileKind, GitExportReq, GitExportResp, GitImportReq, GitImportResp,
    HeadMove, HunkOut, IdentityInfo, InfoReq, InitReq, LandReq, LandResp, LandStatus, LineOp,
    LineRange, LinesOut, LogCutOut, LogPage, LogReq, LossyCommitOut, Meta, OpChangesetOut, OpError,
    OpLogPage, OpLogReq, OperationKind, OperationOut, PathHistoryEntry, PathHistoryPage,
    PathHistoryReq, PublishedChange, PushReq, PushResp, PushStatus, ReadFileReq, ReadTreeReq,
    RebaseReq, RebaseResp, RebaseStatus, RefDeleteReq, RefListReq, RefListResp, RefMoveOut, RefOut,
    RefSetReq, RefSetResp, RemoteAddReq, RemoteHead, RemoteInfo, RemoteList, RemoteListReq,
    RemoteRemoveReq, RemoteRemoveResp, RepoInfo, ResolutionInput, ResolutionsInput, ResolveRevReq,
    ResolvedRev, RevKind, ShowReq, SignatureOut, Signatures, SyncReq, SyncResp, SyncStatus,
    SyncStatusKind, SyncStatusReq, TreeEntryOut, TreeListing, UndoReq, UndoResp, VerifyFinding,
    VerifyReport, VerifyReq, WsChange, WsChangeKind, WsCheckInput, WsCommitReq, WsCommitResp,
    WsCreateReq, WsDeleteReq, WsDeleteResp, WsDiffReq, WsDiscardReq, WsDiscardResp, WsInfo,
    WsInfoReq, WsLandReq, WsLandResp, WsList, WsListReq, WsMaterializeReq, WsMaterializeResp,
    WsRebaseReq, WsRebaseResp, WsRebasing, WsResolveReq, WsResolveResp, WsState, WsStatus,
    WsStatusReq,
};

/// `built`, written as JSON, cut to its schema's required fields and read back, is unchanged.
#[track_caller]
fn reads_back<T: Serialize + DeserializeOwned + JsonSchema + PartialEq + Debug>(built: &T) {
    // 1. The required fields, as the published schema names them.
    let schema = schemars::schema_for!(T).to_value();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // 2. Only those fields, read back.
    let Value::Object(mut fields) = serde_json::to_value(built).unwrap() else {
        panic!("{built:?} is not written as an object");
    };
    fields.retain(|name, _| required.contains(&name.as_str()));
    let read: T = serde_json::from_value(Value::Object(fields)).unwrap();
    assert_eq!(&read, built);
}

/// `T::default()` is what an empty JSON object reads.
#[track_caller]
fn reads_empty<T: DeserializeOwned + Default + PartialEq + Debug>() {
    let read: T = serde_json::from_str("{}").unwrap();
    assert_eq!(read, T::default(), "{}", std::any::type_name::<T>());
}

/// The requests and the input types inside them.
#[test]
fn every_request_new_fills_the_serde_defaults() {
    reads_back(&BlobGetReq::new(Vec::new()));
    reads_back(&BlobHasReq::new(Vec::new()));
    reads_back(&BlobPutReq::new(ContentInput::Inline("x".to_owned())));
    reads_back(&ChangeInput::new("x", ChangeOp::Put));
    reads_back(&CloneReq::new("x", "x"));
    reads_back(&CommitReq::new(Vec::new(), "x"));
    reads_back(&DiffReq::new("main", "main"));
    reads_back(&FetchReq::new("x"));
    reads_back(&GitExportReq::new("x"));
    reads_back(&GitImportReq::new("x"));
    reads_back(&InitReq::new("x", "x"));
    reads_back(&LandReq::new("main"));
    reads_back(&LineRange::new(7, 7));
    reads_back(&PathHistoryReq::new("x"));
    reads_back(&PushReq::new("x"));
    reads_back(&ReadFileReq::new("main", "x"));
    reads_back(&ReadTreeReq::new("main"));
    reads_back(&RebaseReq::new("main"));
    reads_back(&RefDeleteReq::new("x", "main"));
    reads_back(&RefSetReq::new("x", "main"));
    reads_back(&RemoteAddReq::new("x", "x"));
    reads_back(&RemoteRemoveReq::new("x"));
    reads_back(&ResolutionsInput::new("main", BTreeMap::new()));
    reads_back(&ResolveRevReq::new("main"));
    reads_back(&ShowReq::new("main"));
    reads_back(&SyncReq::new("x"));
    reads_back(&SyncStatusReq::new("x"));
    reads_back(&UndoReq::new(7));
    reads_back(&WsCommitReq::new("x", "x"));
    reads_back(&WsCreateReq::new("x"));
    reads_back(&WsDeleteReq::new("x"));
    reads_back(&WsDiffReq::new("x"));
    reads_back(&WsDiscardReq::new("x"));
    reads_back(&WsInfoReq::new("x"));
    reads_back(&WsLandReq::new("x"));
    reads_back(&WsMaterializeReq::new("x", Vec::new()));
    reads_back(&WsRebaseReq::new("x"));
    reads_back(&WsResolveReq::new("x"));
    reads_back(&WsStatusReq::new("x"));
}

/// The responses.
#[test]
fn every_response_new_fills_the_serde_defaults() {
    reads_back(&BlobGetResp::new(Vec::new(), Vec::new(), true));
    reads_back(&BlobHasResp::new(Vec::new()));
    reads_back(&BlobPutResp::new("x", 7, true));
    reads_back(&CloneResp::new(
        RepoInfo::new("x", "x", 7, 7, 7),
        RemoteInfo::new("x", "x", "x", Vec::new()),
        FetchResp::new("x", "x", 7, Vec::new(), true, 7),
        7,
    ));
    reads_back(&CommitResp::new("x", "x", 7, 7));
    reads_back(&DeriveResp::new(7, 7));
    reads_back(&FetchResp::new("x", "x", 7, Vec::new(), true, 7));
    reads_back(&GitExportResp::new(7, 7, "x", "x", "x", true, true, 7));
    reads_back(&GitImportResp::new(
        7,
        7,
        "x",
        "x",
        "x",
        true,
        Vec::new(),
        7,
    ));
    reads_back(&LandResp::new(
        LandStatus::Landed,
        "x",
        "x",
        "x",
        "x",
        7,
        true,
        7,
        Vec::new(),
    ));
    reads_back(&PushResp::new("x", "x", PushStatus::Pushed, "x", "x", 7, 7));
    reads_back(&RebaseResp::new(
        RebaseStatus::Rebased,
        "x",
        "x",
        "x",
        "x",
        7,
    ));
    reads_back(&RefListResp::new(Vec::new()));
    reads_back(&RefSetResp::new("x", true));
    reads_back(&RemoteRemoveResp::new("x", 7));
    reads_back(&SyncResp::new(
        "x",
        "x",
        SyncStatusKind::UpToDate,
        7,
        Vec::new(),
        Vec::new(),
        HeadMove::new("x"),
        HeadMove::new("x"),
    ));
    reads_back(&UndoResp::new(7, Vec::new(), true));
    reads_back(&WsCommitResp::new("x", "x", 7, 7, true));
    reads_back(&WsDeleteResp::new("x", "x"));
    reads_back(&WsDiscardResp::new(Vec::new()));
    reads_back(&WsLandResp::new(
        LandResp::new(
            LandStatus::Landed,
            "x",
            "x",
            "x",
            "x",
            7,
            true,
            7,
            Vec::new(),
        ),
        7,
    ));
    reads_back(&WsMaterializeResp::new(Vec::new(), 7));
    reads_back(&WsRebaseResp::new("x", "x"));
    reads_back(&WsResolveResp::new(Vec::new(), Vec::new()));
}

/// The `*Out` items of responses and the envelope.
#[test]
fn every_item_new_fills_the_serde_defaults() {
    reads_back(&BlobOut::new("x", 7, ContentEncoding::Utf8, "x"));
    reads_back(&ChangeOut::new("x", ChangeOp::Put));
    reads_back(&DiffLineOut::new(LineOp::Context, "x"));
    reads_back(&FileConflictOut::new("x", ConflictKindOut::Content));
    reads_back(&HunkOut::new(
        7,
        LinesOut::new(7, 7),
        LinesOut::new(7, 7),
        LinesOut::new(7, 7),
    ));
    reads_back(&LinesOut::new(7, 7));
    reads_back(&LogCutOut::new(7, 7));
    reads_back(&LossyCommitOut::new("x", "x", Vec::new()));
    reads_back(&OpChangesetOut::new("x", "x", 7, "x"));
    reads_back(&OperationOut::new(
        7,
        OperationKind::Commit,
        Vec::new(),
        7,
        Vec::new(),
    ));
    reads_back(&RefMoveOut::new("x", "x", 7));
    reads_back(&RefOut::new("x", "x", 7));
    reads_back(&SignatureOut::new("x", "x", 7, 7));
    reads_back(&TreeEntryOut::new("x", EntryKindOut::File, "x"));
}

/// The other types: pages, reports and the parts of responses.
#[test]
fn every_shared_new_fills_the_serde_defaults() {
    reads_back(&Advanced::new("x", "x", true));
    reads_back(&Attribution::new("x"));
    reads_back(&BranchUpdate::new("x"));
    reads_back(&ChangesetDetail::new(
        "x",
        7,
        Vec::new(),
        "x",
        Attribution::new("x"),
        7,
        "x",
        Vec::new(),
        7,
        true,
        BTreeMap::new(),
        Signatures::new(
            SignatureOut::new("x", "x", 7, 7),
            SignatureOut::new("x", "x", 7, 7),
        ),
    ));
    reads_back(&ChangesetSummary::new("x", Vec::new(), "x", 7, "x"));
    reads_back(&ConflictReport::new(
        "x",
        "x",
        "x",
        Vec::new(),
        Vec::new(),
        true,
    ));
    reads_back(&Diff::new("x", "x", Vec::new(), true));
    reads_back(&DiffHunk::new(7, 7, 7, 7, Vec::new()));
    reads_back(&DroppedChange::new("x", DropReason::Trailer));
    reads_back(&FileContent::new(
        "x",
        7,
        FileKind::File,
        ContentEncoding::Utf8,
        "x",
        true,
    ));
    reads_back(&FileDiff::new("x", DiffStatus::Added));
    reads_back(&HeadMove::new("x"));
    reads_back(&IdentityInfo::new("x"));
    reads_back(&LogPage::new(Vec::new(), true));
    reads_back(&Meta::new(7, BTreeMap::new(), "x"));
    reads_back(&OpLogPage::new(Vec::new(), true));
    reads_back(&PathHistoryEntry::new("x", ChangeOp::Put, "x", 7, "x"));
    reads_back(&PathHistoryPage::new("x", Vec::new(), true));
    reads_back(&PublishedChange::new("x", "x", "x"));
    reads_back(&RemoteHead::new("x", "x"));
    reads_back(&RemoteInfo::new("x", "x", "x", Vec::new()));
    reads_back(&RemoteList::new(Vec::new()));
    reads_back(&RepoInfo::new("x", "x", 7, 7, 7));
    reads_back(&ResolvedRev::new("x", RevKind::Ref));
    reads_back(&Signatures::new(
        SignatureOut::new("x", "x", 7, 7),
        SignatureOut::new("x", "x", 7, 7),
    ));
    reads_back(&SyncStatus::new("x", "x", true, 7, 7, Vec::new(), true));
    reads_back(&TreeListing::new(Vec::new(), true));
    reads_back(&VerifyFinding::new("x"));
    reads_back(&VerifyReport::new(7, 7, Vec::new()));
    reads_back(&WsChange::new("x", WsChangeKind::Added));
    reads_back(&WsInfo::new("x", "x", "x", "x"));
    reads_back(&WsList::new(Vec::new()));
    reads_back(&WsRebasing::new("x", Vec::new()));
    reads_back(&WsStatus::new(
        "x",
        "x",
        "x",
        "x",
        7,
        WsState::Clean,
        Vec::new(),
        true,
    ));
}

/// The requests with no required field.
#[test]
fn every_default_is_the_empty_request() {
    reads_empty::<DeriveReq>();
    reads_empty::<InfoReq>();
    reads_empty::<LogReq>();
    reads_empty::<OpLogReq>();
    reads_empty::<RefListReq>();
    reads_empty::<RemoteListReq>();
    reads_empty::<ResolutionInput>();
    reads_empty::<VerifyReq>();
    reads_empty::<WsCheckInput>();
    reads_empty::<WsListReq>();
}

/// `OpError::with_message` replaces the message only.
#[test]
fn with_message_keeps_the_rest() {
    let error = OpError::new(ErrorCode::Busy, "first").with_hint("again");
    let renamed = error.clone().with_message("second");
    assert_eq!(renamed.message, "second");
    assert_eq!(renamed.with_message("first"), error);
}
