//! FORMAT 3's feature list, `meta/features` (the FORMAT 3 design §2.6), and the nonce every
//! FORMAT 3 frame ends in (§2.7).
//!
//! ```text
//! "swarmops-features 1\n" (name "\n")*
//! name = [a-z][a-z0-9_]{0,31}, in ascending byte order, each once; at most 32 names
//! ```
//!
//! A record kind a FORMAT 3 store holds by choice (a fact kind) is written only once its feature
//! is durable in the list ([`enable`], W10). A store that lists a feature this build does not
//! support ([`SUPPORTED_FEATURES`]) is refused by every open, read and write with
//! [`StoreError::TooNew`], before anything is written: no damage is reported in a store a newer
//! build wrote. Decoding a frame never reads the list (judgement H2): it takes the kinds of the
//! build's features ([`known_kinds`]).
//!
//! The build's seams for the tests, behind `test-hooks` only: `supported_for_tests` widens the
//! supported set, and `seed_nonces` makes the nonces a sequence from a seed. A release build has
//! neither: code that names one does not compile there.

use std::io::{self, Read};
#[cfg(feature = "test-hooks")]
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::frame::FactKind;
use crate::error::{FileOp, IoFailure, StoreError};
use crate::format::StoreFormat;
use crate::fs_ops::{self, FsMethod, StoreFs};
use crate::layout::Layout;

/// The features this build supports: the fact kinds whose meaning it implements. Empty: the
/// fact kinds decode, but no milestone has given one its meaning yet, so this build enables none
/// and opens no store that lists one.
pub const SUPPORTED_FEATURES: &[&str] = &[];

/// The most names `meta/features` holds.
pub const MAX_FEATURES: usize = 32;

/// The longest `meta/features`, in bytes (the FORMAT 3 design §2.6). A read takes one byte more
/// at most, so a longer file is refused without being held.
const MAX_FILE_LEN: usize = 1_141;

/// What `meta/features` starts with.
const HEADER: &[u8] = b"swarmops-features 1\n";

/// What the header of every list format starts with, its decimal version and a newline after.
const HEADER_STEM: &[u8] = b"swarmops-features ";

/// The longest feature name, in bytes.
const MAX_NAME_LEN: usize = 32;

/// The format of the FORMAT 3 store a [`StoreError::TooNew`] of a feature names.
const FORMAT_3: u32 = 3;

/// Each fact kind's feature name.
const KIND_FEATURES: [(FactKind, &str); 8] = [
    (FactKind::Review, "review"),
    (FactKind::Annotation, "annotation"),
    (FactKind::Check, "check"),
    (FactKind::Queue, "queue"),
    (FactKind::Withdrawal, "withdrawal"),
    (FactKind::Lineage, "lineage"),
    (FactKind::Provenance, "provenance"),
    (FactKind::Tag, "tag_object"),
];

/// A feature list: its names, in ascending byte order, each once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Features {
    names: Vec<String>,
}

impl Features {
    /// Its names this build does not support, in order.
    pub(crate) fn unsupported(&self) -> Vec<String> {
        (self.names.iter())
            .filter(|name| !supported(name))
            .cloned()
            .collect()
    }

    /// Its bytes, as `meta/features` holds them.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = HEADER.to_vec();
        for name in &self.names {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }

    /// The list `bytes` hold, or why they hold none.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Features, String> {
        // 1. The length and the header, before any name.
        if bytes.len() > MAX_FILE_LEN {
            return Err(format!("is longer than {MAX_FILE_LEN} bytes"));
        }
        let Some(body) = bytes.strip_prefix(HEADER) else {
            return Err("does not start with its header".to_owned());
        };

