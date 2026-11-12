//! Tags with git's tag object (the annotated tags design §2.1, §2.2): a tag is its ref
//! `tags/<path>`, naming a changeset, as before; an annotated one also holds the object's bytes
//! as a blob, which a Tag fact names.
//!
//! ```text
//!  meta/refs2/<stem>   tags/<path> → changeset T          (a tag ref, unchanged)
//!  objects/<blob B>    git's tag object, its bytes as sent
//!  Tag fact            subject ref_name_key("tags/<path>"), other T
//!                      Annotated(B) | Lightweight
//! ```
//!
//! The binding rule: tag `tags/<path>` at T is annotated with blob B iff the last Tag fact about
//! its name is `Annotated(B)` and names T; otherwise it is lightweight. So a raw `ref set` or an
//! `op undo` that makes the tag again at T carries its last object again, and one elsewhere
//! carries none.
//!
//! An annotated create writes the blob first (W9), then the ref's create and its fact in one
//! frame; the store lists the feature `tag_object` before that frame (W10). A lightweight create
//! writes the ref's create alone, unless its name held a Tag fact already: then a Lightweight
//! fact too, so that the old object is not carried. A tag never moves: a create of a name held
//! at another target, or with another object, is [`StoreError::TagImmutable`]; one alike is
//! [`Applied::Already`].

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use swarmops_core::{BlobId, ChangesetId, Hashed, ObjectId, ObjectKind};
use swarmops_store::{
    CasResult, Expect, Fact, FactKind, MetaRead, MetaWrite, ObjectStore, RefKind, StoreError,
    StoreFormat, ref_kind, ref_name_key,
};

use crate::error::RepoError;
use crate::oplog::OpId;
use crate::read::RepoRead;
use crate::repo::{Repo, post_commit_warning, ref_error};

/// The most tags one frame creates: as many refs as a `ref txn` moves (each ref is a file of its
/// own, each fact stats its blob under the lock), so that a frame stays within the design's
/// 100 ms a step. 1,000 tags are 16 frames. A larger value needs a measurement first (the
/// annotated tags design's decision 15).
pub const TAG_BATCH: usize = 64;

/// The feature a Tag fact needs (the FORMAT 3 design §2.6).
const FEATURE: &str = "tag_object";

/// The schema byte every Tag fact body starts with.
const SCHEMA: u8 = 1;

/// A Tag fact's types (the FORMAT 3 design §2.8, its amendment of 2026-11-12).
#[derive(Clone, Copy)]
enum TagType {
    /// The tag carries the object in the blob its body names.
    Annotated,
    /// The tag carries no object.
    Lightweight,
}

impl TagType {
    /// Its type number in the fact.
    fn code(self) -> u8 {
        match self {
            TagType::Annotated => 0,
            TagType::Lightweight => 1,
        }
    }
}

/// What a tag create makes the tag carry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TagObject {
    /// No object: the tag names its changeset alone.
    Lightweight,
    /// git's tag object, its bytes as git hashes them (after `tag <len>\0`), kept as they are.
    Annotated(Vec<u8>),
}

/// What a tag carries, as the store reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TagKind {
    /// No object.
    Lightweight,
    /// The object in `blob`.
    Annotated {
        /// The blob that holds the object's bytes.
        blob: BlobId,
    },
}

/// A tag, as the store reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TagRead {
    /// Its ref, `tags/<path>`.
    pub name: String,
    /// The changeset it names.
    pub target: ChangesetId,
    /// What it carries.
    pub kind: TagKind,
}

impl TagRead {
    /// Tag `name` at `target`, carrying `kind`.
    pub fn new(name: String, target: ChangesetId, kind: TagKind) -> TagRead {
        TagRead { name, target, kind }
    }
}

/// Whether a create wrote the tag or found it made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Applied {
    /// It created the tag.
    New,
    /// The tag was there already, at the same target with the same object: nothing written.
    Already,
}

