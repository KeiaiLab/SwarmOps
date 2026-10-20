//! The derived-value store ([`DerivedStore`], storage spec §3 and §5): values in immutable,
//! checksummed files under `meta/derived/`, one per slot (name, version, subject, key), written
//! without a sync and read back verified; a file that fails its checks reads as missing and is
//! removed, so that the value is derived again.
//!
//! Every test owns a fresh store root (`tempfile::tempdir()`), so tests are independent and
//! may run in any order or in parallel.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_core::{ChangesetId, ObjectId, ObjectKind, hash_object};
use swarmops_store::{
    ChangesetMeta, DerivedSlot, DerivedStore, FileOp, FsMethod, FsObjectStore, InsertOutcome,
    Layout, MetaStore, ObjectStore, RealFs, StoreError,
};
use swarmops_testkit::faults::checked;
use swarmops_testkit::fs::{Call, FailingFs, RecordingFs, Watch, Watched};
use swarmops_testkit::store::new_root;
use tempfile::TempDir;

/// The fixed part of a value file's header: `"SWDV" | u8 1 | u8 enc | u16le 0 | u32le key_len |
/// u64le value_len` (storage spec §3).
const HEADER_LEN: usize = 20;

/// The trailer at the end of a value file: BLAKE3, 32 bytes.
const TRAILER_LEN: usize = 32;

/// Subject `n`: a real hash, as the ids a store holds look.
fn subject(n: u64) -> ObjectId {
    hash_object(ObjectKind::Changeset, &n.to_le_bytes())
}

/// The slot of derivation `name` at `version`, about subject `n`, under `key`.
fn slot<'a>(name: &'a str, version: u32, n: u64, key: &'a [u8]) -> DerivedSlot<'a> {
    DerivedSlot {
        name,
        version,
        subject: subject(n),
        key,
    }
}

/// `len` bytes of a value that starts at `seed`: a pattern that repeats, so that one over 4 KiB
/// compresses.
fn value(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add(u8::try_from(i % 251).unwrap()))
        .collect()
}

/// A derived store on the real filesystem, at a fresh root that lives as long as the `TempDir`.
fn new_store() -> (TempDir, Layout, DerivedStore) {
    let (dir, layout) = new_root();
    let store = DerivedStore::open(layout.clone(), Arc::new(RealFs));
    (dir, layout, store)
}

/// Where `slot`'s file must be, relative to the store root, worked out here from storage spec
/// §3: `meta/derived/<name>/<version>/<2 hex>/<62 hex>`, the subject's hex digits, then
/// `.<16 hex>`, the first 8 bytes of BLAKE3 of the key, when the key is not empty.
fn slot_file(slot: &DerivedSlot<'_>) -> Utf8PathBuf {
    let hex = slot.subject.to_hex();
    let mut name = hex[2..].to_owned();
    if !slot.key.is_empty() {
        name.push('.');
        for byte in &blake3::hash(slot.key).as_bytes()[..8] {
            write!(name, "{byte:02x}").unwrap();
        }
    }
    Utf8PathBuf::from(format!(
        "meta/derived/{}/{}/{}/{name}",
        slot.name,
        slot.version,
        &hex[..2]
    ))
}

/// The trailer storage spec §3 gives `slot`'s file holding `value`, worked out here:
/// `blake3("swarmops:derived:v1\0" ++ u8 name_len ++ name ++ u32le version ++ subject ++
/// u32le key_len ++ key ++ u64le value_len ++ value)`.
fn trailer(slot: &DerivedSlot<'_>, value: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"swarmops:derived:v1\0");
    hasher.update(&[u8::try_from(slot.name.len()).unwrap()]);
    hasher.update(slot.name.as_bytes());
    hasher.update(&slot.version.to_le_bytes());
    hasher.update(&slot.subject.0);
    hasher.update(&u32::try_from(slot.key.len()).unwrap().to_le_bytes());
    hasher.update(slot.key);
    hasher.update(&u64::try_from(value.len()).unwrap().to_le_bytes());
    hasher.update(value);
    *hasher.finalize().as_bytes()
}

