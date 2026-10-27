//! The key that authenticates export oid memos (storage spec §5, §7): 32 bytes kept outside the
//! store, so that whoever can write the store but not the key cannot place a memo the export or
//! the import takes as true.
//!
//! A memo under a key is the value and a MAC over it: `blake3::keyed_hash(key, name ‖ 0x00 ‖
//! subject ‖ value)`, the derivation's name, the object the value is about and the value without
//! the MAC. Its slot's key is the key's id ([`MemoKey::id`]), so a memo made under another key is
//! never read under this one: it is missing, not wrong.

use std::fmt::{self, Write as _};

use swarmops_core::ObjectId;

/// The length of a key, in bytes.
pub const MEMO_KEY_LEN: usize = 32;

/// The length of a key's id, in bytes: the slot key of the memos made under it.
pub const MEMO_KEY_ID_LEN: usize = 16;

/// The length of a memo's MAC, in bytes.
pub(crate) const MAC_LEN: usize = 32;

/// A key that authenticates export oid memos: see [the module documentation](self). Its
/// [`Debug`](fmt::Debug) shows the key's id, never the key.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoKey([u8; MEMO_KEY_LEN]);

impl MemoKey {
    /// The key whose bytes are `bytes`.
    pub fn from_bytes(bytes: [u8; MEMO_KEY_LEN]) -> MemoKey {
        MemoKey(bytes)
    }

    /// The key's id: the first 16 bytes of BLAKE3 of the key. The slot key of every memo made
    /// under it.
    pub fn id(&self) -> [u8; MEMO_KEY_ID_LEN] {
        let hash = blake3::hash(&self.0);
        let (id, _) = (hash.as_bytes())
            .split_first_chunk::<MEMO_KEY_ID_LEN>()
            .expect("a BLAKE3 hash has 32 bytes");
        *id
    }

    /// The MAC of `value`, the memo `name` of `subject` without its MAC.
    pub(crate) fn mac(&self, name: &str, subject: ObjectId, value: &[u8]) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(&subject.0);
        hasher.update(value);
        hasher.finalize()
    }

    /// Whether `mac` is the MAC of `value`, the memo `name` of `subject`: compared in constant
    /// time.
    pub(crate) fn checks(&self, name: &str, subject: ObjectId, value: &[u8], mac: &[u8]) -> bool {
        let Ok(mac) = <[u8; MAC_LEN]>::try_from(mac) else {
            return false;
        };
        self.mac(name, subject, value) == blake3::Hash::from_bytes(mac)
    }
}

impl fmt::Debug for MemoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id = self.id().iter().fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
        f.debug_struct("MemoKey").field("id", &id).finish()
    }
}
