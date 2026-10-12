//! Content de-duplication.
//!
//! A carver sweeping unallocated space finds copies of files the filesystem
//! parser already recovered (old copies, deleted duplicates, the same photo
//! on two partitions). Files are keyed by their BLAKE3 hash; the first one
//! seen wins and later ones are recorded as duplicates of it.

use std::collections::HashMap;
use std::sync::Mutex;

/// Thread-safe hash index.
#[derive(Debug, Default)]
pub struct Dedup {
    seen: Mutex<HashMap<String, u64>>,
}

impl Dedup {
    /// Empty index.
    pub fn new() -> Self {
        Dedup::default()
    }

    /// Record `hash` for file `id`. Returns the id of an earlier file with
    /// the same content, if any (in which case `id` is not recorded).
    pub fn check_insert(&self, hash: &str, id: u64) -> Option<u64> {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        match seen.get(hash) {
            Some(&first) => Some(first),
            None => {
                seen.insert(hash.to_string(), id);
                None
            }
        }
    }

    /// Whether `hash` has been seen.
    pub fn contains(&self, hash: &str) -> bool {
        self.seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(hash)
    }

    /// Number of distinct contents seen.
    pub fn len(&self) -> usize {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// True when nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_wins() {
        let d = Dedup::new();
        assert_eq!(d.check_insert("aa", 1), None);
        assert_eq!(d.check_insert("bb", 2), None);
        assert_eq!(d.check_insert("aa", 3), Some(1));
        assert!(d.contains("bb"));
        assert_eq!(d.len(), 2);
    }
}
