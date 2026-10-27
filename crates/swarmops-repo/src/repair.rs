//! `repair` (the M6 design §7): rebuilds the projections of the journal, quarantines damaged
//! objects and drops damaged cache entries, under the store's maintenance lease. It never cuts
//! the journal; damage past its torn tail is reported and refused.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::os::unix::fs as unix_fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde::de::DeserializeOwned;
use swarmops_core::{ChangesetId, ObjectId, bytes};
use swarmops_store::external_ids;
use swarmops_store::meta::fsck;
use swarmops_store::{
    DerivedSlot, ExternalEntry, Frame, Index, Journal, Layout, MaintLease, MetaStore, ObjectStore,
    RealFs, Record, RefState, StoreError, StoreFormat, StoredValue, check_ref_name_in,
};

use crate::derive::{Derivation, FirstParentSkipList, LastModified, LastTouch, PathKey, SkipList};
use crate::error::RepoError;
use crate::repo::Repo;
use crate::verify::{Depth, MismatchCause, VerifyProblem, VerifyReport};

/// How long repair waits for `meta/lock` once it holds the maintenance lease.
const META_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How many journal frames a full scan reads at a time.
const FRAME_BATCH: usize = 4_096;

/// The four sorted-view files in `meta/sorted/` and the byte width of one entry in each.
const SORTED_VIEWS: [(&str, u64); 4] = [
    ("ids.key", 32),
    ("ids.rec", 8),
    ("dh.key", 32),
    ("dh.rec", 8),
];

/// The domain the FORMAT 2 file name of a ref is hashed under (the M6 design §2.5).
const REF_NAME_HASH_DOMAIN: &[u8] = b"swarmops:refname:v1\0";

/// How many hex digits of the name's hash name a FORMAT 2 ref file.
const REF_STEM_LEN: usize = 32;

/// What [`Repo::repair`] is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairRequest {
    /// List what it would do, with `done` false, and write nothing.
    pub dry_run: bool,
}

impl RepairRequest {
    /// A repair that mends, or with `dry_run` only lists.
    pub fn new(dry_run: bool) -> RepairRequest {
        RepairRequest { dry_run }
    }
}

/// What a [`RepairAction`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepairKind {
    /// Writes `journal.off` afresh from the journal.
    RebuildOffsets,
    /// Replays the journal into the index and its sorted views again.
    RebuildIndex,
    /// Writes a ref's file again from the journal.
    RebuildRef,
    /// Moves an object file that does not hash to its id, or does not decode, to
    /// `quarantine/objects/`.
    QuarantineObject,
    /// Removes a derived value's file that fails its checks.
    DropDerived,
    /// Removes an external id entry that is damaged or names no changeset of the store.
    DropExternalId,
}

impl RepairKind {
    /// Its name in the API (`RepairAction.kind`): `rebuild_offsets`, `rebuild_index`,
    /// `rebuild_ref`, `quarantine_object`, `drop_derived` or `drop_external_id`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RebuildOffsets => "rebuild_offsets",
            Self::RebuildIndex => "rebuild_index",
            Self::RebuildRef => "rebuild_ref",
            Self::QuarantineObject => "quarantine_object",
            Self::DropDerived => "drop_derived",
            Self::DropExternalId => "drop_external_id",
        }
    }
}

/// One thing [`Repo::repair`] did, or would do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairAction {
    /// What.
    pub kind: RepairKind,
    /// On what: an object id, a ref name or a file class (never a host path).
    pub target: String,
    /// Whether it was done: false for a dry run.
    pub done: bool,
}

impl RepairAction {
    /// Takes every field.
    pub fn new(kind: RepairKind, target: impl Into<String>, done: bool) -> RepairAction {
        RepairAction {
            kind,
            target: target.into(),
            done,
        }
    }
}

/// What [`Repo::repair`] did, and what a verify after it found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepairReport {
    /// What it did, in order.
    pub actions: Vec<RepairAction>,
    /// The verify after it.
    pub after: VerifyReport,
}

