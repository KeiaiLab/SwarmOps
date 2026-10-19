//! [`MemSink`], a [`GitSink`] held in memory for the export's tests: what it is told to write,
//! in order (its log), and the images a power loss after any of its calls could leave, which
//! [`reaches_only_kept`] judges.

use std::collections::{BTreeMap, BTreeSet};

use swarmops_git::{
    BranchUpdate, GitOid, GitSink, ObjectFormat, RawCommit, RawKind, RawTree, SinkError, Stored,
    git_hash,
};

/// How a [`MemSink`] holds an object. Whether a written one is durable yet is the log's to say
/// (a later `sync` named it), which [`MemSink::image`] replays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// In a pack, as the sink was made: durable, and trusted as it is.
    Packed,
    /// Written: a loose object.
    Loose,
}

/// What a [`MemSink`] runs at the start of each `cas_branch`: [`MemSink::on_cas`].
type CasHook = Box<dyn FnMut(&str, &mut Option<GitOid>)>;

/// A call of a [`MemSink`] that changed what it holds, as its log records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkCall {
    /// [`GitSink::write_object`] of the object `oid`, whether or not the sink held it already.
    Write {
        /// The object's id.
        oid: GitOid,
    },
    /// [`GitSink::sync`] of `oids`, as given.
    Sync {
        /// The objects it named.
        oids: Vec<GitOid>,
    },
    /// A [`GitSink::cas_branch`] that moved the branch `name`.
    Cas {
        /// The branch, without `refs/heads/`.
        name: String,
        /// What it named before: `None` when there was no such branch.
        from: Option<GitOid>,
        /// What it names now.
        to: GitOid,
    },
}

/// Which pending objects (written, and named by no `sync` since) a [`MemSink::image`] keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// None: the power loss took every one.
    None,
    /// All: the filesystem had written every one back.
    All,
    /// Each by a coin tossed from this seed and its id: the same objects each time for a seed.
    Seeded(u64),
}

impl Keep {
    /// Whether this keeps the pending object `oid`.
    fn keeps(self, oid: GitOid) -> bool {
        match self {
            Keep::None => false,
            Keep::All => true,
            Keep::Seeded(seed) => {
                let word = oid.0.first_chunk::<8>().expect("an id is 20 bytes");
                // SplitMix64's output function, over the seed and the id's first 8 bytes.
                let mut z = (seed ^ u64::from_le_bytes(*word)).wrapping_add(0x9e37_79b9_7f4a_7c15);
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                (z ^ (z >> 31)) & 1 == 1
            }
        }
    }
}

/// What a [`MemSink`] holds after a power loss: its durable objects and the pending ones the
/// image's [`Keep`] kept, and its branches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemImage {
    /// Every object it holds, by id: its kind and its bytes.
    pub objects: BTreeMap<GitOid, (RawKind, Vec<u8>)>,
    /// Every branch, by name without `refs/heads/`, and the object it names.
    pub branches: BTreeMap<String, GitOid>,
}

/// A [`GitSink`] held in memory, for the export's tests.
///
/// An object written is pending until a [`sync`](GitSink::sync) names it, and is then durable;
/// one the sink was made with ([`MemSink::with_packed`]) is packed, durable and trusted. A
/// branch moved by [`cas_branch`](GitSink::cas_branch) is durable at once, as the trait says.
/// Its log ([`MemSink::log`]) records each call that changed what it holds, and
/// [`MemSink::image`] replays a prefix of it into what a power loss right then would leave,
/// which [`reaches_only_kept`] judges. It also fails a call on purpose ([`MemSink::fail_at`]),
/// says which branch is checked out as it is told ([`MemSink::set_checked_out`]), and runs a
/// hook at each compare-and-swap ([`MemSink::on_cas`]).
///
/// It answers as [`GixSink`](swarmops_git::GixSink) does wherever both can: the same
/// [`Stored`], [`BranchUpdate`] and [`SinkError::Missing`] for the same calls.
#[derive(Default)]
pub struct MemSink {
    /// Every object it holds, by id: its kind, its bytes, and how it holds it.
    objects: BTreeMap<GitOid, (RawKind, Vec<u8>, Held)>,
    /// The branches [`MemSink::set_branch`] set: what every image starts from.
    seeded: BTreeMap<String, GitOid>,
    /// Every branch now, by name without `refs/heads/`.
    branches: BTreeMap<String, GitOid>,
    /// What [`GitSink::checked_out`] returns.
    checked_out: Option<String>,
    /// Each call that changed what it holds, in order.
    log: Vec<SinkCall>,
    /// How many mutating calls it was made, failed ones included.
    calls: usize,
    /// The mutating call that fails, counted from 1.
    fail_at: Option<usize>,
    /// What runs at the start of each `cas_branch`.
    on_cas: Option<CasHook>,
}

