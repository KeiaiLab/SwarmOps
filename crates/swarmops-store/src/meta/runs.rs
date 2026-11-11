//! FORMAT 3's sorted views (the FORMAT 3 design §2.4, §2.5): immutable runs, each the sorted
//! views of a range of records, listed by one manifest that only an atomic replace changes.
//!
//! ```text
//! meta/sorted3/MANIFEST                      "SWMF" ver generation count (domain from to)* crc
//! meta/sorted3/<d>-<from:016x>-<to:016x>.run "SWRN" ver domain from to n0 n1 crc | 0 | 1 | crc
//! meta/sorted3/tmp/                          a run as it is built, before its rename
//! ```
//!
//! A run of domain 0 (`c`, the changeset index) holds the id view then the delta-hash view of
//! its records; one of domain 1 (`f`, the fact rows) the subject view then the non-zero others.
//! Its bytes are a pure function of its records, so a run built twice is the same file. Readers
//! open the runs the manifest names; one that is gone sends them to the manifest again, since a
//! merge replaced it ([`open_views`]). [`super::views::step`] builds and merges them.

use std::io::{self, Read};
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::bytes;

use super::index::Index;
use super::sorted::{Pair, SortedView, Views};
use super::store::MetaStore;
use crate::error::{FileOp, StoreError};
use crate::fs_ops::{self, StoreFs};
use crate::layout::Layout;

/// How many records past the runs a domain's tail holds before a step builds a run of them:
/// FORMAT 1's and 2's compaction threshold (the FORMAT 3 design §2.5 rule 2).
pub const RUN_MIN: u64 = Index::COMPACT_THRESHOLD;

/// How many times longer than the run after it a run must be to stay apart: a shorter one is
/// merged with it (the FORMAT 3 design §2.5 rule 5).
pub const RUN_FANOUT: u64 = 4;

/// The most runs a manifest lists (the FORMAT 3 design §2.5).
pub const MAX_RUNS: usize = 64;

/// How many records past the runs a commit leaves before it steps itself, once it unlocked:
/// below it the view work is the caller's ([`MetaStore::step_views`]) or gc's (the FORMAT 3
/// design's amendment of 2026-11-11, G12).
pub const RUN_BACKLOG_MAX: u64 = 8 * RUN_MIN;

/// The manifest's file name in `meta/sorted3/`.
pub(crate) const MANIFEST: &str = "MANIFEST";

/// The directory of `meta/sorted3/` where runs are built.
pub(crate) const TMP: &str = "tmp";

/// What a run file and the manifest start with.
const RUN_MAGIC: &[u8; 4] = b"SWRN";
const MANIFEST_MAGIC: &[u8; 4] = b"SWMF";

/// The layout version of both.
const VERSION: u8 = 1;

/// A run's header: magic, version, domain, 2 zero bytes, from, to, n0, n1, CRC.
const RUN_HEADER_LEN: u64 = 44;

/// One entry of a section: a 32-byte key and an 8-byte rec.
const KEY_LEN: u64 = 32;
const ENTRY_LEN: u64 = 40;

/// A trailing CRC-32.
const CRC_LEN: u64 = 4;

/// The manifest's fixed part before its runs, and each run's entry.
const MANIFEST_HEADER_LEN: usize = 20;
const MANIFEST_ENTRY_LEN: usize = 24;

/// The longest manifest: [`MAX_RUNS`] entries.
const MANIFEST_MAX_LEN: usize = MANIFEST_HEADER_LEN + MAX_RUNS * MANIFEST_ENTRY_LEN + 4;

/// What a run sorts: a domain of records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Domain {
    /// The changeset index's records (0, `c`): sections by id and by delta hash.
    Changesets,
    /// The fact rows (1, `f`): sections by subject and by other.
    Facts,
}

impl Domain {
    /// Its byte in a run's header and a manifest entry.
    fn byte(self) -> u8 {
        match self {
            Domain::Changesets => 0,
            Domain::Facts => 1,
        }
    }

    /// The domain of `byte`, if any.
    fn of(byte: u8) -> Option<Domain> {
        match byte {
            0 => Some(Domain::Changesets),
            1 => Some(Domain::Facts),
            _ => None,
        }
    }

