//! Storage backend trait.

use std::io;

use crate::ContentHash;

/// Trait for pluggable storage backends.
///
/// MVP: file-per-object. Future: block device, KV engine.
pub trait StorageBackend: Send + Sync {
    /// Store data, returning its content hash.
    fn put(&self, data: &[u8]) -> io::Result<ContentHash>;

    /// Retrieve data by content hash.
    fn get(&self, hash: &ContentHash) -> io::Result<Vec<u8>>;

    /// Check if a content hash exists in the store.
    fn exists(&self, hash: &ContentHash) -> io::Result<bool>;

    /// Delete content by hash. Returns true if it was present.
    fn delete(&self, hash: &ContentHash) -> io::Result<bool>;
}