impl MemSink {
    /// A sink that holds `objects`, each of a kind and its bytes, as packed: durable, and
    /// [`Stored::Packed`], as in a destination git has packed.
    pub fn with_packed(objects: impl IntoIterator<Item = (RawKind, Vec<u8>)>) -> MemSink {
        let mut sink = MemSink::default();
        for (kind, bytes) in objects {
            let oid = git_hash(kind, &bytes);
            sink.objects.insert(oid, (kind, bytes, Held::Packed));
        }
        sink
    }

    /// Sets the branch `name` to `oid`, durably and outside the log, as if it had been so from
    /// the start: every image starts from it.
    pub fn set_branch(&mut self, name: &str, oid: GitOid) {
        self.seeded.insert(name.to_owned(), oid);
        self.branches.insert(name.to_owned(), oid);
    }

    /// Sets what [`GitSink::checked_out`] returns.
    pub fn set_checked_out(&mut self, branch: Option<&str>) {
        self.checked_out = branch.map(str::to_owned);
    }

    /// Each call that changed what it holds, in order: every `write_object` and `sync` that
    /// returned `Ok`, and every `cas_branch` that returned [`BranchUpdate::Applied`].
    pub fn log(&self) -> &[SinkCall] {
        &self.log
    }

    /// Makes its `k`-th mutating call fail: `write_object`, `sync` and `cas_branch` are counted
    /// from 1, failed ones included, and that call returns [`SinkError::Write`] and changes
    /// nothing.
    pub fn fail_at(&mut self, k: usize) {
        self.fail_at = Some(k);
    }

    /// Runs `hook` at the start of each `cas_branch`, with the branch's name and what it names
    /// now, which the hook may change, as another writer moving the branch would: the
    /// compare-and-swap then compares against that. An image does not replay such a move.
    pub fn on_cas(&mut self, hook: impl FnMut(&str, &mut Option<GitOid>) + 'static) {
        self.on_cas = Some(Box::new(hook));
    }

    /// What a power loss right after the first `after` calls of its log would leave: the packed
    /// objects, every object those calls wrote and a later one of them synced, and of the rest
    /// those `keep` keeps; the branches [`MemSink::set_branch`] set, moved as those calls moved
    /// them. `image(0, _)` is the sink before its first call.
    pub fn image(&self, after: usize, keep: Keep) -> MemImage {
        assert!(
            after <= self.log.len(),
            "the log holds {} calls, not {after}",
            self.log.len()
        );
        let held = |oid: &GitOid| {
            let (kind, bytes, _) = &self.objects[oid];
            (*kind, bytes.clone())
        };
        let mut objects: BTreeMap<GitOid, (RawKind, Vec<u8>)> = self
            .objects
            .iter()
            .filter(|(_, (_, _, how))| *how == Held::Packed)
            .map(|(oid, _)| (*oid, held(oid)))
            .collect();
        let mut pending = BTreeSet::new();
        let mut branches = self.seeded.clone();
        for call in &self.log[..after] {
            match call {
                SinkCall::Write { oid } => {
                    if !objects.contains_key(oid) {
                        pending.insert(*oid);
                    }
                }
                SinkCall::Sync { oids } => {
                    for oid in oids {
                        if pending.remove(oid) {
                            objects.insert(*oid, held(oid));
                        }
                    }
                }
                SinkCall::Cas { name, to, .. } => {
                    branches.insert(name.clone(), *to);
                }
            }
        }
        for oid in pending {
            if keep.keeps(oid) {
                objects.insert(oid, held(&oid));
            }
        }
        MemImage { objects, branches }
    }

    /// Counts a mutating call, and fails it if it is the one [`MemSink::fail_at`] named.
    fn call(&mut self, what: &str) -> Result<(), SinkError> {
        self.calls += 1;
        if self.fail_at == Some(self.calls) {
            return Err(SinkError::Write {
                what: what.to_owned(),
                reason: format!("mutating call {} fails on purpose", self.calls),
            });
        }
        Ok(())
    }
}

impl GitSink for MemSink {
    fn object_format(&self) -> ObjectFormat {
        ObjectFormat::Sha1
    }

    fn stored(&self, oid: GitOid) -> Result<Stored, SinkError> {
        Ok(match self.objects.get(&oid) {
            None => Stored::Absent,
            Some((_, _, Held::Packed)) => Stored::Packed,
            Some(_) => Stored::Loose,
        })
    }

