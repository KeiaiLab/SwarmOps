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

mod engine;
mod last_modified;
mod skiplist;

pub use self::last_modified::{LastModified, LastTouch, PathKey};
pub use self::skiplist::{FirstParentSkipList, SkipList};

use std::any::TypeId;
use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use swarmops_core::{BlobId, Changeset, ChangesetId, ObjectKind, Tree, TreeId};
use swarmops_store::{DerivedSlot, DerivedStore, ObjectStore, StoreError};
use thiserror::Error;

use self::engine::{Memo, Node};
use crate::error::RepoError;
use crate::repo::{self, Repo};

/// What a value is about: a changeset, by id, and its object, decoded.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    /// The changeset's id.
    pub id: ChangesetId,
    /// The changeset.
    pub changeset: &'a Changeset,
}

/// One kind of derived value (storage spec §5): how it is made from a changeset ([`Subject`]) and
/// a key, out of immutable objects and other derived values only, so that it never changes once
/// derived. Registered in a repository ([`Repo::register`]), it is derived when asked for
/// ([`Repo::derived`]) and cached in the derived store under its name and version.
pub trait Derivation: Send + Sync + 'static {
    /// Its name, and its directory under `meta/derived/`: 1 to 64 bytes of `[a-z0-9_]`, and not
    /// `tmp`, the directory of the derived store's temp files.
    const NAME: &'static str;
    /// Its version, from 1. A new version is a directory of its own, so that it derives every value
    /// again; the values of older versions wait for gc (M6). Changing what a value means, or how
    /// its type encodes, takes a new version: nothing else ever replaces a value.
    const VERSION: u32;
    /// Whether `swarmops derive` derives it for each new changeset, so that its values are there
    /// before anyone asks. The key of an eager derivation is `()`.
    const EAGER: bool = false;
    /// For a chain of [`Derivation::canonical`] redirects longer than `n` steps, `Some(n)` stores the
    /// value at the chain's end also at the chain's origin and at every `n`-th node from there, so
    /// that a later walk from anywhere on the chain takes at most `n` steps; `None` stores nothing
    /// at a redirect. At least 1.
    const MEMO_EVERY: Option<u32> = None;
    /// What tells a changeset's values apart: `()` for one value per changeset, a path for one per
    /// path, and so on. Its postcard bytes are the slot's key, so the same key must always make the
    /// same bytes (a `HashMap`'s order does not).
    type Key: Serialize + DeserializeOwned + Ord + Clone + Send + Sync + 'static;
    /// The value, stored as its postcard bytes.
    type Value: Serialize + DeserializeOwned + Clone + Send + Sync + 'static;

    /// The values of this derivation that [`Derivation::derive`] reads, derived first where the
    /// store lacks them: by default, the value of every parent under the same key.
    fn deps(&self, subject: Subject<'_>, key: &Self::Key) -> Vec<(ChangesetId, Self::Key)> {
        let parents = &subject.changeset.parents;
        parents
            .iter()
            .map(|&parent| (parent, key.clone()))
            .collect()
    }

    /// A changeset and a key whose value is this one's and cheaper to reach (`blame(cs, p)` is
    /// `blame(last_modifier(cs, p), p)`), or `None` (the default) to derive the value here. A
    /// redirect stores no copy of the value, but the memos of [`Derivation::MEMO_EVERY`].
    fn canonical(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &Self::Key,
    ) -> Result<Option<(ChangesetId, Self::Key)>, DeriveError> {
        let _ = (ctx, subject, key);
        Ok(None)
    }

    /// The value, from objects and from the values of [`Derivation::deps`] and of any derivation,
    /// read through [`DeriveCtx::value`]. A value not derived yet is [`DeriveError::Missing`]: return
    /// it as it is, and the engine derives that value, then calls this again.
    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &Self::Key,
    ) -> Result<Self::Value, DeriveError>;
}

/// What [`Derivation::canonical`] and [`Derivation::derive`] read: the store's objects, and derived
/// values. Nothing else, so that a value depends on immutable inputs alone.
pub struct DeriveCtx<'r> {
    objects: &'r dyn ObjectStore,
    store: &'r DerivedStore,
    registry: &'r Registry,
    /// The values the run has besides the store's.
    memo: &'r Memo,
}

