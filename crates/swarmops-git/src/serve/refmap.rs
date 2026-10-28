//! The map between store refs and the git refs `serve.git` advertises (M14 design §3): `main` and
//! the branches under `refs/heads/`, the tags under `refs/tags/`, `refs/land/<target>` for a push
//! that lands. Remote-tracking refs are not served.

use swarmops_store::{RefKind, StoreFormat};

/// What a git ref a client names stands for in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mapped {
    /// A store ref, by its name and kind.
    Ref {
        /// The store ref's name: `main`, `release/<x>`, a branch or `tags/<path>`.
        store: String,
        /// Its kind.
        kind: RefKind,
    },
    /// `refs/land/<target>`: a push that lands onto the store branch `target`.
    Land {
        /// The branch to land onto.
        target: String,
    },
    /// No store ref: why.
    Refused(Refusal),
}

/// Why a git ref maps to no store ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// A name the store reserves (`remote/…`, `HEAD`).
    Reserved,
    /// A ref outside what `serve` serves.
    NotServed,
    /// A name the store's format does not take, and a later one does.
    FormatRequired {
        /// The format that takes it.
        needs: StoreFormat,
    },
}

/// The git ref the store ref `store` of kind `kind` is served as; `None` for one not served.
pub fn to_git(kind: RefKind, store: &str) -> Option<String> {
    let _ = (kind, store);
    None
}

/// What the git ref `git_ref` stands for in a store of format `format`.
pub fn from_git(format: StoreFormat, git_ref: &str) -> Mapped {
    let _ = (format, git_ref);
    Mapped::Refused(Refusal::NotServed)
}
