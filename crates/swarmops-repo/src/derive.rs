//! The derived-data framework (storage spec §5): values derived from the immutable objects, such
//! as a changeset's first-parent ancestors or the changeset that last touched a path, made when
//! they are asked for ([`Repo::derived`]) and cached in the derived store
//! ([`swarmops_store::DerivedStore`], `meta/derived/`), where later calls read them back.
//!
//! A [`Derivation`] says how one kind of value is made: its name and version (its directory under
//! `meta/derived/`), the values it depends on ([`Derivation::deps`]), a cheaper input with the same
//! value ([`Derivation::canonical`]), and the value itself ([`Derivation::derive`]), from the
//! changeset the value is about (its [`Subject`]) and a key. What `canonical` and `derive` read,
//! they read through a [`DeriveCtx`]: objects, and derived values (their own derivation's
//! included). Never a ref, the journal, the clock or review state (umbrella spec §7), so that a
//! value, once derived, never changes. A [`Registry`] holds a repository's derivations
//! ([`Repo::register`]), each behind an object-safe face over bytes, so that a node of one
//! derivation can need a value of another.
//!
//! # The engine
//!
//! The engine never recurses on the Rust stack. It keeps an explicit stack of nodes, each a
//! derivation's value of one changeset under one key, fills in what a value needs depth first,
//! and evaluates bottom-up; a node leaves the stack once its value is known. For a node:
//!
//! 1. A value the run has, or the store holds, ends it.
//! 2. Else `canonical` may redirect it to a node of the same value, which is followed without
//!    storing anything. Only a derivation with [`Derivation::MEMO_EVERY`] `= Some(n)` stores, for a
//!    chain of redirects longer than `n` steps, the end's value at the chain's origin and at every
//!    `n`-th node from there: the same value, so shortcuts rather than copies.
//! 3. Else the dependencies the store lacks are derived first, one at a time; then `derive` makes
//!    the value, and the store keeps it.
//!
//! `canonical` and `derive` read values through [`DeriveCtx::value`]: one not derived yet is
//! [`DeriveError::Missing`], and the engine derives it, then calls them again. A node met again
//! while it is on the stack is a cycle, and an error ([`RepoError::Derived`]).
//!
//! Keys and values are stored as their postcard bytes (a key of `()` has none), in the derived
//! store's slots: derivation name and version, changeset, key. A put that fails is dropped, and the
//! value returned all the same. A run holds the values a node waits on (its dependencies, and the
//! values its calls asked for) in memory until that node is done, and keeps the values it derived
//! and could not store until it ends: so a run derives each value once, even over a store that
//! takes none. A read of the store that fails is the run's error.
//!
//! # Eager derivations
//!
//! A derivation marked [`Derivation::EAGER`] is also derived ahead of any reader:
//! [`Repo::derive_eager`], which `swarmops derive` runs, derives its value for each changeset the
//! journal records after a frame, in journal order, so that a changeset's parents have theirs by
//! the time its own is derived. A commit derives nothing (storage spec §5).

mod derivation;
mod engine;
mod last_modified;
mod skiplist;

use self::derivation::derivation_error;
pub use self::derivation::{
    Derivation, DeriveCtx, DeriveError, DeriveRun, Need, Registry, Subject,
};
pub(crate) use self::last_modified::touches;
pub use self::last_modified::{LastModified, LastTouch, PathKey};
pub use self::skiplist::{FirstParentSkipList, SkipList};

// The module documentation's links only.
#[cfg(doc)]
use crate::{error::RepoError, repo::Repo};