/// Every file under `dir`, `meta/derived/tmp/` aside, relative to `root`, sorted; empty if `dir`
/// does not exist.
fn files_under(root: &Utf8Path, dir: &Path) -> Vec<Utf8PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => panic!("read {}: {e}", dir.display()),
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_none_or(|name| name != "tmp") {
                    dirs.push(path);
                }
            } else {
                let path = Utf8PathBuf::try_from(path).unwrap();
                files.push(path.strip_prefix(root).unwrap().to_owned());
            }
        }
    }
    files.sort();
    files
}

/// The entries of `meta/derived/tmp/`; none if it does not exist.
fn temp_files(layout: &Layout) -> Vec<PathBuf> {
    match fs::read_dir(layout.derived_tmp()) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("read meta/derived/tmp: {e}"),
    }
}

/// A value put in its slot reads back from the file at the slot's path: a short one (10 bytes)
/// and one over 4 KiB (5 KiB). A read of an empty slot writes nothing, and a second put finds the
/// file and leaves it as it is, even with another value.
#[test]
fn a_value_reads_back_from_its_slot() {
    let (_dir, layout, store) = new_store();
    assert_eq!(store.get(&slot("depth", 1, 1, b"")).unwrap(), None);
    assert!(!layout.derived_dir().exists(), "a read makes no directory");
    for (n, len) in [(1, 10), (2, 5 << 10)] {
        let value = value(len, 7);
        let slot = slot("depth", 1, n, b"");
        assert_eq!(store.get(&slot).unwrap(), None, "nothing stored yet");
        assert!(store.put(&slot, &value).unwrap(), "the first put places it");
        let file = layout.root().join(slot_file(&slot));
        assert!(file.is_file(), "{file}");
        assert_eq!(layout.derived_path(&slot), file);
        assert_eq!(store.get(&slot).unwrap().as_deref(), Some(&value[..]));

        assert!(!store.put(&slot, &value).unwrap(), "the slot is taken");
        assert!(
            !store.put(&slot, b"another").unwrap(),
            "values are immutable"
        );
        assert_eq!(store.get(&slot).unwrap(), Some(value));
    }
    assert_eq!(temp_files(&layout), Vec::<PathBuf>::new());
}

/// A slot is all four of its parts: two keys of one subject (one of them empty), a second
/// version and a second name make four files, each with its own value.
#[test]
fn a_slot_is_name_version_subject_and_key() {
    let (_dir, layout, store) = new_store();
    let slots = [
        slot("last_modified", 1, 1, b""),
        slot("last_modified", 1, 1, b"src/lib.rs"),
        slot("last_modified", 2, 1, b"src/lib.rs"),
        slot("skip_list", 1, 1, b"src/lib.rs"),
    ];
    for (seed, slot) in (0..).zip(&slots) {
        assert!(store.put(slot, &value(10, seed)).unwrap(), "{slot:?}");
    }
    for (seed, slot) in (0..).zip(&slots) {
        assert_eq!(store.get(slot).unwrap(), Some(value(10, seed)), "{slot:?}");
    }
    let mut want: Vec<Utf8PathBuf> = slots.iter().map(slot_file).collect();
    want.sort();
    let files = files_under(layout.root(), layout.derived_dir().as_std_path());
    assert_eq!(files, want);
}

/// A damage that a power loss, a torn write or a stray copy can do to a slot's file.
struct Damage {
    /// What it is.
    what: &'static str,
    /// The file it leaves, given the slot's file and the file of another slot.
    apply: fn(file: &[u8], other: &[u8]) -> Vec<u8>,
}

