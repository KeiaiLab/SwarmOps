//! `index.bin` (§4.2 of the M5 design): the workspace's cache of one row per tracked file, the
//! head's entry there (kind and blob) and the host's stat when its bytes were last found to be
//! that blob. A cache, outside the hash contract: a missing or bad one is rebuilt by hashing.
//!
//! ```text
//!  header   "SWWI" u8 ver=1 u8 0 u16 0 u64 count i64 written_ns       24 bytes, little-endian
//!  paths    u64 offset × (count + 1), then the paths' bytes, sorted
//!  columns  size u64 × count | mtime_ns i64 × count | inode u64 × count | mode u8 × count
//!           | blob [32] × count
//!  check    BLAKE3 of the binding (the head and the prefixes) and everything before it
//! ```
//!
//! The checksum covers what the rows stand for: an index written for another head or other
//! prefixes fails it, as a torn one does, and is rebuilt. `written_ns` is the filesystem's clock
//! when the scan that wrote it started (a temp file's mtime): a row whose mtime is at or after it
//! may have changed within the same tick, and is hashed (the racy rule). Such a row its scan does
//! not confirm by hashing goes into the next index unconfirmed: that index's later time would
//! trust it.

use std::io::Write as _;

use camino::Utf8Path;
use swarmops_api::FileKind;

use crate::state::SWARMWS_DIR;

/// The index's file, in [`SWARMWS_DIR`].
pub const INDEX_BIN: &str = "index.bin";

/// The header's magic.
const MAGIC: &[u8; 4] = b"SWWI";

/// The layout version this build reads and writes.
const VERSION: u8 = 1;

/// The header's length: magic, version, two reserved fields, count, time.
const HEADER: usize = 24;

/// A blob id's length.
const BLOB: usize = 32;

/// The checksum's length.
const CHECK: usize = 32;

/// The `mtime_ns` of a row whose stat was never confirmed: it matches no file.
pub(crate) const UNCONFIRMED: i64 = i64::MIN;

/// One tracked file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    /// The repository path.
    pub(crate) path: String,
    /// The host file's size when its bytes were last the blob's.
    pub(crate) size: u64,
    /// Its mtime then; [`UNCONFIRMED`] when never.
    pub(crate) mtime_ns: i64,
    /// Its inode then; 0 where the platform has none.
    pub(crate) inode: u64,
    /// The head's kind there.
    pub(crate) mode: FileKind,
    /// The head's blob there.
    pub(crate) blob: [u8; BLOB],
}

/// The rows of a workspace, sorted by path, and when the scan that found them started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Index {
    /// The filesystem's time when the scan that wrote it started, in Unix nanoseconds.
    pub(crate) written_ns: i64,
    /// The rows, sorted by path, each once.
    pub(crate) rows: Vec<Row>,
}

/// What an index's rows describe: the workspace's prefixes or the whole tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Extent {
    /// The prefixes alone.
    Prefixes,
    /// The whole tree.
    Whole,
}

/// What an index stands for: the head and the prefixes (or the whole tree) its rows describe.
pub(crate) fn binding(head: &str, prefixes: &[String], extent: Extent) -> String {
    // The bytes predate `Extent`: the whole tree still reads "true", the prefixes "false".
    let all = extent == Extent::Whole;
    format!("{head}\n{all}\n{}", prefixes.join("\n"))
}

/// The index of the workspace at `dir`, if one is there and stands for `binding`; none for a
/// missing, torn, foreign or newer one, which the caller rebuilds.
pub(crate) fn read(dir: &Utf8Path, binding: &str) -> Option<Index> {
    let bytes = std::fs::read(dir.join(SWARMWS_DIR).join(INDEX_BIN)).ok()?;
    decode(&bytes, binding)
}

/// Writes `index`, standing for `binding`, as the index of the workspace at `dir`, through the
/// temp file `temp` (made in `.swarmws/` when the scan started): its bytes, then a rename over
/// the index. No sync: a torn index fails its checksum. An index with a row of a kind this build
/// does not know is not written.
pub(crate) fn write(
    dir: &Utf8Path,
    temp: tempfile::NamedTempFile,
    binding: &str,
    index: &Index,
) -> std::io::Result<()> {
    let mut temp = temp;
    let bytes = encode(index, binding)
        .ok_or_else(|| std::io::Error::other("a file kind this build does not know"))?;
    temp.write_all(&bytes)?;
    temp.persist(dir.join(SWARMWS_DIR).join(INDEX_BIN))
        .map(|_| ())
        .map_err(|e| e.error)
}

/// `index`'s bytes, its checksum bound to `binding`; none when a row's kind is one this build
/// does not know.
pub(crate) fn encode(index: &Index, binding: &str) -> Option<Vec<u8>> {
    // 1. The header.
    let rows = &index.rows;
    let mut out = Vec::with_capacity(HEADER + rows.len() * 96 + CHECK);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[VERSION, 0, 0, 0]);
    out.extend_from_slice(&to_u64(rows.len()).to_le_bytes());
    out.extend_from_slice(&index.written_ns.to_le_bytes());

    // 2. The paths: their offsets, then their bytes.
    let mut offset = 0u64;
    out.extend_from_slice(&offset.to_le_bytes());
    for row in rows {
        offset += to_u64(row.path.len());
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for row in rows {
        out.extend_from_slice(row.path.as_bytes());
    }

    // 3. The columns, one after the other.
    for row in rows {
        out.extend_from_slice(&row.size.to_le_bytes());
    }
    for row in rows {
        out.extend_from_slice(&row.mtime_ns.to_le_bytes());
    }
    for row in rows {
        out.extend_from_slice(&row.inode.to_le_bytes());
    }
    for row in rows {
        out.push(mode_byte(row.mode)?);
    }
    for row in rows {
        out.extend_from_slice(&row.blob);
    }

    // 4. The checksum of the binding and all of it.
    let check = checksum(binding, &out);
    out.extend_from_slice(&check);
    Some(out)
}

