//! The [`Derivation`] trait, its [`Registry`], and the engine's entry points on [`Repo`].

use std::any::TypeId;
use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use swarmops_core::{BlobId, Changeset, ChangesetId, ObjectId, ObjectKind, Tree, TreeId};
use swarmops_store::{DerivedSlot, DerivedStore, Journal, JournalEntry, ObjectStore, StoreError};
use thiserror::Error;

use super::engine::{self, Memo, Node};
use super::{FirstParentSkipList, LastModified};
use crate::error::RepoError;
use crate::repo::{self, Repo};

/// What a value is about: a changeset, by id, and its object, decoded.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct DeriveCtx<'r> {
    pub(super) objects: &'r dyn ObjectStore,
    pub(super) store: &'r DerivedStore,
    pub(super) registry: &'r Registry,
    /// The values the run has besides the store's.
    pub(super) memo: &'r Memo,
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
        let slot = DerivedSlot::new(D::NAME, D::VERSION, cs.0, &node.key);
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct Need {
    /// The derivation's name.
    pub(super) name: &'static str,
    /// The changeset.
    pub(super) cs: ChangesetId,
    /// The key's postcard bytes.
    pub(super) key: Arc<[u8]>,
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
#[non_exhaustive]
pub struct Registry {
    entries: Vec<Entry>,
}

/// A registered derivation: its constants, and the derivation behind its object-safe face.
pub(super) struct Entry {
    pub(super) name: &'static str,
    pub(super) version: u32,
    eager: bool,
    pub(super) memo_every: Option<u32>,
    /// The derivation's type, which tells two derivations of one name apart.
    type_id: TypeId,
    pub(super) derivation: Box<dyn Erased>,
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
    pub(super) fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.name == name)
    }

    /// The derivation at `at`, a place [`Registry::index_of`] or [`Registry::position`] gave.
    pub(super) fn entry(&self, at: usize) -> &Entry {
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
pub(super) trait Erased: Send + Sync {
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
    /// (see [the module documentation](super)). A value depends on objects and other values alone,
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

/// What one [`Repo::derive_eager`] call did: how many of the values it was asked for it derived
/// and how many the store held, and where it leaves the journal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeriveRun {
    /// How many of the values it was asked for (each eager derivation's, of each changeset of the
    /// frames it read) the store lacked: it derived them, and stored them unless a put failed. A
    /// value one of them needed on the way that the store lacked, such as an older changeset's,
    /// is derived too, and not counted.
    pub derived: u64,
    /// How many of the values it was asked for the store held already, which it left as they are.
    pub cached: u64,
    /// The frame to pass as `after` to go on: the last frame the call read; `None` if it read
    /// none, as no frame follows `after`. With a `limit` of 0, the journal's last frame, and
    /// `None` while the journal holds none.
    pub last_frame: Option<u64>,
}

impl<O: ObjectStore> Repo<O> {
    /// Derives, ahead of any reader, the value of every eager derivation ([`Derivation::EAGER`],
    /// whose key is `()`) for each changeset recorded in the journal's frames after frame `after`
    /// (from the first frame for `None`), reading at most `limit` frames: storage spec §5's eager
    /// mode, which `swarmops derive` runs. The frames are read in one snapshot
    /// ([`swarmops_store::MetaRead::journal_after`]), then the changesets taken in their order,
    /// which puts a changeset's parents before it, so that its values find theirs stored. A value
    /// the derived store holds is `cached`, any other is derived as [`Repo::derived`] derives it,
    /// and stored, and counted `derived`. A frame that records no changeset (a ref's move) is
    /// read and passed over.
    ///
    /// With a `limit` of 0 it reads no frame and derives nothing: [`DeriveRun::last_frame`] is
    /// then the journal's last frame, from where a follower derives only what is recorded later
    /// (`swarmops derive --follow`, unless `--from-start`).
    ///
    /// An `after` past the journal's last frame is [`RepoError::PastJournalEnd`], whatever the
    /// `limit`, before anything is derived: no call read it, and reading nothing after it would
    /// pass for being at the journal's end. The other errors are [`Repo::derived`]'s, for the
    /// first value that cannot be made, and [`RepoError::Store`] for a read of the journal that
    /// fails; the values derived before the error stay stored.
    pub fn derive_eager(&self, after: Option<u64>, limit: usize) -> Result<DeriveRun, RepoError> {
        // 1. `after` must be a frame of the journal: one past its end would read as caught up.
        //    A frame a call read is in every later snapshot.
        let read = match after {
            Some(seq) => {
                let read = self.snapshot()?;
                if read.meta.frame(seq)?.is_none() {
                    return Err(RepoError::PastJournalEnd { after: seq });
                }
                Some(read)
            }
            None => None,
        };
        if limit == 0 {
            // What readers count as listed: the seq the next frame takes.
            let journal = Journal::open(&self.layout, Arc::clone(&self.fs))?;
            let last_frame = journal.frame_count()?.checked_sub(1);
            return Ok(DeriveRun {
                last_frame,
                ..DeriveRun::default()
            });
        }

        // 2. The frames' records, read in one snapshot, which no derivation needs past this.
        let read = match read {
            Some(read) => read,
            None => self.snapshot()?,
        };
        let entries = read.meta.journal_after(after, limit)?;
        drop(read);
        let mut run = DeriveRun {
            last_frame: entries.last().map(|entry| entry.seq),
            ..DeriveRun::default()
        };
        // Each eager derivation, and its key: `()`'s bytes (`Registry::register` checks it).
        let mut eager = Vec::new();
        for (deriv, entry) in self.derivations.entries.iter().enumerate() {
            if entry.eager {
                let key: Arc<[u8]> = to_bytes(entry.name, "key", &())?.into();
                eager.push((deriv, entry, key));
            }
        }
        for cs in entries.iter().filter_map(recorded_changeset) {
            for (deriv, entry, key) in &eager {
                let slot = DerivedSlot::new(entry.name, entry.version, cs.0, key);
                if self.derived.get(&slot)?.is_some() {
                    run.cached += 1;
                    continue;
                }
                let node = Node {
                    deriv: *deriv,
                    cs,
                    key: Arc::clone(key),
                };
                engine::run(&self.objects, &self.derived, &self.derivations, node)?;
                run.derived += 1;
            }
        }
        Ok(run)
    }
}

/// The changeset that journal entry `entry` records, if it is a changeset record.
fn recorded_changeset(entry: &JournalEntry) -> Option<ChangesetId> {
    if entry.kind != "changeset" {
        return None;
    }
    // A changeset record's key is the id's 32 bytes, as the journal holds them.
    let id: [u8; 32] = entry.key.as_slice().try_into().ok()?;
    Some(ChangesetId::new(ObjectId::new(id)))
}

/// The error of derivation `name`: [`RepoError::Derived`].
pub(super) fn derivation_error(name: &str, why: impl Into<String>) -> RepoError {
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