/// Each damage: cut short by a byte, zero-filled, a byte of the stored value flipped, a byte of the
/// trailer flipped, the file of another slot copied over it.
const DAMAGES: [Damage; 5] = [
    Damage {
        what: "cut short by a byte",
        apply: |file, _| file[..file.len() - 1].to_vec(),
    },
    Damage {
        what: "zero-filled",
        apply: |file, _| vec![0; file.len()],
    },
    Damage {
        what: "a byte of the stored value flipped",
        apply: |file, _| {
            let mut file = file.to_vec();
            // The stored value lies between the header with the key ("key", 3 bytes) and the
            // trailer.
            let start = HEADER_LEN + 3;
            let middle = start + (file.len() - TRAILER_LEN - start) / 2;
            file[middle] ^= 0x01;
            file
        },
    },
    Damage {
        what: "a byte of the trailer flipped",
        apply: |file, _| {
            let mut file = file.to_vec();
            *file.last_mut().unwrap() ^= 0x80;
            file
        },
    },
    Damage {
        what: "another slot's file copied over it",
        apply: |_, other| other.to_vec(),
    },
];

/// Each damage, to a value stored raw (10 bytes) and one stored with zstd (5 KiB): the slot reads
/// as missing, its file is removed, and a new put stores the value again. The other slot's file
/// is that of the same name, version, key and value, about another subject: only the trailer, over
/// the slot and the value, tells the two apart.
#[test]
fn a_damaged_value_file_reads_as_missing_and_is_removed() {
    let (_dir, layout, store) = new_store();
    for (n, len) in [(1, 10), (3, 5 << 10)] {
        let value = value(len, 3);
        let slot = slot("depth", 1, n, b"key");
        let other = DerivedSlot {
            subject: subject(n + 1),
            ..slot
        };
        let file = layout.derived_path(&slot);
        assert!(store.put(&other, &value).unwrap());
        let other_file = fs::read(layout.derived_path(&other)).unwrap();
        for damage in &DAMAGES {
            assert!(
                store.put(&slot, &value).unwrap(),
                "{len} B, {}",
                damage.what
            );
            let damaged = (damage.apply)(&fs::read(&file).unwrap(), &other_file);
            fs::write(&file, damaged).unwrap();

            let what = format!("{len} B, {}", damage.what);
            assert_eq!(store.get(&slot).unwrap(), None, "{what}");
            assert!(!file.exists(), "{what}: the file is removed");
            assert!(store.put(&slot, &value).unwrap(), "{what}: put again");
            assert_eq!(store.get(&slot).unwrap().as_deref(), Some(&value[..]));
            fs::remove_file(&file).unwrap();
        }
        assert_eq!(store.get(&other).unwrap(), Some(value), "untouched");
    }
}

/// A zstd value file whose header records one byte more than an object holds
/// (`MAX_OBJECT_LEN + 1`, 512 MiB + 1), its frame honest about that length (zstd's own encoder,
/// fed zeros, records it: about 16 KiB on disk) and its trailer right for those zeros, is refused
/// before its frame is decoded: the slot reads as missing and its file is removed, as any value
/// file that does not check out is. Nothing the size of the value is allocated: the process's
/// peak resident memory (`VmHWM`, Linux; nextest runs each test in a process of its own) grows by
/// less than half the claimed length, where decoding it would write all of it.
#[test]
fn a_zstd_value_past_the_object_limit_is_refused_before_it_is_decoded() {
    use std::io::Write as _;

    use swarmops_store::MAX_OBJECT_LEN;

    let (_dir, layout, store) = new_store();
    let slot = slot("depth", 1, 1, b"");
    let len = MAX_OBJECT_LEN + 1;
    let zeros = vec![0_u8; 1 << 20];
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.set_pledged_src_size(Some(len)).unwrap();
    encoder.include_contentsize(true).unwrap();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"swarmops:derived:v1\0");
    hasher.update(&[u8::try_from(slot.name.len()).unwrap()]);
    hasher.update(slot.name.as_bytes());
    hasher.update(&slot.version.to_le_bytes());
    hasher.update(&slot.subject.0);
    hasher.update(&0_u32.to_le_bytes());
    hasher.update(&len.to_le_bytes());
    let mut left = len;
    while left > 0 {
        let n = usize::try_from(left.min(1 << 20)).unwrap();
        encoder.write_all(&zeros[..n]).unwrap();
        hasher.update(&zeros[..n]);
        left -= u64::try_from(n).unwrap();
    }
    let frame = encoder.finish().unwrap();
    assert!(frame.len() < 64 << 10, "{} bytes of zstd", frame.len());

    let mut file = b"SWDV".to_vec();
    file.extend([1, 1, 0, 0]);
    file.extend(0_u32.to_le_bytes());
    file.extend(len.to_le_bytes());
    file.extend(&frame);
    file.extend(hasher.finalize().as_bytes());
    let path = layout.derived_path(&slot);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &file).unwrap();

    let before = peak_resident_kib();
    let got = store.get(&slot).unwrap().map(|value| value.len());
    let grew = peak_resident_kib().saturating_sub(before);
    assert_eq!(
        got, None,
        "a value past the object limit reads as missing (the read grew the peak resident memory \
         by {grew} KiB)"
    );
    assert!(!path.exists(), "and its file is removed");
    assert!(
        grew < len / 2 / 1024,
        "the read grew the peak resident memory by {grew} KiB"
    );
}

