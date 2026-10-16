//! The seeded data every workload writes and reads: changesets with real BLAKE3 ids (as a
//! store's are spread), in chains, the same in every run.

use swarmops_core::{ObjectKind, hash_object};

/// A 32-byte changeset id.
pub type Id = [u8; 32];

/// The chains the workloads draw their changesets from. Each workload writes chains of its
/// own, so that no two workloads' changesets collide in a store they share.
pub mod chain {
    /// The bulk-loaded stores of W3–W6.
    pub const SEED: u64 = 0;
    /// W1's single writer.
    pub const W1: u64 = 1;
    /// W2's shared root changeset.
    pub const W2_ROOT: u64 = 2;
    /// W2's writer `w` writes chain `W2_WRITER + w`.
    pub const W2_WRITER: u64 = 100;
    /// W4's killed writer in trial `t` writes chain `W4_WRITER + t`.
    pub const W4_WRITER: u64 = 1_000;
    /// W4's recovering writer in trial `t` writes chain `W4_RECOVER + t`.
    pub const W4_RECOVER: u64 = 2_000;
    /// W6's writer in run `r` writes chain `W6_WRITER + r`.
    pub const W6_WRITER: u64 = 3_000;
    /// The traced writers of the sync-call count.
    pub const SYNC: u64 = 4_000;
}

/// One changeset as the workloads write it: what SwarmOps' metadata index holds (the store
/// itself numbers it and computes its generation from its parents).
#[derive(Debug, Clone)]
pub struct Changeset {
    pub id: Id,
    pub delta_hash: [u8; 32],
    pub parents: Vec<Id>,
}

/// Changeset `n` of chain `chain`: its parent is changeset `n - 1` of the chain, or `root`
/// (if any) for `n = 0`.
pub fn changeset(chain: u64, n: u64, root: Option<Id>) -> Changeset {
    let parents = match n.checked_sub(1) {
        Some(prev) => vec![id(chain, prev)],
        None => root.into_iter().collect(),
    };
    Changeset {
        id: id(chain, n),
        delta_hash: delta_hash(chain, n),
        parents,
    }
}

/// The id of changeset `n` of chain `chain`.
pub fn id(chain: u64, n: u64) -> Id {
    hash_object(ObjectKind::Changeset, &key(chain, n)).0
}

/// The delta hash of changeset `n` of chain `chain`: one of its own, as a change that was
/// never rebased has.
pub fn delta_hash(chain: u64, n: u64) -> [u8; 32] {
    hash_object(ObjectKind::Blob, &key(chain, n)).0
}

fn key(chain: u64, n: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&chain.to_le_bytes());
    key[8..].copy_from_slice(&n.to_le_bytes());
    key
}

/// SplitMix64's output function: a bijection on `u64` that scatters its input (as the
/// store's benches use it to pick lookup targets).
pub fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The id `hex` spells (64 hex digits).
pub fn parse_id(hex: &str) -> Option<Id> {
    if hex.len() != 64 {
        return None;
    }
    let mut id = [0; 32];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(id)
}