/// What a tag create did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TagMade {
    /// Whether it wrote the tag.
    pub applied: Applied,
    /// Steps after the commit point that failed, one line each: the tag is made all the same.
    pub warnings: Vec<String>,
}

impl TagMade {
    /// A create that did `applied`, with no warning.
    pub fn new(applied: Applied) -> TagMade {
        TagMade {
            applied,
            warnings: Vec::new(),
        }
    }
}

/// One tag of a batch: its name, its target and its object.
pub type TagWant = (String, ChangesetId, TagObject);

/// One tag's outcome in a batch.
type Decided = Result<TagMade, RepoError>;

impl<O: ObjectStore> Repo<O> {
    /// Creates tag `name` (`tags/<path>`) at `target` carrying `object`, for `actor`: one tag of
    /// [`Repo::tags_create`], its refusal returned as the error.
    pub fn tag_create(
        &self,
        name: &str,
        target: ChangesetId,
        object: TagObject,
        actor: &str,
    ) -> Result<TagMade, RepoError> {
        let want = [(name.to_owned(), target, object)];
        let mut made = self.tags_create(&want, actor)?;
        made.pop()
            .unwrap_or_else(|| Err(RepoError::Internal("a batch of one decided none".into())))
    }

    /// Creates the tags of `batch` for `actor`, each decided on its own, in the batch's order:
    /// [`TAG_BATCH`] tags a frame, each frame a write transaction of its own, so a batch cut
    /// between frames keeps the ones before, and run again finds them [`Applied::Already`].
    ///
    /// Refused for its tag alone, before anything is written for it: a name that is no tag,
    /// [`RepoError::RefName`]; FORMAT 1, [`StoreError::FormatRequired`] (`needs` 2); an object
    /// below FORMAT 3, [`StoreError::FormatRequired`] naming `tag_object` (`needs` 3); a target
    /// the store does not hold, [`RepoError::NotFound`]; the name held at another target or with
    /// another object, [`StoreError::TagImmutable`]. A failure of the store fails the call: the
    /// frames committed before it stay.
    pub fn tags_create(&self, batch: &[TagWant], actor: &str) -> Result<Vec<Decided>, RepoError> {
        // 1. Each tag checked alone; those that pass are pending.
        let mut made: Vec<Decided> = (batch.iter())
            .map(|(name, _, object)| self.check_tag(name, object))
            .collect();
        let pending: Vec<usize> = (0..batch.len()).filter(|&i| made[i].is_ok()).collect();

        // 2. The pending objects' blobs, durable before a frame names them (W9).
        let kinds = self.put_tag_blobs(batch, &pending)?;

        // 3. A frame for each TAG_BATCH pending tags.
        for chunk in pending.chunks(TAG_BATCH) {
            let decided = self.write_tags(batch, &kinds, chunk, actor)?;
            for (&i, outcome) in chunk.iter().zip(decided) {
                made[i] = outcome;
            }
        }
        Ok(made)
    }

    /// Refuses tag `name` with `object` before anything is written; a placeholder
    /// [`Applied::New`] when it passes, which the frame's decision replaces.
    fn check_tag(&self, name: &str, object: &TagObject) -> Decided {
        self.check_ref_name(name)?;
        if ref_kind(name) != Some(RefKind::Tag) {
            return Err(RepoError::RefName(name.to_owned()));
        }
        let format = self.format();
        if matches!(object, TagObject::Annotated(_)) && format != StoreFormat::V3 {
            return Err(RepoError::Store(StoreError::FormatRequired {
                name: FEATURE.to_owned(),
                needs: StoreFormat::V3.version(),
                store: format.version(),
            }));
        }
        Ok(TagMade::new(Applied::New))
    }