type DerivedTargetKey = (String, ObjectId, Option<[u8; 8]>);
type ExpectedRefs = (BTreeMap<String, RefState>, BTreeMap<String, String>);
type OffsetsState = (Option<TempStage>, Layout, bool, Option<Vec<Frame>>);

impl<O: ObjectStore> Repo<O> {
    /// Mends what verify finds that the journal or a quarantine can mend (the M6 design §7):
    ///
    /// 1. Under the maintenance lease ([`MaintLease::try_acquire`]), checks `journal.off` and, if
    ///    it is missing or damaged, rebuilds it from `journal.log` ([`fsck::rebuild_offsets`]).
    ///    Damage in `journal.log` beyond a torn tail fails with [`StoreError::JournalCorrupt`] and
    ///    leaves the journal untouched.
    /// 2. Runs a deep [`Repo::verify`].
    /// 3. Rebuilds the changeset index and its sorted views ([`fsck::rebuild_index`]) if they or
    ///    the checkpoint are damaged or disagree with the journal.
    /// 4. Rebuilds the ref files ([`fsck::rebuild_refs`]) if any ref file is corrupt, dangling,
    ///    missing or stale against the journal's `RefMove` records.
    /// 5. Quarantines every corrupt object file under `<store>/quarantine/objects/<64 hex>` with
    ///    `rename_durable`, freeing its slot so that a put or import of the same content can
    ///    restore it.
    /// 6. Drops damaged or mismatched derived values and damaged, dangling or mismatched external
    ///    id entries.
    /// 7. Runs a deep [`Repo::verify`] again for `after` (or keeps the initial verify when nothing
    ///    was changed).
    pub fn repair(&self, req: RepairRequest) -> Result<RepairReport, RepoError> {
        let lease_path = self.layout.meta_path("maint.lock");
        let lease_existed = lease_path.exists();
        let lease = MaintLease::try_acquire(&self.layout)?;
        let result = self.repair_under_lease(&lease, req);
        drop(lease);
        if req.dry_run && !lease_existed {
            let _ = fs::remove_file(lease_path.as_std_path());
        }
        result
    }

