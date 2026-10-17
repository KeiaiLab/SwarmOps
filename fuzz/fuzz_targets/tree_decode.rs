//! Decoding arbitrary bytes never panics, and whatever decodes re-encodes to exactly the bytes it
//! was decoded from.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_core::{Changeset, Tree};

fuzz_target!(|data: &[u8]| {
    if let Ok(tree) = Tree::decode(data) {
        assert_eq!(
            tree.encode(),
            data,
            "a decoded tree re-encodes to its bytes"
        );
    }
    if let Ok(cs) = Changeset::decode(data) {
        let bytes = cs.encode().expect("a decoded changeset is canonical");
        assert_eq!(bytes, data, "a decoded changeset re-encodes to its bytes");
    }
});
