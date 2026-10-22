//! SwarmOps API: the operation contract (operations spec §0, §7).
//!
//! Every operation takes a request type and returns a response type or an [`OpError`]. All of
//! them are serde and JSON Schema types, so the local store, a future remote client and the CLI
//! exchange exactly the same JSON. [`StoreOps`] is the store boundary and [`Identity`] the caller
//! of a mutation. The CLI prints one [`Envelope`] per command and exits with [`OpError::exit`];
//! `swarmops schema` prints [`schema_for_all`], whose snapshot is where contract changes are
//! reviewed.

pub mod envelope;
pub mod error;
pub mod identity;
pub mod ops;
pub mod remote;
pub mod rev;

pub use envelope::{API_VERSION, Envelope, LogCutOut, Meta, RecoveredOut};
pub use error::{ErrorCode, OpError};
pub use identity::Identity;
pub use ops::{
    Advanced, Attribution, ChangeInput, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary,
    CommitReq, CommitResp, ConflictKindOut, ConflictReport, ContentEncoding, ContentInput,
    DEFAULT_DERIVE_LIMIT, DEFAULT_DIFF_CONTEXT, DEFAULT_DIFF_LIMIT, DEFAULT_DIFF_MAX_BYTES,
    DEFAULT_LOG_LIMIT, DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_BYTES, DEFAULT_MAX_CHANGES,
    DEFAULT_MAX_CHANGESETS, DEFAULT_MAX_CONFLICT_BYTES, DEFAULT_OP_LOG_LIMIT,
    DEFAULT_PATH_HISTORY_LIMIT, DEFAULT_READ_TREE_LIMIT, DeriveReq, DeriveResp, Diff, DiffFormat,
    DiffHunk, DiffLineOut, DiffReq, DiffStatus, EntryKindOut, FileConflictOut, FileContent,
    FileDiff, FileKind, GitExportReq, GitExportResp, GitImportReq, GitImportResp, HunkOut,
    IdentityInfo, InfoReq, InitReq, LandReq, LandResp, LandStatus, LineOp, LineRange, LinesOut,
    LogPage, LogReq, LossyClass, LossyCommitOut, MAX_LAND_ATTEMPTS, MAX_TREE_DEPTH, ModeInput,
    OpChangesetOut, OpLogPage, OpLogReq, OperationKind, OperationOut, PathHistoryEntry,
    PathHistoryPage, PathHistoryReq, ReadFileReq, ReadTreeReq, RefDeleteReq, RefListReq,
    RefListResp, RefMoveOut, RefOut, RefSetReq, RefSetResp, RepoInfo, ShowReq, SignatureOut,
    Signatures, StoreOps, TreeEntryOut, TreeListing, UndoReq, UndoResp, VerifyFinding,
    VerifyReport, VerifyReq,
};
pub use ops::{
    BlobGetReq, BlobGetResp, BlobOut, DEFAULT_BLOB_GET_MAX_BYTES, DEFAULT_CHECK_TIMEOUT_S,
    DEFAULT_WS_STATUS_LIMIT, LandMode, RebaseReq, RebaseResp, RebaseStatus, StatusAfter, WsChange,
    WsChangeKind, WsCheckInput, WsCommitReq, WsCommitResp, WsCreateReq, WsDeleteReq, WsDeleteResp,
    WsDiffReq, WsDiscardReq, WsDiscardResp, WsInfo, WsInfoReq, WsLandReq, WsLandResp, WsList,
    WsListReq, WsMaterializeReq, WsMaterializeResp, WsRebaseAction, WsRebaseReq, WsRebaseResp,
    WsRebasing, WsResolveReq, WsResolveResp, WsState, WsStatus, WsStatusReq,
};
pub use ops::{BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp};
pub use ops::{ResolutionInput, ResolutionsInput, Side};
pub use ops::{ResolveRevReq, ResolvedRev, RevKind};
pub use remote::{
    BranchUpdate, CloneReq, CloneResp, DropReason, DroppedChange, FetchReq, FetchResp, HeadMove,
    PublishedChange, PushReq, PushResp, PushStatus, RemoteAddReq, RemoteHead, RemoteInfo,
    RemoteList, RemoteListReq, RemoteRemoveReq, RemoteRemoveResp, SyncMode, SyncReq, SyncResp,
    SyncStatus, SyncStatusKind, SyncStatusReq,
};
pub use rev::Rev;

/// The object [`schema_for_all`] returns: each listed type's JSON Schema under the type's name.
macro_rules! schemas {
    ($($ty:ident $(<$arg:ty>)?),+ $(,)?) => {
        serde_json::Value::Object(serde_json::Map::from_iter([
            $((
                stringify!($ty).to_owned(),
                schemars::schema_for!($ty $(<$arg>)?).to_value(),
            ),)+
        ]))
    };
}

