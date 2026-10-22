//! The external id index (`ExternalIds`, storage spec §7): entries written by the object rules
//! and read back, their names made durable before a put returns, a put again writing nothing and
//! never replacing an entry, damage reported as `ExternalIdCorrupt`, names checked before any
//! call, and a put crashed at any of its calls leaving whole entries or none.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`) with the `meta/` and
//! `objects/tmp/` a store has.

use std::fmt::Write as _;
use std::fs;
#[cfg(feature = "test-hooks")]
use std::os::unix::process::ExitStatusExt;
use std::sync::Arc;

#[cfg(feature = "test-hooks")]
use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectKind, hash_object};
#[cfg(feature = "test-hooks")]
use swarmops_store::{CRASH_AT_ENV, CrashAtFs, FsOps};
use swarmops_store::{
    ExternalEntry, ExternalIds, ExternalPut, Layout, RealFs, SYSTEM_GIT, StoreError,
};
#[cfg(feature = "test-hooks")]
use swarmops_testkit::child::{self, SIGABRT, this_test};
use swarmops_testkit::fs::RecordingFs;
use swarmops_testkit::store::new_root;
#[cfg(feature = "test-hooks")]
use swarmops_testkit::test_name;
use tempfile::TempDir;

/// Set only in the children that `external_ids_crashing_at_any_call_leave_whole_entries_or_none`
/// spawns: the store root.
#[cfg(feature = "test-hooks")]
const CRASH_ROOT: &str = "SWARMOPS_TEST_EXTERNAL_IDS_CRASH_ROOT";

/// Changeset `n` of these tests: a real hash, so that the ids look like a store's.
fn changeset(n: u8) -> ChangesetId {
    ChangesetId(hash_object(ObjectKind::Changeset, &[n]))
}

/// A 20-byte key, as a git oid is: `fanout`, then `n` 19 times.
fn key(fanout: u8, n: u8) -> Vec<u8> {
    let mut key = vec![n; 20];
    key[0] = fanout;
    key
}

/// An entry that names changeset `n`.
fn entry(n: u8, faithful: bool) -> ExternalEntry {
    ExternalEntry {
        changeset: changeset(n),
        faithful,
    }
}

/// Three entries in two fan-out directories, in key order: two in `aa/`, the second not
/// faithful, and one in `bb/`.
fn three() -> Vec<(Vec<u8>, ExternalEntry)> {
    vec![
        (key(0xaa, 1), entry(1, true)),
        (key(0xaa, 2), entry(2, false)),
        (key(0xbb, 3), entry(3, true)),
    ]
}

/// The lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        write!(hex, "{byte:02x}").expect("a String takes every write");
        hex
    })
}

/// A store root as a store has it before its first external id: `meta/` and `objects/tmp/`.
fn store_root() -> (TempDir, Layout) {
    let (dir, layout) = new_root();
    fs::create_dir_all(layout.meta_dir()).unwrap();
    fs::create_dir_all(layout.objects_tmp()).unwrap();
    (dir, layout)
}

/// The index of the store `layout` lays out, on the real filesystem.
fn index_of(layout: &Layout) -> ExternalIds {
    ExternalIds::new(layout.clone(), Arc::new(RealFs))
}

/// Whether `result` is a `BadCall`.
fn bad_call<T>(result: &Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::BadCall(_)))
}

/// Whether `result` is the `ExternalIdCorrupt` of key `key` of system `git`.
fn corrupt<T>(result: &Result<T, StoreError>, key: &[u8]) -> bool {
    matches!(
        result,
        Err(StoreError::ExternalIdCorrupt { system, key: named }) if system == "git" && *named == hex(key)
    )
}

