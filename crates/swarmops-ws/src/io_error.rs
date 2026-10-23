//! `E_IO` for the workspace's and the store's durable files.

use std::io;

use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, OpError};
use swarmops_store::FsMethod;

/// `E_IO` for a call `method` on `file` (a path below the workspace's or the store's root) that
/// failed with `e`: `details.fs_op` and `details.file` name them.
pub(crate) fn io_failed(method: FsMethod, file: &str, e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("{method} {file}: {e}")).with_details(Map::from_iter([
        ("fs_op".to_owned(), Value::from(method.as_str())),
        ("file".to_owned(), Value::from(file)),
    ]))
}
