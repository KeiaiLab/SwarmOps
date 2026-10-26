//! Tests of the rebuild of the journal's projections (the M6 design §7): `journal.off` from the
//! frames of `journal.log`, the index and its sorted views by the journal, the ref files from its
//! `RefMove` records. Each runs on both formats.
//!
//! Offsets are worked out from the layouts: a `journal.off` entry is 8 bytes in FORMAT 1 (the
//! frame's offset) and 16 in FORMAT 2 (`offset | payload_crc | ecrc`), so a frame's offset is the
//! entry's first 8 bytes in both; a frame header is 8 bytes in FORMAT 1 and 24 in FORMAT 2.

use std::fs;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_store::meta::fsck;
use swarmops_store::{
    CasResult, ChangesetMeta, FsOps, Journal, Layout, MetaStore, RealFs, RefMoveEntry, RefState,
    StoreError, StoreFormat,
};
use swarmops_testkit::store::new_root;
use tempfile::TempDir;

/// Both formats, which every test runs on.
const FORMATS: [StoreFormat; 2] = [StoreFormat::V1, StoreFormat::V2];

/// Who moves the refs.
const ACTOR: &str = "test/1";

/// How many frames the small stores hold: one changeset each, `main` moved to it.
const FRAMES: u64 = 12;

/// The most zeroed entries at the end of `journal.off` that recovery drops.
const MAX_ZEROED: u64 = 512;

/// The size of one `journal.off` entry.
fn entry_len(format: StoreFormat) -> u64 {
    match format {
        StoreFormat::V1 => 8,
        _ => 16,
    }
}

/// A fresh store root of `format`.
fn root(format: StoreFormat) -> (TempDir, Layout) {
    let (dir, layout) = new_root();
    (dir, Layout::with_format(layout.root(), format))
}

fn real() -> Arc<dyn FsOps> {
    Arc::new(RealFs::new())
}

/// The bytes of the id of changeset `n`.
fn id_bytes(n: u64) -> [u8; 32] {
    let mut bytes = [0xa5; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    bytes
}

/// The id of changeset `n`.
fn id(n: u64) -> ChangesetId {
    ChangesetId::new(ObjectId::new(id_bytes(n)))
}

/// Changeset `n`, a child of `n - 1` (a root for 0), as `insert_changeset` takes it.
fn cs(n: u64) -> ChangesetMeta {
    let mut delta_hash = [0x3c; 32];
    delta_hash[..8].copy_from_slice(&n.to_le_bytes());
    let parents = n.checked_sub(1).map(id).into_iter().collect();
    ChangesetMeta::new(id(n), 0, 0, delta_hash, parents)
}

/// Moves `name` from what it names now to `new` in `write`.
fn advance(write: &mut swarmops_store::MetaWrite<'_>, name: &str, new: Option<ChangesetId>) {
    let old = write.get_ref(name).unwrap();
    let moved = write.cas_ref(name, old, new, ACTOR).unwrap();
    assert!(matches!(moved, CasResult::Applied { .. }), "{moved:?}");
}

/// A store of `format` of `frames` frames, frame `n` inserting changeset `n` and moving `main` to
/// it; `release/a` moves in every fourth from frame 1, `release/gone` is made in frame 6 and
/// deleted in frame 9. Flushed.
fn build(format: StoreFormat, frames: u64) -> (TempDir, Layout) {
    let (dir, layout) = root(format);
    let mut store = MetaStore::create_in(layout.clone(), real()).unwrap();
    store.set_parallelism(1);
    for n in 0..frames {
        let mut write = store.write().unwrap();
        write.insert_changeset(cs(n)).unwrap();
        advance(&mut write, "main", Some(id(n)));
        if n % 4 == 1 {
            advance(&mut write, "release/a", Some(id(n)));
        }
        if n == 6 {
            advance(&mut write, "release/gone", Some(id(n)));
        }
        if n == 9 {
            advance(&mut write, "release/gone", None);
        }
        write.commit().unwrap();
    }
    store.flush().unwrap();
    (dir, layout)
}

/// A store of `format` of `count` changesets in frames of 100, `main` at the last: past
/// `Index::COMPACT_THRESHOLD`, so that its sorted views cover records. Flushed.
fn build_wide(format: StoreFormat, count: u64) -> (TempDir, Layout) {
    let (dir, layout) = root(format);
    let mut store = MetaStore::create_in(layout.clone(), real()).unwrap();
    store.set_parallelism(1);
    for first in (0..count).step_by(100) {
        let mut write = store.write().unwrap();
        let batch = (first..count.min(first + 100)).map(cs).collect();
        write.insert_changesets_batch(batch).unwrap();
        advance(&mut write, "main", Some(id(count.min(first + 100) - 1)));
        write.commit().unwrap();
    }
    store.flush().unwrap();
    (dir, layout)
}

/// What a store shows: every changeset of `0..count`, every ref, `main`'s reflog.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    changesets: Vec<Option<ChangesetMeta>>,
    refs: Vec<(String, RefState)>,
    reflog: Vec<RefMoveEntry>,
}

