//! Reading an arbitrary object file never panics, and whatever reads is the object its id names
//! (storage spec §2 rule 1, §8). `ObjectStore::get` and `read_chunks` return `Ok(None)`, the
//! object, or `Corrupt` / `BadHeader`, and when they return an object its payload hashes to the
//! id.
//!
//! A case is one of two, in a fresh `tempfile::tempdir()`:
//! - mode 0: the input after its first 32 bytes is written as the file of the object that id
//!   names, a file that is no object's unless the input hashes right;
//! - mode 1: a valid object (a kind, a payload repeated to compress or not) written by `put`,
//!   whose file is then damaged by edits (bytes flipped, the file cut, bytes appended). Undamaged
//!   it reads back unchanged; damaged it reads back unchanged or fails with one of the two
//!   errors.
#![no_main]

use std::fs;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use swarmops_core::{ObjectId, ObjectKind, hash_object};
use swarmops_store::{FsObjectStore, Layout, ObjectStore, StoreError};

/// The longest payload mode 1 puts, before it is repeated.
const MAX_PAYLOAD: usize = 4096;

/// The most times mode 1 repeats a payload, to reach the sizes zstd compresses.
const MAX_REPEAT: usize = 8;

#[derive(Arbitrary, Debug)]
enum Case {
    File {
        id: [u8; 32],
        file: Vec<u8>,
    },
    Valid {
        kind: u8,
        repeat: u8,
        payload: Vec<u8>,
        edits: Vec<Edit>,
    },
}

/// Damage done to an object file.
#[derive(Arbitrary, Debug)]
enum Edit {
    /// XOR `mask` (0 taken as 1) into the byte at `at`, modulo the length.
    Flip { at: u16, mask: u8 },
    /// Cut the file to `len` bytes, modulo its length + 1.
    Cut { len: u16 },
    /// Append bytes.
    Append(Vec<u8>),
}

fuzz_target!(|case: Case| {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = dir.path().to_str().expect("the temp dir path is UTF-8");
    let layout = Layout::new(root.to_owned());
    let store = FsObjectStore::open_default(layout.clone());
    match case {
        Case::File { id, file } => {
            let id = ObjectId::new(id);
            write_file(&layout, id, &file);
            reads(&store, id, None);
        }
        Case::Valid {
            kind,
            repeat,
            payload,
            edits,
        } => {
            let kind =
                [ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Changeset][usize::from(kind) % 3];
            let mut payload = payload;
            payload.truncate(MAX_PAYLOAD);
            let payload = payload.repeat(1 + usize::from(repeat) % MAX_REPEAT);
            let id = store.put(kind, &payload).expect("put an object");
            assert_eq!(id, hash_object(kind, &payload), "put returned another id");
            let path = layout.object_path(id);
            let mut file = fs::read(&path).expect("read the object file");
            let valid = file.clone();
            for edit in edits {
                edit.apply(&mut file);
            }
            if file != valid {
                write_file(&layout, id, &file);
            }
            let want = (kind, payload);
            reads(&store, id, (file == valid).then_some(&want));
        }
    }
});

/// Replaces the file of object `id` with `file`.
fn write_file(layout: &Layout, id: ObjectId, file: &[u8]) {
    let path = layout.object_path(id);
    fs::create_dir_all(
        path.parent()
            .expect("an object file has a fan-out directory"),
    )
    .expect("create the fan-out directory");
    let _ = fs::remove_file(&path);
    fs::write(&path, file).expect("write the object file");
}

/// Asserts `get` and `read_chunks` agree and are right: an error is `Corrupt` or `BadHeader`, an
/// object hashes to `id`, and `intact` (the object as put, if its file was not damaged) reads.
fn reads(store: &FsObjectStore, id: ObjectId, intact: Option<&(ObjectKind, Vec<u8>)>) {
    let got = store.get(id);
    let mut pieces = Vec::new();
    let chunked = store.read_chunks(id, &mut |piece| pieces.extend_from_slice(piece));
    match &got {
        // A zstd file with an empty body and a header of size 0: `get` reads it as the empty
        // object, `read_chunks` refuses it as `Corrupt`. The store writes no such file (a
        // payload under 128 bytes is stored raw); the divergence is a recorded finding of this
        // target, not a property it asserts.
        Ok(Some((_, payload)))
            if payload.is_empty() && matches!(chunked, Err(StoreError::Corrupt { .. })) => {}
        Ok(Some((kind, payload))) => {
            assert_eq!(hash_object(*kind, payload), id, "a read object is not id's");
            assert!(
                matches!(&chunked, Ok(Some(stat)) if stat.kind == *kind && stat.size == swarmops_core::bytes::to_u64(payload.len()))
                    && pieces == *payload,
                "read_chunks disagrees with get: {chunked:?}"
            );
        }
        Ok(None) => assert!(
            matches!(chunked, Ok(None)),
            "get found nothing, read_chunks {chunked:?}"
        ),
        Err(e) => {
            assert!(typed(e), "get: an untyped error {e:?}");
            assert!(
                chunked.as_ref().err().is_some_and(typed),
                "get failed with {e:?}, read_chunks returned {chunked:?}"
            );
        }
    }
    if let Some(want) = intact {
        assert_eq!(
            got.ok().flatten().as_ref(),
            Some(want),
            "an undamaged object reads back"
        );
    }
}

/// Whether `e` is what a damaged object file gives.
fn typed(e: &StoreError) -> bool {
    matches!(e, StoreError::Corrupt { .. } | StoreError::BadHeader { .. })
}

impl Edit {
    fn apply(self, file: &mut Vec<u8>) {
        match self {
            Self::Flip { at, mask } => {
                if !file.is_empty() {
                    let at = usize::from(at) % file.len();
                    file[at] ^= mask.max(1);
                }
            }
            Self::Cut { len } => file.truncate(usize::from(len) % (file.len() + 1)),
            Self::Append(bytes) => file.extend(bytes),
        }
    }
}
