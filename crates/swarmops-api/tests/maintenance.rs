//! The maintenance operations (M6) on a `StoreOps` written before them: the trait's default
//! bodies answer `E_INVALID_ARG`, so that an implementor outside this crate keeps compiling.

use swarmops_api::*;

/// A store that has every required operation and none of the maintenance; no test calls the
/// former.
struct Older;

impl StoreOps for Older {
    fn repo_info(&self, _: InfoReq) -> Result<RepoInfo, OpError> {
        unreachable!("not called")
    }
    fn commit(&self, _: &Identity, _: CommitReq) -> Result<CommitResp, OpError> {
        unreachable!("not called")
    }
    fn land(&self, _: &Identity, _: LandReq) -> Result<LandResp, OpError> {
        unreachable!("not called")
    }
    fn rebase(&self, _: &Identity, _: RebaseReq) -> Result<RebaseResp, OpError> {
        unreachable!("not called")
    }
    fn resolve_rev(&self, _: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        unreachable!("not called")
    }
    fn blob_put(&self, _: &Identity, _: BlobPutReq) -> Result<BlobPutResp, OpError> {
        unreachable!("not called")
    }
    fn blob_has(&self, _: BlobHasReq) -> Result<BlobHasResp, OpError> {
        unreachable!("not called")
    }
    fn blob_get(&self, _: BlobGetReq) -> Result<BlobGetResp, OpError> {
        unreachable!("not called")
    }
    fn read_file(&self, _: ReadFileReq) -> Result<FileContent, OpError> {
        unreachable!("not called")
    }
    fn read_tree(&self, _: ReadTreeReq) -> Result<TreeListing, OpError> {
        unreachable!("not called")
    }
    fn show(&self, _: ShowReq) -> Result<ChangesetDetail, OpError> {
        unreachable!("not called")
    }
    fn log(&self, _: LogReq) -> Result<LogPage, OpError> {
        unreachable!("not called")
    }
    fn path_history(&self, _: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        unreachable!("not called")
    }
    fn diff(&self, _: DiffReq) -> Result<Diff, OpError> {
        unreachable!("not called")
    }
    fn ref_list(&self, _: RefListReq) -> Result<RefListResp, OpError> {
        unreachable!("not called")
    }
    fn ref_set(&self, _: &Identity, _: RefSetReq) -> Result<RefSetResp, OpError> {
        unreachable!("not called")
    }
    fn ref_delete(&self, _: &Identity, _: RefDeleteReq) -> Result<RefSetResp, OpError> {
        unreachable!("not called")
    }
    fn op_log(&self, _: OpLogReq) -> Result<OpLogPage, OpError> {
        unreachable!("not called")
    }
    fn undo(&self, _: &Identity, _: UndoReq) -> Result<UndoResp, OpError> {
        unreachable!("not called")
    }
    fn verify(&self, _: VerifyReq) -> Result<VerifyReport, OpError> {
        unreachable!("not called")
    }
    fn derive(&self, _: DeriveReq) -> Result<DeriveResp, OpError> {
        unreachable!("not called")
    }
}

#[test]
fn new_store_ops_default_to_not_built() {
    let store = Older;
    let refused = [
        store.gc(GcReq::default()).unwrap_err(),
        store.repair(RepairReq::default()).unwrap_err(),
        store.upgrade(UpgradeReq::default()).unwrap_err(),
    ];
    for (error, op) in refused.iter().zip(["gc", "repair", "upgrade"]) {
        assert_eq!(error.code, "E_INVALID_ARG", "{op}: {error:?}");
        assert!(!error.retryable, "{op}");
        assert!(error.message.contains(op), "{op}: {}", error.message);
    }
}