fn shown(layout: &Layout, count: u64) -> Result<Shown, StoreError> {
    let store = MetaStore::open_in(layout.clone(), real())?;
    let read = store.read()?;
    Ok(Shown {
        changesets: (0..count)
            .map(|n| read.changeset(id(n)))
            .collect::<Result<_, _>>()?,
        refs: read.list_refs()?,
        reflog: read.reflog("main", 10_000)?,
    })
}

/// The bytes of `path`.
fn bytes(path: impl AsRef<std::path::Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

/// Appends `data` to `path`.
fn append(path: impl AsRef<std::path::Path>, data: &[u8]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut file, data).unwrap();
}

/// Writes `data` over `path` at `offset`.
fn write_at(path: impl AsRef<std::path::Path>, offset: u64, data: &[u8]) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(data, offset).unwrap();
}

/// Where frame `seq` starts, as entry `seq` of `journal.off` says.
fn frame_offset(layout: &Layout, seq: u64) -> u64 {
    let off = bytes(layout.journal_off());
    let at = usize::try_from(seq * entry_len(layout.format())).unwrap();
    u64::from_le_bytes(off[at..at + 8].try_into().unwrap())
}

/// Whether reading every frame fails as damage.
fn frames_corrupt(layout: &Layout) -> bool {
    let journal = Journal::open(layout, real()).unwrap();
    matches!(
        journal.frames_from(0, 10_000),
        Err(StoreError::JournalCorrupt { .. })
    )
}

/// A store whose `journal.off` `damage` broke, rebuilt: the entries come back as the journal
/// wrote them, the log is unchanged, the store shows what it showed, and takes a write.
fn assert_offsets_rebuilt(what: &str, damage: impl Fn(&Layout)) {
    for format in FORMATS {
        let (_dir, layout) = build(format, FRAMES);
        let (log, off) = (bytes(layout.journal_log()), bytes(layout.journal_off()));
        let before = shown(&layout, FRAMES).unwrap();
        damage(&layout);
        assert_ne!(bytes(layout.journal_off()), off, "{what}, {format:?}");

        let rebuilt = fsck::rebuild_offsets(&layout, real()).unwrap();
        assert_eq!(rebuilt.written, FRAMES, "{what}, {format:?}");
        assert_eq!(bytes(layout.journal_off()), off, "{what}, {format:?}");
        assert_eq!(bytes(layout.journal_log()), log, "{what}, {format:?}");
        assert!(!frames_corrupt(&layout), "{what}, {format:?}");
        assert_eq!(
            shown(&layout, FRAMES).unwrap(),
            before,
            "{what}, {format:?}"
        );
        let store = MetaStore::open_in(layout.clone(), real()).unwrap();
        let mut write = store.write().unwrap();
        advance(&mut write, "main", Some(id(0)));
        write.commit().unwrap();
    }
}

/// More zeroed entries at the end of `journal.off` than a power loss leaves fail every write;
/// the rebuild lists the frames again.
#[test]
fn offsets_past_many_zeroed_entries_are_rebuilt() {
    assert_offsets_rebuilt("zeroed entries", |layout| {
        let zeros = (MAX_ZEROED + 88) * entry_len(layout.format());
        append(
            layout.journal_off(),
            &vec![0; usize::try_from(zeros).unwrap()],
        );
        let store = MetaStore::open_in(layout.clone(), real()).unwrap();
        assert!(matches!(
            store.write(),
            Err(StoreError::JournalCorrupt { offset: 0 })
        ));
    });
}

