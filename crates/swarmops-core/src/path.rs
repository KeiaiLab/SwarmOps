//! Validated repository paths.

use std::fmt;

use crate::error::PathError;

/// A UTF-8, `/`-separated path inside a repository: no empty, `.` or `..`
/// components, no leading or trailing slash, no NUL. Ordered by bytes, never
/// normalized.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    /// Validates `s` and wraps it.
    pub fn new(s: impl Into<String>) -> Result<Self, PathError> {
        let s = s.into();
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.contains('\0') {
            return Err(PathError::Nul);
        }
        if s.starts_with('/') || s.ends_with('/') {
            return Err(PathError::LeadingOrTrailingSlash);
        }
        for component in s.split('/') {
            match component {
                "" => return Err(PathError::EmptyComponent),
                "." | ".." => return Err(PathError::DotComponent),
                _ => {}
            }
        }
        Ok(Self(s))
    }

    /// The path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `/`-separated components, in order.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The directory part, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<Self> {
        self.0.rfind('/').map(|i| Self(self.0[..i].to_owned()))
    }

    /// The last component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

/// Checks that `name` is one valid path component, as a tree entry's name must be (storage spec
/// §1): no `/`, not empty, no NUL, not `.` or `..`, checked in that order. Allocates nothing.
/// For a name without `/` the error is the one [`RepoPath::new`] gives, which keeps its own
/// loop: its `EmptyComponent` for `a//b` is not this check's `Empty` for a bare empty name.
pub(crate) fn check_component(name: &str) -> Result<(), PathError> {
    if name.contains('/') {
        return Err(PathError::Slash);
    }
    if name.is_empty() {
        return Err(PathError::Empty);
    }
    if name.contains('\0') {
        return Err(PathError::Nul);
    }
    if name == "." || name == ".." {
        return Err(PathError::DotComponent);
    }
    Ok(())
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", self.0)
    }
}
