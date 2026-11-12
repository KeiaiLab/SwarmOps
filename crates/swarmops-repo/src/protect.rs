//! The judge of a ref's move (the U-22 design §2): which refs move forward only, and which of
//! them a caller may force. One rule per kind of name ([`RefKind`]), one test for "forward"
//! everywhere: the old target is on the new target's first-parent chain
//! ([`RepoRead::is_ancestor`]).
//!
//! | Name | Rule |
//! |---|---|
//! | `main` | forward only, never forced |
//! | `release/<x>` | forward, or anywhere with [`MoveMode::Force`] |
//! | `remote/<r>/main`, `remote/<r>/release/<x>` | forward only, never forced |
//! | any other `remote/<r>/<path>`, a branch, a tag | free here |
//!
//! A tag's move is the store's refusal, which runs before this judge. A name of no kind is
//! refused ([`RepoError::RefName`]), never read as free. A creation, a deletion and a move to
//! the same target are no moves: they pass, and no changeset is read for them.
//!
//! [`RepoRead::check_ref_move`] judges a request; [`RepoRead::move_class`] reads a move that
//! was made, for the logs. A rule of requests, not of stored frames: journal replay and the
//! checks of a whole store do not ask it.

use swarmops_core::ChangesetId;
use swarmops_store::{ObjectStore, RefKind, ref_kind};

use crate::error::RepoError;
use crate::read::RepoRead;

/// The store name of a tracking ref starts with it: `remote/<remote>/<path>`.
const REMOTE_PREFIX: &str = "remote/";

/// How a ref may move from one target to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum MoveRule {
    /// Anywhere.
    Free,
    /// Along the new target's first-parent chain only.
    Forward,
    /// As [`MoveRule::Forward`], or anywhere with [`MoveMode::Force`].
    ForwardOrForce,
}

/// What the caller of [`RepoRead::check_ref_move`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoveMode {
    /// A forward move.
    Advance,
    /// Any move the ref's rule lets a caller force.
    Force,
}

/// What a judged move is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoveClass {
    /// Forward, no move, or a move of a ref this judge does not mark.
    Plain,
    /// A move of a `release/<x>` ref whose old target is off the new one's first-parent chain.
    NotForward,
}

/// The rule of ref `name`, by its kind. [`RepoError::RefName`] for a name of no kind, such as
/// `release/a/b` or `main/x`; [`RepoError::Internal`] for a kind this build does not know.
pub(crate) fn move_rule(name: &str) -> Result<MoveRule, RepoError> {
    let Some(kind) = ref_kind(name) else {
        return Err(RepoError::RefName(name.to_owned()));
    };

    match kind {
        RefKind::Main => Ok(MoveRule::Forward),
        RefKind::Release => Ok(MoveRule::ForwardOrForce),
        RefKind::RemoteTracking => Ok(tracking_rule(name)),
        RefKind::Branch | RefKind::Tag => Ok(MoveRule::Free),
        _ => Err(RepoError::unknown_variant("RefKind")),
    }
}