    /// Stores the objects of the tags `pending` names in `batch` as blobs, flushed once: what
    /// each of `batch`'s tags is to carry, by index.
    fn put_tag_blobs(
        &self,
        batch: &[TagWant],
        pending: &[usize],
    ) -> Result<HashMap<usize, TagKind>, RepoError> {
        let mut kinds = HashMap::with_capacity(pending.len());
        let mut blobs = Vec::new();
        for &i in pending {
            let TagObject::Annotated(bytes) = &batch[i].2 else {
                kinds.insert(i, TagKind::Lightweight);
                continue;
            };
            let blob = Hashed::new(ObjectKind::Blob, bytes.clone());
            kinds.insert(
                i,
                TagKind::Annotated {
                    blob: BlobId::new(blob.id()),
                },
            );
            blobs.push(blob);
        }
        if blobs.is_empty() {
            return Ok(kinds);
        }
        self.objects.put_hashed_batch(blobs)?;
        self.objects.flush()?;
        Ok(kinds)
    }

    /// Decides the tags `chunk` names in `batch`, under the lock, and commits what it queued as
    /// one frame. A tag's refusal is its outcome; a failure of the store fails the frame.
    fn write_tags(
        &self,
        batch: &[TagWant],
        kinds: &HashMap<usize, TagKind>,
        chunk: &[usize],
        actor: &str,
    ) -> Result<Vec<Decided>, RepoError> {
        // 1. The lock, then the committed facts as of it.
        let mut write = self.meta.write()?;
        let read = self.meta.read()?;
        let mut frame = TagFrame {
            write: &mut write,
            read: &read,
            made: HashMap::new(),
            ts: unix_now(),
            actor,
        };

        // 2. Each tag queued, found made, or refused.
        let mut decided = Vec::with_capacity(chunk.len());
        for &i in chunk {
            let (name, target, _) = &batch[i];
            let want = kinds.get(&i).copied().unwrap_or(TagKind::Lightweight);
            decided.push(frame.queue(name, *target, want)?);
        }

        // 3. The frame, if it holds a record: a failure after its commit point is a warning.
        if write.queued().is_empty() {
            return Ok(decided);
        }
        let warning = match write.commit() {
            Ok(_) => None,
            Err(StoreError::PostCommit {
                frame_seq,
                frame_crc,
                source,
            }) => Some(post_commit_warning(
                "the tags",
                OpId::new(frame_seq, frame_crc),
                &source,
            )),
            Err(e) => return Err(e.into()),
        };
        for made in decided.iter_mut().flatten() {
            if made.applied == Applied::New {
                made.warnings.extend(warning.clone());
            }
        }
        Ok(decided)
    }
}

/// One frame of tag creates, being queued.
struct TagFrame<'w, 's> {
    /// The write transaction, its lock held.
    write: &'w mut MetaWrite<'s>,
    /// The committed metadata, read under the lock.
    read: &'w MetaRead<'s>,
    /// What each tag this frame created carries.
    made: HashMap<String, TagKind>,
    /// When the frame's facts are made, in Unix seconds.
    ts: i64,
    /// Who makes them.
    actor: &'w str,
}

impl TagFrame<'_, '_> {
    /// Queues the create of tag `name` at `target` carrying `want`, or finds it made alike
    /// ([`Applied::Already`]), or refuses it.
    fn queue(
        &mut self,
        name: &str,
        target: ChangesetId,
        want: TagKind,
    ) -> Result<Decided, RepoError> {
        // 1. A tag there: made alike, or immutable.
        let key = ref_name_key(name);
        let current = self.write.get_ref(name).map_err(ref_error)?;
        if let Some(at) = current {
            let held = match self.made.get(name) {
                Some(&kind) => kind,
                None => kind_of(&self.read.facts_by_subject(key)?, at)?,
            };
            if at == target && held == want {
                return Ok(Ok(TagMade::new(Applied::Already)));
            }
            return Ok(Err(StoreError::TagImmutable(name.to_owned()).into()));
        }

        // 2. The ref's create; a target the store does not hold is this tag's refusal.
        match self.write.cas_ref(name, None, Some(target), self.actor) {
            Ok(CasResult::Applied { .. }) => {}
            Ok(_) => return Err(RepoError::Internal("a tag's create found it made".into())),
            Err(e @ StoreError::UnknownRefTarget(_)) => return Ok(Err(ref_error(e))),
            Err(e) => return Err(ref_error(e)),
        }

        // 3. Its fact: an object always; none only if its name never held a Tag fact.
        let held_facts =
            (self.read.facts_by_subject(key)?.iter()).any(|fact| fact.kind() == FactKind::Tag);
        let fact = match want {
            TagKind::Annotated { blob } => {
                Some(self.fact(key, target, TagType::Annotated, Some(blob))?)
            }
            TagKind::Lightweight if held_facts => {
                Some(self.fact(key, target, TagType::Lightweight, None)?)
            }
            TagKind::Lightweight => None,
        };
        if let Some(fact) = fact {
            self.write.put_fact(fact, Expect::Any)?;
        }
        self.made.insert(name.to_owned(), want);
        Ok(Ok(TagMade::new(Applied::New)))
    }