/// An entry crafted to point into its frame misnumbers the frames after it.
#[test]
fn a_crafted_entry_is_rebuilt() {
    assert_offsets_rebuilt("crafted entry", |layout| {
        let at = 3 * entry_len(layout.format());
        let crafted = frame_offset(layout, 3) + 1;
        write_at(layout.journal_off(), at, &crafted.to_le_bytes());
        assert!(frames_corrupt(layout));
    });
}

/// A zeroed entry before the end is no power loss's.
#[test]
fn a_deep_zeroed_entry_is_rebuilt() {
    assert_offsets_rebuilt("deep zeroed entry", |layout| {
        let len = entry_len(layout.format());
        write_at(
            layout.journal_off(),
            2 * len,
            &vec![0; usize::try_from(len).unwrap()],
        );
        assert!(frames_corrupt(layout));
    });
}

/// A journal whose offsets are right rebuilds to the same offsets.
#[test]
fn sound_offsets_rebuild_to_themselves() {
    assert_offsets_rebuilt("no damage", |layout| {
        // A partial entry at the end, which any writer drops: the file differs, the entries not.
        append(layout.journal_off(), &[0; 3]);
    });
}

/// A torn frame at the end of the log, behind damaged offsets, is cut as a writer cuts it.
#[test]
fn a_torn_tail_is_cut_as_a_writer_cuts_it() {
    assert_offsets_rebuilt("torn tail", |layout| {
        append(layout.journal_log(), b"torn");
        let len = entry_len(layout.format());
        write_at(
            layout.journal_off(),
            2 * len,
            &vec![0; usize::try_from(len).unwrap()],
        );
    });
}

/// Damage the rebuild cannot vouch for leaves `journal.log` and `journal.off` byte for byte as
/// they were: `JournalCorrupt` at the frame where the walk stopped.
fn assert_refused(what: &str, damage: impl Fn(&Layout) -> u64) {
    for format in FORMATS {
        let (_dir, layout) = build(format, FRAMES);
        let offset = damage(&layout);
        let (log, off) = (bytes(layout.journal_log()), bytes(layout.journal_off()));
        let refused = fsck::rebuild_offsets(&layout, real());
        assert!(
            matches!(refused, Err(StoreError::JournalCorrupt { offset: at }) if at == offset),
            "{what}, {format:?}: {refused:?}"
        );
        assert_eq!(bytes(layout.journal_log()), log, "{what}, {format:?}");
        assert_eq!(bytes(layout.journal_off()), off, "{what}, {format:?}");
    }
}

/// A frame in the middle whose payload fails its CRC is damage, never a torn tail.
#[test]
fn a_damaged_middle_payload_is_refused() {
    assert_refused("payload", |layout| {
        let start = frame_offset(layout, 2);
        let header = match layout.format() {
            StoreFormat::V1 => 8,
            _ => 24,
        };
        let at = start + header + 1;
        let byte = bytes(layout.journal_log())[usize::try_from(at).unwrap()];
        write_at(layout.journal_log(), at, &[byte ^ 0xff]);
        start
    });
}

/// A frame in the middle whose header is damaged is too: the frames after it were listed.
#[test]
fn a_damaged_middle_header_is_refused() {
    assert_refused("header", |layout| {
        let start = frame_offset(layout, 2);
        let byte = bytes(layout.journal_log())[usize::try_from(start).unwrap()];
        write_at(layout.journal_log(), start, &[byte ^ 0x01]);
        start
    });
}

/// Frames `journal.off` listed and the log lost are not listed away.
#[test]
fn frames_lost_from_the_log_are_refused() {
    assert_refused("lost frame", |layout| {
        let last = frame_offset(layout, FRAMES - 1);
        let log = fs::OpenOptions::new()
            .write(true)
            .open(layout.journal_log())
            .unwrap();
        log.set_len(last).unwrap();
        last
    });
}