/// The rule of tracking ref `name`: forward for a remote's `main` and `release/<x>`, free for
/// any other path. Example: `remote/origin/release/1.0` is forward, `remote/origin/topic` free.
fn tracking_rule(name: &str) -> MoveRule {
    // The path after `remote/<remote>/`.
    let path = name
        .strip_prefix(REMOTE_PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .map(|(_remote, path)| path);

    match path.and_then(ref_kind) {
        Some(RefKind::Main | RefKind::Release) => MoveRule::Forward,
        _ => MoveRule::Free,
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// What a move of ref `name` from `old` to `new` is, for the logs:
    /// [`MoveClass::NotForward`] for a `release/<x>` ref with both ends set, apart, and `old`
    /// not [`RepoRead::is_ancestor`] of `new`; [`MoveClass::Plain`] for every other move, with
    /// no changeset read. `main` and tracking refs are never marked: `main` leaves its chain
    /// only when a sync relocates it, which is no force. [`RepoError::RefName`] for a name of
    /// no kind; [`RepoError::NotFound`] if the snapshot does not hold an end it reads.
    pub fn move_class(
        &self,
        name: &str,
        old: Option<ChangesetId>,
        new: Option<ChangesetId>,
    ) -> Result<MoveClass, RepoError> {
        // Only a release is marked.
        if move_rule(name)? != MoveRule::ForwardOrForce {
            return Ok(MoveClass::Plain);
        }

        // A move: both ends set and apart.
        let (Some(head), Some(target)) = (old, new) else {
            return Ok(MoveClass::Plain);
        };
        if head == target || self.is_ancestor(head, target)? {
            return Ok(MoveClass::Plain);
        }
        Ok(MoveClass::NotForward)
    }

    /// Judges a request to move ref `name` from `old` to `new` by the ref's rule ([the module
    /// documentation](self)). A creation, a deletion, no move, a forward move and any move of a
    /// free ref pass as [`MoveClass::Plain`] in either mode. Another move of a `release/<x>` ref
    /// passes only with [`MoveMode::Force`], as [`MoveClass::NotForward`]; without it, and for
    /// `main` and the tracking refs of `main` and `release/<x>` in either mode, it is
    /// [`RepoError::HistoryProtected`], with no operation and nothing recovered. A `new` of a
    /// generation at most `old`'s is judged from two index records. [`RepoError::RefName`] for
    /// a name of no kind, whatever the ends; [`RepoError::NotFound`] if the snapshot does not
    /// hold an end it reads.
    pub fn check_ref_move(
        &self,
        name: &str,
        old: Option<ChangesetId>,
        new: Option<ChangesetId>,
        mode: MoveMode,
    ) -> Result<MoveClass, RepoError> {
        // 1. A free ref moves anywhere.
        let rule = move_rule(name)?;
        if rule == MoveRule::Free {
            return Ok(MoveClass::Plain);
        }

        // 2. A move: both ends set and apart.
        let (Some(head), Some(target)) = (old, new) else {
            return Ok(MoveClass::Plain);
        };
        if head == target {
            return Ok(MoveClass::Plain);
        }

        // 3. Forward: the head on the target's first-parent chain.
        if self.is_ancestor(head, target)? {
            return Ok(MoveClass::Plain);
        }

        // 4. Any other move: only a forced one of a ref that can be forced.
        if rule == MoveRule::ForwardOrForce && mode == MoveMode::Force {
            return Ok(MoveClass::NotForward);
        }
        Err(RepoError::HistoryProtected {
            name: name.to_owned(),
            head,
            target,
            op: None,
            recovered: Box::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of the five kinds has its rule, asserted one by one: `main` forward, a release
    /// forward or forced, a branch and a tag free, a tracking ref by the path it tracks.
    #[test]
    fn move_rule_names_every_kind() {
        let rule = |name: &str| move_rule(name).unwrap();

        // 1. Main.
        assert_eq!(ref_kind("main"), Some(RefKind::Main));
        assert_eq!(rule("main"), MoveRule::Forward);

        // 2. Release.
        assert_eq!(ref_kind("release/1.0"), Some(RefKind::Release));
        assert_eq!(rule("release/1.0"), MoveRule::ForwardOrForce);

        // 3. Branch.
        assert_eq!(ref_kind("feature/x"), Some(RefKind::Branch));
        assert_eq!(rule("feature/x"), MoveRule::Free);

        // 4. Tag.
        assert_eq!(ref_kind("tags/v1"), Some(RefKind::Tag));
        assert_eq!(rule("tags/v1"), MoveRule::Free);

        // 5. Remote tracking: forward for main and a release, free for any other path.
        assert_eq!(
            ref_kind("remote/origin/main"),
            Some(RefKind::RemoteTracking)
        );
        assert_eq!(rule("remote/origin/main"), MoveRule::Forward);
        assert_eq!(rule("remote/origin/release/1.0"), MoveRule::Forward);
        assert_eq!(rule("remote/origin/feature/x"), MoveRule::Free);
        assert_eq!(rule("remote/origin/tags/v1"), MoveRule::Free);
        assert_eq!(rule("remote/origin/release/a/b"), MoveRule::Free);
        assert_eq!(rule("remote/origin/main/x"), MoveRule::Free);
    }

    /// A name of no kind has no rule: it is refused by name, never free.
    #[test]
    fn a_name_of_no_kind_has_no_rule() {
        for name in ["release/a/b", "main/x", "tags", "remote/origin", ""] {
            assert!(
                matches!(move_rule(name), Err(RepoError::RefName(refused)) if refused == name),
                "{name:?}"
            );
        }
    }
}