    fn read(&self, oid: GitOid) -> Result<(RawKind, Vec<u8>), SinkError> {
        let (kind, bytes, _) = self.objects.get(&oid).ok_or(SinkError::Missing { oid })?;
        Ok((*kind, bytes.clone()))
    }

    fn write_object(&mut self, kind: RawKind, bytes: &[u8]) -> Result<GitOid, SinkError> {
        self.call("write_object")?;
        let oid = git_hash(kind, bytes);
        self.objects
            .entry(oid)
            .or_insert_with(|| (kind, bytes.to_vec(), Held::Loose));
        self.log.push(SinkCall::Write { oid });
        Ok(oid)
    }

    fn sync(&mut self, oids: &[GitOid]) -> Result<(), SinkError> {
        self.call("sync")?;
        if let Some(&oid) = oids.iter().find(|oid| !self.objects.contains_key(oid)) {
            return Err(SinkError::Missing { oid });
        }
        self.log.push(SinkCall::Sync {
            oids: oids.to_vec(),
        });
        Ok(())
    }

    fn branch(&self, name: &str) -> Result<Option<GitOid>, SinkError> {
        Ok(self.branches.get(name).copied())
    }

    fn checked_out(&self) -> Result<Option<String>, SinkError> {
        Ok(self.checked_out.clone())
    }

    fn cas_branch(
        &mut self,
        name: &str,
        expected: Option<GitOid>,
        new: GitOid,
    ) -> Result<BranchUpdate, SinkError> {
        self.call("cas_branch")?;
        if !self.objects.contains_key(&new) {
            return Err(SinkError::Missing { oid: new });
        }
        let mut actual = self.branches.get(name).copied();
        if let Some(hook) = &mut self.on_cas {
            hook(name, &mut actual);
            match actual {
                Some(oid) => self.branches.insert(name.to_owned(), oid),
                None => self.branches.remove(name),
            };
        }
        if actual != expected {
            return Ok(BranchUpdate::Mismatch { actual });
        }
        self.branches.insert(name.to_owned(), new);
        self.log.push(SinkCall::Cas {
            name: name.to_owned(),
            from: expected,
            to: new,
        });
        Ok(BranchUpdate::Applied)
    }
}

/// Whether every object the branch `branch` of `image` reaches is in `image`: its commit, every
/// parent of each commit, each commit's tree, and every tree and blob each tree names (a gitlink
/// names a commit of another repository, which no repository holds); commits and trees are read
/// with [`RawCommit::parse`] and [`RawTree::parse`]. `Ok` for a branch the image does not have;
/// otherwise `Err` names the first object missing, its kind and its id, or one of another kind
/// than its referrer says, or one that does not parse.
pub fn reaches_only_kept(image: &MemImage, branch: &str) -> Result<(), String> {
    let Some(&tip) = image.branches.get(branch) else {
        return Ok(());
    };
    let mut stack = vec![(tip, RawKind::Commit)];
    let mut seen = BTreeSet::new();
    while let Some((oid, kind)) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let Some((held, bytes)) = image.objects.get(&oid) else {
            return Err(format!(
                "{kind} {oid}, which {branch} reaches, is not in the image"
            ));
        };
        if *held != kind {
            return Err(format!(
                "{oid}, which {branch} reaches as a {kind}, is a {held} in the image"
            ));
        }
        match kind {
            RawKind::Commit => {
                let commit = RawCommit::parse(bytes).map_err(|error| format!("{oid}: {error}"))?;
                let parents = commit.parents.iter().rev();
                stack.extend(parents.map(|&parent| (parent, RawKind::Commit)));
                stack.push((commit.tree, RawKind::Tree));
            }
            RawKind::Tree => {
                let tree = RawTree::parse(bytes).map_err(|error| format!("{oid}: {error}"))?;
                let entries = tree.entries.iter().rev();
                stack.extend(
                    entries.filter_map(|entry| Some((entry.oid, entry_kind(&entry.mode)?))),
                );
            }
            RawKind::Blob | RawKind::Tag => {}
        }
    }
    Ok(())
}

/// The kind of object a tree entry of `mode` (its octal digits) names, by the mode's file type:
/// a tree for a directory, nothing for a gitlink, a blob for anything else.
fn entry_kind(mode: &[u8]) -> Option<RawKind> {
    let digits = std::str::from_utf8(mode).ok();
    let mode = digits.and_then(|digits| u32::from_str_radix(digits, 8).ok());
    match mode.map(|mode| mode & 0o170_000) {
        Some(0o040_000) => Some(RawKind::Tree),
        Some(0o160_000) => None,
        _ => Some(RawKind::Blob),
    }
}