#[test]
fn put_batch_then_get_reads_each_entry() {
    let (_dir, layout) = store_root();
    let index = index_of(&layout);
    let entries = three();
    let put = index.put_batch(SYSTEM_GIT, &entries).unwrap();
    assert_eq!(
        put,
        ExternalPut {
            written: 3,
            existing: 0,
            conflicts: Vec::new(),
        }
    );
    for (key, entry) in &entries {
        assert_eq!(
            index.get(SYSTEM_GIT, key).unwrap(),
            Some(*entry),
            "{}",
            hex(key)
        );
        // One 44-byte file, named by the key's hex past its first byte, in the fan-out
        // directory its first byte names.
        let hex = hex(key);
        let file = layout
            .root()
            .join(format!("meta/external_ids/git/{}/{}", &hex[..2], &hex[2..]));
        assert_eq!(layout.external_id_path(SYSTEM_GIT, key), file);
        assert_eq!(fs::metadata(&file).unwrap().len(), 44);
    }
    assert_eq!(index.get(SYSTEM_GIT, &key(0xaa, 9)).unwrap(), None);
    assert_eq!(
        index.get(SYSTEM_GIT, &key(0xcc, 1)).unwrap(),
        None,
        "no fan-out directory"
    );
    assert_eq!(
        index.get("swarmops-0123", &key(0xaa, 1)).unwrap(),
        None,
        "no directory of that system"
    );
    // The temp files were linked into place and removed.
    assert_eq!(fs::read_dir(layout.objects_tmp()).unwrap().count(), 0);
    // The listing holds each entry, by key; the systems, the one written to.
    let listed: Vec<(Vec<u8>, ExternalEntry)> = index
        .list(SYSTEM_GIT)
        .unwrap()
        .into_iter()
        .map(|(key, entry)| (key, entry.unwrap()))
        .collect();
    assert_eq!(listed, entries);
    assert_eq!(index.systems().unwrap(), ["git"]);
}

/// What a put's calls do, one at a time (its reads, `exists` and `read`, are not pinned): the
/// directories it needs are made first; each entry is written to a temp file in `objects/tmp/`
/// and then linked into place; and after the last link come only fsyncs, one of each directory
/// whose entries it changed: every fan-out directory it put an entry into, and the parent of each
/// directory it made (`meta/`, `meta/external_ids/` and `meta/external_ids/git/` the first time);
/// none of a fan-out directory it did not touch.
#[test]
fn put_batch_makes_its_names_durable_before_it_returns() {
    let (_dir, layout) = store_root();
    let (recording, calls) = RecordingFs::new();
    let recording = recording.naming_paths_in(layout.root().as_std_path().to_owned());
    let mut index = ExternalIds::new(layout.clone(), Arc::new(recording));
    index.set_parallelism(1);
    let file = |key: &[u8]| {
        let hex = hex(key);
        format!("meta/external_ids/git/{}/{}", &hex[..2], &hex[2..])
    };
    // The put of `entries`, which must fsync exactly `fsynced` after its last link.
    let put = |entries: &[(Vec<u8>, ExternalEntry)], fsynced: &[&str]| {
        index.put_batch(SYSTEM_GIT, entries).unwrap();
        let made = calls();
        let at = |method: &str| -> Vec<usize> {
            let calls = made.iter().enumerate();
            calls
                .filter(|(_, call)| call.starts_with(method))
                .map(|(at, _)| at)
                .collect()
        };
        let (temps, links) = (at("write_durable_temp "), at("link_noclobber "));
        let linked: Vec<&str> = (links.iter())
            .map(|&at| made[at].strip_prefix("link_noclobber ").unwrap())
            .collect();
        let want: Vec<String> = entries.iter().map(|(key, _)| file(key)).collect();
        assert_eq!(linked, want, "{made:#?}");
        assert_eq!(temps.len(), links.len(), "{made:#?}");
        for (temp, link) in temps.iter().zip(&links) {
            assert_eq!(made[*temp], "write_durable_temp objects/tmp", "{made:#?}");
            assert!(temp < link, "{made:#?}");
        }
        let dirs = at("create_dir_all meta/");
        assert!(dirs.iter().all(|&dir| dir < temps[0]), "{made:#?}");
        let after: Vec<&str> = made[links.last().unwrap() + 1..]
            .iter()
            .map(|call| call.strip_prefix("fsync_dir ").expect("only fsyncs"))
            .collect();
        let mut sorted = after.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, fsynced, "{made:#?}");
        assert_eq!(at("fsync_dir ").len(), fsynced.len(), "{made:#?}");
    };

    put(
        &three(),
        &[
            "meta",
            "meta/external_ids",
            "meta/external_ids/git",
            "meta/external_ids/git/aa",
            "meta/external_ids/git/bb",
        ],
    );
    // With the directories there: an entry into `aa/` and one into a new `cc/` fsync those two
    // and `git/`, which holds `cc/` now; not `bb/`, `meta/` or `meta/external_ids/`.
    put(
        &[
            (key(0xaa, 4), entry(4, true)),
            (key(0xcc, 5), entry(5, true)),
        ],
        &[
            "meta/external_ids/git",
            "meta/external_ids/git/aa",
            "meta/external_ids/git/cc",
        ],
    );
    // Nothing to put, nothing done.
    assert_eq!(
        index.put_batch(SYSTEM_GIT, &[]).unwrap(),
        ExternalPut::default()
    );
    assert_eq!(calls(), Vec::<String>::new());
}

