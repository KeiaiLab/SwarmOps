//! `repair` (the M6 design §7): the journal's projections rebuilt, damaged objects
//! quarantined, damaged cache entries dropped, and a verify after it.

use swarmops_api::{OpError, RepairAction, RepairReq, RepairResp, VerifySummary};
use swarmops_repo::{RepairRequest, RepoError, StoreError};

use crate::error;
use crate::local::Recording;

/// `repair` on the store `ops` reads.
/// It needs no identity: it records nothing, no journal frame and no agent.
/// A held maintenance lease is `E_MAINTENANCE_BUSY`, at once; journal damage beyond a torn tail
/// is `E_STORE_CORRUPT` with `details.offset`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "StoreOps::repair hands its request over by value"
)]
pub(crate) fn run(ops: &Recording<'_>, req: RepairReq) -> Result<RepairResp, OpError> {
    let RepairReq { dry_run, .. } = req;
    let report = ops
        .ops
        .repo
        .repair(RepairRequest::new(dry_run))
        .map_err(repair_error)?;
    let actions = report
        .actions
        .into_iter()
        .map(|action| RepairAction::new(action.kind.as_str(), action.target, action.done))
        .collect();
    let after = VerifySummary::new(
        report.after.objects_checked,
        report.after.changesets_checked,
        report.after.found,
    );
    Ok(RepairResp::new(actions, after))
}

fn repair_error(e: RepoError) -> OpError {
    match e {
        RepoError::Store(StoreError::JournalCorrupt { offset }) => {
            error::op(RepoError::Store(StoreError::JournalCorrupt { offset }))
                .with_details(error::details([("offset", offset.into())]))
        }
        other => error::op(other),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::FileExt;

    use camino::Utf8PathBuf;
    use serde_json::Value;
    use swarmops_api::{
        BlobPutReq, ChangeInput, ChangeOp, CommitReq, ContentInput, ErrorCode, Identity,
        RepairAction, RepairReq, StoreOps, VerifyReq,
    };
    use swarmops_core::ObjectId;
    use swarmops_store::{Layout, MaintLease};

    use crate::LocalOps;

    fn who() -> Identity {
        Identity::parse("test/1").unwrap()
    }

    fn assert_no_host_path(value: &Value, root_str: &str) {
        match value {
            Value::String(s) => {
                assert!(
                    !s.contains(root_str) && !s.starts_with('/'),
                    "response string {s:?} carries a host path (root: {root_str:?})"
                );
            }
            Value::Array(items) => {
                for item in items {
                    assert_no_host_path(item, root_str);
                }
            }
            Value::Object(map) => {
                for val in map.values() {
                    assert_no_host_path(val, root_str);
                }
            }
            _ => {}
        }
    }

    /// `repair` through `StoreOps`: quarantines a damaged blob so `blob_put` of the same bytes
    /// restores it, refuses mid-journal damage with `E_STORE_CORRUPT` (`details.offset`) leaving
    /// `journal.log` byte-identical, refuses a held maintenance lease with `E_MAINTENANCE_BUSY`,
    /// and carries no host path in its response.
    #[test]
    fn repair_mends_quarantines_and_refuses_mid_log_damage() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, info) = LocalOps::init(&root, "test").unwrap();
        let format = match info.store_version {
            1 => swarmops_store::StoreFormat::V1,
            _ => swarmops_store::StoreFormat::V2,
        };
        let layout = Layout::with_format(&root, format);

        let blob_resp = ops
            .blob_put(
                &who(),
                BlobPutReq::new(ContentInput::Inline("hello repair\n".to_owned())),
            )
            .unwrap();
        let blob_id: ObjectId = blob_resp.blob.parse().unwrap();
        let mut ch1 = ChangeInput::new("hello.txt", ChangeOp::Put);
        ch1.content = Some(ContentInput::Inline("hello repair\n".to_owned()));
        let mut commit = CommitReq::new(vec![ch1], "initial");
        commit.advance = Some("main".to_owned());
        ops.commit(&who(), commit).unwrap();
        let mut ch2 = ChangeInput::new("other.txt", ChangeOp::Put);
        ch2.content = Some(ContentInput::Inline("second\n".to_owned()));
        let mut commit2 = CommitReq::new(vec![ch2], "second");
        commit2.advance = Some("main".to_owned());
        ops.commit(&who(), commit2).unwrap();

        // Corrupt the blob file on disk.
        let blob_path = layout.object_path(blob_id);
        fs::write(&blob_path, b"corrupt object bytes").unwrap();

        let dry = ops.repair(RepairReq::new(true)).unwrap();
        assert_eq!(
            dry.actions,
            [RepairAction::new(
                "quarantine_object",
                blob_id.to_hex(),
                false
            )]
        );
        assert!(blob_path.exists());
        assert_no_host_path(&serde_json::to_value(&dry).unwrap(), root.as_str());

        let repaired = ops.repair(RepairReq::new(false)).unwrap();
        assert_eq!(
            repaired.actions,
            [RepairAction::new(
                "quarantine_object",
                blob_id.to_hex(),
                true
            )]
        );
        assert!(!blob_path.exists());
        assert!(
            root.join("quarantine/objects")
                .join(blob_id.to_hex())
                .is_file()
        );
        assert_no_host_path(&serde_json::to_value(&repaired).unwrap(), root.as_str());

        // Restore the blob with blob_put of the same bytes; deep verify is then clean.
        let put_again = ops
            .blob_put(
                &who(),
                BlobPutReq::new(ContentInput::Inline("hello repair\n".to_owned())),
            )
            .unwrap();
        assert!(put_again.written);
        let verified = ops.verify(VerifyReq::new(true)).unwrap();
        assert_eq!(verified.found, 0, "{verified:?}");

        // A second repair does nothing.
        let second = ops.repair(RepairReq::new(false)).unwrap();
        assert!(second.actions.is_empty(), "{second:?}");
        assert_eq!(second.after.found, 0);

        // Held maintenance lease -> E_MAINTENANCE_BUSY.
        let held = MaintLease::try_acquire(&layout).unwrap();
        let busy = ops.repair(RepairReq::new(true)).unwrap_err();
        assert_eq!(busy.code, ErrorCode::MaintenanceBusy.as_str(), "{busy:?}");
        assert!(busy.retryable, "{busy:?}");
        drop(held);

        // Mid-journal damage -> E_STORE_CORRUPT with details.offset, journal.log byte-identical.
        let log_path = layout.journal_log();
        let header_len: u64 = match format {
            swarmops_store::StoreFormat::V1 => 8,
            _ => 24,
        };
        let mut orig_log = fs::read(&log_path).unwrap();
        orig_log[usize::try_from(header_len).unwrap()] ^= 0xff;
        let file = fs::OpenOptions::new().write(true).open(&log_path).unwrap();
        file.write_all_at(
            &[orig_log[usize::try_from(header_len).unwrap()]],
            header_len,
        )
        .unwrap();
        drop(file);
        let damaged_log = fs::read(&log_path).unwrap();

        let err = ops.repair(RepairReq::new(false)).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreCorrupt.as_str(), "{err:?}");
        assert_eq!(
            err.details.get("offset"),
            Some(&Value::from(0_u64)),
            "{err:?}"
        );
        assert_eq!(fs::read(&log_path).unwrap(), damaged_log);
    }
}