/// The JSON Schema (2020-12) of every request, response and shared type, keyed by type name.
///
/// [`Envelope`] is listed as `Envelope<serde_json::Value>`: its `result` is an operation's
/// response, whose type has its own entry.
pub fn schema_for_all() -> serde_json::Value {
    // The store's operations' types, then the git remotes', then the workspaces'.
    let mut all = store_schemas();
    for more in [remote_schemas(), workspace_schemas()] {
        if let (serde_json::Value::Object(all), serde_json::Value::Object(more)) = (&mut all, more)
        {
            all.extend(more);
        }
    }
    all
}

/// The schemas of the store's operations' types and of those every operation shares.
fn store_schemas() -> serde_json::Value {
    schemas![
        // Shared by every operation.
        Envelope<serde_json::Value>,
        Rev,
        Identity,
        ErrorCode,
        OpError,
        Meta,
        RecoveredOut,
        LogCutOut,
        // init, info, whoami.
        InitReq,
        InfoReq,
        RepoInfo,
        IdentityInfo,
        // commit.
        CommitReq,
        ChangeInput,
        ChangeOp,
        ContentInput,
        ModeInput,
        CommitResp,
        Advanced,
        // blob_put, blob_has.
        BlobPutReq,
        BlobPutResp,
        BlobHasReq,
        BlobHasResp,
        // resolve_rev.
        ResolveRevReq,
        ResolvedRev,
        RevKind,
        // read_file, read_tree.
        ReadFileReq,
        LineRange,
        FileContent,
        FileKind,
        ContentEncoding,
        EntryKindOut,
        ReadTreeReq,
        TreeListing,
        TreeEntryOut,
        // show, log.
        ShowReq,
        ChangesetDetail,
        Attribution,
        Signatures,
        SignatureOut,
        ChangeOut,
        LogReq,
        LogPage,
        ChangesetSummary,
        // path_history.
        PathHistoryReq,
        PathHistoryPage,
        PathHistoryEntry,
        // diff.
        DiffReq,
        DiffFormat,
        Diff,
        FileDiff,
        DiffStatus,
        DiffHunk,
        DiffLineOut,
        LineOp,
        // ref list, ref set, ref delete, verify.
        RefListReq,
        RefListResp,
        RefOut,
        RefSetReq,
        RefSetResp,
        RefDeleteReq,
        VerifyReq,
        VerifyReport,
        VerifyFinding,
        // derive.
        DeriveReq,
        DeriveResp,
        // git import, which runs on the client, as init does.
        GitImportReq,
        LossyClass,
        GitImportResp,
        LossyCommitOut,
        // git export, which runs on the client too.
        GitExportReq,
        GitExportResp,
        // land, op log, undo (M4).
        LandReq,
        ResolutionsInput,
        ResolutionInput,
        Side,
        LandResp,
        LandStatus,
        ConflictReport,
        FileConflictOut,
        ConflictKindOut,
        HunkOut,
        LinesOut,
        OpLogReq,
        OpLogPage,
        OperationOut,
        OperationKind,
        OpChangesetOut,
        RefMoveOut,
        UndoReq,
        UndoResp,
        // rebase, blob_get (M5).
        RebaseReq,
        RebaseResp,
        RebaseStatus,
        BlobGetReq,
        BlobGetResp,
        BlobOut,
    ]
}

/// The schemas of the git remotes' types (M13), which run on the client, as git import and
/// export do.
fn remote_schemas() -> serde_json::Value {
    schemas![
        CloneReq,
        CloneResp,
        RemoteAddReq,
        RemoteInfo,
        RemoteRemoveReq,
        RemoteRemoveResp,
        RemoteListReq,
        RemoteList,
        FetchReq,
        FetchResp,
        RemoteHead,
        BranchUpdate,
        PushReq,
        PushResp,
        PushStatus,
        SyncReq,
        SyncMode,
        SyncResp,
        SyncStatusKind,
        PublishedChange,
        DroppedChange,
        DropReason,
        HeadMove,
        SyncStatusReq,
        SyncStatus,
    ]
}

/// The schemas of the workspaces' types (M5), which run on the client, as git import and export
/// do.
fn workspace_schemas() -> serde_json::Value {
    schemas![
        StatusAfter,
        WsCreateReq,
        WsInfo,
        WsStatusReq,
        WsStatus,
        WsState,
        WsChange,
        WsChangeKind,
        WsRebasing,
        WsDiffReq,
        WsCommitReq,
        WsCommitResp,
        WsRebaseReq,
        WsRebaseAction,
        WsRebaseResp,
        WsResolveReq,
        WsResolveResp,
        WsLandReq,
        WsCheckInput,
        LandMode,
        WsLandResp,
        WsMaterializeReq,
        WsMaterializeResp,
        WsDiscardReq,
        WsDiscardResp,
        WsListReq,
        WsList,
        WsInfoReq,
        WsDeleteReq,
        WsDeleteResp,
    ]
}