/// The index files and sorted views of `layout`, by name.
fn index_files(layout: &Layout) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for dir in ["idx", "sorted"] {
        let mut names: Vec<_> = fs::read_dir(layout.meta_path(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        for name in names {
            let path = layout.meta_path(dir).join(&name);
            files.push((format!("{dir}/{name}"), bytes(path)));
        }
    }
    files.push((
        "parents.log".to_owned(),
        bytes(layout.meta_path("parents.log")),
    ));
    files
}

/// The index a fresh store replays from `layout`'s journal: a new store whose journal files are
/// `layout`'s, its checkpoint at 0, flushed. With the journal, what a rebuilt index must equal.
fn fresh_replay(layout: &Layout) -> (TempDir, Layout) {
    let (dir, fresh) = root(layout.format());
    MetaStore::create_in(fresh.clone(), real()).unwrap();
    fs::copy(layout.journal_log(), fresh.journal_log()).unwrap();
    fs::copy(layout.journal_off(), fresh.journal_off()).unwrap();
    Journal::open(&fresh, real()).unwrap().set_ckpt(0).unwrap();
    let mut store = MetaStore::open_in(fresh.clone(), real()).unwrap();
    store.set_parallelism(1);
    store.flush().unwrap();
    (dir, fresh)
}

/// The rebuilt index of `layout` holds what a fresh replay of its journal holds, byte for byte,
/// and the journals are alike (both may have appended the same `Compacted` frame).
fn assert_like_fresh(layout: &Layout, fresh: &Layout, what: &str) {
    assert_eq!(index_files(layout), index_files(fresh), "{what}");
    assert_eq!(
        bytes(layout.journal_log()),
        bytes(fresh.journal_log()),
        "{what}"
    );
}

/// A store whose index `damage` broke, which a normal write keeps (the records lie before the
/// checkpoint, or in the views), rebuilt like a fresh replay of its journal.
fn assert_index_rebuilt(what: &str, wide: bool, damage: impl Fn(&Layout, u64)) {
    for format in FORMATS {
        let count = if wide { 1_234 } else { FRAMES };
        let (_dir, layout) = if wide {
            build_wide(format, count)
        } else {
            build(format, count)
        };
        let before = shown(&layout, count).unwrap();
        damage(&layout, count);
        MetaStore::open_in(layout.clone(), real())
            .unwrap()
            .flush()
            .unwrap();
        assert_ne!(shown(&layout, count).ok(), Some(before.clone()), "{what}");

        let (_fresh_dir, fresh) = fresh_replay(&layout);
        let rebuilt = fsck::rebuild_index(&layout, real()).unwrap();
        assert!(rebuilt.written > 0, "{what}, {format:?}: {rebuilt:?}");
        assert_like_fresh(&layout, &fresh, &format!("{what}, {format:?}"));
        assert_eq!(shown(&layout, count).unwrap(), before, "{what}, {format:?}");
    }
}

/// The record slot `rec` of column `col` (`ids`, `gen`, `dh`, `par`), entries `width` wide.
fn col_at(layout: &Layout, col: &str, rec: u64, width: u64, data: &[u8]) {
    write_at(
        layout.meta_path(&format!("idx/{col}.col")),
        rec * width,
        data,
    );
}

/// A generation crafted into a record before the checkpoint.
#[test]
fn crafted_generations_are_rebuilt() {
    assert_index_rebuilt("crafted generation", false, |layout, _| {
        col_at(layout, "gen", 3, 8, &999u64.to_le_bytes());
    });
}

/// The last records zero-filled, their lengths kept.
#[test]
fn a_zero_filled_index_tail_is_rebuilt() {
    assert_index_rebuilt("zero-filled tail", false, |layout, count| {
        for rec in count - 3..count {
            col_at(layout, "ids", rec, 32, &[0; 32]);
            col_at(layout, "gen", rec, 8, &[0; 8]);
            col_at(layout, "dh", rec, 32, &[0; 32]);
        }
    });
}

/// A record naming a changeset the journal does not hold there.
#[test]
fn a_mismatched_record_is_rebuilt() {
    assert_index_rebuilt("mismatched record", false, |layout, _| {
        col_at(layout, "ids", 4, 32, &id_bytes(999));
    });
}

/// Records the sorted views cover, and a key of a view, damaged.
#[test]
fn damage_the_views_cover_is_rebuilt() {
    assert_index_rebuilt("in the views", true, |layout, _| {
        col_at(layout, "gen", 10, 8, &7u64.to_le_bytes());
        write_at(layout.meta_path("sorted/ids.key"), 32 * 20, &[0xff; 32]);
    });
}

/// A sound index rebuilds to what it was.
#[test]
fn a_sound_index_rebuilds_like_a_fresh_replay() {
    for format in FORMATS {
        let (_dir, layout) = build_wide(format, 1_100);
        let before = shown(&layout, 1_100).unwrap();
        let (_fresh_dir, fresh) = fresh_replay(&layout);
        fsck::rebuild_index(&layout, real()).unwrap();
        assert_like_fresh(&layout, &fresh, &format!("{format:?}"));
        assert_eq!(shown(&layout, 1_100).unwrap(), before, "{format:?}");
    }
}

/// Every ref file under `layout`'s refs directory, by its path there.
fn ref_files(layout: &Layout) -> Vec<(Utf8PathBuf, Vec<u8>)> {
    fn walk(
        dir: &camino::Utf8Path,
        base: &camino::Utf8Path,
        out: &mut Vec<(Utf8PathBuf, Vec<u8>)>,
    ) {
        for entry in dir.read_dir_utf8().unwrap() {
            let path = entry.unwrap().into_path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push((path.strip_prefix(base).unwrap().to_owned(), bytes(&path)));
            }
        }
    }
    let mut files = Vec::new();
    walk(&layout.refs_dir(), &layout.refs_dir(), &mut files);
    files.sort();
    files
}

