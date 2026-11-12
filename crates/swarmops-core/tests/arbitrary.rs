//! Stable-toolchain equivalent of the fuzz targets: `Arbitrary`-generated values
//! round-trip through the canonical codec. Requires `--features arbitrary`.
#![cfg(feature = "arbitrary")]

use arbitrary::{Arbitrary, Unstructured};
use proptest::prelude::*;
use swarmops_core::{BlobId, Changeset, LeafKind, ObjectId, Tree, TreeEntry};

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

/// A tree entry draws its name, then one of the four kinds, then the id: the order of the
/// `(kind, id)` pair a target replaced, so that existing fuzz inputs keep decoding to the same
/// shapes. Any other order decodes these bytes to another tree.
#[test]
fn a_tree_entry_draws_name_kind_id_in_that_order() {
    // 1. One entry; a name of one byte, `a`; the third kind, a symlink; the id's 32 bytes.
    let id: [u8; 32] = std::array::from_fn(|i| u8::try_from(i + 1).unwrap());
    let mut raw = vec![0, 0, 0, 2];
    raw.extend(id);

    // 2. Exactly that entry, and no byte left over.
    let mut u = Unstructured::new(&raw);
    let tree = Tree::arbitrary(&mut u).unwrap();
    let blob = BlobId::new(ObjectId::from(id));
    let want = TreeEntry::leaf("a", LeafKind::Symlink, blob);
    assert_eq!(tree, Tree::new(vec![want]).unwrap());
    assert!(u.is_empty(), "{} bytes left", u.len());
}