    fn repair_under_lease(
        &self,
        lease: &MaintLease,
        req: RepairRequest,
    ) -> Result<RepairReport, RepoError> {
        let mut actions = Vec::new();
        let (dry_stage, effective_layout, offsets_rebuilt, frames) =
            self.repair_journal_offsets(req.dry_run, &mut actions)?;

        // 2. Deep verify of the store.
        let verified = self.verify(Depth::Deep)?;
        let dry_meta = if req.dry_run && offsets_rebuilt {
            Some(MetaStore::open_in(effective_layout.clone(), Arc::new(RealFs::new()))?.check()?)
        } else {
            None
        };

        // 3. Index, sorted views and checkpoint.
        let needs_rebuild_index = self.needs_rebuild_index(
            &effective_layout,
            &verified,
            dry_meta.as_ref(),
            frames.as_deref(),
        )?;
        if needs_rebuild_index {
            actions.push(RepairAction::new(
                RepairKind::RebuildIndex,
                "changeset_index",
                !req.dry_run,
            ));
        }

        // 4. Ref files.
        let refs_to_rebuild = self.refs_to_rebuild(
            &effective_layout,
            &verified,
            dry_meta.as_ref(),
            frames.as_deref(),
        )?;
        for name in &refs_to_rebuild {
            actions.push(RepairAction::new(
                RepairKind::RebuildRef,
                name.clone(),
                !req.dry_run,
            ));
        }

        if !req.dry_run {
            // Rebuild refs before rebuild_index or flush so that MetaStore::write's replay never
            // trips on a corrupt ref file or miscounts a missing ref's version from 0.
            if !refs_to_rebuild.is_empty() {
                fsck::rebuild_refs(&self.layout, Arc::clone(&self.fs))?;
            }
            if needs_rebuild_index {
                fsck::rebuild_index(&self.layout, Arc::clone(&self.fs))?;
            } else if offsets_rebuilt || !refs_to_rebuild.is_empty() {
                MetaStore::open_in(self.layout.clone(), Arc::clone(&self.fs))?.flush()?;
            }
        }
        drop(dry_stage);

        // 5. Corrupt objects to quarantine.
        let corrupt_objects = collect_corrupt_objects(&verified);
        for id in &corrupt_objects {
            actions.push(RepairAction::new(
                RepairKind::QuarantineObject,
                id.to_hex(),
                !req.dry_run,
            ));
        }

        // 6. Damaged derived values and external id entries to drop.
        let derived_to_drop = self.collect_derived_to_drop(&verified)?;
        for (name, subject, _) in derived_to_drop.keys() {
            actions.push(RepairAction::new(
                RepairKind::DropDerived,
                format!("{name}:{subject}"),
                !req.dry_run,
            ));
        }
        let external_to_drop = self.collect_external_to_drop(&verified, frames.as_deref())?;
        for (system, key) in external_to_drop.keys() {
            actions.push(RepairAction::new(
                RepairKind::DropExternalId,
                format!("{system}:{key}"),
                !req.dry_run,
            ));
        }

        if !req.dry_run
            && (!corrupt_objects.is_empty()
                || !derived_to_drop.is_empty()
                || !external_to_drop.is_empty())
        {
            let _meta_lock = lease.meta_lock(&self.layout, META_LOCK_TIMEOUT)?;
            let store_fs = self.store_fs();
            if !corrupt_objects.is_empty() {
                self.quarantine_objects(&store_fs, &corrupt_objects)?;
            }
            for path in derived_to_drop.values() {
                remove_file_durable(&store_fs, path)?;
            }
            for path in external_to_drop.values() {
                remove_file_durable(&store_fs, path)?;
            }
        }

        // 7. Re-verify when changes were made.
        let after = if !req.dry_run && !actions.is_empty() {
            self.verify(Depth::Deep)?
        } else {
            verified
        };
        Ok(RepairReport { actions, after })
    }

    /// Diagnoses and (when `!dry_run`) rebuilds `journal.off` from `journal.log`.
    fn repair_journal_offsets(
        &self,
        dry_run: bool,
        actions: &mut Vec<RepairAction>,
    ) -> Result<OffsetsState, RepoError> {
        let mut dry_stage: Option<TempStage> = None;
        let mut effective_layout = self.layout.clone();
        let mut offsets_rebuilt = false;
        let frames = match self.check_journal_offsets() {
            Ok(frames) => Some(frames),
            Err(StoreError::JournalCorrupt { .. }) => {
                if dry_run {
                    let (dir, staged) = stage_dry_run_offsets(&self.layout)?;
                    fsck::rebuild_offsets(&staged, Arc::new(RealFs::new()))?;
                    restore_staged_ckpt(&self.layout, &staged)?;
                    let frames = read_all_journal_frames(&staged, Arc::new(RealFs::new())).ok();
                    effective_layout = staged;
                    dry_stage = Some(dir);
                    offsets_rebuilt = true;
                    actions.push(RepairAction::new(
                        RepairKind::RebuildOffsets,
                        "journal_offsets",
                        false,
                    ));
                    frames
                } else {
                    if !self.layout.journal_off().exists() {
                        self.store_fs()
                            .append_durable(self.layout.journal_off().as_std_path(), &[])?;
                    }
                    fsck::rebuild_offsets(&self.layout, Arc::clone(&self.fs))?;
                    offsets_rebuilt = true;
                    actions.push(RepairAction::new(
                        RepairKind::RebuildOffsets,
                        "journal_offsets",
                        true,
                    ));
                    read_all_journal_frames(&self.layout, Arc::clone(&self.fs)).ok()
                }
            }
            Err(e) => return Err(e.into()),
        };
        Ok((dry_stage, effective_layout, offsets_rebuilt, frames))
    }

