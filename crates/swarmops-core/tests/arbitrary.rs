//! Stable-toolchain equivalent of the fuzz targets: `Arbitrary`-generated values
//! round-trip through the canonical codec. Requires `--features arbitrary`.
#![cfg(feature = "arbitrary")]

use arbitrary::{Arbitrary, Unstructured};
use proptest::prelude::*;
use swarmops_core::{Changeset, Tree};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn arbitrary_changeset_roundtrips(raw in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut u = Unstructured::new(&raw);
        if let Ok(cs) = Changeset::arbitrary(&mut u) {
            let bytes = cs.encode().unwrap();
            prop_assert_eq!(Changeset::decode(&bytes).unwrap(), cs);
        }
    }

    #[test]
    fn arbitrary_tree_roundtrips(raw in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut u = Unstructured::new(&raw);
        if let Ok(t) = Tree::arbitrary(&mut u) {
            prop_assert_eq!(Tree::decode(&t.encode()).unwrap(), t);
        }
    }
}
