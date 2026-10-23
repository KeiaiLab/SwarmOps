//! [`LastModified`], a built-in derivation (storage spec §5): the newest changeset on a changeset's
//! first-parent chain that touched a path, which path history chases from one touch to the one
//! before ([`RepoRead::path_history`](crate::RepoRead::path_history)).

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use swarmops_core::{ChangesetId, DeltaEntry, ObjectId, RepoPath};

use super::{Derivation, DeriveCtx, DeriveError, Subject};

/// The newest changeset on the first-parent chain of a changeset, the changeset itself included,
/// whose delta has an entry at a path or below it: the last one that changed what the path holds,
/// a file's content or kind, or anything below a directory. `None` where no changeset on the
/// chain did. A changeset touches a path when its delta, sorted by path bytes, has an entry at the
/// path or one that starts with the path and a `/`.
///
/// A changeset that does not touch the path redirects to its first parent
/// ([`Derivation::canonical`]), so that a value is derived only at a changeset that touches the
/// path (itself) and at a root that does not (`None`), and nothing is stored for the changesets
/// in between, but the memos: a chain of more than 64 redirects stores its value at its origin
/// and at every 64th changeset from there ([`Derivation::MEMO_EVERY`]), so that a later walk from
/// anywhere on it takes at most 64 steps. It reads its own changesets' deltas alone, and no other
/// value.
///
/// Its key is the path ([`PathKey`]), its value the changeset ([`LastTouch`]), which
/// [`RepoRead::last_modified`](crate::RepoRead::last_modified) gives.
#[derive(Debug, Clone, Copy, Default)]
pub struct LastModified;

/// [`LastModified`]'s key: a path, stored as its string, which decodes into a path again (the
/// derived store keeps a key as its postcard bytes, and `swarmops-core`'s types have no serde
/// form of their own).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathKey(pub RepoPath);

impl PathKey {
    /// The key of `path`.
    pub fn new(path: RepoPath) -> PathKey {
        PathKey(path)
    }
}

impl From<RepoPath> for PathKey {
    fn from(path: RepoPath) -> PathKey {
        PathKey(path)
    }
}

/// The path's string.
impl Serialize for PathKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

/// A string that is a repository path; anything else is an error.
impl<'de> Deserialize<'de> for PathKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<PathKey, D::Error> {
        let path = String::deserialize(deserializer)?;
        RepoPath::new(path).map(PathKey).map_err(D::Error::custom)
    }
}

/// [`LastModified`]'s value: the changeset that last touched the path, `None` if no changeset on
/// the chain did.
///
/// Stored, as postcard encodes it, as an option of the id's 32 bytes: what version 1 of
/// [`LastModified`] reads back, so any other layout takes a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LastTouch(pub Option<ChangesetId>);

impl LastTouch {
    /// The value naming `toucher`, `None` for no changeset.
    pub fn new(toucher: Option<ChangesetId>) -> LastTouch {
        LastTouch(toucher)
    }
}

impl From<Option<ChangesetId>> for LastTouch {
    fn from(toucher: Option<ChangesetId>) -> LastTouch {
        LastTouch(toucher)
    }
}

/// An option of the id's 32 bytes.
impl Serialize for LastTouch {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.map(|id| id.0.0).serialize(serializer)
    }
}

/// The id of an option of 32 bytes, as [`LastTouch`]'s `Serialize` writes it.
impl<'de> Deserialize<'de> for LastTouch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<LastTouch, D::Error> {
        let id = Option::<[u8; 32]>::deserialize(deserializer)?;
        Ok(LastTouch(id.map(|id| ChangesetId(ObjectId(id)))))
    }
}

impl Derivation for LastModified {
    const NAME: &'static str = "last_modified";
    const VERSION: u32 = 1;
    const MEMO_EVERY: Option<u32> = Some(64);
    type Key = PathKey;
    type Value = LastTouch;

    /// None: a changeset's own delta says whether it touches the path, and one that does not
    /// redirects to its first parent rather than read that one's value.
    fn deps(&self, _: Subject<'_>, _: &PathKey) -> Vec<(ChangesetId, PathKey)> {
        Vec::new()
    }

