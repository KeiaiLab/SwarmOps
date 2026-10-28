//! The read side over ssh (M14 design §4): the store opened, its read checked, `serve.git`
//! refreshed, then git's `upload-pack` on it.

use swarmops_api::{Identity, OpError};

use super::entry::SshRequest;
use super::{ServeInvocation, Served, not_built};

/// Serves `request`, an `upload-pack`, to `who`.
pub fn run(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &SshRequest,
) -> Result<Served, OpError> {
    let _ = (invocation, who, request);
    Err(not_built("git-upload-pack"))
}
