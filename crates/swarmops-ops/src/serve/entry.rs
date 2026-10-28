//! The request of an ssh connection (M14 design §2.1): `SSH_ORIGINAL_COMMAND` split as git quotes
//! it, with no shell, into the git service and the store it names; and that store's directory
//! under `--root`.

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{ErrorCode, OpError};
use swarmops_git::serve::child::Service;

use super::{not_built, refused};

/// The command line of M11's store-to-store sync, which a forced command does not serve yet.
const STDIO_COMMAND: &str = "swarmops serve --stdio";

/// What an ssh client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshRequest {
    /// `upload-pack` or `receive-pack`.
    pub service: Service,
    /// The store's name, its path stripped of `/`, `~/`, `.git` and a trailing `/`.
    pub store: String,
}

impl SshRequest {
    /// The request for `service` on the store named `store`.
    pub fn new(service: Service, store: impl Into<String>) -> SshRequest {
        SshRequest {
            service,
            store: store.into(),
        }
    }
}

/// The request `command` (`SSH_ORIGINAL_COMMAND`) makes. `swarmops serve --stdio` is
/// `E_INVALID_ARG` `not_served`: M11 serves it.
pub fn parse_ssh(command: &str) -> Result<SshRequest, OpError> {
    let stdio = command
        .strip_prefix(STDIO_COMMAND)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '));
    if stdio {
        return Err(refused(
            ErrorCode::InvalidArg,
            "not_served",
            "swarmops serve --stdio, store-to-store sync, is not served yet",
        ));
    }
    Err(not_built("serving git over ssh"))
}

/// The directory of the store named `name` under `root`, which must stay under it.
pub fn store_under(root: &Utf8Path, name: &str) -> Result<Utf8PathBuf, OpError> {
    let _ = (root, name);
    Err(not_built("serving git over ssh"))
}