/// The file of ref `main`: the one whose bytes name `main`'s target.
fn main_file(layout: &Layout) -> Utf8PathBuf {
    let target = id(FRAMES - 1).to_string();
    let files = ref_files(layout);
    let (path, _) = (files.iter())
        .find(|(_, data)| String::from_utf8_lossy(data).contains(&target))
        .unwrap();
    layout.refs_dir().join(path)
}

/// The refs of the small stores: `main` moved in every frame, `release/a` in every fourth from
/// frame 1, `release/gone` made and deleted.
fn expected_refs() -> Vec<(String, RefState)> {
    vec![
        ("main".to_owned(), RefState::new(id(FRAMES - 1), FRAMES)),
        ("release/a".to_owned(), RefState::new(id(9), 3)),
    ]
}

/// A store whose ref files `damage` broke, the refs rebuilt from the journal: every ref at its
/// target and version, `written` files written or removed, and a second rebuild writes nothing.
fn assert_refs_rebuilt(what: &str, written: u64, damage: impl Fn(&Layout)) {
    for format in FORMATS {
        let (_dir, layout) = build(format, FRAMES);
        let before = shown(&layout, FRAMES).unwrap();
        assert_eq!(before.refs, expected_refs(), "{format:?}");
        damage(&layout);

        let rebuilt = fsck::rebuild_refs(&layout, real()).unwrap();
        assert_eq!(rebuilt.written, written, "{what}, {format:?}");
        assert_eq!(
            shown(&layout, FRAMES).unwrap(),
            before,
            "{what}, {format:?}"
        );
        let again = fsck::rebuild_refs(&layout, real()).unwrap();
        assert_eq!(again.written, 0, "{what}, {format:?}");
    }
}

/// A ref file that does not read fails every snapshot; it is written again.
#[test]
fn a_damaged_ref_file_is_rewritten() {
    assert_refs_rebuilt("damaged", 1, |layout| {
        fs::write(main_file(layout), b"not a ref file\n").unwrap();
        assert!(shown(layout, FRAMES).is_err());
    });
}

/// A ref file holding a state of another ref's, and a missing one.
#[test]
fn stale_and_missing_ref_files_are_written_again() {
    assert_refs_rebuilt("stale and missing", 2, |layout| {
        let main = main_file(layout);
        let other = (ref_files(layout).into_iter())
            .map(|(path, _)| layout.refs_dir().join(path))
            .find(|path| *path != main)
            .unwrap();
        fs::copy(&other, &main).unwrap();
        fs::remove_file(&other).unwrap();
    });
}

/// Every ref file gone: the versions are counted again from the journal, one per transaction
/// that changed the target, and a deleted ref stays deleted.
#[test]
fn every_ref_file_is_written_from_the_journal() {
    assert_refs_rebuilt("all gone", 2, |layout| {
        fs::remove_dir_all(layout.refs_dir()).unwrap();
        fs::create_dir(layout.refs_dir()).unwrap();
    });
}