    /// Reads all journal frames if `journal.off` is present and consistent with `journal.log`;
    /// returns `StoreError::JournalCorrupt` when `journal.off` is missing (while `journal.log`
    /// exists) or reading the frames fails as corrupt.
    fn check_journal_offsets(&self) -> Result<Vec<Frame>, StoreError> {
        if self.layout.journal_log().exists() && !self.layout.journal_off().exists() {
            return Err(StoreError::JournalCorrupt { offset: 0 });
        }
        read_all_journal_frames(&self.layout, Arc::clone(&self.fs))
    }

    /// Whether the changeset index, its sorted views or the checkpoint need [`fsck::rebuild_index`].
    fn needs_rebuild_index(
        &self,
        effective_layout: &Layout,
        verified: &VerifyReport,
        dry_meta: Option<&swarmops_store::MetaCheck>,
        frames: Option<&[Frame]>,
    ) -> Result<bool, RepoError> {
        let problem_triggers = verified.problems.iter().any(|problem| match problem {
            VerifyProblem::IndexMismatch {
                cause: MismatchCause::Journal | MismatchCause::SortedView,
                ..
            } => true,
            VerifyProblem::MetaCorrupt(what) => is_index_or_ckpt_corrupt(what),
            _ => false,
        });
        if problem_triggers {
            return Ok(true);
        }
        if let Some(meta) = dry_meta {
            let meta_triggers = !meta.journal_mismatches.is_empty()
                || !meta.view_mismatches.is_empty()
                || (meta.meta_corrupt.iter()).any(|what| is_index_or_ckpt_corrupt(what));
            if meta_triggers {
                return Ok(true);
            }
        }
        if let Some(frames) = frames {
            let journal_changesets = bytes::to_u64(
                frames
                    .iter()
                    .flat_map(|frame| &frame.records)
                    .filter(|record| matches!(record, Record::Changeset { .. }))
                    .count(),
            );
            let Ok(index) = Index::open(effective_layout, Arc::clone(&self.fs)) else {
                return Ok(true);
            };
            if index.len() != journal_changesets
                || index.held()? != journal_changesets
                || !sorted_views_aligned(effective_layout)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The ref names whose files need [`fsck::rebuild_refs`], in sorted order.
    fn refs_to_rebuild(
        &self,
        effective_layout: &Layout,
        verified: &VerifyReport,
        dry_meta: Option<&swarmops_store::MetaCheck>,
        frames: Option<&[Frame]>,
    ) -> Result<BTreeSet<String>, RepoError> {
        let mut names = BTreeSet::new();
        let mut stem_to_name = BTreeMap::new();
        if let Some(frames) = frames {
            let (expected, stems) = expected_ref_states(self.layout.format(), frames)?;
            stem_to_name = stems;
            let disk_refs =
                MetaStore::open_in(effective_layout.clone(), Arc::clone(&self.fs))?.ref_files()?;
            for (name, state) in &expected {
                let matches = disk_refs
                    .iter()
                    .any(|file| file.name == *name && file.state.as_ref().ok() == Some(state));
                if !matches {
                    names.insert(name.clone());
                }
            }
            for file in disk_refs {
                let resolved = stem_to_name
                    .get(&file.name)
                    .cloned()
                    .unwrap_or(file.name.clone());
                if expected.get(&resolved) != file.state.as_ref().ok() {
                    names.insert(resolved);
                }
            }
        }
        for problem in &verified.problems {
            if let VerifyProblem::CorruptRef(name) | VerifyProblem::DanglingRef(name) = problem {
                let resolved = stem_to_name
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                names.insert(resolved);
            }
        }
        if let Some(meta) = dry_meta {
            for (name, _) in meta.ref_problems() {
                let resolved = stem_to_name
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                names.insert(resolved);
            }
        }
        Ok(names)
    }

    /// Moves each object in `ids` from `objects/<xx>/<62 hex>` to
    /// `quarantine/objects/<64 hex>` with `rename_durable`, syncing the object file before the
    /// rename (W6) and the fan-out directory after it.
    fn quarantine_objects(
        &self,
        store_fs: &swarmops_store::StoreFs,
        ids: &[ObjectId],
    ) -> Result<(), RepoError> {
        let root = self.layout.root();
        let quarantine = root.join("quarantine");
        let quarantine_objects = quarantine.join("objects");
        let quarantine_existed = store_fs.exists(quarantine.as_std_path())?;
        let objects_existed = store_fs.exists(quarantine_objects.as_std_path())?;
        store_fs.create_dir_all(quarantine_objects.as_std_path())?;
        if !objects_existed {
            store_fs.fsync_dir(quarantine.as_std_path())?;
        }
        if !quarantine_existed {
            store_fs.fsync_dir(root.as_std_path())?;
        }
        let mut fanouts = BTreeSet::new();
        for &id in ids {
            let from = self.layout.object_path(id);
            if !store_fs.exists(from.as_std_path())? {
                continue;
            }
            let to = quarantine_objects.join(id.to_hex());
            store_fs.sync_data(from.as_std_path())?;
            store_fs.rename_durable(from.as_std_path(), to.as_std_path())?;
            if let Some(fanout) = from.parent() {
                fanouts.insert(fanout.to_owned());
            }
        }
        for fanout in fanouts {
            store_fs.fsync_dir(fanout.as_std_path())?;
        }
        Ok(())
    }

    /// Every derived value file that fails its format/trailer checks, fails to decode for its
    /// derivation, or was reported as [`VerifyProblem::DerivedMismatch`].
    fn collect_derived_to_drop(
        &self,
        verified: &VerifyReport,
    ) -> Result<BTreeMap<DerivedTargetKey, Utf8PathBuf>, RepoError> {
        let mut drop_files = BTreeMap::new();
        for problem in &verified.problems {
            if let VerifyProblem::DerivedMismatch {
                name,
                changeset,
                key,
            } = problem
            {
                let slot = DerivedSlot::new(name, 1, changeset.0, key);
                let path = self.layout.derived_path(&slot);
                drop_files.insert((name.clone(), changeset.0, key_hash(key)), path);
            }
        }
        for (name, version) in self.derived_name_versions()? {
            for listed in self.derived.list(&name, version)? {
                let path = derived_file_path(
                    &self.layout,
                    &name,
                    version,
                    listed.subject,
                    listed.key_hash,
                );
                let Some(StoredValue { key, value, .. }) =
                    self.derived.read_listed(&name, version, &listed)?
                else {
                    drop_files.insert((name.clone(), listed.subject, listed.key_hash), path);
                    continue;
                };
                if !derived_payload_valid(&name, version, &key, &value) {
                    drop_files.insert((name.clone(), listed.subject, listed.key_hash), path);
                }
            }
        }
        Ok(drop_files)
    }

    /// Every `(name, version)` directory under `meta/derived/` that
    /// [`swarmops_store::DerivedStore::list`] accepts.
    fn derived_name_versions(&self) -> Result<Vec<(String, u32)>, RepoError> {
        let dir = self.layout.derived_dir();
        let entries = match fs::read_dir(dir.as_std_path()) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(self
                    .store_fs()
                    .locate(swarmops_store::FileOp::ReadDir, &dir, e)
                    .into());
            }
        };
        let mut pairs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                self.store_fs()
                    .locate(swarmops_store::FileOp::ReadDir, &dir, e)
            })?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !is_valid_derived_name(&name) {
                continue;
            }
            let name_dir = dir.join(&name);
            let Ok(versions) = fs::read_dir(name_dir.as_std_path()) else {
                continue;
            };
            for ver_entry in versions.flatten() {
                let Ok(ver_str) = ver_entry.file_name().into_string() else {
                    continue;
                };
                if let Ok(version) = ver_str.parse::<u32>()
                    && version >= 1
                    && ver_str == version.to_string()
                {
                    pairs.push((name.clone(), version));
                }
            }
        }
        pairs.sort();
        Ok(pairs)
    }

    /// Every external id entry that is corrupt, dangling or mismatched.
    fn collect_external_to_drop(
        &self,
        verified: &VerifyReport,
        frames: Option<&[Frame]>,
    ) -> Result<BTreeMap<(String, String), Utf8PathBuf>, RepoError> {
        let mut drop_entries = BTreeMap::new();
        for problem in &verified.problems {
            match problem {
                VerifyProblem::ExternalIdCorrupt { system, key }
                | VerifyProblem::ExternalIdDangling { system, key, .. }
                | VerifyProblem::ExternalIdMismatch { system, key, .. } => {
                    if let Some(raw_key) = decode_hex(key) {
                        let path = self.layout.external_id_path(system, &raw_key);
                        drop_entries.insert((system.clone(), key.clone()), path);
                    }
                }
                _ => {}
            }
        }
        if let Some(frames) = frames {
            let known: HashSet<ChangesetId> = frames
                .iter()
                .flat_map(|frame| &frame.records)
                .filter_map(|record| match record {
                    Record::Changeset { id, .. } => Some(*id),
                    _ => None,
                })
                .collect();
            for system in self.external_ids.systems()? {
                for (raw_key, entry) in self.external_ids.list(&system)? {
                    let bad = match entry {
                        Err(StoreError::ExternalIdCorrupt { .. }) => true,
                        Err(e) => return Err(e.into()),
                        Ok(ExternalEntry { changeset, .. }) => !known.contains(&changeset),
                    };
                    if bad {
                        let hex = external_ids::lower_hex(&raw_key);
                        let path = self.layout.external_id_path(&system, &raw_key);
                        drop_entries.insert((system.clone(), hex), path);
                    }
                }
            }
        }
        Ok(drop_entries)
    }
}