/// A put of a value longer than an object holds (`MAX_OBJECT_LEN + 1` zeros, whose pages stay
/// untouched until something reads them) is a call the store does not take: `BadCall`, before
/// any filesystem call, since no read would take such a value back.
#[test]
fn a_put_of_a_value_past_the_object_limit_is_refused() {
    use swarmops_store::MAX_OBJECT_LEN;

    let (_dir, layout) = new_root();
    let (fs, calls) = RecordingFs::new();
    let store = DerivedStore::open(layout.clone(), Arc::new(fs));
    let past = vec![0_u8; usize::try_from(MAX_OBJECT_LEN + 1).unwrap()];
    let got = store.put(&slot("depth", 1, 1, b""), &past);
    assert!(matches!(got, Err(StoreError::BadCall(_))), "{got:?}");
    assert_eq!(calls(), Vec::<String>::new(), "no filesystem call");
    assert!(!layout.derived_dir().exists(), "nothing written");
}

/// The process's peak resident memory, in KiB: `VmHWM` of `/proc/self/status` on Linux; 0
/// elsewhere, where the check that uses it checks nothing.
fn peak_resident_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|kib| kib.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Holds each `link_noclobber` until all the racers have come to theirs, so that each has written
/// its temp file before any links one into place.
struct LinkTogether(Barrier);

impl Watch for LinkTogether {
    fn before(&self, call: &Call<'_>) -> io::Result<()> {
        if call.method == FsMethod::LinkNoclobber {
            self.0.wait();
        }
        Ok(())
    }
}

/// Eight puts of one slot at once, each past its `exists` and with its temp file written before
/// any links: exactly one places the file, the others find it there, and no temp file is left.
#[test]
fn racing_puts_place_one_file() {
    const RACERS: usize = 8;
    let (_dir, layout) = new_root();
    let fs = Watched(LinkTogether(Barrier::new(RACERS)), RealFs);
    let store = DerivedStore::open(layout.clone(), Arc::new(fs));
    let slot = slot("depth", 1, 1, b"");
    let value = value(5 << 10, 9);
    let placed: Vec<bool> = thread::scope(|scope| {
        let racers: Vec<_> = (0..RACERS)
            .map(|_| scope.spawn(|| store.put(&slot, &value).unwrap()))
            .collect();
        racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .collect()
    });
    assert_eq!(
        placed.iter().filter(|&&placed| placed).count(),
        1,
        "{placed:?}"
    );
    let files = files_under(layout.root(), layout.derived_dir().as_std_path());
    assert_eq!(files, [slot_file(&slot)]);
    assert_eq!(
        temp_files(&layout),
        Vec::<PathBuf>::new(),
        "meta/derived/tmp/"
    );
    assert_eq!(store.get(&slot).unwrap(), Some(value));
}

