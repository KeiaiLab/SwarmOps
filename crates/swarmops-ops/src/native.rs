//! Native store-to-store sync (M11-B, native sync design): the host's side of the wire whose
//! contract is [`swarmops_api::wire`].
//!
//! ```text
//! host ── Link::call(request line, upload chunks) ──▶ upstream (serve --stdio)
//!      ◀─ Answer: envelope line, then its chunks ───┘
//!         chunk: objects read one chunk at a time, every id checked (V1)
//!         key:   the land key of a change on a target (ss1-…)
//! ```
//!
//! - [`link`]: the [`Link`](link::Link) a host calls the upstream through.
//! - `chunk`: the chunk layout that carries blobs and changesets.
//! - `key`: the land key the upstream lands a change under.

pub(crate) mod chunk;
pub(crate) mod key;
pub mod link;

use swarmops_api::{ErrorCode, OpError};

use crate::error;

/// Why a peer stopped answering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// It closed the stream, or the stream broke.
    Disconnected,
    /// It sent nothing for the stall deadline.
    Timeout,
}

impl Cause {
    /// Its `details.cause`.
    fn as_str(self) -> &'static str {
        match self {
            Cause::Disconnected => "disconnected",
            Cause::Timeout => "timeout",
        }
    }

    /// The cause of a failed read or write.
    pub(crate) fn of(failed: &std::io::Error) -> Cause {
        match failed.kind() {
            std::io::ErrorKind::TimedOut => Cause::Timeout,
            _ => Cause::Disconnected,
        }
    }
}

/// `E_REMOTE_UNREACHABLE` with `details.cause`: retryable, nothing moved.
pub(crate) fn unreachable(cause: Cause) -> OpError {
    let cause = cause.as_str();
    error::error(
        ErrorCode::RemoteUnreachable,
        format!("the native remote did not answer ({cause})"),
    )
    .with_details(error::details([("cause", cause.into())]))
    .with_hint("run it again once the network is back: nothing changed")
}

/// `E_INVALID_ARG` with `details.reason`: bytes the wire does not read.
pub(crate) fn refused(reason: &str, message: impl Into<String>) -> OpError {
    error::error(ErrorCode::InvalidArg, message)
        .with_details(error::details([("reason", reason.into())]))
}