/// The index `bytes` hold, if they are one this build writes, standing for `binding`.
pub(crate) fn decode(bytes: &[u8], binding: &str) -> Option<Index> {
    // 1. The checksum, over the binding and everything before it.
    let body_len = bytes.len().checked_sub(CHECK)?;
    let (body, check) = bytes.split_at(body_len);
    if checksum(binding, body) != check {
        return None;
    }

    // 2. The header.
    let mut at = Reader { bytes: body, at: 0 };
    if at.take(4)? != MAGIC || at.take(4)? != [VERSION, 0, 0, 0] {
        return None;
    }
    let count = usize::try_from(at.u64()?).ok()?;
    let written_ns = i64::from_le_bytes(at.array()?);

    // 3. The paths, each UTF-8, in strictly rising order.
    let offsets = (0..=count)
        .map(|_| at.u64())
        .collect::<Option<Vec<u64>>>()?;
    let total = usize::try_from(*offsets.last()?).ok()?;
    let text = at.take(total)?;
    let mut paths = Vec::with_capacity(count);
    for pair in offsets.windows(2) {
        let (start, end) = (
            usize::try_from(pair[0]).ok()?,
            usize::try_from(pair[1]).ok()?,
        );
        let path = std::str::from_utf8(text.get(start..end)?).ok()?;
        if paths.last().is_some_and(|last: &&str| *last >= path) {
            return None;
        }
        paths.push(path);
    }

    // 4. The columns, and nothing after them.
    let sizes = (0..count).map(|_| at.u64()).collect::<Option<Vec<u64>>>()?;
    let mtimes = (0..count)
        .map(|_| at.array().map(i64::from_le_bytes))
        .collect::<Option<Vec<i64>>>()?;
    let inodes = (0..count).map(|_| at.u64()).collect::<Option<Vec<u64>>>()?;
    let modes = (at.take(count)?.iter())
        .map(|byte| byte_mode(*byte))
        .collect::<Option<Vec<FileKind>>>()?;
    let blobs = (0..count)
        .map(|_| at.array::<BLOB>())
        .collect::<Option<Vec<[u8; BLOB]>>>()?;
    if at.at != body.len() {
        return None;
    }

    // 5. The rows.
    let rows = (0..count)
        .map(|n| Row {
            path: paths[n].to_owned(),
            size: sizes[n],
            mtime_ns: mtimes[n],
            inode: inodes[n],
            mode: modes[n],
            blob: blobs[n],
        })
        .collect();
    Some(Index { written_ns, rows })
}

/// The checksum of an index body `body` standing for `binding`.
fn checksum(binding: &str, body: &[u8]) -> [u8; CHECK] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&to_u64(binding.len()).to_le_bytes());
    hasher.update(binding.as_bytes());
    hasher.update(body);
    *hasher.finalize().as_bytes()
}

/// The `mode` column's byte of `kind`; none for a kind this build does not know.
fn mode_byte(kind: FileKind) -> Option<u8> {
    match kind {
        FileKind::File => Some(0),
        FileKind::Exec => Some(1),
        FileKind::Symlink => Some(2),
        _ => None,
    }
}

/// The kind a `mode` column's byte names, if any.
fn byte_mode(byte: u8) -> Option<FileKind> {
    match byte {
        0 => Some(FileKind::File),
        1 => Some(FileKind::Exec),
        2 => Some(FileKind::Symlink),
        _ => None,
    }
}

/// `n` as a u64.
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A cursor over an index's bytes.
struct Reader<'a> {
    /// The bytes.
    bytes: &'a [u8],
    /// Where the next read starts.
    at: usize,
}

impl<'a> Reader<'a> {
    /// The next `n` bytes, if there are that many.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let taken = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    /// The next `N` bytes, as an array.
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    /// The next little-endian u64.
    fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index of two rows.
    fn sample() -> Index {
        let row = |path: &str, mode, fill| Row {
            path: path.to_owned(),
            size: 7,
            mtime_ns: 1_792_630_000_000_000_000,
            inode: 42,
            mode,
            blob: [fill; BLOB],
        };
        Index {
            written_ns: 1_792_630_000_000_000_001,
            rows: vec![
                row("src/a.txt", FileKind::File, 1),
                row("src/run.sh", FileKind::Exec, 2),
            ],
        }
    }

    /// An index reads back as written, for its binding only; any byte flipped, a byte cut or
    /// added, or rows out of order, and it reads as none.
    #[test]
    fn an_index_round_trips_for_its_binding_only() {
        // 1. Written and read back.
        let index = sample();
        let bytes = encode(&index, "head").unwrap();
        assert_eq!(&bytes[..4], MAGIC);
        assert_eq!(decode(&bytes, "head"), Some(index.clone()));
        assert_eq!(
            decode(&encode(&Index::default(), "h").unwrap(), "h"),
            Some(Index::default())
        );

        // 2. Another binding, a flipped byte, a short or long file: none.
        assert_eq!(decode(&bytes, "other"), None);
        for at in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[at] ^= 1;
            assert_eq!(decode(&bad, "head"), None, "byte {at}");
        }
        assert_eq!(decode(&bytes[..bytes.len() - 1], "head"), None);
        assert_eq!(decode(&[], "head"), None);

        // 3. Rows out of order, checksummed all the same: none.
        let mut swapped = index;
        swapped.rows.reverse();
        assert_eq!(decode(&encode(&swapped, "head").unwrap(), "head"), None);
    }
}