/// A put writes its file without a single sync (storage spec §3: a cache that nothing durable
/// names): an `exists` of the slot, the directories it needs, one `append` of a new temp file in
/// `meta/derived/tmp/`, named `<pid>-<counter>-<16 hex>`, and a `link_noclobber` into the slot;
/// none of `sync_data`, `fsync_dir`, `write_durable_temp` or `append_durable`, nor any other call
/// that flushes. Opening the store makes no call; a put that finds the file makes only its
/// `exists`, and a read only its `read`.
#[test]
fn a_put_never_syncs() {
    let (_dir, layout) = new_root();
    let (fs, calls) = RecordingFs::new();
    let fs = fs.naming_paths_in(layout.root().as_std_path().to_path_buf());
    let store = DerivedStore::open(layout.clone(), Arc::new(fs));
    assert_eq!(calls(), Vec::<String>::new(), "opening touches nothing");
    let flushing: BTreeSet<&str> = FsMethod::ALL
        .into_iter()
        .filter(|method| method.flushes())
        .map(FsMethod::as_str)
        .collect();
    for (n, len) in [(1, 10), (2, 5 << 10)] {
        let slot = slot("depth", 1, n, b"key");
        let value = value(len, 1);
        let file = slot_file(&slot);
        assert!(store.put(&slot, &value).unwrap());
        let put = calls();
        let method = |call: &String| call.split(' ').next().unwrap().to_owned();
        assert_eq!(put.first(), Some(&format!("exists {file}")), "{put:?}");
        assert_eq!(
            put.last(),
            Some(&format!("link_noclobber {file}")),
            "{put:?}"
        );
        let methods: BTreeSet<String> = put.iter().map(method).collect();
        let want = ["append", "create_dir_all", "exists", "link_noclobber"];
        assert_eq!(methods, want.map(String::from).into(), "{put:?}");
        assert!(
            put.iter()
                .all(|call| !flushing.contains(method(call).as_str())),
            "{put:?}"
        );
        let appends: Vec<usize> = (0..put.len())
            .filter(|&i| put[i].starts_with("append "))
            .collect();
        assert_eq!(appends.len(), 1, "one append: {put:?}");
        let made: Vec<usize> = (0..put.len())
            .filter(|&i| put[i].starts_with("create_dir_all "))
            .collect();
        assert!(made.iter().all(|&i| i < appends[0]), "{put:?}");
        let temp = put[appends[0]]
            .strip_prefix("append meta/derived/tmp/")
            .unwrap_or_else(|| panic!("a temp file in meta/derived/tmp/: {put:?}"));
        let parts: Vec<&str> = temp.split('-').collect();
        assert_eq!(parts.len(), 3, "<pid>-<counter>-<16 hex>: {temp}");
        assert_eq!(parts[0], std::process::id().to_string(), "{temp}");
        assert!(parts[1].parse::<u64>().is_ok(), "{temp}");
        assert!(
            parts[2].len() == 16 && parts[2].bytes().all(|b| b.is_ascii_hexdigit()),
            "{temp}"
        );

        assert!(!store.put(&slot, &value).unwrap());
        assert_eq!(calls(), [format!("exists {file}")]);
        assert_eq!(store.get(&slot).unwrap(), Some(value));
        assert_eq!(calls(), [format!("read {file}")]);
    }
}

