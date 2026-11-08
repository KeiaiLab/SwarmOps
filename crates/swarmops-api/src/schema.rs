//! [`schema_for_all`]: the JSON Schema of every operation's types, the contract `swarmops schema`
//! prints and its snapshot reviews.

/// The object [`schema_for_all`] returns: each listed type's JSON Schema under the type's name.
/// Each name is a type the crate root re-exports.
macro_rules! schemas {
    ($($ty:ident $(<$arg:ty>)?),+ $(,)?) => {
        serde_json::Value::Object(serde_json::Map::from_iter([
            $((
                stringify!($ty).to_owned(),
                schemars::schema_for!(crate::$ty $(<$arg>)?).to_value(),
            ),)+
        ]))
    };
}

/// The JSON Schema (2020-12) of every request, response and shared type, keyed by type name.
///
/// [`Envelope`](crate::Envelope) is listed as `Envelope<serde_json::Value>`: its `result` is an operation's
/// response, whose type has its own entry.
pub fn schema_for_all() -> serde_json::Value {
    // The store's operations' types, then ancestry's, ref log's, revert's, the conflict
    // report's touches, the maintenance's, the git remotes', the workspaces' and serve's records.
    let mut all = store_schemas();
    let groups = [
        ancestry_schemas(),
        ref_log_schemas(),
        revert_schemas(),
        land_check_schemas(),
        land_group_schemas(),
        conflict_schemas(),
        maintenance_schemas(),
        remote_schemas(),
        workspace_schemas(),
        serve_schemas(),
    ];
    for more in groups {
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
        RefKind,
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

/// The schemas of ancestry's types (the M16 design §5); its entries are `log`'s
/// `ChangesetSummary`.
fn ancestry_schemas() -> serde_json::Value {
    schemas![AncestryReq, AncestryResp]
}

/// The schemas of ref log's types (the M18 design §3.2).
fn ref_log_schemas() -> serde_json::Value {
    schemas![RefLogReq, RefLogPage, RefLogEntryOut]
}

/// The schemas of revert's types (the M18 design §4).
fn revert_schemas() -> serde_json::Value {
    schemas![RevertReq, RevertResp]
}

/// The schemas of the conflict report's touches (the M18 design §5).
fn conflict_schemas() -> serde_json::Value {
    schemas![TouchOut]
}

/// The schemas of land check's types (the M18 design §6.2).
fn land_check_schemas() -> serde_json::Value {
    schemas![
        LandCheckReq,
        LandItemIn,
        LandCheckResp,
        LandCheckItemOut,
        ItemOutcome,
        PathSummaryOut,
        PathChange
    ]
}

/// The schemas of group land's types (the M18 design §6.3).
fn land_group_schemas() -> serde_json::Value {
    schemas![LandGroupReq, LandGroupResp, LandGroupItemOut]
}

/// The schemas of the maintenance's types (M6): gc, repair and upgrade.
fn maintenance_schemas() -> serde_json::Value {
    schemas![
        GcReq,
        GcResp,
        GcCount,
        RepairReq,
        RepairResp,
        RepairAction,
        VerifySummary,
        UpgradeReq,
        UpgradeResp,
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
        WsSnapshotReq,
        SnapshotWrite,
        WsSnapshotResp,
        WsListReq,
        WsList,
        WsInfoReq,
        WsDeleteReq,
        WsDeleteResp,
    ]
}

/// The schemas of the records `serve` writes of each request it served (the M17 design §6).
fn serve_schemas() -> serde_json::Value {
    schemas![ServeRecord, ServeService]
}