        // 2. Each name on a line of its own, the last line ended too.
        if body.is_empty() {
            return Ok(Features::default());
        }
        let Some(body) = body.strip_suffix(b"\n") else {
            return Err("does not end in a newline".to_owned());
        };
        let mut names: Vec<String> = Vec::new();
        for line in body.split(|&b| b == b'\n') {
            let Some(name) = std::str::from_utf8(line).ok().filter(|name| is_name(name)) else {
                return Err(format!("holds a line that is no feature name: {line:?}"));
            };
            if names.last().is_some_and(|last| last.as_str() >= name) {
                return Err(format!("names {name:?} out of order or twice"));
            }
            names.push(name.to_owned());
        }

        // 3. The count.
        if names.len() > MAX_FEATURES {
            return Err(format!("names more than {MAX_FEATURES} features"));
        }
        Ok(Features { names })
    }
}

/// Whether `name` is a feature name: `[a-z][a-z0-9_]{0,31}`.
fn is_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && name.len() <= MAX_NAME_LEN
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The feature list of the store of `layout`: empty if `meta/features` is missing (as it is in
/// every store but FORMAT 3's). One of a later list format is [`StoreError::TooNew`] naming no
/// feature. One that is no list, or longer than any, is `InvalidData`, a failed
/// read of `meta/features` (named by its place in the store, never by the root). Read by path, at
/// most 1,142 bytes: no lock, no [`crate::FsOps`] call.
pub(crate) fn read(layout: &Layout, fs: &StoreFs) -> Result<Features, StoreError> {
    let path = layout.features_file();
    let failed = |e| fs.locate(FileOp::Fs(FsMethod::Read), &path, e);

    // 1. The bytes, capped one past the longest list.
    let mut bytes = Vec::new();
    match fs_ops::open_regular(&path) {
        Ok(file) => {
            let cap = u64::try_from(MAX_FILE_LEN + 1).unwrap_or(u64::MAX);
            file.take(cap).read_to_end(&mut bytes).map_err(failed)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Features::default()),
        Err(e) => return Err(failed(e).into()),
    }

    // 2. A later list format is a newer build's (the FORMAT 3 design's amendment of
    // 2026-11-11, G5): refused as too new, not as damage.
    if is_later_format(&bytes) {
        return Err(StoreError::TooNew {
            format: FORMAT_3,
            features: Vec::new(),
        });
    }

    // 3. The list they hold.
    Features::decode(&bytes).map_err(|why| {
        let why = format!("meta/features {why}");
        failed(io::Error::new(io::ErrorKind::InvalidData, why)).into()
    })
}

/// Whether `bytes` start with the header of a list format after 1: `swarmops-features <n>\n`,
/// `n` decimal without a leading zero and above 1 (e.g. `swarmops-features 2\n`).
fn is_later_format(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(HEADER_STEM) else {
        return false;
    };
    let Some(end) = rest.iter().position(|&b| b == b'\n') else {
        return false;
    };
    let version = &rest[..end];
    let decimal =
        version.first().is_some_and(|&b| b != b'0') && version.iter().all(u8::is_ascii_digit);
    decimal && version != b"1"
}

/// Refuses the store of `layout`, if it is FORMAT 3 and lists a feature this build does not
/// support, with [`StoreError::TooNew`] naming those features. Other formats hold no list.
pub(crate) fn check(layout: &Layout, fs: &StoreFs) -> Result<(), StoreError> {
    if layout.format() != StoreFormat::V3 {
        return Ok(());
    }
    let unsupported = read(layout, fs)?.unsupported();
    if unsupported.is_empty() {
        return Ok(());
    }
    Err(StoreError::TooNew {
        format: FORMAT_3,
        features: unsupported,
    })
}

/// What `e`, an error of reading the journal of the store of `layout`, is: if it is damage
/// ([`StoreError::JournalCorrupt`]) in a FORMAT 3 store that lists a feature this build does not
/// support, read again now, [`StoreError::TooNew`] (a record of a newer build, enabled since the
/// list was last read: the design §2.6); else `e`.
pub(crate) fn too_new_or(layout: &Layout, fs: &StoreFs, e: StoreError) -> StoreError {
    if !matches!(e, StoreError::JournalCorrupt { .. }) {
        return e;
    }
    match check(layout, fs) {
        Err(too_new @ StoreError::TooNew { .. }) => too_new,
        _ => e,
    }
}

/// Adds the features of `kinds` to the list of the store of `layout`, under its lock, before a
/// frame holding records of those kinds is appended (W10): the list is replaced atomically and
/// durably ([`crate::FsOps::atomic_replace`]) when it lacks one, and left alone otherwise. A kind
/// whose feature this build does not support is [`StoreError::BadCall`], and nothing is written.
/// Features are never removed.
pub(crate) fn enable(layout: &Layout, fs: &StoreFs, kinds: &[FactKind]) -> Result<(), StoreError> {
    // 1. The names, each one this build supports.
    let mut wanted = Vec::new();
    for &kind in kinds {
        let name = feature_of(kind);
        if !supported(name) {
            return Err(StoreError::BadCall(format!(
                "a {kind:?} fact needs the feature {name:?}, which this build does not support"
            )));
        }
        wanted.push(name);
    }

    // 2. The list, with those it lacks, written if it grew.
    let mut features = read(layout, fs)?;
    let before = features.names.len();
    for name in wanted {
        if let Err(at) = features
            .names
            .binary_search_by(|have| have.as_str().cmp(name))
        {
            features.names.insert(at, name.to_owned());
        }
    }
    if features.names.len() == before {
        return Ok(());
    }
    if features.names.len() > MAX_FEATURES {
        return Err(StoreError::BadCall(format!(
            "a feature list holds at most {MAX_FEATURES} names"
        )));
    }
    let path = layout.features_file();
    fs.atomic_replace(path.as_std_path(), &features.encode())?;
    Ok(())
}

/// The feature name of fact kind `kind`.
pub(crate) fn feature_of(kind: FactKind) -> &'static str {
    KIND_FEATURES
        .iter()
        .find(|(of, _)| *of == kind)
        .map_or("", |(_, name)| name)
}