impl DeriveCtx<'_> {
    /// Changeset `id`, decoded from its object. [`RepoError::MissingObject`] if the store lacks it.
    pub fn changeset(&self, id: ChangesetId) -> Result<Changeset, DeriveError> {
        let payload = repo::object(self.objects, id.0, ObjectKind::Changeset)?;
        Ok(Changeset::decode(&payload).map_err(RepoError::from)?)
    }

    /// Tree `id`, decoded from its object. [`RepoError::MissingObject`] if the store lacks it.
    pub fn tree(&self, id: TreeId) -> Result<Tree, DeriveError> {
        Ok(repo::tree(self.objects, id)?)
    }

    /// The bytes of blob `id`. [`RepoError::MissingObject`] if the store lacks it.
    pub fn blob(&self, id: BlobId) -> Result<Vec<u8>, DeriveError> {
        Ok(repo::object(self.objects, id.0, ObjectKind::Blob)?)
    }

    /// Derivation `D`'s value of changeset `cs` under `key`, if the run has it or the store holds
    /// it; [`DeriveError::Missing`] if neither, for the engine to derive it. [`RepoError::Derived`]
    /// if `D` is not registered.
    pub fn value<D: Derivation>(
        &self,
        cs: ChangesetId,
        key: &D::Key,
    ) -> Result<D::Value, DeriveError> {
        let deriv = self.registry.index_of::<D>()?;
        let node = Node {
            deriv,
            cs,
            key: key_bytes::<D>(key)?.into(),
        };
        if let Some(value) = self.memo.get(&node) {
            return Ok(value_of::<D>(value)?);
        }
        let slot = DerivedSlot {
            name: D::NAME,
            version: D::VERSION,
            subject: cs.0,
            key: &node.key,
        };
        match self.store.get(&slot)? {
            Some(value) => Ok(value_of::<D>(&value)?),
            None => Err(DeriveError::Missing(Need {
                name: D::NAME,
                cs,
                key: node.key,
            })),
        }
    }

    /// The derived store the run reads values from and stores them in ([`Repo::derived_store`]),
    /// for a derivation that also memoizes values of its own per object rather than per changeset
    /// (the git export's tree and blob oids, M8), under names no derivation registers. What it
    /// stores there must, as a derived value does, depend on immutable objects alone.
    pub fn derived_store(&self) -> &DerivedStore {
        self.store
    }
}

/// Why [`Derivation::canonical`] or [`Derivation::derive`] gave no value.
#[derive(Debug, Error)]
pub enum DeriveError {
    /// A value it reads is not derived yet ([`DeriveCtx::value`]). Returned as it is, it has the
    /// engine derive that value, then call again.
    #[error("{0}")]
    Missing(Need),
    /// Anything else, which ends the run with this error: the store failed, an object is missing or
    /// damaged, or a value cannot be made ([`RepoError::Derived`], a bug of a derivation).
    #[error(transparent)]
    Repo(#[from] RepoError),
}

/// A store failure, as [`DeriveError::Repo`].
impl From<StoreError> for DeriveError {
    fn from(e: StoreError) -> Self {
        Self::Repo(e.into())
    }
}

/// The value a [`DeriveError::Missing`] waits for: a derivation's value of a changeset under a key.
/// Opaque: only the engine reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// The derivation's name.
    name: &'static str,
    /// The changeset.
    cs: ChangesetId,
    /// The key's postcard bytes.
    key: Arc<[u8]>,
}

impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}'s value of changeset {} is not derived yet",
            self.name, self.cs
        )
    }
}

/// The derivations a repository makes values of, by name: the built-ins ([`Registry::builtins`]),
/// and the ones registered since ([`Registry::register`], [`Repo::register`]).
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
}

/// A registered derivation: its constants, and the derivation behind its object-safe face.
struct Entry {
    name: &'static str,
    version: u32,
    eager: bool,
    memo_every: Option<u32>,
    /// The derivation's type, which tells two derivations of one name apart.
    type_id: TypeId,
    derivation: Box<dyn Erased>,
}

impl Registry {
    /// A registry of no derivation.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// The registry a repository opens with: the built-in derivations of storage spec §5 that this
    /// build has, [`FirstParentSkipList`] and [`LastModified`].
    pub fn builtins() -> Registry {
        let mut registry = Registry::new();
        // A built-in keeps every rule `register` checks, so this never fails.
        registry
            .register(FirstParentSkipList)
            .expect("a built-in derivation is one the engine keeps");
        registry
            .register(LastModified)
            .expect("a built-in derivation is one the engine keeps");
        registry
    }