#[test]
fn put_batch_again_writes_nothing_and_another_entry_is_a_conflict() {
    let (_dir, layout) = store_root();
    let index = index_of(&layout);
    let entries = three();
    index.put_batch(SYSTEM_GIT, &entries).unwrap();
    let file_of = |key: &[u8]| fs::read(layout.external_id_path(SYSTEM_GIT, key)).unwrap();
    let before: Vec<Vec<u8>> = entries.iter().map(|(key, _)| file_of(key)).collect();

    // The same three: each there already, nothing written; each fan-out directory fsynced all
    // the same, as for an object found there (another writer may not have fsynced it yet).
    let (recording, calls) = RecordingFs::new();
    let watched = ExternalIds::new(layout.clone(), Arc::new(recording));
    assert_eq!(
        watched.put_batch(SYSTEM_GIT, &entries).unwrap(),
        ExternalPut {
            written: 0,
            existing: 3,
            conflicts: Vec::new(),
        }
    );
    let made = calls();
    let writes = ["create_dir_all", "write_durable_temp", "link_noclobber"];
    assert!(
        made.iter()
            .all(|call| !writes.iter().any(|write| call.starts_with(write))),
        "{made:?}"
    );
    assert_eq!(
        made.iter()
            .filter(|call| call.starts_with("fsync_dir"))
            .count(),
        2,
        "{made:?}"
    );

    // Another changeset under a key that is there: named as a conflict, the file as it was;
    // the rest of the call is written.
    let put = index
        .put_batch(
            SYSTEM_GIT,
            &[
                (key(0xaa, 2), entry(9, false)),
                (key(0xbb, 7), entry(9, true)),
            ],
        )
        .unwrap();
    assert_eq!(
        put,
        ExternalPut {
            written: 1,
            existing: 0,
            conflicts: vec![key(0xaa, 2)],
        }
    );
    assert_eq!(
        index.get(SYSTEM_GIT, &key(0xaa, 2)).unwrap(),
        Some(entry(2, false))
    );
    assert_eq!(
        index.get(SYSTEM_GIT, &key(0xbb, 7)).unwrap(),
        Some(entry(9, true))
    );
    // So is the same changeset under another faithfulness.
    let put = index
        .put_batch(SYSTEM_GIT, &[(key(0xaa, 1), entry(1, false))])
        .unwrap();
    assert_eq!(put.conflicts, [key(0xaa, 1)]);
    for ((key, _), before) in entries.iter().zip(&before) {
        assert_eq!(file_of(key), *before, "{}", hex(key));
    }
}

#[test]
fn a_damaged_entry_is_external_id_corrupt() {
    let (_dir, layout) = store_root();
    let index = index_of(&layout);
    let entries = three();
    index.put_batch(SYSTEM_GIT, &entries).unwrap();
    let path = |key: &[u8]| layout.external_id_path(SYSTEM_GIT, key);
    // The first cut short, the third with a CRC byte flipped.
    let (cut, flipped) = (&entries[0].0, &entries[2].0);
    let bytes = fs::read(path(cut)).unwrap();
    fs::write(path(cut), &bytes[..43]).unwrap();
    let mut bytes = fs::read(path(flipped)).unwrap();
    bytes[43] ^= 0x01;
    fs::write(path(flipped), bytes).unwrap();

    for key in [cut, flipped] {
        let got = index.get(SYSTEM_GIT, key);
        assert!(corrupt(&got, key), "{}: {got:?}", hex(key));
    }
    let message = index.get(SYSTEM_GIT, cut).unwrap_err().to_string();
    assert_eq!(
        message,
        format!(
            "the entry of external id {} of system git is corrupt",
            hex(cut)
        )
    );
    // The listing says the same of them, beside the whole one; and of anything under an
    // entry's name that is not a file.
    let not_a_file = key(0xbb, 8);
    fs::create_dir(path(&not_a_file)).unwrap();
    let listed = index.list(SYSTEM_GIT).unwrap();
    let keys: Vec<&Vec<u8>> = listed.iter().map(|(key, _)| key).collect();
    assert_eq!(keys, [cut, &entries[1].0, flipped, &not_a_file]);
    assert!(corrupt(&listed[0].1, cut));
    assert_eq!(listed[1].1.as_ref().ok(), Some(&entries[1].1));
    assert!(corrupt(&listed[2].1, flipped));
    assert!(corrupt(&listed[3].1, &not_a_file));
    // A put never replaces it: the damaged file stays, named as a conflict.
    let put = index.put_batch(SYSTEM_GIT, &entries[..1]).unwrap();
    assert_eq!(put.conflicts, std::slice::from_ref(cut));
    assert_eq!(fs::metadata(path(cut)).unwrap().len(), 43);
}