/// Reads every committed frame of `layout`'s journal from 0.
fn read_all_journal_frames(
    layout: &Layout,
    fs: Arc<dyn swarmops_store::FsOps>,
) -> Result<Vec<Frame>, StoreError> {
    let journal = Journal::open(layout, fs)?;
    let mut frames = Vec::new();
    loop {
        let read = journal.frames_from(bytes::to_u64(frames.len()), FRAME_BATCH)?;
        if read.is_empty() {
            break;
        }
        frames.extend(read);
    }
    let again = usize::from(!frames.is_empty());
    let start = frames.last().map_or(0, |frame| frame.offset);
    let tail = journal.tail_frames_from(start)?;
    frames.extend(tail.into_iter().skip(again));
    Ok(frames)
}

struct TempStage(Utf8PathBuf);

impl TempStage {
    fn new() -> io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let nonce = RandomState::new().hash_one(SEQ.fetch_add(1, Ordering::Relaxed));
        let dir = base.join(format!(
            "swarmops-repair-dry-{}-{nonce:016x}",
            std::process::id()
        ));
        fs::create_dir_all(&dir)?;
        let utf8 = Utf8PathBuf::from_path_buf(dir)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "temp_dir is not UTF-8"))?;
        Ok(Self(utf8))
    }
}

