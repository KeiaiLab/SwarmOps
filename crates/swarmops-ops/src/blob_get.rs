//! `blob_get` (M5; operations spec §1's `blob_get` row): blobs by id, in batches up to a byte
//! budget, each encoded as `read_file` encodes a file. A workspace's materialisation reads its
//! contents through it; it is no MCP tool.
//!
//! The M5 plan's task W1 froze the entry point; task W3 writes its body, which until then fails
//! with `E_INVALID_ARG` naming it.

use swarmops_api::{BlobGetReq, BlobGetResp, ErrorCode, OpError};

use crate::local::Recording;

/// `blob_get` on the store `ops` reads.
pub(crate) fn run(_ops: &Recording<'_>, _req: BlobGetReq) -> Result<BlobGetResp, OpError> {
    Err(OpError::new(
        ErrorCode::InvalidArg,
        "blob_get is not built yet (the M5 plan's task W3)",
    ))
}