/// An entry's file longer than an entry (44 bytes), however long, is no entry, and is read no
/// further than a byte past one (45 bytes, storage spec §7): `get` and `list` call it
/// `ExternalIdCorrupt`, and a put of its key names it as a conflict and leaves it as it is. Each
/// file here is the entry it was, then a byte too many, or a hole (no disk) to 64 MiB, or to
/// 1 TiB, more than a host holds in memory. What a call read is what this thread's `read` calls
/// returned meanwhile, by its `rchar` count (`/proc/thread-self/io`, a file only Linux has, so
/// this test runs on Linux alone); the entry itself reads back in 44 bytes.
#[cfg(target_os = "linux")]
#[test]
fn an_entry_file_longer_than_an_entry_is_corrupt_and_read_no_further() {
    use std::fs::{File, OpenOptions};
    use std::io::Read as _;

    use swarmops_core::bytes::to_u64;

    /// The bytes the `read` calls of this thread have returned so far, and the length of the
    /// text that says so, which reading it adds.
    fn rchar() -> (u64, u64) {
        let text = fs::read_to_string("/proc/thread-self/io").expect("read /proc/thread-self/io");
        let count = (text.lines())
            .find_map(|line| line.strip_prefix("rchar:"))
            .and_then(|count| count.trim().parse().ok())
            .expect("an rchar line in /proc/thread-self/io");
        (count, to_u64(text.len()))
    }
    /// What `call` returns, with the bytes the `read` calls of this thread returned during it.
    fn reading<T>(call: impl FnOnce() -> T) -> (T, u64) {
        let (before, own) = rchar();
        let value = call();
        let (after, _) = rchar();
        (value, after - before - own)
    }

    let (_dir, layout) = store_root();
    let mut index = index_of(&layout);
    index.set_parallelism(1);
    let (id, named) = (key(0xaa, 1), entry(1, true));
    index.put_batch(SYSTEM_GIT, &[(id.clone(), named)]).unwrap();
    let path = layout.external_id_path(SYSTEM_GIT, &id);
    let whole = fs::read(&path).unwrap();
    let (got, read) = reading(|| index.get(SYSTEM_GIT, &id));
    assert_eq!((got.unwrap(), read), (Some(named), 44));

    let longer = [
        ("a byte too many", 45),
        ("a hole to 64 MiB", 64 << 20),
        ("a hole to 1 TiB", 1 << 40),
    ];
    for (what, len) in longer {
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len).unwrap();
        let (got, read) = reading(|| index.get(SYSTEM_GIT, &id));
        assert!(corrupt(&got, &id), "get, {what}: {got:?}");
        assert!(read <= 45, "get read {read} bytes of the file, {what}");
        let (listed, read) = reading(|| index.list(SYSTEM_GIT).unwrap());
        let [(listed_key, listed_entry)] = &listed[..] else {
            panic!("list, {what}: one entry, not {listed:?}");
        };
        assert_eq!(*listed_key, id, "list, {what}");
        assert!(corrupt(listed_entry, &id), "list, {what}: {listed_entry:?}");
        assert!(read <= 45, "list read {read} bytes of the file, {what}");
        let (put, read) = reading(|| index.put_batch(SYSTEM_GIT, &[(id.clone(), named)]));
        assert_eq!(
            put.unwrap(),
            ExternalPut {
                written: 0,
                existing: 0,
                conflicts: vec![id.clone()],
            },
            "put, {what}"
        );
        assert!(read <= 45, "put read {read} bytes of the file, {what}");
        // The file as it was: its length, and the entry it begins with.
        assert_eq!(fs::metadata(&path).unwrap().len(), len, "{what}");
        let mut head = [0; 44];
        File::open(&path).unwrap().read_exact(&mut head).unwrap();
        assert_eq!(head[..], whole[..], "{what}");
    }
}