    /// The Tag fact of type `ty` about the name of `key` at `target`, naming `blob`.
    fn fact(
        &self,
        key: [u8; 32],
        target: ChangesetId,
        ty: TagType,
        blob: Option<BlobId>,
    ) -> Result<Fact, StoreError> {
        let mut body = vec![SCHEMA];
        if let Some(blob) = blob {
            body.extend_from_slice(&blob.as_object_id().0);
        }
        let other = target.as_object_id().0;
        Fact::new(
            FactKind::Tag,
            ty.code(),
            key,
            other,
            self.ts,
            self.actor.to_owned(),
            body,
        )
    }
}

impl<O: ObjectStore> RepoRead<'_, O> {
    /// Tag `name` (`tags/<path>`), if the snapshot holds it: its target, and what it carries by
    /// the binding rule (the module documentation). [`RepoError::RefName`] for a name that is no
    /// tag; FORMAT 1, [`StoreError::FormatRequired`].
    pub fn tag(&self, name: &str) -> Result<Option<TagRead>, RepoError> {
        if ref_kind(name) != Some(RefKind::Tag) {
            return Err(RepoError::RefName(name.to_owned()));
        }
        let Some(state) = self.get_ref(name)? else {
            return Ok(None);
        };
        let facts = self.meta.facts_by_subject(ref_name_key(name))?;
        let kind = kind_of(&facts, state.target)?;
        Ok(Some(TagRead::new(name.to_owned(), state.target, kind)))
    }

    /// Every tag whose ref name starts with `prefix` (`tags/` for all), by name, each as
    /// [`RepoRead::tag`] reads it.
    pub fn tags(&self, prefix: &str) -> Result<Vec<TagRead>, RepoError> {
        let mut tags = Vec::new();
        for (name, state) in self.list_refs()? {
            if ref_kind(&name) != Some(RefKind::Tag) || !name.starts_with(prefix) {
                continue;
            }
            let facts = self.meta.facts_by_subject(ref_name_key(&name))?;
            let kind = kind_of(&facts, state.target)?;
            tags.push(TagRead::new(name, state.target, kind));
        }
        Ok(tags)
    }
}

/// What a tag at `target` carries, by the binding rule: the object of the last Tag fact among
/// `facts` (about its name, in journal order) if that fact is Annotated and names `target`.
fn kind_of(facts: &[Fact], target: ChangesetId) -> Result<TagKind, RepoError> {
    // 1. The last Tag fact, if it is an object's at this target.
    let last = facts.iter().rev().find(|fact| fact.kind() == FactKind::Tag);
    let Some(fact) = last else {
        return Ok(TagKind::Lightweight);
    };
    let annotated = fact.ty() == TagType::Annotated.code();
    if !annotated || fact.other() != target.as_object_id().0 {
        return Ok(TagKind::Lightweight);
    }

    // 2. Its blob: the body after the schema byte (Fact::new checked it).
    let id = (fact.body().get(1..))
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| RepoError::Internal("a Tag fact names no blob".into()))?;
    Ok(TagKind::Annotated {
        blob: BlobId::new(ObjectId::new(id)),
    })
}

/// Now, in Unix seconds.
fn unix_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
