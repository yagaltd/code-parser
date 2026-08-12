/// In-memory cache of file path → blake3 hash.
///
/// Used to skip re-parsing when file content hasn't changed.
use std::collections::HashMap;

/// Tracks the last-known blake3 hash for each file path.
#[derive(Debug, Default)]
pub struct HashCache {
    map: HashMap<String, String>,
}

impl HashCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Returns `true` if the hash matches the cached value → skip re-parse.
    pub fn is_unchanged(&self, path: &str, hash: &str) -> bool {
        self.map.get(path).map_or(false, |prev| prev == hash)
    }

    /// Update the cache with a new hash. Returns the old hash if it changed.
    pub fn update(&mut self, path: &str, hash: &str) -> Option<String> {
        self.map.insert(path.to_string(), hash.to_string())
    }

    /// Remove a path from the cache (file deleted).
    pub fn remove(&mut self, path: &str) {
        self.map.remove(path);
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_hash_returns_true() {
        let mut cache = HashCache::new();
        cache.update("foo.rs", "abc123");
        assert!(cache.is_unchanged("foo.rs", "abc123"));
    }

    #[test]
    fn changed_hash_returns_false() {
        let mut cache = HashCache::new();
        cache.update("foo.rs", "abc123");
        assert!(!cache.is_unchanged("foo.rs", "def456"));
    }

    #[test]
    fn unknown_path_returns_false() {
        let cache = HashCache::new();
        assert!(!cache.is_unchanged("unknown.rs", "abc123"));
    }

    #[test]
    fn update_returns_old_hash() {
        let mut cache = HashCache::new();
        assert_eq!(cache.update("foo.rs", "abc123"), None);
        assert_eq!(cache.update("foo.rs", "def456"), Some("abc123".to_string()));
    }

    #[test]
    fn remove_clears_entry() {
        let mut cache = HashCache::new();
        cache.update("foo.rs", "abc123");
        assert_eq!(cache.len(), 1);
        cache.remove("foo.rs");
        assert_eq!(cache.len(), 0);
        assert!(!cache.is_unchanged("foo.rs", "abc123"));
    }

    #[test]
    fn clear_removes_all() {
        let mut cache = HashCache::new();
        cache.update("a.rs", "111");
        cache.update("b.rs", "222");
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }
}