impl Drop for TempStage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.as_std_path());
    }
}

/// Stages a temporary store layout for a dry-run `rebuild_offsets`, linking the read-only files
/// from `layout` and copying `journal.off` and `journal.ckpt` so the real store is untouched.
fn stage_dry_run_offsets(layout: &Layout) -> Result<(TempStage, Layout), StoreError> {
    let map_io = |e| {
        StoreError::from(swarmops_store::IoFailure::new(
            swarmops_store::FileOp::Open,
            None,
            e,
        ))
    };
    let dir = TempStage::new().map_err(map_io)?;
    let root = dir.0.join("store");
    let staged = Layout::with_format(root, layout.format());
    fs::create_dir_all(staged.meta_dir().as_std_path()).map_err(map_io)?;
    if layout.format_file().exists() {
        fs::copy(
            layout.format_file().as_std_path(),
            staged.format_file().as_std_path(),
        )
        .map_err(map_io)?;
    }
    if layout.journal_log().exists() {
        unix_fs::symlink(
            layout.journal_log().as_std_path(),
            staged.journal_log().as_std_path(),
        )
        .map_err(map_io)?;
    }
    if layout.journal_off().exists() {
        fs::copy(
            layout.journal_off().as_std_path(),
            staged.journal_off().as_std_path(),
        )
        .map_err(map_io)?;
    } else {
        fs::write(staged.journal_off().as_std_path(), b"").map_err(map_io)?;
    }
    if layout.journal_ckpt().exists() {
        fs::copy(
            layout.journal_ckpt().as_std_path(),
            staged.journal_ckpt().as_std_path(),
        )
        .map_err(map_io)?;
    }
    for sub in ["idx", "sorted", "parents.log", "refs", "refs2"] {
        let src = layout.meta_path(sub);
        if src.exists() {
            let dst = staged.meta_path(sub);
            unix_fs::symlink(src.as_std_path(), dst.as_std_path()).map_err(map_io)?;
        }
    }
    Ok((dir, staged))
}

