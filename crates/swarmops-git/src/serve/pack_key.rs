//! The key and the digest of `serve`'s pack cache (the M17 design §5): a BLAKE3 hash of what
//! `upload-pack` asked its `pack-objects` for, so that identical requests share one pack, and a
//! BLAKE3 hash of an entry's pack, which its header carries.
//!
//! ```text
//! BLAKE3("swarmops:serve-pack:v2\0" ‖ part(git identity) ‖ count(args) ‖ part(arg)… ‖ part(stdin))
//!   part(x) = x's length, 8 bytes little-endian ‖ x        ─▶ 64 lowercase hex
//! ```
//!
//! A cache key outside the store's format, never its hash contract: the parts are framed by
//! their lengths, so no two requests share a key, and a change of them is a new domain. Version
//! 1's first part was `git --version`'s line; version 2's names the binary without running it
//! (the M17 closure design §3.2), and version 1's entries leave by the cache's bound.

use std::io::{self, Read};

/// What every pack key's hash starts with: its domain and version.
const DOMAIN: &[u8] = b"swarmops:serve-pack:v2\0";

/// How much of an entry [`PackDigest::of`] reads at once.
const CHUNK: usize = 64 << 10;

/// The cache key of a pack request: `git_identity` (the git binary that packs, as its file is:
/// no two binaries share one), `args` (the line `upload-pack` appended, the bytes of the file
/// `--shallow-file` names in place of its path) and `stdin` (what it fed `pack-objects`).
pub fn pack_key(git_identity: &str, args: &[&[u8]], stdin: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    part(&mut hasher, git_identity.as_bytes());
    hasher.update(&to_u64(args.len()).to_le_bytes());
    for arg in args {
        part(&mut hasher, arg);
    }
    part(&mut hasher, stdin);
    hasher.finalize().to_hex().to_string()
}

/// Adds `bytes` to `hasher`, its length first.
fn part(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&to_u64(bytes.len()).to_le_bytes());
    hasher.update(bytes);
}

/// `n` as a `u64`: a `usize` always fits on the 64-bit hosts SwarmOps runs on.
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The digest of a pack as it is written, a chunk at a time: what an entry's header names.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PackDigest {
    /// The hash so far.
    hasher: blake3::Hasher,
    /// The bytes hashed so far.
    len: u64,
}

impl PackDigest {
    /// The digest of no bytes yet.
    pub fn new() -> PackDigest {
        PackDigest::default()
    }

    /// The digest of everything `reader` holds, and its length.
    pub fn of(reader: &mut impl Read) -> io::Result<(String, u64)> {
        let mut digest = PackDigest::new();
        let mut chunk = vec![0; CHUNK];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => return Ok(digest.finish()),
                Ok(read) => read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            digest.update(&chunk[..read]);
        }
    }

    /// Adds `bytes`, the pack's next.
    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.len += to_u64(bytes.len());
    }

    /// The digest, 64 lowercase hex digits, and the length hashed.
    pub fn finish(&self) -> (String, u64) {
        (self.hasher.finalize().to_hex().to_string(), self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::{DOMAIN, PackDigest, pack_key};

    /// Each part of a request moves the key, and parts framed by their lengths never run into
    /// each other: `["ab", "c"]` is not `["a", "bc"]`.
    #[test]
    fn each_part_moves_the_key() {
        let key = pack_key("git version 2.43.0", &[b"pack-objects", b"--revs"], b"w\n");
        assert_eq!(key.len(), 64);
        assert_eq!(
            key,
            pack_key("git version 2.43.0", &[b"pack-objects", b"--revs"], b"w\n")
        );
        let others = [
            pack_key("git version 2.39.5", &[b"pack-objects", b"--revs"], b"w\n"),
            pack_key("git version 2.43.0", &[b"pack-objects", b"--thin"], b"w\n"),
            pack_key("git version 2.43.0", &[b"pack-objects", b"--revs"], b"v\n"),
            pack_key("git version 2.43.0", &[b"pack-objects"], b"--revs\nw\n"),
        ];
        assert!(others.iter().all(|other| *other != key), "{others:?}");
        assert_ne!(
            pack_key("v", &[b"ab", b"c"], b""),
            pack_key("v", &[b"a", b"bc"], b"")
        );
    }

    /// The recipe is the second one: identities replaced `git --version` in the first part.
    #[test]
    fn the_domain_is_v2() {
        assert_eq!(DOMAIN, b"swarmops:serve-pack:v2\0");
    }

    /// Two git binaries never share a key: their identities differ by path, or by the file a
    /// path names.
    #[test]
    fn two_binaries_get_two_keys() {
        let args: [&[u8]; 2] = [b"pack-objects", b"--revs"];
        let identities = [
            "64769 1201 3456 1700000000000000000 /usr/bin/git",
            "64769 1202 3456 1700000000000000000 /usr/bin/git",
            "64769 1201 3456 1700000000000000000 /opt/git/bin/git",
        ];
        let keys: Vec<String> = (identities.iter())
            .map(|identity| pack_key(identity, &args, b"w\n"))
            .collect();
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[0], keys[2]);
        assert_ne!(keys[1], keys[2]);
    }

    /// The digest of chunks is the digest of their bytes read at once.
    #[test]
    fn a_digest_is_of_the_bytes() {
        let mut digest = PackDigest::new();
        digest.update(b"PACK");
        digest.update(b"\0\0\0\x02");
        let read = PackDigest::of(&mut &b"PACK\0\0\0\x02"[..]).unwrap();
        assert_eq!(digest.finish(), read);
        assert_eq!(read.1, 8);
        assert_eq!(read.0, blake3::hash(b"PACK\0\0\0\x02").to_hex().to_string());
    }
}
