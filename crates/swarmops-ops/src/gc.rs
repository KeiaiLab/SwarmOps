//! `gc` (the M6 design §8): the store's leftovers removed, counted by kind.

use swarmops_api::{GcCount, GcReq, GcResp, OpError};
use swarmops_repo::GcRequest;

use crate::error;
use crate::local::Recording;

/// `gc` on the store `ops` reads.
/// It needs no identity: it records nothing, no journal frame and no agent.
/// A held maintenance lease is `E_MAINTENANCE_BUSY`, at once.
#[expect(
    clippy::needless_pass_by_value,
    reason = "StoreOps::gc hands its request over by value"
)]
pub(crate) fn run(ops: &Recording<'_>, req: GcReq) -> Result<GcResp, OpError> {
    let GcReq { dry_run, .. } = req;
    let report = ops
        .ops
        .repo
        .gc(GcRequest::new(dry_run))
        .map_err(error::op)?;
    let removed = report
        .removed
        .iter()
        .map(|&(kind, count)| GcCount::new(kind.as_str(), count))
        .collect();
    Ok(GcResp::new(removed, report.bytes, report.orphan_objects))
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use swarmops_api::{ErrorCode, GcCount, GcReq, StoreOps};
    use swarmops_store::{Layout, MaintLease};

    use crate::LocalOps;

    /// gc through the operations: a leftover counted by its API name, then none; a held lease
    /// is `E_MAINTENANCE_BUSY`.
    #[test]
    fn gc_reports_kinds_by_name_and_a_held_lease_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "test").unwrap();
        std::fs::create_dir_all(root.join("meta.tmp/meta")).unwrap();

        let resp = ops.gc(GcReq::new(false)).unwrap();
        assert_eq!(resp.removed, [GcCount::new("meta_staging", 1)]);
        assert_eq!(ops.gc(GcReq::new(false)).unwrap().removed, []);

        let _held = MaintLease::try_acquire(&Layout::new(&root)).unwrap();
        let busy = ops.gc(GcReq::new(true)).unwrap_err();
        assert_eq!(busy.code, ErrorCode::MaintenanceBusy.as_str(), "{busy:?}");
        assert!(busy.retryable, "{busy:?}");
    }
}
