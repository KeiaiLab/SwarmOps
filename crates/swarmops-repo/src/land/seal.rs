//! The seal of a land's changeset: what a land's caller adds to the changeset the land makes
//! before that changeset is written, when the land must carry a signature.
//!
//! A signature is over the bytes of the git commit a changeset exports to, which the repository
//! cannot render: the git side depends on this crate, not this crate on it. So a land that must
//! be signed takes a function ([`SealFn`]). It is called outside every lock with the changeset
//! the land would write, its parents final, and answers with a [`Seal`]: the message, the extra
//! headers and the metadata that make the changeset the signed commit's own, or a refusal
//! ([`SealRefused`]), which ends the land with nothing recorded.
//!
//! ```text
//!   unsigned changeset ──▶ SealFn ──▶ Seal ──▶ Seal::apply ──▶ the changeset the land writes
//!   (its parents final)    (the caller's)                      (another id)
//! ```
//!
//! The repository knows nothing of what a seal holds, as it knows nothing of why a gate refuses
//! ([`Gated`](super::Gated)).

use std::error::Error;
use std::fmt;

use swarmops_core::{Changeset, CodecError};

/// What a [`SealFn`] gives the changeset a land makes, so that the changeset is the one of the
/// commit that was signed: a message and extra headers in place of the unsigned changeset's, and
/// metadata beside its own ([`Seal::apply`]). The repository reads none of it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Seal {
    /// The sealed changeset's message, in place of the unsigned one's.
    pub message: Vec<u8>,
    /// The sealed changeset's extra headers, in place of the unsigned one's.
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Metadata the sealed changeset holds beside the unsigned one's, in any order: no key of
    /// the unsigned changeset's, and no key twice.
    pub metadata: Vec<(String, Vec<u8>)>,
}

impl Seal {
    /// A seal of these fields.
    pub fn new(
        message: Vec<u8>,
        extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
        metadata: Vec<(String, Vec<u8>)>,
    ) -> Seal {
        Seal {
            message,
            extra_headers,
            metadata,
        }
    }

    /// The sealed changeset: `unsigned`'s parents, root, delta, author and committer, the seal's
    /// message and extra headers, and the metadata of both, merged by key. A key that both hold,
    /// or that the seal gives twice, is [`CodecError::UnsortedMetadata`]: a seal adds to a
    /// changeset's metadata and replaces none of it.
    pub fn apply(self, unsigned: &Changeset) -> Result<Changeset, CodecError> {
        let mut metadata = unsigned.metadata.clone();
        metadata.extend(self.metadata);
        metadata.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
        Changeset::new(
            unsigned.parents.clone(),
            unsigned.root,
            unsigned.delta.clone(),
            unsigned.author.clone(),
            unsigned.committer.clone(),
            self.message,
            self.extra_headers,
            metadata,
        )
    }
}

/// The function a land that must be sealed calls: with the unsigned changeset, its parents
/// final, outside every lock. A land calls it once per attempt that reaches its write, so a
/// target that moved is sealed again on its new head.
pub type SealFn<'a> = dyn FnMut(&Changeset) -> Result<Seal, SealRefused> + 'a;

/// Why a [`SealFn`] gave no seal: the caller's own error, which the land hands back as it is
/// ([`RepoError::SealRefused`](crate::RepoError::SealRefused)). [`SealRefused::new`] makes one.
#[derive(Debug)]
#[non_exhaustive]
pub struct SealRefused(pub Box<dyn Error + Send + Sync>);

impl SealRefused {
    /// A refusal that carries `error`.
    pub fn new(error: impl Into<Box<dyn Error + Send + Sync>>) -> SealRefused {
        SealRefused(error.into())
    }
}

impl fmt::Display for SealRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for SealRefused {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}