    /// Its letter in a run's file name.
    fn letter(self) -> char {
        match self {
            Domain::Changesets => 'c',
            Domain::Facts => 'f',
        }
    }
}

/// One run: the records `[from, to)` of a domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) domain: Domain,
    pub(crate) from: u64,
    pub(crate) to: u64,
}

impl Run {
    /// Its file name in `meta/sorted3/`: `c-0000000000000000-0000000000000400.run`.
    pub(crate) fn name(&self) -> String {
        let letter = self.domain.letter();
        format!("{letter}-{:016x}-{:016x}.run", self.from, self.to)
    }

    /// How many records it covers.
    pub(crate) fn len(&self) -> u64 {
        self.to - self.from
    }
}

/// The manifest: the runs, by (domain, from), and its generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) generation: u64,
    pub(crate) runs: Vec<Run>,
}

impl Manifest {
    /// The manifest of a new store: generation 0, no run.
    pub(crate) fn empty() -> Manifest {
        Manifest {
            generation: 0,
            runs: Vec::new(),
        }
    }

    /// Where `domain`'s runs end: they split `[0, covered)` (M1).
    pub(crate) fn covered(&self, domain: Domain) -> u64 {
        (self.runs.iter())
            .filter(|run| run.domain == domain)
            .map(|run| run.to)
            .max()
            .unwrap_or(0)
    }

    /// Its bytes (the FORMAT 3 design §2.5).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let count = u32::try_from(self.runs.len()).expect("at most MAX_RUNS runs");
        let mut bytes = Vec::with_capacity(MANIFEST_HEADER_LEN + 4 + 24 * self.runs.len());
        bytes.extend_from_slice(MANIFEST_MAGIC);
        bytes.extend_from_slice(&[VERSION, 0, 0, 0]);
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        for run in &self.runs {
            bytes.push(run.domain.byte());
            bytes.extend_from_slice(&[0; 7]);
            bytes.extend_from_slice(&run.from.to_le_bytes());
            bytes.extend_from_slice(&run.to.to_le_bytes());
        }
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// The manifest `bytes` hold, checked whole: its layout, CRC and length, at most
    /// [`MAX_RUNS`] runs in (domain, from) order, each domain's runs splitting `[0, covered)`
    /// with no gap or overlap (M1). Anything else is [`StoreError::IndexCorrupt`]: a manifest is
    /// only ever replaced whole.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Manifest, StoreError> {
        let corrupt = |what| StoreError::IndexCorrupt { what };

        // 1. The fixed part and the length its count implies, then the CRC.
        if bytes.len() < MANIFEST_HEADER_LEN + 4 || bytes.len() > MANIFEST_MAX_LEN {
            return Err(corrupt(
                "meta/sorted3/MANIFEST has a length no manifest has",
            ));
        }
        if &bytes[..4] != MANIFEST_MAGIC || bytes[4..8] != [VERSION, 0, 0, 0] {
            return Err(corrupt("meta/sorted3/MANIFEST is no manifest of version 1"));
        }
        let count = usize::try_from(u32_at(bytes, 16)).unwrap_or(usize::MAX);
        if count > MAX_RUNS || bytes.len() != MANIFEST_HEADER_LEN + count * MANIFEST_ENTRY_LEN + 4 {
            return Err(corrupt(
                "meta/sorted3/MANIFEST's run count disagrees with its length",
            ));
        }
        let end = bytes.len() - 4;
        if u32_at(bytes, end) != crc32fast::hash(&bytes[..end]) {
            return Err(corrupt("meta/sorted3/MANIFEST fails its CRC"));
        }

        // 2. The runs, each domain's from 0 on, back to back.
        let mut runs: Vec<Run> = Vec::with_capacity(count);
        for entry in bytes[MANIFEST_HEADER_LEN..end].chunks(MANIFEST_ENTRY_LEN) {
            let domain = Domain::of(entry[0]).filter(|_| entry[1..8] == [0; 7]);
            let domain = domain.ok_or(corrupt("a manifest entry names no domain"))?;
            let (from, to) = (u64_at(entry, 8), u64_at(entry, 16));
            let expected = match runs.last() {
                Some(last) if last.domain == domain => last.to,
                Some(last) if last.domain > domain => {
                    return Err(corrupt("the manifest's runs are out of domain order"));
                }
                _ => 0,
            };
            if from != expected || to <= from {
                return Err(corrupt("the manifest's runs leave a gap or overlap"));
            }
            runs.push(Run { domain, from, to });
        }
        Ok(Manifest {
            generation: u64_at(bytes, 8),
            runs,
        })
    }
}

