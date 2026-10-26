//! Reading a FORMAT 2 ref file never panics, and whatever reads is what the bytes say (the M6
//! design §2.5): the file is `<name>\n<64 hex>\n<version>\n`, `name` a FORMAT 2 ref name whose
//! hash is the file's name, `version` a decimal of at least 1. The reader takes the hex in either
//! case and the version as `u64::from_str` does (padding and `+` too), so the lines are compared
//! by meaning, not by bytes; the store writes lowercase and plain decimals.
//!
//! The input is written, in a fresh `tempfile::tempdir()` holding an empty FORMAT 2 store, as
//! the file of two refs: the one named by its first line (the name it claims, so that the name
//! check can pass), and the one named `feature/x`. `MetaStore::ref_files` lists each file under
//! the name it holds, or under its file name if it does not read, with a state, or an error that
//! is `InvalidData`. A reader's `get_ref` agrees with the listing.
#![no_main]

use std::fs;
use std::io;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use swarmops_store::{Layout, MetaStore, StoreError, StoreFormat, ref_kind};
use swarmops_testkit::fs::NoSyncFs;

/// The domain separation of a ref file's name (the M6 design §2.5).
const STEM_DOMAIN: &[u8] = b"swarmops:refname:v1\0";

/// The hex digits of a file name: the first 16 bytes of the hash.
const STEM_BYTES: usize = 16;

/// A name the first line may not claim, so that a case holds a file for a name of its own too.
const FIXED: &str = "feature/x";

fuzz_target!(|data: &[u8]| {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let root = dir.path().to_str().expect("the temp dir path is UTF-8");
    let layout = Layout::with_format(root.to_owned(), StoreFormat::V2);
    let store = MetaStore::create_in(layout.clone(), Arc::new(NoSyncFs)).expect("create a store");
    let refs = layout.refs_dir();
    fs::create_dir_all(&refs).expect("create refs2/");

    let claimed = data
        .split(|b| *b == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .unwrap_or(FIXED);
    for name in [claimed, FIXED] {
        fs::write(refs.join(stem(name)), data).expect("write a ref file");
        check(&store, data);
        fs::remove_file(refs.join(stem(name))).expect("remove the ref file");
    }
});

/// Lists the store's ref files and reads them back, checking the properties.
fn check(store: &MetaStore, data: &[u8]) {
    let files = match store.ref_files() {
        Ok(files) => files,
        Err(e) => {
            assert!(invalid(&e), "ref_files: an untyped error {e:?}");
            return;
        }
    };
    for file in files {
        match file.state {
            Err(e) => assert!(invalid(&e), "{}: an untyped error {e:?}", file.name),
            Ok(state) => {
                assert!(
                    ref_kind(&file.name).is_some(),
                    "{} is no ref name",
                    file.name
                );
                // The file is three lines, as the layout says. The reader takes the hex in
                // either case and the version as `u64::from_str` does (padding and `+` too);
                // the store writes lowercase and plain decimals.
                let text = std::str::from_utf8(data).expect("a file that reads is UTF-8");
                let lines: Vec<&str> = text.split('\n').collect();
                assert!(
                    matches!(lines.as_slice(), [name, hex, version, ""]
                        if *name == file.name
                            && hex.len() == 64
                            && hex.to_ascii_lowercase() == state.target.to_string()
                            && version.parse() == Ok(state.version)),
                    "the file reads as {state:?} but its lines say {lines:?}"
                );
                assert!(state.version >= 1, "a version of 0 reads");
                let read = store.read().expect("read the store");
                assert_eq!(
                    read.get_ref(&file.name)
                        .expect("get a ref the listing read"),
                    Some(state),
                    "get_ref disagrees with the listing"
                );
            }
        }
    }
}

/// Whether `e` is the error an unreadable ref file gives: `Io` with `InvalidData`.
fn invalid(e: &StoreError) -> bool {
    matches!(e, StoreError::Io(e) if e.kind() == io::ErrorKind::InvalidData)
}

/// The file name of ref `name`.
fn stem(name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(STEM_DOMAIN);
    hasher.update(name.as_bytes());
    let hash = hasher.finalize();
    hash.as_bytes()[..STEM_BYTES]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
