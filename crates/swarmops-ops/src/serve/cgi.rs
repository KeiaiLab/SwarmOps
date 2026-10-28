//! `serve --cgi` (M14 design §2.2): smart HTTP behind a web server, through git's `http-backend`
//! on `serve.git`.

use swarmops_api::{Identity, OpError};

use super::{ServeInvocation, Served, not_built};

/// Serves the CGI request of `invocation` to `who`.
pub fn run(invocation: &ServeInvocation, who: &Identity) -> Result<Served, OpError> {
    let _ = (invocation, who);
    Err(not_built("serving git over HTTP"))
}