/// The bytes of `run`'s file, its two sections `sections` (each sorted by key, then rec), with
/// both CRCs (the FORMAT 3 design §2.4).
pub(crate) fn encode_run(run: &Run, sections: [&[([u8; 32], u64)]; 2]) -> Vec<u8> {
    let entries: usize = sections.iter().map(|section| section.len()).sum();
    let mut bytes = Vec::with_capacity(48 + 40 * entries);

    // 1. The header, with its CRC.
    bytes.extend_from_slice(RUN_MAGIC);
    bytes.extend_from_slice(&[VERSION, run.domain.byte(), 0, 0]);
    let counts = sections.map(|section| bytes::to_u64(section.len()));
    for field in [run.from, run.to, counts[0], counts[1]] {
        bytes.extend_from_slice(&field.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());

    // 2. Each section's keys, then its recs; then the CRC of both.
    for section in sections {
        for (key, _) in section {
            bytes.extend_from_slice(key);
        }
        for (_, rec) in section {
            bytes.extend_from_slice(&rec.to_le_bytes());
        }
    }
    let crc = crc32fast::hash(&bytes[44..]);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// The manifest of `layout`, its bytes with it: read whole (at most 1,560 bytes) and checked
/// ([`Manifest::decode`]). A missing one is [`StoreError::IndexCorrupt`]: the store's creation
/// made it, and only replaces follow.
pub(crate) fn read_manifest(
    layout: &Layout,
    fs: &StoreFs,
) -> Result<(Vec<u8>, Manifest), StoreError> {
    let path = layout.views_dir().join(MANIFEST);
    let file = fs_ops::open_regular(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => StoreError::IndexCorrupt {
            what: "meta/sorted3/MANIFEST is missing",
        },
        _ => fs.locate(FileOp::Open, &path, e).into(),
    })?;

    // One byte past the longest manifest tells a longer file from it.
    let mut bytes = Vec::new();
    (file.take(bytes::to_u64(MANIFEST_MAX_LEN) + 1))
        .read_to_end(&mut bytes)
        .map_err(|e| fs.locate(FileOp::ReadAt, &path, e))?;
    let manifest = Manifest::decode(&bytes)?;
    Ok((bytes, manifest))
}

/// The views of `layout`'s changeset runs, as the manifest names them now (the FORMAT 3 design
/// §2.5 M3): a run that is gone sends the read to the manifest again, since a merge replaced it
/// meanwhile; the same manifest naming a missing run again is [`StoreError::IndexCorrupt`], and
/// a manifest changed under every one of [`MetaStore::SNAPSHOT_ROUNDS`] reads
/// [`StoreError::SnapshotContended`] (retryable).
pub(crate) fn open_views(layout: &Layout, fs: &StoreFs) -> Result<Views, StoreError> {
    open_views_with(layout, fs, &mut || {})
}

/// [`open_views`], `between` run after each read of the manifest and before its runs are opened:
/// where a merge's replace and removal can come.
fn open_views_with(
    layout: &Layout,
    fs: &StoreFs,
    between: &mut dyn FnMut(),
) -> Result<Views, StoreError> {
    let mut last: Option<Vec<u8>> = None;
    for _ in 0..MetaStore::SNAPSHOT_ROUNDS {
        let (bytes, manifest) = read_manifest(layout, fs)?;
        between();
        if let Some(pairs) = open_pairs(layout, fs, &manifest)? {
            let covered = manifest.covered(Domain::Changesets);
            return Ok(Views::of_runs(pairs, covered));
        }
        if last.as_ref() == Some(&bytes) {
            return Err(StoreError::IndexCorrupt {
                what: "the manifest names a run that is not there",
            });
        }
        last = Some(bytes);
    }
    Err(StoreError::SnapshotContended {
        rounds: MetaStore::SNAPSHOT_ROUNDS,
    })
}

/// The views of each changeset run of `manifest`, in record order, every run it names opened
/// and its header checked; `None` if one is not there.
fn open_pairs(
    layout: &Layout,
    fs: &StoreFs,
    manifest: &Manifest,
) -> Result<Option<Vec<Pair>>, StoreError> {
    let mut pairs = Vec::new();
    for run in &manifest.runs {
        let Some(opened) = open_run(layout, fs, run)? else {
            return Ok(None);
        };
        if run.domain == Domain::Changesets {
            pairs.push(opened);
        }
    }
    Ok(Some(pairs))
}

/// The two sections of `run`'s file, its header checked against its name and its length before
/// any read of them; `None` if the file is not there. A header or a length out of the design's
/// layout is [`StoreError::IndexCorrupt`]. The trailing CRC is left to verify: lookups check
/// each rec they read against the index columns instead.
fn open_run(layout: &Layout, fs: &StoreFs, run: &Run) -> Result<Option<Pair>, StoreError> {
    let corrupt = |what| StoreError::IndexCorrupt { what };
    let path = layout.views_dir().join(run.name());
    let file = match fs_ops::open_regular(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fs.locate(FileOp::Open, &path, e).into()),
    };

    // 1. The header, read whole and checked.
    let len = (file.metadata())
        .map_err(|e| fs.locate(FileOp::Stat, &path, e))?
        .len();
    if len < RUN_HEADER_LEN + CRC_LEN {
        return Err(corrupt("a run file is shorter than its header"));
    }
    let mut header = [0; 44];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut header, 0)
        .map_err(|e| fs.locate(FileOp::ReadAt, &path, e))?;
    let crc_ok = u32_at(&header, 40) == crc32fast::hash(&header[..40]);
    let named = header[..4] == *RUN_MAGIC
        && header[4..8] == [VERSION, run.domain.byte(), 0, 0]
        && (u64_at(&header, 8), u64_at(&header, 16)) == (run.from, run.to);
    if !crc_ok || !named {
        return Err(corrupt("a run file's header disagrees with its name"));
    }

    // 2. The section counts against the run's records and the file's length.
    let (n0, n1) = (u64_at(&header, 24), u64_at(&header, 32));
    let n1_fits = match run.domain {
        Domain::Changesets => n1 == run.len(),
        Domain::Facts => n1 <= run.len(),
    };
    let expected = (n0.checked_add(n1))
        .and_then(|n| n.checked_mul(ENTRY_LEN))
        .and_then(|n| n.checked_add(RUN_HEADER_LEN + CRC_LEN));
    if n0 != run.len() || !n1_fits || expected != Some(len) {
        return Err(corrupt("a run file's length disagrees with its header"));
    }

    // 3. The sections: keys, then recs, each.
    let file = Arc::new(file);
    let covers = run.from..run.to;
    let section = |first: u64, n: u64| {
        let bases = (first, first + KEY_LEN * n);
        SortedView::section(fs, &file, &path, bases, n, covers.clone())
    };
    let ids = section(RUN_HEADER_LEN, n0);
    let dh = section(RUN_HEADER_LEN + ENTRY_LEN * n0, n1);
    Ok(Some(Pair { ids, dh }))
}

