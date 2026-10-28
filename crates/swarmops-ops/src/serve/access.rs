//! Who may do what through `serve` (M14 design §5): the store's `access.toml`, read on every
//! connection, and the caller named by [`resolve_identity`], the one resolver of `serve`'s
//! identity.

use swarmops_api::{ErrorCode, Identity, OpError};

use super::{IdentitySource, not_built, refused};
use crate::LocalOps;

/// The most bytes `access.toml` may hold: a longer one is invalid, `E_POLICY_DENIED`.
pub const MAX_ACCESS_BYTES: u64 = 1 << 20;

/// What the hint of a connection that names no caller says to do.
const NO_IDENTITY_HINT: &str = "name the caller with --as <family>/<instance> on serve's command \
line: the command= of its authorized_keys line, or the web server's CGI entry";

/// What a caller asks to do with a ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Fetch or clone it.
    Read,
    /// Move it forward.
    Push,
    /// Move it otherwise than forward.
    Force,
    /// Delete it.
    Delete,
    /// Land onto it, through `refs/land/<ref>`.
    Land,
}

/// A store's grants, as `access.toml` gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Access {}

impl Access {
    /// The grants of the store `ops` serves.
    pub fn load(ops: &LocalOps) -> Result<Access, OpError> {
        let _ = ops;
        Err(not_built("access.toml"))
    }

    /// Whether `who` may do `action` with the store ref `name`: `E_POLICY_DENIED` if not.
    pub fn check(&self, who: &Identity, action: Action, name: &str) -> Result<(), OpError> {
        let _ = (who, action, name);
        Err(not_built("access.toml"))
    }
}

/// The caller `source` names: `--as`'s identity, or `<family>/<remote_user>` under
/// `--user-family`. Naming none is `E_AUTH` `no_identity`.
pub fn resolve_identity(
    source: &IdentitySource,
    remote_user: Option<&str>,
) -> Result<Identity, OpError> {
    match source {
        IdentitySource::Fixed(who) => Ok(who.clone()),
        IdentitySource::UserFamily(family) => {
            let _ = (family, remote_user);
            Err(not_built("--user-family"))
        }
        IdentitySource::Absent => Err(refused(
            ErrorCode::Auth,
            "no_identity",
            "serve names no caller: --as is not given, and SWARMOPS_AGENT does not name one for \
             serve",
        )
        .with_hint(NO_IDENTITY_HINT)),
    }
}

#[cfg(test)]
mod tests {
    use swarmops_api::{ErrorCode, Identity};

    use super::resolve_identity;
    use crate::serve::IdentitySource;

    /// No `--as` names no caller, whatever a web server's user is: `E_AUTH` `no_identity`.
    #[test]
    fn absent_is_e_auth_no_identity() {
        for remote_user in [None, Some("ada")] {
            let error = resolve_identity(&IdentitySource::Absent, remote_user).unwrap_err();
            assert_eq!(error.code, ErrorCode::Auth.as_str(), "{error:?}");
            assert_eq!(error.details["reason"], "no_identity", "{error:?}");
        }
    }

    /// `--as` names the caller, whatever a web server's user is.
    #[test]
    fn fixed_is_its_identity() {
        let who = Identity::parse("person/ada").unwrap();
        let source = IdentitySource::Fixed(who.clone());
        assert_eq!(resolve_identity(&source, None).unwrap(), who);
        assert_eq!(resolve_identity(&source, Some("bob")).unwrap(), who);
    }
}
