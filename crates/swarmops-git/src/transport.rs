//! The transport of the git remotes (M13): the user's own git, spawned against the store's
//! mirror, moves the packs; SwarmOps decides what moves (operations spec §6).
//!
//! ```text
//!   swarmops-ops (fetch, push, sync)
//!          │  Transport (this module's trait)
//!          ▼
//!   GitCli ── spawns ──▶ git fetch / git push / git ls-remote ──▶ the remote
//!          │                 GIT_DIR = <store>/git/mirror.git
//!          ▼
//!   the classifier: exit status, push's porcelain, git's messages ──▶ TransportError
//! ```
//!
//! One function builds every child, so every rule holds for every call: the binary
//! (`SWARMOPS_GIT`, else `git` on `PATH`, 2.39 or later), an environment cleaned of the variables
//! that point git at another repository, configuration passed in the environment and never in the
//! arguments (`core.fsync=all` among it), `--end-of-options` before every URL and refspec, no
//! shell, and a push that never forces. A token a remote names reaches git as a header scoped to
//! the remote's URL, and is redacted from everything git prints before it enters an error.
//!
//! Failures are classified by one table (`transport/classify.rs`); every one leaves the store
//! as it was.

mod classify;
mod git_cli;

pub use git_cli::{
    Cause, Credential, GitCli, NewObjects, PushOutcome, PushUpdate, RefSpec, RejectReason,
    RemoteUrl, Transport, TransportError, UrlRefused,
};