/// The path of `run`'s file in `layout`.
pub(crate) fn run_path(layout: &Layout, run: &Run) -> Utf8PathBuf {
    layout.views_dir().join(run.name())
}

/// The path of `name` in `meta/sorted3/tmp/`.
pub(crate) fn tmp_path(layout: &Layout, name: &str) -> Utf8PathBuf {
    layout.views_dir().join(TMP).join(name)
}

/// Whether `name` is a run's file name: a domain letter, two 16-digit hex numbers, `.run`.
pub(crate) fn is_run_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".run") else {
        return false;
    };
    let mut parts = stem.split('-');
    let letter = parts.next().is_some_and(|l| l == "c" || l == "f");
    let hex = |part: Option<&str>| {
        part.is_some_and(|p| p.len() == 16 && p.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    letter && hex(parts.next()) && hex(parts.next()) && parts.next().is_none()
}

/// The `u32le` at `at` of `bytes`.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

/// The `u64le` at `at` of `bytes`.
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    use super::*;

    /// The lines of a golden file of `tests/golden3/`, decoded from hex.
    fn golden(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/golden3/{name}.hex", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).unwrap();
        let hex = text.lines().next().unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }

    /// Key `n` of the goldens: 32 bytes of `n`.
    fn key(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// The manifest golden decodes to its runs, and they encode to it again.
    #[test]
    fn the_manifest_matches_its_golden() {
        let bytes = golden("manifest");
        let manifest = Manifest::decode(&bytes).unwrap();
        let run = |domain, from, to| Run { domain, from, to };
        let want = Manifest {
            generation: 5,
            runs: vec![
                run(Domain::Changesets, 0, 1024),
                run(Domain::Changesets, 1024, 1280),
                run(Domain::Facts, 0, 3),
            ],
        };
        assert_eq!(manifest, want);
        assert_eq!(want.encode(), bytes);
        assert_eq!(
            Manifest::decode(&Manifest::empty().encode()).unwrap(),
            Manifest::empty()
        );
    }

    /// Each byte of the manifest golden flipped is refused, or reads as another manifest.
    #[test]
    fn a_flipped_manifest_byte_is_refused() {
        let bytes = golden("manifest");
        let want = Manifest::decode(&bytes).unwrap();
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x01;
            if let Ok(other) = Manifest::decode(&flipped) {
                assert_ne!(other, want, "byte {at}");
            }
        }
    }

    /// The two run goldens are what the encoder writes for their sections.
    #[test]
    fn the_runs_match_their_goldens() {
        let run_c = Run {
            domain: Domain::Changesets,
            from: 0,
            to: 3,
        };
        let ids = [(key(1), 2), (key(2), 0), (key(3), 1)];
        let dh = [(key(4), 1), (key(6), 0), (key(6), 2)];
        assert_eq!(encode_run(&run_c, [&ids, &dh]), golden("run_c"));
        let run_f = Run {
            domain: Domain::Facts,
            ..run_c
        };
        let subjects = [(key(7), 0), (key(7), 2), (key(8), 1)];
        let others = [(key(9), 0), (key(9), 2)];
        assert_eq!(encode_run(&run_f, [&subjects, &others]), golden("run_f"));
        assert_eq!(run_c.name(), "c-0000000000000000-0000000000000003.run");
        assert!(is_run_name(&run_f.name()));
        assert!(!is_run_name("c-0-3.run"));
    }

    /// A reader whose run a merge removed between its read of the manifest and its open reads
    /// the new manifest and its runs; the same manifest naming a missing run is damage.
    #[test]
    fn a_reader_reads_the_manifest_again_after_a_merge() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let layout = Layout::with_format(root, crate::StoreFormat::V3);
        let fs = StoreFs::new(Arc::new(crate::RealFs::new()), root);
        let views = layout.views_dir();
        std::fs::create_dir_all(&views).unwrap();

        // 1. Generation 1 names run [0, 3), which is there: the views read.
        let run = Run {
            domain: Domain::Changesets,
            from: 0,
            to: 3,
        };
        let ids = [(key(1), 2), (key(2), 0), (key(3), 1)];
        let dh = [(key(4), 1), (key(6), 0), (key(6), 2)];
        let manifest = |generation, runs: Vec<Run>| Manifest { generation, runs }.encode();
        std::fs::write(views.join(MANIFEST), manifest(1, vec![run])).unwrap();
        std::fs::write(run_path(&layout, &run), encode_run(&run, [&ids, &dh])).unwrap();
        assert_eq!(open_views(&layout, &fs).unwrap().sorted_count(), 3);

        // 2. Between the reader's manifest read and its open, a merge removes the run after it
        //    replaced the manifest with one of no run: the second read sees that one.
        let mut calls = 0;
        let mut merge = || {
            calls += 1;
            if calls == 1 {
                std::fs::write(views.join(MANIFEST), manifest(2, Vec::new())).unwrap();
                std::fs::remove_file(run_path(&layout, &run)).unwrap();
            }
        };
        let read = open_views_with(&layout, &fs, &mut merge).unwrap();
        assert_eq!((read.sorted_count(), calls), (0, 2));

        // 3. The same manifest naming a missing run, read twice: damage.
        std::fs::write(views.join(MANIFEST), manifest(3, vec![run])).unwrap();
        let got = open_views(&layout, &fs).map(|views| views.sorted_count());
        assert!(
            matches!(got, Err(StoreError::IndexCorrupt { .. })),
            "{got:?}"
        );
    }
}
