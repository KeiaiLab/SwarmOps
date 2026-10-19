//! Error types shared across the core crate.

use thiserror::Error;

/// Why a hex string is not an [`crate::ObjectId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IdError {
    /// The string did not have exactly 64 hex characters; carries the length seen.
    #[error("object id must be 64 hex characters, got {0}")]
    Length(usize),
    /// A character outside `[0-9a-fA-F]` was present.
    #[error("object id contains a non-hex character")]
    NonHex,
}

/// Why a string is not a valid [`crate::RepoPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathError {
    /// The path was the empty string.
    #[error("path is empty")]
    Empty,
    /// Two slashes in a row.
    #[error("path has an empty component")]
    EmptyComponent,
    /// A `.` or `..` component.
    #[error("path has a `.` or `..` component")]
    DotComponent,
    /// The path started or ended with `/`.
    #[error("path starts or ends with `/`")]
    LeadingOrTrailingSlash,
    /// A NUL byte was present.
    #[error("path contains a NUL byte")]
    Nul,
    /// A single path component (a tree entry's name) contains `/`.
    #[error("a single path component contains `/`")]
    Slash,
}