/// Restores `layout`'s original checkpoint into `staged` after a dry-run `rebuild_offsets`.
fn restore_staged_ckpt(layout: &Layout, staged: &Layout) -> Result<(), StoreError> {
    if layout.journal_ckpt().exists() {
        fs::copy(
            layout.journal_ckpt().as_std_path(),
            staged.journal_ckpt().as_std_path(),
        )
        .map_err(|e| {
            StoreError::from(swarmops_store::IoFailure::new(
                swarmops_store::FileOp::Open,
                None,
                e,
            ))
        })?;
    }
    Ok(())
}

/// Whether a [`VerifyProblem::MetaCorrupt`] message names a damaged index or checkpoint.
fn is_index_or_ckpt_corrupt(what: &str) -> bool {
    what.starts_with("the changeset index does not open")
        || what.starts_with("meta/journal.ckpt ")
        || what.starts_with("meta/journal2.ckpt ")
}

/// Whether all four sorted-view files in `meta/sorted/` have the same entry count and no
/// partial entry.
fn sorted_views_aligned(layout: &Layout) -> bool {
    let sorted = layout.meta_path("sorted");
    let mut expected: Option<u64> = None;
    for (name, width) in SORTED_VIEWS {
        let Ok(meta) = fs::metadata(sorted.join(name).as_std_path()) else {
            return false;
        };
        if !meta.len().is_multiple_of(width) {
            return false;
        }
        let count = meta.len() / width;
        if let Some(first) = expected {
            if count != first {
                return false;
            }
        } else {
            expected = Some(count);
        }
    }
    true
}

/// Computes the expected ref states from all `frames` and a mapping from FORMAT 2 file stems to
/// ref names.
fn expected_ref_states(format: StoreFormat, frames: &[Frame]) -> Result<ExpectedRefs, StoreError> {
    let mut states: BTreeMap<String, RefState> = BTreeMap::new();
    let mut stems = BTreeMap::new();
    for frame in frames {
        let mut targets = BTreeMap::new();
        for record in &frame.records {
            if let Record::RefMove { name, new, .. } = record {
                check_ref_name_in(format, name).map_err(|_| StoreError::JournalCorrupt {
                    offset: frame.offset,
                })?;
                if format == StoreFormat::V2 {
                    stems.insert(ref_file_stem(name), name.clone());
                }
                targets.insert(name.as_str(), *new);
            }
        }
        for (name, target) in targets {
            let next = match (states.get(name).copied(), target) {
                (_, None) => None,
                (Some(current), Some(target)) if current.target == target => Some(current),
                (current, Some(target)) => Some(RefState::new(
                    target,
                    current.map_or(0, |s| s.version).saturating_add(1),
                )),
            };
            match next {
                Some(state) => states.insert(name.to_owned(), state),
                None => states.remove(name),
            };
        }
    }
    Ok((states, stems))
}

