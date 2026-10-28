//! The push side over ssh (M14 design §6): the store opened, its read checked, `serve.git`
//! refreshed and its hook in place, then git's `receive-pack` on it, which runs the hook.

use swarmops_api::{Identity, OpError};

use super::entry::SshRequest;
use super::{ServeInvocation, Served, not_built};

/// The most bytes a push may send: git's `receive.maxInputSize`.
pub const MAX_PUSH_BYTES: u64 = 2 << 30;

/// Serves `request`, a `receive-pack`, to `who`.
pub fn run(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &SshRequest,
) -> Result<Served, OpError> {
    let _ = (invocation, who, request);
    Err(not_built("git-receive-pack"))
}
