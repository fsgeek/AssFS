pub mod fuse_ops;
pub mod graph;
pub mod projection;
pub mod refcount;
pub mod stats;
pub mod store;
pub mod txn;

use serde::{Deserialize, Serialize};

/// BLAKE3 content hash — 32 bytes.
pub type ContentHash = [u8; 32];

/// Stable object identity.
pub type ObjectId = uuid::Uuid;

/// Log sequence number — monotonically increasing position in the log.
pub type LogSequenceNumber = u64;

/// Transaction identifier.
pub type TransactionId = u64;

/// Sentinel for "no content" (empty file, directory node, etc.)
pub const EMPTY_HASH: ContentHash = [0u8; 32];

/// Compute the BLAKE3 hash of data.
pub fn content_hash(data: &[u8]) -> ContentHash {
    *blake3::hash(data).as_bytes()
}

/// Format a ContentHash as a hex string (for file names, display, etc.)
pub fn hash_hex(hash: &ContentHash) -> String {
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Node types in the graph namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeType {
    File,
    Directory,
    Symlink,
    /// A tag anchor — pure graph node, not visible in the POSIX tree.
    TagAnchor,
}

/// Edge types in the graph namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EdgeType {
    /// Parent → child (directory entry)
    Child,
    /// Symbolic link
    Symlink,
    /// Hard link (additional name for same object)
    Hardlink,
    /// Tag association (file → tag anchor, or registry → tag anchor)
    Tag,
}