/// The fact kinds this build decodes: those whose feature it supports.
pub(crate) fn known_kinds() -> Vec<FactKind> {
    (KIND_FEATURES.iter())
        .filter(|(_, name)| supported(name))
        .map(|(kind, _)| *kind)
        .collect()
}

/// Whether this build supports feature `name`: [`SUPPORTED_FEATURES`], and in a build with the
/// test hooks the names `supported_for_tests` adds.
fn supported(name: &str) -> bool {
    if SUPPORTED_FEATURES.contains(&name) {
        return true;
    }
    #[cfg(feature = "test-hooks")]
    if lock(&FOR_TESTS).iter().any(|added| added == name) {
        return true;
    }
    false
}

/// A new nonce for a FORMAT 3 frame: eight bytes from the operating system's random source
/// (the FORMAT 3 design §2.7), or, in a build with the test hooks while `seed_nonces` holds,
/// the next of its sequence. A random source that fails is [`StoreError::NotCommitted`]: nothing
/// is written.
pub(crate) fn draw_nonce() -> Result<u64, StoreError> {
    #[cfg(feature = "test-hooks")]
    if let Some(state) = lock(&SEED).as_mut() {
        *state = state.wrapping_add(SEED_STEP);
        return Ok(mix(*state));
    }
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|e| StoreError::NotCommitted {
        source: IoFailure::new(
            FileOp::Fs(FsMethod::Read),
            None,
            io::Error::other(format!("drawing a frame's nonce failed: {e}")),
        ),
    })?;
    Ok(u64::from_le_bytes(bytes))
}

// ---- The test hooks' seams ----

/// The features `supported_for_tests` adds, while its guards hold.
#[cfg(feature = "test-hooks")]
static FOR_TESTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The state of the nonce sequence `seed_nonces` sets, while its guard holds.
#[cfg(feature = "test-hooks")]
static SEED: Mutex<Option<u64>> = Mutex::new(None);