/// An entry's read that fails is located as a failed `read` of the entry's file, named by its
/// place in the store and never by the store's root, a host path: here a directory in an
/// entry's place, which `get` refuses before it opens it (no store file is one).
#[test]
fn a_failed_entry_read_names_the_file_by_its_place_in_the_store() {
    use std::io;

    use swarmops_store::{FileOp, FsMethod};

    let (_dir, layout) = store_root();
    let index = index_of(&layout);
    let id = key(0xaa, 1);
    fs::create_dir_all(layout.external_id_path(SYSTEM_GIT, &id)).unwrap();
    let got = index.get(SYSTEM_GIT, &id);
    let Err(StoreError::Io(failed)) = &got else {
        panic!("a directory in an entry's place: {got:?}");
    };
    let file = format!("meta/external_ids/git/aa/{}", hex(&id[1..]));
    assert_eq!(
        (failed.op, failed.file.as_deref()),
        (
            FileOp::Fs(FsMethod::Read),
            Some(camino::Utf8Path::new(&file))
        )
    );
    assert_eq!(failed.kind(), io::ErrorKind::InvalidData, "{failed}");
    let message = failed.to_string();
    assert!(!message.contains(layout.root().as_str()), "{message}");
}

/// The listing passes over what is no entry's name, as the object store's passes over strays:
/// a fan-out directory not named by two lowercase hex digits, a file not named by an even
/// count of them (2 to 126), a file in place of a fan-out directory, and a directory of the
/// index not named as a system is.
#[test]
fn list_and_systems_pass_over_strays() {
    let (_dir, layout) = store_root();
    let index = index_of(&layout);
    assert_eq!(index.systems().unwrap(), Vec::<String>::new());
    assert!(index.list(SYSTEM_GIT).unwrap().is_empty());
    let entries = three();
    index.put_batch(SYSTEM_GIT, &entries).unwrap();
    let git = layout.external_ids_dir().join("git");

    // A case-insensitive volume (macOS's APFS by default) folds `AA` onto the entries' `aa` and
    // `Git` onto `git`: such a stray cannot exist there, so it is made only where it can.
    let folds_case = fs::exists(git.join("AA")).unwrap();
    for dir in ["AA", "a", "zz", "aaa"] {
        if folds_case && dir == "AA" {
            continue;
        }
        fs::create_dir(git.join(dir)).unwrap();
        fs::write(git.join(dir).join("0102"), b"stray").unwrap();
    }
    for file in ["0", "ABCD", "abc", "xyz1", ".tmp123456", &"ab".repeat(64)] {
        fs::write(git.join("aa").join(file), b"stray").unwrap();
    }
    fs::write(git.join("cc"), b"a file where a fan-out directory would be").unwrap();
    if !folds_case {
        fs::create_dir(layout.external_ids_dir().join("Git")).unwrap();
    }
    fs::write(layout.external_ids_dir().join("stray"), b"stray").unwrap();
    let listed: Vec<(Vec<u8>, ExternalEntry)> = index
        .list(SYSTEM_GIT)
        .unwrap()
        .into_iter()
        .map(|(key, entry)| (key, entry.unwrap()))
        .collect();
    assert_eq!(listed, entries);
    index
        .put_batch("swarmops-0a1b", &[(key(0x01, 1), entry(1, true))])
        .unwrap();
    assert_eq!(index.systems().unwrap(), ["git", "swarmops-0a1b"]);
}