    /// The first parent, under the same path, for a changeset that does not touch it; `None` for
    /// one that does, and for a root.
    fn canonical(
        &self,
        _: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &PathKey,
    ) -> Result<Option<(ChangesetId, PathKey)>, DeriveError> {
        if touches(&subject.changeset.delta, &key.0) {
            return Ok(None);
        }
        let parent = subject.changeset.parents.first();
        Ok(parent.map(|&parent| (parent, key.clone())))
    }

    /// The changeset itself where it touches the path; `None` at a root that does not.
    fn derive(
        &self,
        _: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &PathKey,
    ) -> Result<LastTouch, DeriveError> {
        let touched = touches(&subject.changeset.delta, &key.0);
        Ok(LastTouch(touched.then_some(subject.id)))
    }
}

/// Whether `delta`, sorted by path bytes as a changeset's is, has an entry at `path` or below it:
/// a search for the path, then one for the first entry at or after `path/`, which is below the
/// path if it starts with that (the entries below a path are contiguous, but not next to the
/// path's own: `a-b` sorts between `a` and `a/c`).
pub(crate) fn touches(delta: &[DeltaEntry], path: &RepoPath) -> bool {
    fn at(entry: &DeltaEntry) -> &[u8] {
        entry.path.as_str().as_bytes()
    }
    let path = path.as_str().as_bytes();
    if delta.binary_search_by(|entry| at(entry).cmp(path)).is_ok() {
        return true;
    }
    let below = [path, b"/"].concat();
    let first = delta.partition_point(|entry| at(entry) < below.as_slice());
    delta
        .get(first)
        .is_some_and(|entry| at(entry).starts_with(&below))
}

#[cfg(test)]
mod tests {
    use swarmops_core::{BlobId, DeltaOp, LeafKind};

    use super::*;

    fn delta(paths: &[&str]) -> Vec<DeltaEntry> {
        paths
            .iter()
            .map(|p| DeltaEntry {
                path: RepoPath::new(*p).unwrap(),
                op: DeltaOp::Put {
                    kind: LeafKind::File,
                    id: BlobId(ObjectId([0; 32])),
                },
                renamed_from: None,
            })
            .collect()
    }

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    /// A delta touches a path at it or below it, never at a path that merely starts with the
    /// same bytes (`a-b`, `ab`), nor above it.
    #[test]
    fn a_delta_touches_a_path_at_it_or_below_it() {
        let d = delta(&["a", "a-b", "a/c", "ab", "b/x/y"]);
        for p in ["a", "a/c", "b", "b/x", "b/x/y", "ab", "a-b"] {
            assert!(touches(&d, &path(p)), "{p}");
        }
        for p in ["a/c/d", "a/b", "b/x/y/z", "c", "b/xy", "aa"] {
            assert!(!touches(&d, &path(p)), "{p}");
        }
        let below_only = delta(&["a-b", "a/c"]);
        assert!(touches(&below_only, &path("a")));
        assert!(!touches(&delta(&["a-b", "ab"]), &path("a")));
        assert!(!touches(&[], &path("a")));
    }

    /// A key is stored as its path's string and decodes into the same path; bytes that are no
    /// path do not decode. A value is stored as an option of the id's 32 bytes.
    #[test]
    fn keys_and_values_are_stored_as_their_bytes() {
        let key = PathKey(path("src/lib.rs"));
        let bytes = postcard::to_stdvec(&key).unwrap();
        assert_eq!(bytes, [&[10_u8][..], b"src/lib.rs"].concat());
        assert_eq!(postcard::from_bytes::<PathKey>(&bytes).unwrap(), key);
        let not_a_path = postcard::to_stdvec("a//b").unwrap();
        assert!(postcard::from_bytes::<PathKey>(&not_a_path).is_err());

        let touch = LastTouch(Some(ChangesetId(ObjectId([7; 32]))));
        let bytes = postcard::to_stdvec(&touch).unwrap();
        assert_eq!(bytes, [&[1_u8][..], &[7; 32]].concat());
        assert_eq!(postcard::from_bytes::<LastTouch>(&bytes).unwrap(), touch);
        assert_eq!(postcard::to_stdvec(&LastTouch(None)).unwrap(), [0]);
        assert_eq!(
            postcard::from_bytes::<LastTouch>(&[0]).unwrap(),
            LastTouch(None)
        );
    }
}