    /// Registers `derivation`, so that its values can be derived. [`RepoError::Derived`], with the
    /// registry unchanged, for a derivation the engine cannot keep: a name that is not 1 to 64
    /// bytes of `[a-z0-9_]`, or is `tmp`; a version of 0; a [`Derivation::MEMO_EVERY`] of
    /// `Some(0)`; an eager derivation whose key is not `()`; or a name registered already.
    pub fn register<D: Derivation>(&mut self, derivation: D) -> Result<(), RepoError> {
        let refuse = |why: &str| Err(derivation_error(D::NAME, why));
        if !is_name(D::NAME) {
            return refuse("a derivation's name is 1 to 64 bytes of [a-z0-9_], and not tmp");
        }
        if D::VERSION == 0 {
            return refuse("version 0: versions start at 1");
        }
        if D::MEMO_EVERY == Some(0) {
            return refuse("MEMO_EVERY of 0: a memo every n steps takes an n of at least 1");
        }
        if D::EAGER && TypeId::of::<D::Key>() != TypeId::of::<()>() {
            return refuse("an eager derivation derives one value per changeset: its key is ()");
        }
        if self.position(D::NAME).is_some() {
            return refuse("a derivation of this name is registered already");
        }
        self.entries.push(Entry {
            name: D::NAME,
            version: D::VERSION,
            eager: D::EAGER,
            memo_every: D::MEMO_EVERY,
            type_id: TypeId::of::<D>(),
            derivation: Box::new(Typed(derivation)),
        });
        Ok(())
    }

    /// Where derivation `D` is: [`RepoError::Derived`] if it is not registered, or another
    /// derivation is registered under its name.
    fn index_of<D: Derivation>(&self) -> Result<usize, RepoError> {
        match self.position(D::NAME) {
            Some(at) if self.entries[at].type_id == TypeId::of::<D>() => Ok(at),
            Some(_) => Err(derivation_error(
                D::NAME,
                "another derivation is registered under this name",
            )),
            None => Err(derivation_error(
                D::NAME,
                "not registered: Repo::register adds a derivation",
            )),
        }
    }

    /// Where the derivation named `name` is, if it is registered.
    fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.name == name)
    }

    /// The derivation at `at`, a place [`Registry::index_of`] or [`Registry::position`] gave.
    fn entry(&self, at: usize) -> &Entry {
        &self.entries[at]
    }
}

/// Each derivation's name, version, eagerness and memo spacing, in the order they were registered.
impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.iter();
        f.debug_list()
            .entries(entries.map(|e| (e.name, e.version, e.eager, e.memo_every)))
            .finish()
    }
}

/// Whether `name` is one a derivation takes: 1 to 64 bytes of `[a-z0-9_]`, and not `tmp`, the
/// derived store's rule for its directories ([`swarmops_store::DerivedSlot::name`]).
fn is_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && name != "tmp"
}

/// A [`Derivation`] behind a face of bytes, its keys and values as their postcard bytes: what the
/// registry holds, so that the engine walks the nodes of every derivation alike.
trait Erased: Send + Sync {
    /// [`Derivation::deps`], over key bytes.
    fn deps(
        &self,
        subject: Subject<'_>,
        key: &[u8],
    ) -> Result<Vec<(ChangesetId, Vec<u8>)>, RepoError>;

    /// [`Derivation::canonical`], over key bytes.
    fn canonical(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &[u8],
    ) -> Result<Option<(ChangesetId, Vec<u8>)>, DeriveError>;

    /// [`Derivation::derive`], over key and value bytes.
    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &[u8],
    ) -> Result<Vec<u8>, DeriveError>;
}

/// A derivation, as the registry holds it.
struct Typed<D>(D);

impl<D: Derivation> Erased for Typed<D> {
    fn deps(
        &self,
        subject: Subject<'_>,
        key: &[u8],
    ) -> Result<Vec<(ChangesetId, Vec<u8>)>, RepoError> {
        let key = key_of::<D>(key)?;
        let deps = self.0.deps(subject, &key);
        deps.iter()
            .map(|(cs, key)| Ok((*cs, key_bytes::<D>(key)?)))
            .collect()
    }

