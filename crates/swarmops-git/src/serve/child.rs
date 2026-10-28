//! Host git as the server's child (M14 design §0.1): `upload-pack`, `receive-pack` or
//! `http-backend` on `serve.git`, built by [`GitCli`]'s one builder, its configuration in the
//! environment.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::ExitStatus;

use super::repo::{ServeError, ServeRepo};
use crate::transport::GitCli;

/// The git service a connection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Service {
    /// `git upload-pack`: fetch and clone.
    UploadPack,
    /// `git receive-pack`: push.
    ReceivePack,
    /// `git http-backend`: either, over smart HTTP.
    HttpBackend,
}

/// What the child gets besides its configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildEnv {
    /// The client's `GIT_PROTOCOL`, passed on as it came.
    pub git_protocol: Option<OsString>,
    /// Variables the child gets: the hook's `SWARMOPS_SERVE_*`, a CGI request's metavariables.
    pub vars: BTreeMap<String, OsString>,
}

impl ChildEnv {
    /// No protocol and no variables.
    pub fn new() -> ChildEnv {
        ChildEnv::default()
    }
}

/// Runs `service` on `repo` with `env`, its stdin and stdout the connection's; its exit.
pub fn run(
    git: &GitCli,
    service: Service,
    repo: &ServeRepo,
    env: &ChildEnv,
) -> Result<ExitStatus, ServeError> {
    let _ = (git, service, repo, env);
    Err(ServeError::NotBuilt)
}