/// A slot whose name is not 1 to 64 bytes of `[a-z0-9_]`, is `tmp` (the temp files' directory),
/// or whose version is 0, is a call the store does not take: `BadCall` from a put and from a get,
/// before any filesystem call. The bounds themselves are taken.
#[test]
fn a_bad_slot_name_is_refused() {
    let (_dir, layout) = new_root();
    let (fs, calls) = RecordingFs::new();
    let store = DerivedStore::open(layout.clone(), Arc::new(fs));
    let long = "a".repeat(65);
    let bad = [
        ("", 1),
        ("A", 1),
        ("a-b", 1),
        ("a/b", 1),
        ("..", 1),
        ("tmp", 1),
        (long.as_str(), 1),
        ("depth", 0),
    ];
    for (name, version) in bad {
        let slot = slot(name, version, 1, b"");
        let refused = |result: Result<_, StoreError>| matches!(result, Err(StoreError::BadCall(_)));
        assert!(
            refused(store.put(&slot, b"value").map(drop)),
            "put {name:?} {version}"
        );
        assert!(
            refused(store.get(&slot).map(drop)),
            "get {name:?} {version}"
        );
    }
    assert_eq!(
        calls(),
        Vec::<String>::new(),
        "a refused slot touches nothing"
    );
    assert!(!layout.derived_dir().exists(), "nothing written");

    let longest = "z".repeat(64);
    for name in [
        longest.as_str(),
        "abcdefghijklmnopqrstuvwxyz_0123456789",
        "tmp_",
    ] {
        let slot = slot(name, u32::MAX, 1, b"");
        assert!(store.put(&slot, b"value").unwrap(), "{name}");
        assert_eq!(store.get(&slot).unwrap().as_deref(), Some(&b"value"[..]));
    }
}

/// A put or a read that fails names the derived file, relative to the store's root, as every
/// store failure does: the slot's file for its link and its read, the temp file for its append.
/// A failed put leaves neither a file in the slot nor its temp file. A damaged file whose removal
/// fails reads as missing all the same, never as an error.
#[test]
fn a_failure_names_the_derived_file() {
    let (_dir, layout) = new_root();
    let slot = slot("depth", 1, 1, b"key");
    let file = slot_file(&slot);
    let failing = |fails: FsMethod| {
        let fs = FailingFs::new(move |method, _| method == fails);
        DerivedStore::open(layout.clone(), Arc::new(fs))
    };
    let failure = |result: Result<bool, StoreError>| match result {
        Err(StoreError::Io(failure)) => failure,
        other => panic!("{other:?}"),
    };

    let link = failure(failing(FsMethod::LinkNoclobber).put(&slot, b"value"));
    assert_eq!(link.op, FileOp::Fs(FsMethod::LinkNoclobber));
    assert_eq!(link.file.as_ref(), Some(&file));
    let append = failure(failing(FsMethod::Append).put(&slot, b"value"));
    assert_eq!(append.op, FileOp::Fs(FsMethod::Append));
    let temp = append.file.unwrap();
    assert!(temp.starts_with("meta/derived/tmp"), "{temp}");
    assert!(!layout.derived_path(&slot).exists(), "no file in the slot");
    assert_eq!(
        temp_files(&layout),
        Vec::<PathBuf>::new(),
        "no temp file left"
    );

    let store = DerivedStore::open(layout.clone(), Arc::new(RealFs));
    assert!(store.put(&slot, b"value").unwrap());
    match failing(FsMethod::Read).get(&slot) {
        Err(StoreError::Io(read)) => {
            assert_eq!(read.op, FileOp::Fs(FsMethod::Read));
            assert_eq!(read.file.as_ref(), Some(&file));
        }
        other => panic!("{other:?}"),
    }

    fs::write(layout.derived_path(&slot), b"torn").unwrap();
    let cannot_remove = failing(FsMethod::Remove);
    assert_eq!(cannot_remove.get(&slot).unwrap(), None);
    assert!(layout.derived_path(&slot).exists(), "the removal failed");
    assert_eq!(cannot_remove.get(&slot).unwrap(), None, "and again");
}

