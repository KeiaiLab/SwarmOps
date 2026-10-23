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
mod schema;

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
pub use ops::{
    GcCount, GcReq, GcResp, RefKind, RepairAction, RepairReq, RepairResp, UpgradeReq, UpgradeResp,
    VERIFY_MAX_FINDINGS, VerifySummary,
};
pub use ops::{ResolutionInput, ResolutionsInput, Side};
pub use ops::{ResolveRevReq, ResolvedRev, RevKind};
pub use remote::{
    BranchUpdate, CloneReq, CloneResp, DropReason, DroppedChange, FetchReq, FetchResp, HeadMove,
    PublishedChange, PushReq, PushResp, PushStatus, RemoteAddReq, RemoteHead, RemoteInfo,
    RemoteList, RemoteListReq, RemoteRemoveReq, RemoteRemoveResp, SyncMode, SyncReq, SyncResp,
    SyncStatus, SyncStatusKind, SyncStatusReq,
};
pub use rev::Rev;
pub use schema::schema_for_all;
