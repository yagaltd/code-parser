/// blake3 helpers for content hashing.
use blake3::Hasher;

/// Compute blake3 lowercase hex digest of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Hasher::new();
    h.update(bytes);
    h.finalize().to_hex().to_string()
}

/// Compute blake3 lowercase hex digest of a string.
pub fn hash_str(s: &str) -> String {
    hash_bytes(s.as_bytes())
}