#[test]
fn a_bad_system_or_key_is_bad_call() {
    let (_dir, layout) = store_root();
    let index = index_of(&layout);
    let good = key(0xaa, 1);
    let long = "g".repeat(65);
    for system in ["Git", "a/b", "", "-git", "git_", ".", long.as_str()] {
        assert!(bad_call(&index.get(system, &good)), "{system:?}");
        let put = index.put_batch(system, &[(good.clone(), entry(1, true))]);
        assert!(bad_call(&put), "{system:?}");
        assert!(bad_call(&index.list(system)), "{system:?}");
    }
    // A key of 2 to 64 bytes: its first byte names the fan-out directory, the rest the file.
    for bad in [Vec::new(), vec![0xaa], vec![0xaa; 65]] {
        assert!(
            bad_call(&index.get(SYSTEM_GIT, &bad)),
            "{} bytes",
            bad.len()
        );
        // Refused before anything of the call is written, the good entry before it included.
        let put = index.put_batch(
            SYSTEM_GIT,
            &[
                (good.clone(), entry(1, true)),
                (bad.clone(), entry(2, true)),
            ],
        );
        assert!(bad_call(&put), "{} bytes", bad.len());
    }
    assert!(!layout.external_ids_dir().exists(), "nothing made");
    // The longest system name, a name that starts with a digit, and the shortest and the
    // longest key are taken.
    let longest = "g".repeat(64);
    for system in [longest.as_str(), "0git"] {
        let edges = [
            (vec![0xaa, 0xbb], entry(1, true)),
            (vec![0xcc; 64], entry(2, true)),
        ];
        assert_eq!(
            index.put_batch(system, &edges).unwrap().written,
            2,
            "{system}"
        );
        for (key, entry) in &edges {
            assert_eq!(index.get(system, key).unwrap(), Some(*entry), "{system}");
        }
    }
}

/// Every file under `dir`, recursively.
#[cfg(feature = "test-hooks")]
fn files_under(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in dir.read_dir_utf8().unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                dirs.push(entry.into_path());
            } else {
                files.push(entry.into_path());
            }
        }
    }
    files
}

/// A put of three entries crashed at each of its calls (a child under `CrashAtFs`, one call at a
/// time): each entry is then whole or absent, and the temp files lie in `objects/tmp/` alone;
/// the same put again completes it.
#[test]
#[cfg(feature = "test-hooks")]
fn external_ids_crashing_at_any_call_leave_whole_entries_or_none() {
    const TEST: &str = "external_ids_crashing_at_any_call_leave_whole_entries_or_none";
    if let Some(root) = std::env::var_os(CRASH_ROOT) {
        let root = Utf8PathBuf::from_path_buf(root.into()).expect("UTF-8 store root");
        let k = std::env::var(CRASH_AT_ENV).unwrap().parse().unwrap();
        let mut index = ExternalIds::new(Layout::new(root), Arc::new(CrashAtFs::new(RealFs, k)));
        index.set_parallelism(1);
        let put = index.put_batch(SYSTEM_GIT, &three());
        panic!("the crash hook did not fire: put_batch returned {put:?}");
    }
    let (_dir, layout) = store_root();
    let counted = Arc::new(CrashAtFs::new(RealFs, 0));
    let mut index = ExternalIds::new(layout, Arc::clone(&counted) as Arc<dyn FsOps>);
    index.set_parallelism(1);
    index.put_batch(SYSTEM_GIT, &three()).unwrap();
    let calls = counted.calls();
    assert!(calls > 3 * 4, "a put of three makes more calls: {calls}");

    for k in 1..=calls {
        let (_dir, layout) = store_root();
        let child = child::run(
            this_test(&test_name!(TEST))
                .env(CRASH_ROOT, layout.root())
                .env(CRASH_AT_ENV, k.to_string()),
        );
        assert_eq!(
            child.status.signal(),
            Some(SIGABRT),
            "k = {k}: {:?}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
        let index = index_of(&layout);
        let mut present = 0;
        for (key, entry) in three() {
            match index.get(SYSTEM_GIT, &key) {
                Ok(Some(got)) => {
                    assert_eq!(got, entry, "k = {k}: {}", hex(&key));
                    present += 1;
                }
                Ok(None) => {}
                Err(e) => panic!("k = {k}: {}: {e}", hex(&key)),
            }
        }
        for file in files_under(layout.root()) {
            let entry = three()
                .iter()
                .any(|(key, _)| file == layout.external_id_path(SYSTEM_GIT, key));
            assert!(
                entry || file.starts_with(layout.objects_tmp()),
                "k = {k}: {file} is neither an entry nor a temp file in objects/tmp/"
            );
        }
        let put = index.put_batch(SYSTEM_GIT, &three()).unwrap();
        assert_eq!(
            (put.written + put.existing, put.existing),
            (3, present),
            "k = {k}"
        );
    }
}
