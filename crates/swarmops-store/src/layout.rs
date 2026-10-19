//! Where each piece of a store lives on disk (storage spec §2).
//!
//! ```text
//! <root>/FORMAT                 "swarmops-store 1\n", written last by a create
//! <root>/REPO                   the repository's identity: its id, then its name
//! <root>/meta/                  metadata store files (storage spec §3), among them:
//! <root>/meta/lock              the flock that serializes metadata writers
//! <root>/meta/journal.log       append-only journal frames, the metadata's source of truth
//! <root>/meta/journal.off       each frame's start offset in journal.log, u64le
//! <root>/meta/journal.ckpt      how far the journal is materialized, a u64le offset
//! <root>/meta/derived/<name>/<version>/<2hex>/<62hex>[.<16hex>]
//!                               one derived value: a cache, outside the journal, never synced
//! <root>/meta/derived/tmp/      temp files of derived values, written before they are linked
//! <root>/objects/tmp/           temp files staged before they are linked into place
//! <root>/objects/<2hex>/<62hex> loose objects, one fan-out directory per first id byte
//! ```

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::ObjectId;

use crate::meta::derived::{DerivedSlot, is_derived_name};

/// The paths of one store, all derived from its root. Computes paths only; touches nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: Utf8PathBuf,
}

impl Layout {
    /// The layout of the store rooted at `root`.
    pub fn new(root: impl Into<Utf8PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store root.
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// `<root>/FORMAT`, the store version marker.
    pub fn format_file(&self) -> Utf8PathBuf {
        self.root.join("FORMAT")
    }

    /// `<root>/REPO`, the repository's identity: a line with its id (32 hex digits), then its
    /// name, verbatim, to the end of the file.
    pub fn repo_file(&self) -> Utf8PathBuf {
        self.root.join("REPO")
    }

    /// `<root>/meta`, the metadata store's directory.
    pub fn meta_dir(&self) -> Utf8PathBuf {
        self.root.join("meta")
    }

    /// `<root>/meta/<rel>`; `rel` is a relative path inside `meta/`.
    pub fn meta_path(&self, rel: &str) -> Utf8PathBuf {
        debug_assert!(
            Utf8Path::new(rel).is_relative(),
            "meta path {rel} must be relative"
        );
        self.meta_dir().join(rel)
    }

    /// `<root>/meta/lock`, whose exclusive flock serializes metadata writers.
    pub fn meta_lock(&self) -> Utf8PathBuf {
        self.meta_path("lock")
    }

    /// `<root>/meta/journal.log`, the append-only journal of metadata frames.
    pub fn journal_log(&self) -> Utf8PathBuf {
        self.meta_path("journal.log")
    }

    /// `<root>/meta/journal.off`, the start offset of each journal frame (`u64le` each).
    pub fn journal_off(&self) -> Utf8PathBuf {
        self.meta_path("journal.off")
    }

    /// `<root>/meta/journal.ckpt`, the journal offset up to which the metadata files are
    /// materialized (`u64le`).
    pub fn journal_ckpt(&self) -> Utf8PathBuf {
        self.meta_path("journal.ckpt")
    }

    /// `<root>/meta/derived`, the directory of the derived values (storage spec §3, §5).
    pub fn derived_dir(&self) -> Utf8PathBuf {
        self.meta_path("derived")
    }

    /// `<root>/meta/derived/tmp`, where a derived value's file is written before it is linked
    /// into its slot: under `meta/derived/`, so on its filesystem.
    pub fn derived_tmp(&self) -> Utf8PathBuf {
        self.derived_dir().join("tmp")
    }

    /// The file of `slot`'s value:
    /// `<root>/meta/derived/<name>/<version>/<first 2 hex>/<remaining 62 hex>`, the subject's hex
    /// digits, then `.<16 hex>` when the key is not empty: the first 8 bytes of BLAKE3 of the
    /// key. The slot's name must be one a store takes (1 to 64 bytes of `[a-z0-9_]`, not `tmp`),
    /// as [`crate::DerivedStore`] checks before it asks: another could name a path out of the
    /// name's directory.
    pub fn derived_path(&self, slot: &DerivedSlot<'_>) -> Utf8PathBuf {
        debug_assert!(
            is_derived_name(slot.name),
            "derived value name {:?} is not one a store takes",
            slot.name
        );
        let hex = slot.subject.to_hex();
        let (fanout, rest) = hex.split_at(2);
        let file = if slot.key.is_empty() {
            rest.to_owned()
        } else {
            let hash = blake3::hash(slot.key);
            let (first, _) = hash.as_bytes().split_first_chunk::<8>().expect("32 bytes");
            format!("{rest}.{:016x}", u64::from_be_bytes(*first))
        };
        self.derived_dir()
            .join(slot.name)
            .join(slot.version.to_string())
            .join(fanout)
            .join(file)
    }

    /// `<root>/objects`, the parent of the fan-out directories and `tmp`.
    pub fn objects_dir(&self) -> Utf8PathBuf {
        self.root.join("objects")
    }

    /// `<root>/objects/tmp`, where object files are written before being linked into place;
    /// inside `objects/`, so on the objects' filesystem (a hard link cannot cross devices).
    pub fn objects_tmp(&self) -> Utf8PathBuf {
        self.objects_dir().join("tmp")
    }

    /// `<root>/objects/<first 2 hex>/<remaining 62 hex>`.
    pub fn object_path(&self, id: ObjectId) -> Utf8PathBuf {
        let hex = id.to_hex();
        let (fanout, rest) = hex.split_at(2);
        self.objects_dir().join(fanout).join(rest)
    }
}

/// False for a path under `/mnt/`, where WSL mounts Windows drives (drvfs); true otherwise.
/// A store must live on a native Linux filesystem (storage spec §2).
///
/// Compares path components, so pass an absolute path; `/mnt` itself counts as native.
pub fn is_native_linux_fs(path: &Utf8Path) -> bool {
    match path.strip_prefix("/mnt") {
        Ok(below) => below.as_str().is_empty(),
        Err(_) => true,
    }
}