/// The step of the nonce sequence: splitmix64's.
#[cfg(feature = "test-hooks")]
const SEED_STEP: u64 = 0x9e37_79b9_7f4a_7c15;

/// Splitmix64's output of `state`.
#[cfg(feature = "test-hooks")]
fn mix(state: u64) -> u64 {
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `mutex`'s value: a panicking holder leaves a value the seams can still use.
#[cfg(feature = "test-hooks")]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a seam set, undone when it is dropped: `supported_for_tests`' features, or
/// `seed_nonces`' sequence. The seams are the process's: every store in it sees them.
#[cfg(feature = "test-hooks")]
#[derive(Debug)]
#[non_exhaustive]
#[must_use = "the seam is undone when its guard is dropped"]
pub struct Guard {
    undo: Undo,
}

/// What dropping a [`Guard`] restores.
#[cfg(feature = "test-hooks")]
#[derive(Debug)]
enum Undo {
    /// The features supported for tests before.
    Features(Vec<String>),
    /// The nonce sequence before.
    Seed(Option<u64>),
}

#[cfg(feature = "test-hooks")]
impl Drop for Guard {
    fn drop(&mut self) {
        match &mut self.undo {
            Undo::Features(before) => *lock(&FOR_TESTS) = std::mem::take(before),
            Undo::Seed(before) => *lock(&SEED) = *before,
        }
    }
}

/// Makes this build support the features `names` too, until the guard is dropped: a test's
/// way to enable a fact kind, and to decode it (the FORMAT 3 design §2.6). The test hooks alone
/// have it.
#[cfg(feature = "test-hooks")]
pub fn supported_for_tests(names: &[&str]) -> Guard {
    let mut added = lock(&FOR_TESTS);
    let before = added.clone();
    added.extend(names.iter().map(|&name| name.to_owned()));
    Guard {
        undo: Undo::Features(before),
    }
}

/// Makes the nonces this process draws a sequence fixed by `seed`, until the guard is dropped:
/// the same seed, the same nonces, in order. The test hooks alone have it.
#[cfg(feature = "test-hooks")]
pub fn seed_nonces(seed: u64) -> Guard {
    let before = lock(&SEED).replace(seed);
    Guard {
        undo: Undo::Seed(before),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden list of the FORMAT 3 design §6 (`features.hex`): its two names, its bytes
    /// again, and no flipped byte that reads as the same list.
    #[test]
    fn features_match_their_golden() {
        // 1. The golden: one line of hex.
        let golden = include_str!("../../tests/golden3/features.hex");
        let line = golden.lines().next().expect("one line");
        let bytes: Vec<u8> = (0..line.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&line[at..at + 2], 16).expect("hex"))
            .collect();

        // 2. Decoded, encoded again.
        let features = Features::decode(&bytes).expect("the golden decodes");
        assert_eq!(features.names, ["review", "withdrawal"]);
        assert_eq!(features.encode(), bytes);

        // 3. Each byte flipped: refused, or another list.
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x01;
            assert_ne!(
                Features::decode(&flipped).ok(),
                Some(features.clone()),
                "{at}"
            );
        }
    }

    /// Every other shape is refused: no header, a name out of order, twice, past 32 bytes or
    /// with another byte, no final newline, more than 32 names.
    #[test]
    fn another_shape_is_no_list() {
        let list = |names: &[&str]| {
            let mut bytes = HEADER.to_vec();
            for name in names {
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(b'\n');
            }
            bytes
        };
        assert_eq!(Features::decode(HEADER), Ok(Features::default()));
        assert!(Features::decode(b"").is_err());
        assert!(Features::decode(&list(&["b", "a"])).is_err());
        assert!(Features::decode(&list(&["a", "a"])).is_err());
        assert!(Features::decode(&list(&["Ab"])).is_err());
        assert!(Features::decode(&list(&["1a"])).is_err());
        assert!(Features::decode(&list(&[""])).is_err());
        assert!(Features::decode(&list(&[&"a".repeat(33)])).is_err());
        assert!(Features::decode(&list(&[&"a".repeat(32)])).is_ok());
        let mut open = list(&["a"]);
        open.pop();
        assert!(Features::decode(&open).is_err());
        let names: Vec<String> = (0..33).map(|i| format!("f{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(Features::decode(&list(&names)).is_err());
        assert!(Features::decode(&list(&names[..32])).is_ok());
    }

    /// The directory of the provisional fact body goldens, under `tests/golden3/`.
    const PROVISIONAL: &str = "provisional";

    /// The provisional goldens among `files` (file names in `tests/golden3/provisional/`) of a
    /// kind whose feature is in `supported`, and each supported feature no fact kind has: what
    /// [`an_enabled_feature_has_no_provisional_golden`] refuses. A supported feature's bodies are
    /// confirmed: its goldens leave `provisional/` (the FORMAT 3 design's amendment of
    /// 2026-11-11, G9).
    fn provisional_offenders(supported: &[&str], files: &[String]) -> Vec<String> {
        let mut offenders = Vec::new();
        for &feature in supported {
            // 1. A feature that names no kind: fail closed.
            let kind = FactKind::ALL
                .into_iter()
                .find(|&kind| feature_of(kind) == feature);
            let Some(kind) = kind else {
                offenders.push(format!("{feature}: no fact kind"));
                continue;
            };

            // 2. Its kind's provisional goldens.
            let stem = format!("fact_{}_", kind.code());
            offenders.extend(files.iter().filter(|file| file.starts_with(&stem)).cloned());
        }
        offenders
    }

    /// The file names in `tests/golden3/provisional/`, sorted.
    fn provisional_files() -> Vec<String> {
        let dir = format!("{}/tests/golden3/{PROVISIONAL}", env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .expect("tests/golden3/provisional/")
            .map(|entry| entry.expect("an entry").file_name().into_string().unwrap())
            .collect();
        files.sort();
        files
    }

    /// No fact body of a feature this build supports is still provisional: the build constant,
    /// never the test seam's wider set.
    #[test]
    fn an_enabled_feature_has_no_provisional_golden() {
        let offenders = provisional_offenders(SUPPORTED_FEATURES, &provisional_files());
        assert_eq!(offenders, Vec::<String>::new());
        assert_eq!(provisional_files().len(), 37, "the design §2.8's 37 cells");
    }

    /// The detector's self-check: a supported feature whose goldens are still provisional is
    /// caught, and so is a supported feature no kind has (fail closed).
    #[test]
    fn the_provisional_detector_catches_a_planted_one() {
        let files = provisional_files();
        let caught = provisional_offenders(&["review"], &files);
        assert_eq!(caught.len(), 8, "{caught:?}");
        assert!(caught.iter().all(|file| file.starts_with("fact_3_")));
        assert_eq!(
            provisional_offenders(&["nothing"], &files),
            ["nothing: no fact kind"]
        );

        // The planted file alone, in a list without the real ones.
        let planted = ["fact_10_1.hex".to_owned()];
        assert_eq!(provisional_offenders(&["provenance"], &planted), planted);
        assert_eq!(
            provisional_offenders(&["lineage"], &planted),
            Vec::<String>::new()
        );
    }

    /// Each fact kind has a feature name of its own.
    #[test]
    fn each_kind_has_its_feature() {
        for kind in FactKind::ALL {
            assert!(is_name(feature_of(kind)), "{kind:?}");
        }
        let mut names: Vec<&str> = KIND_FEATURES.iter().map(|(_, name)| *name).collect();
        names.dedup();
        assert_eq!(names.len(), FactKind::ALL.len());
    }
}