    fn canonical(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &[u8],
    ) -> Result<Option<(ChangesetId, Vec<u8>)>, DeriveError> {
        let key = key_of::<D>(key)?;
        match self.0.canonical(ctx, subject, &key)? {
            Some((cs, key)) => Ok(Some((cs, key_bytes::<D>(&key)?))),
            None => Ok(None),
        }
    }

    fn derive(
        &self,
        ctx: &DeriveCtx<'_>,
        subject: Subject<'_>,
        key: &[u8],
    ) -> Result<Vec<u8>, DeriveError> {
        let key = key_of::<D>(key)?;
        let value = self.0.derive(ctx, subject, &key)?;
        Ok(to_bytes(D::NAME, "value", &value)?)
    }
}

impl<O: ObjectStore> Repo<O> {
    /// Derivation `D`'s value of changeset `cs` under `key`: read back from the derived store if it
    /// holds it, derived otherwise (with every value it needs that the store lacks), and stored
    /// (see [the module documentation](self)). A value depends on objects and other values alone,
    /// so it is the same whenever it is asked for.
    ///
    /// [`RepoError::Derived`] if `D` is not registered ([`Repo::register`]), and for a value that
    /// cannot be made: a cycle among the values it needs, or a key or value that does not encode
    /// or decode (a stored value of an older encoding, from a derivation whose `VERSION` stayed).
    /// [`RepoError::MissingObject`] if the store lacks an object it reads, `cs` among them; a
    /// failed read of the derived store is [`RepoError::Store`]. A failed put is no error.
    pub fn derived<D: Derivation>(
        &self,
        cs: ChangesetId,
        key: &D::Key,
    ) -> Result<D::Value, RepoError> {
        let deriv = self.derivations.index_of::<D>()?;
        let root = Node {
            deriv,
            cs,
            key: key_bytes::<D>(key)?.into(),
        };
        let value = engine::run(&self.objects, &self.derived, &self.derivations, root)?;
        value_of::<D>(&value)
    }

    /// Registers `derivation` with this repository, so that [`Repo::derived`] makes its values:
    /// [`Registry::register`].
    pub fn register<D: Derivation>(&mut self, derivation: D) -> Result<(), RepoError> {
        self.derivations.register(derivation)
    }
}

/// The error of derivation `name`: [`RepoError::Derived`].
fn derivation_error(name: &str, why: impl Into<String>) -> RepoError {
    RepoError::Derived {
        name: name.to_owned(),
        why: why.into(),
    }
}

/// `value`'s postcard bytes: a key or a value (`what`) of derivation `name`.
fn to_bytes<T: Serialize>(name: &str, what: &str, value: &T) -> Result<Vec<u8>, RepoError> {
    postcard::to_stdvec(value)
        .map_err(|e| derivation_error(name, format!("a {what} does not encode: {e}")))
}

/// What `bytes`, every one of them, encode: a key or a value (`what`) of derivation `name`.
fn from_bytes<T: DeserializeOwned>(name: &str, what: &str, bytes: &[u8]) -> Result<T, RepoError> {
    let len = bytes.len();
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Ok(value),
        Ok((_, rest)) => Err(derivation_error(
            name,
            format!(
                "a {what} of {len} bytes decodes with {} bytes left",
                rest.len()
            ),
        )),
        Err(e) => Err(derivation_error(
            name,
            format!("a {what} of {len} bytes does not decode: {e}"),
        )),
    }
}

/// `key`'s postcard bytes, the key of derivation `D`'s slots.
fn key_bytes<D: Derivation>(key: &D::Key) -> Result<Vec<u8>, RepoError> {
    to_bytes(D::NAME, "key", key)
}

/// The key of derivation `D` that `bytes` encode.
fn key_of<D: Derivation>(bytes: &[u8]) -> Result<D::Key, RepoError> {
    from_bytes(D::NAME, "key", bytes)
}

/// The value of derivation `D` that `bytes` encode. A stored value that does not decode is of an
/// older encoding of the value, whose derivation kept its `VERSION`: an error, as it would be
/// derived and found unreadable again.
fn value_of<D: Derivation>(bytes: &[u8]) -> Result<D::Value, RepoError> {
    from_bytes(D::NAME, "value", bytes).map_err(|e| match e {
        RepoError::Derived { name, why } => RepoError::Derived {
            name,
            why: format!("{why}; a derivation whose values change takes a new VERSION"),
        },
        other => other,
    })
}
