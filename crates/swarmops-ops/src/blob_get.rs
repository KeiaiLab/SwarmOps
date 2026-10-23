//! `blob_get` (M5; operations spec §1's `blob_get` row): blobs by id, in batches up to a byte
//! budget, each encoded as `read_file` encodes a file. A workspace's materialisation reads its
//! contents through it; it is no MCP tool.
//!
//! The ids are read in one snapshot ([`swarmops_repo::RepoRead::blobs`]). A blob is never cut:
//! the ones that do not fit in `max_bytes` are left for the next request, `truncated`.

use swarmops_api::{BlobGetReq, BlobGetResp, BlobOut, ErrorCode, OpError};
use swarmops_core::bytes;
use swarmops_core::{BlobId, ObjectId};

use crate::content;
use crate::error;
use crate::local::Recording;

/// `blob_get` on the store `ops` reads.
pub(crate) fn run(ops: &Recording<'_>, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
    let BlobGetReq { ids, max_bytes, .. } = req;

    // 1. Every id is checked before the store is read.
    let ids = ids
        .iter()
        .map(|id| blob_id(id))
        .collect::<Result<Vec<_>, _>>()?;

    // 2. One snapshot for the whole batch.
    let read = ops.ops.repo.snapshot().map_err(error::op)?;
    let got = read.blobs(&ids, max_bytes).map_err(error::op)?;

    // 3. Each blob encoded as a whole file is.
    let blobs = got
        .found
        .into_iter()
        .map(|(id, bytes)| {
            let encoded = content::encode(&bytes, bytes.len());
            BlobOut::new(
                id.to_string(),
                bytes::to_u64(bytes.len()),
                encoded.encoding,
                encoded.content,
            )
        })
        .collect();

    Ok(BlobGetResp::new(
        blobs,
        got.missing.iter().map(ToString::to_string).collect(),
        got.truncated,
    ))
}

/// A blob id from its 64 hex digits; `E_INVALID_ARG` naming the id otherwise.
fn blob_id(id: &str) -> Result<BlobId, OpError> {
    ObjectId::from_hex(id).map(BlobId::new).map_err(|e| {
        OpError::new(
            ErrorCode::InvalidArg,
            format!("blob id {id:?} is not 64 hex digits: {e}"),
        )
    })
}