/// A ref file no move names is removed; a file that is no ref file (a temp file) is left.
#[test]
fn a_ref_file_no_move_names_is_removed() {
    for format in FORMATS {
        let (_dir, layout) = build(format, FRAMES);
        let before = shown(&layout, FRAMES).unwrap();
        // The file of a ref `release/ghost`, as a store that moved it wrote it.
        let (_other_dir, other) = build(format, FRAMES);
        let store = MetaStore::open_in(other.clone(), real()).unwrap();
        let mut write = store.write().unwrap();
        advance(&mut write, "release/ghost", Some(id(0)));
        write.commit().unwrap();
        let ours = ref_files(&layout);
        for (path, data) in ref_files(&other) {
            if !ours.iter().any(|(mine, _)| *mine == path) {
                let to = layout.refs_dir().join(&path);
                fs::create_dir_all(to.parent().unwrap()).unwrap();
                fs::write(to, data).unwrap();
            }
        }
        let temp = layout.refs_dir().join(".tmpA1b2C3");
        fs::write(&temp, b"half a ref file").unwrap();
        assert_ne!(shown(&layout, FRAMES).unwrap(), before, "{format:?}");

        let rebuilt = fsck::rebuild_refs(&layout, real()).unwrap();
        assert_eq!(rebuilt.written, 1, "{format:?}");
        assert_eq!(shown(&layout, FRAMES).unwrap(), before, "{format:?}");
        assert!(temp.exists(), "{format:?}");
    }
}

/// The rebuilds under the order checker and through a crash at each of their calls.
#[cfg(feature = "test-hooks")]
mod hooked {
    use std::ffi::OsString;
    use std::os::unix::process::ExitStatusExt;

    use swarmops_store::{CRASH_AT_ENV, CrashAtFs};
    use swarmops_testkit::child::{SIGABRT, run, this_test};
    use swarmops_testkit::faults::checked;
    use swarmops_testkit::store::sweep::copy_of;
    use swarmops_testkit::test_name;

    use super::*;

    /// Set only in the children of the crash sweep: the store root, then its format.
    const CRASH_ROOT: &str = "SWARMOPS_TEST_FSCK_CRASH_ROOT";
    const CRASH_FORMAT: &str = "SWARMOPS_TEST_FSCK_CRASH_FORMAT";

    /// Every rebuild keeps the write order the store's rules ask for.
    #[test]
    fn the_rebuilds_pass_the_order_check() {
        for format in FORMATS {
            let (_dir, layout) = build(format, FRAMES);
            write_at(layout.journal_off(), entry_len(format), &[0; 16][..8]);
            fsck::rebuild_offsets(&layout, checked(RealFs::new())).unwrap();
            col_at(&layout, "gen", 3, 8, &999u64.to_le_bytes());
            fsck::rebuild_index(&layout, checked(RealFs::new())).unwrap();
            fs::write(main_file(&layout), b"not a ref file\n").unwrap();
            fsck::rebuild_refs(&layout, checked(RealFs::new())).unwrap();
            assert_eq!(shown(&layout, FRAMES).unwrap().refs, expected_refs());
        }
    }

    /// The index rebuild of the store at `root`, in a child, through a `CrashAtFs` that aborts on
    /// its k-th call, under the order checker.
    fn crash_child(root: OsString) {
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let format = match std::env::var(CRASH_FORMAT).unwrap().as_str() {
            "1" => StoreFormat::V1,
            _ => StoreFormat::V2,
        };
        let root = Utf8PathBuf::from_path_buf(root.into()).unwrap();
        let layout = Layout::with_format(root, format);
        fsck::rebuild_index(&layout, checked(CrashAtFs::new(RealFs::new(), k))).unwrap();
    }

    /// A store whose generation of record 3 is crafted, before the checkpoint.
    fn crash_template(format: StoreFormat) -> (TempDir, Layout) {
        let (dir, layout) = build(format, FRAMES);
        col_at(&layout, "gen", 3, 8, &999u64.to_le_bytes());
        (dir, layout)
    }

