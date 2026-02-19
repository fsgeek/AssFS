//! Graph nodes — objects in the namespace.

use serde::{Deserialize, Serialize};

use crate::{ContentHash, NodeType, ObjectId, EMPTY_HASH};

/// Metadata common to all nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetadata {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: i64,
    pub atime_nsec: u32,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub ctime: i64,
    pub ctime_nsec: u32,
    /// Number of graph edges pointing to this node (POSIX nlink).
    pub nlink: u32,
}

impl NodeMetadata {
    pub fn now() -> (i64, u32) {
        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (dur.as_secs() as i64, dur.subsec_nanos())
    }

    pub fn new_dir(uid: u32, gid: u32, mode: u32) -> Self {
        let (sec, nsec) = Self::now();
        Self {
            mode,
            uid,
            gid,
            size: 0,
            atime: sec,
            atime_nsec: nsec,
            mtime: sec,
            mtime_nsec: nsec,
            ctime: sec,
            ctime_nsec: nsec,
            nlink: 2, // . and parent
        }
    }

    pub fn new_file(uid: u32, gid: u32, mode: u32) -> Self {
        let (sec, nsec) = Self::now();
        Self {
            mode,
            uid,
            gid,
            size: 0,
            atime: sec,
            atime_nsec: nsec,
            mtime: sec,
            mtime_nsec: nsec,
            ctime: sec,
            ctime_nsec: nsec,
            nlink: 1,
        }
    }
}

/// A node in the graph namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: ObjectId,
    pub node_type: NodeType,
    pub content_hash: ContentHash,
    pub metadata: NodeMetadata,
}

impl GraphNode {
    pub fn new_dir(id: ObjectId, uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            id,
            node_type: NodeType::Directory,
            content_hash: EMPTY_HASH,
            metadata: NodeMetadata::new_dir(uid, gid, mode),
        }
    }

    pub fn new_file(id: ObjectId, uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            id,
            node_type: NodeType::File,
            content_hash: EMPTY_HASH,
            metadata: NodeMetadata::new_file(uid, gid, mode),
        }
    }

    /// Create a tag anchor node — pure graph node, not in any directory.
    pub fn new_tag_anchor(id: ObjectId) -> Self {
        Self {
            id,
            node_type: NodeType::TagAnchor,
            content_hash: EMPTY_HASH,
            metadata: NodeMetadata::new_file(0, 0, 0),
        }
    }
}
