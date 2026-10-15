//! Decoding arbitrary bytes never panics.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_core::{Changeset, Tree};

fuzz_target!(|data: &[u8]| {
    let _ = Tree::decode(data);
    let _ = Changeset::decode(data);
});