/// The bytes of a value file are storage spec §3's layout: `"SWDV" | u8 1 | u8 enc | u16le 0 |
/// u32le key_len | u64le value_len | key | payload | trailer`, the payload the value itself up to
/// 4 KiB (`enc` 0) and its zstd frame over that (`enc` 1), the trailer BLAKE3 over the domain,
/// the slot and the value before compression ([`trailer`]).
#[test]
fn a_value_file_holds_the_documented_layout() {
    let (_dir, layout, store) = new_store();
    let key = b"src/lib.rs";
    for (n, len, enc) in [
        (1, 10, 0),
        (2, 4 << 10, 0),
        (3, (4 << 10) + 1, 1),
        (4, 5 << 10, 1),
    ] {
        let value = value(len, 5);
        let slot = slot("last_modified", 3, n, key);
        assert!(store.put(&slot, &value).unwrap());
        let file = fs::read(layout.root().join(slot_file(&slot))).unwrap();
        let what = format!("a value of {len} bytes");
        assert_eq!(file[..4], *b"SWDV", "{what}");
        assert_eq!(file[4], 1, "{what}: the layout's version");
        assert_eq!(file[5], enc, "{what}: the encoding");
        assert_eq!(file[6..8], [0, 0], "{what}");
        assert_eq!(
            file[8..12],
            10_u32.to_le_bytes(),
            "{what}: the key's length"
        );
        let len_le = u64::try_from(len).unwrap().to_le_bytes();
        assert_eq!(file[12..HEADER_LEN], len_le, "{what}: the value's length");
        assert_eq!(file[HEADER_LEN..HEADER_LEN + key.len()], *key, "{what}");
        let (payload, end) = file[HEADER_LEN + key.len()..].split_at(file.len() - 62);
        let stored = match enc {
            0 => payload.to_vec(),
            _ => zstd::bulk::decompress(payload, len).unwrap(),
        };
        assert_eq!(stored, value, "{what}");
        assert_eq!(end, trailer(&slot, &value), "{what}: the trailer");
    }
}

/// Derived values need no write-ordering rule, since nothing durable names them, and their files
/// and names, never synced, break none of the rules of the store around them: under the order
/// checker, values are put, read and found damaged among the object writes and the metadata
/// transactions of a store that commits changesets, moves `main` and moves its checkpoint.
#[test]
fn derived_writes_pass_the_order_checker() {
    let (_dir, layout) = new_root();
    let fs = checked(RealFs);
    let meta = MetaStore::create_with_fs(layout.root(), Arc::clone(&fs)).unwrap();
    let objects = FsObjectStore::open_shared(layout.clone(), Arc::clone(&fs));
    let derived = DerivedStore::open(layout.clone(), Arc::clone(&fs));
    let mut parent: Option<ChangesetId> = None;
    for n in 0..2_u8 {
        let raw = slot("depth", 1, n.into(), b"");
        let zstd = slot("depth", 1, n.into(), b"key");
        assert!(derived.put(&raw, &value(10, n)).unwrap());
        let blob = objects.put(ObjectKind::Blob, &[n]).unwrap();
        assert!(derived.put(&zstd, &value(5 << 10, n)).unwrap());
        objects.flush().unwrap();

        let id = ChangesetId(hash_object(ObjectKind::Changeset, blob.0.as_slice()));
        let mut write = meta.write().unwrap();
        let changeset = ChangesetMeta {
            id,
            seq: 0,
            generation: 0,
            delta_hash: blob.0,
            parents: parent.into_iter().collect(),
        };
        assert!(matches!(
            write.insert_changeset(changeset).unwrap(),
            InsertOutcome::Inserted { .. }
        ));
        write.cas_ref("main", parent, Some(id), "test/1").unwrap();
        assert_eq!(derived.get(&raw).unwrap(), Some(value(10, n)));
        write.commit().unwrap();
        parent = Some(id);

        fs::write(layout.derived_path(&zstd), b"torn").unwrap();
        assert_eq!(derived.get(&zstd).unwrap(), None);
        assert!(derived.put(&zstd, &value(5 << 10, n)).unwrap());
    }
    meta.flush().unwrap();
    let mut want: Vec<Utf8PathBuf> = (0..2_u8)
        .flat_map(|n| {
            [
                slot("depth", 1, n.into(), b""),
                slot("depth", 1, n.into(), b"key"),
            ]
        })
        .map(|slot| slot_file(&slot))
        .collect();
    want.sort();
    let files = files_under(layout.root(), layout.derived_dir().as_std_path());
    assert_eq!(files, want);
}
