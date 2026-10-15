//! decode(encode(x)) == x for arbitrary canonical changesets.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_core::Changeset;

fuzz_target!(|cs: Changeset| {
    let bytes = cs.encode().expect("arbitrary changesets are canonical");
    assert_eq!(Changeset::decode(&bytes).expect("canonical bytes decode"), cs);
});