    /// A crash at any call of an index rebuild leaves a store that one more write recovers, and
    /// then a rebuild run again makes like a fresh replay. From the rebuild's commit point on
    /// (the sorted views emptied, the checkpoint at 0) the one write rebuilds it: a crash never
    /// takes back a rebuild that reached it.
    #[test]
    fn a_crash_at_any_call_of_an_index_rebuild_recovers() {
        if let Some(root) = std::env::var_os(CRASH_ROOT) {
            crash_child(root);
            return;
        }
        let test = "a_crash_at_any_call_of_an_index_rebuild_recovers";
        for format in FORMATS {
            let (_template_dir, template) = crash_template(format);
            let (_fresh_dir, fresh) = fresh_replay(&template);
            let calls = {
                let (_dir, root) = copy_of(template.root());
                let counter = Arc::new(CrashAtFs::new(RealFs::new(), 0));
                let layout = Layout::with_format(root, format);
                fsck::rebuild_index(&layout, Arc::clone(&counter) as Arc<dyn FsOps>).unwrap();
                counter.calls()
            };
            assert!(calls > 5, "{calls} calls");
            let mut rebuilt_from = None;
            for k in 1..=calls + 1 {
                let (_dir, root) = copy_of(template.root());
                let layout = Layout::with_format(root, format);
                let child = run(this_test(&test_name!(test))
                    .env(CRASH_ROOT, layout.root())
                    .env(CRASH_FORMAT, format.version().to_string())
                    .env(CRASH_AT_ENV, k.to_string()));
                let ended = if k <= calls {
                    child.status.signal() == Some(SIGABRT)
                } else {
                    child.status.success()
                };
                assert!(
                    ended,
                    "{format:?} k={k}: {:?}\n{}",
                    child.status,
                    String::from_utf8_lossy(&child.stderr)
                );

                let mut store = MetaStore::open_in(layout.clone(), checked(RealFs::new())).unwrap();
                store.set_parallelism(1);
                store.flush().unwrap();
                let generation = (store.read().unwrap().changeset(id(3)).unwrap())
                    .unwrap()
                    .generation;
                match rebuilt_from {
                    None if generation == 4 => rebuilt_from = Some(k),
                    None => assert_eq!(generation, 999, "{format:?} k={k}"),
                    Some(_) => assert_eq!(generation, 4, "{format:?} k={k}: rebuilt before"),
                }
                fsck::rebuild_index(&layout, real()).unwrap();
                assert_like_fresh(&layout, &fresh, &format!("{format:?} k={k}"));
            }
            assert!(
                rebuilt_from.is_some_and(|k| k <= calls),
                "{format:?}: {rebuilt_from:?}"
            );
        }
    }
}

/// The time each rebuild takes on a store of 200,000 changesets, against the 10 s writers wait
/// for `meta/lock` (the M6 plan's risk R-1: the index rebuild holds the lock for the whole
/// journal's replay). Printed, not asserted: a measurement.
#[test]
#[ignore = "slow, ~60 s: builds and rebuilds a store of 200,000 changesets per format"]
fn rebuild_times_of_a_store_of_200k_changesets() {
    const COUNT: u64 = 200_000;
    for format in FORMATS {
        let (_dir, layout) = root(format);
        let mut store = MetaStore::create_in(layout.clone(), real()).unwrap();
        store.set_parallelism(1);
        for first in (0..COUNT).step_by(10_000) {
            let mut write = store.write().unwrap();
            write
                .insert_changesets_batch((first..first + 10_000).map(cs).collect())
                .unwrap();
            advance(&mut write, "main", Some(id(first + 9_999)));
            write.commit().unwrap();
        }
        store.flush().unwrap();
        drop(store);

        let started = std::time::Instant::now();
        fsck::rebuild_offsets(&layout, real()).unwrap();
        let offsets = started.elapsed();
        let started = std::time::Instant::now();
        let index = fsck::rebuild_index(&layout, real()).unwrap();
        let rebuild_index = started.elapsed();
        let started = std::time::Instant::now();
        fsck::rebuild_refs(&layout, real()).unwrap();
        let refs = started.elapsed();
        let read = MetaStore::open_in(layout.clone(), real()).unwrap();
        assert_eq!(read.read().unwrap().count_changesets().unwrap(), COUNT);
        println!(
            "{format:?}: {COUNT} changesets: offsets {offsets:?}, index {rebuild_index:?} \
             ({} replayed), refs {refs:?}; lock timeout 10 s",
            index.written
        );
    }
}
