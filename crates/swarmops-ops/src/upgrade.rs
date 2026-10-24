//! `upgrade` (the M6 design §3): a FORMAT 1 store moved to FORMAT 2.

use swarmops_api::{OpError, UpgradeReq, UpgradeResp};

use crate::error;
use crate::local::Recording;

/// `upgrade` of the store `ops` reads.
/// It needs no identity: it records nothing, no journal frame and no agent.
/// A held maintenance lease is `E_MAINTENANCE_BUSY`, at once. The handle `ops` holds reads
/// FORMAT 1 still: its next operation fails as `E_STORE_CORRUPT`, and one opened afresh reads
/// FORMAT 2.
#[expect(
    clippy::needless_pass_by_value,
    reason = "StoreOps::upgrade hands its request over by value"
)]
pub(crate) fn run(ops: &Recording<'_>, req: UpgradeReq) -> Result<UpgradeResp, OpError> {
    let UpgradeReq { .. } = req;
    let report = ops.ops.repo.upgrade().map_err(error::op)?;
    Ok(UpgradeResp::new(
        report.from.version(),
        report.to.version(),
        report.upgraded,
    ))
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use swarmops_api::{ErrorCode, InfoReq, StoreOps, UpgradeReq, UpgradeResp};
    use swarmops_store::{Layout, MaintLease};

    use crate::LocalOps;

    /// An upgrade through the operations: 1 to 2, then nothing; the handle that made it fails
    /// closed as `E_STORE_CORRUPT` and a reopen reads FORMAT 2; a held lease is
    /// `E_MAINTENANCE_BUSY`, retryable.
    #[test]
    fn upgrade_reports_formats_and_leaves_old_handles_failing_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, info) = LocalOps::init_as(&root, "test", Some(1)).unwrap();
        assert_eq!(info.store_version, 1);

        // 1. A held lease, refused at once.
        let held = MaintLease::try_acquire(&Layout::new(&root)).unwrap();
        let busy = ops.upgrade(UpgradeReq::new()).unwrap_err();
        assert_eq!(busy.code, ErrorCode::MaintenanceBusy.as_str(), "{busy:?}");
        assert!(busy.retryable, "{busy:?}");
        drop(held);

        // 2. The upgrade, and the handle that made it.
        let resp = ops.upgrade(UpgradeReq::new()).unwrap();
        assert_eq!(resp, UpgradeResp::new(1, 2, true));
        let stale = ops.repo_info(InfoReq::default()).unwrap_err();
        assert_eq!(stale.code, ErrorCode::StoreCorrupt.as_str(), "{stale:?}");

        // 3. A reopen reads FORMAT 2, and a second upgrade does nothing.
        let fresh = LocalOps::open(&root).unwrap();
        assert_eq!(
            fresh.repo_info(InfoReq::default()).unwrap().store_version,
            2
        );
        let again = fresh.upgrade(UpgradeReq::new()).unwrap();
        assert_eq!(again, UpgradeResp::new(2, 2, false));
    }
}