/// The FORMAT 2 file stem of ref `name`.
fn ref_file_stem(name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(REF_NAME_HASH_DOMAIN);
    hasher.update(name.as_bytes());
    let hex = hasher.finalize().to_hex();
    hex[..REF_STEM_LEN].to_owned()
}

/// The corrupt object ids reported in `verified`.
fn collect_corrupt_objects(verified: &VerifyReport) -> Vec<ObjectId> {
    let mut ids = Vec::new();
    for problem in &verified.problems {
        if let VerifyProblem::CorruptObject(id) = problem {
            ids.push(*id);
        }
    }
    ids
}

/// Whether `name` is a valid derivation directory name (`1..=64` bytes of `[a-z0-9_]`, not `tmp`).
fn is_valid_derived_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && name != "tmp"
}

/// Whether the decoded `(key, value)` of a whole derived file is valid for known derivations.
fn derived_payload_valid(name: &str, version: u32, key: &[u8], value: &[u8]) -> bool {
    match (name, version) {
        (FirstParentSkipList::NAME, FirstParentSkipList::VERSION) => {
            key.is_empty() && decodes_whole::<SkipList>(value)
        }
        (LastModified::NAME, LastModified::VERSION) => {
            decodes_whole::<PathKey>(key) && decodes_whole::<LastTouch>(value)
        }
        ("git_tree_oid" | "git_blob_oid", 1) => key.is_empty() && value.len() == 20,
        ("git_oid", 1) => key.is_empty() && value.len() == 21 && matches!(value[20], 0 | 1),
        // The memos under a memo key: the key's 16-byte id as the slot key, the value then a
        // 32-byte MAC.
        ("git_tree_oid_mac" | "git_blob_oid_mac", 1) => key.len() == 16 && value.len() == 52,
        ("git_oid_mac", 1) => {
            key.len() == 16
                && value.len() == 54
                && matches!(value[20], 0 | 1)
                && matches!(value[21], 0 | 1)
        }
        _ => true,
    }
}

fn decodes_whole<T: DeserializeOwned>(bytes: &[u8]) -> bool {
    matches!(postcard::take_from_bytes::<T>(bytes), Ok((_, [])))
}

/// The first 8 bytes of BLAKE3 of `key`, or `None` for an empty key.
fn key_hash(key: &[u8]) -> Option<[u8; 8]> {
    if key.is_empty() {
        return None;
    }
    let hash = blake3::hash(key);
    let (first, _) = hash.as_bytes().split_first_chunk::<8>().expect("32 bytes");
    Some(*first)
}

/// The path of a listed derived file.
fn derived_file_path(
    layout: &Layout,
    name: &str,
    version: u32,
    subject: ObjectId,
    key_hash: Option<[u8; 8]>,
) -> Utf8PathBuf {
    let hex = subject.to_hex();
    let (fanout, rest) = hex.split_at(2);
    let file = match key_hash {
        None => rest.to_owned(),
        Some(hash) => format!("{rest}.{:016x}", u64::from_be_bytes(hash)),
    };
    layout
        .derived_dir()
        .join(name)
        .join(version.to_string())
        .join(fanout)
        .join(file)
}

/// Decodes a lowercase hex string into bytes.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let (pairs, odd) = hex.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|&[high, low]| Some((digit(high)? << 4) | digit(low)?))
        .collect()
}

/// Removes `path` if it exists and fsyncs its parent directory.
fn remove_file_durable(
    store_fs: &swarmops_store::StoreFs,
    path: &Utf8Path,
) -> Result<(), RepoError> {
    if store_fs.exists(path.as_std_path())? {
        store_fs.remove(path.as_std_path())?;
        if let Some(parent) = path.parent() {
            store_fs.fsync_dir(parent.as_std_path())?;
        }
    }
    Ok(())
}
