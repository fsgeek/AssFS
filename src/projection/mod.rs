//! Hierarchical projection — maps graph namespace to POSIX tree.
//!
//! A "projection" is a spanning tree extracted from the graph, defined by
//! a root node and eligible edge types. Multiple projections = multiple
//! views of the same data (deferred to Phase 3).
//!
//! Manages inode number assignment: synthetic, per-mount. UUID ↔ inode
//! mappings assigned on first reference, stable for mount lifetime.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ObjectId;

/// Inode number type (matches FUSE).
pub type Ino = u64;

/// Root inode (FUSE convention).
pub const ROOT_INO: Ino = 1;

/// The inode table: bidirectional mapping between ObjectId and inode number.
pub struct InodeTable {
    inner: Mutex<InodeTableInner>,
}

struct InodeTableInner {
    next_ino: Ino,
    id_to_ino: HashMap<ObjectId, Ino>,
    ino_to_id: HashMap<Ino, ObjectId>,
}

impl InodeTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(InodeTableInner {
                next_ino: ROOT_INO + 1, // 1 is reserved for root
                id_to_ino: HashMap::new(),
                ino_to_id: HashMap::new(),
            }),
        }
    }

    /// Set the root node's inode mapping.
    pub fn set_root(&self, root_id: ObjectId) {
        let mut inner = self.inner.lock().unwrap();
        inner.id_to_ino.insert(root_id, ROOT_INO);
        inner.ino_to_id.insert(ROOT_INO, root_id);
    }

    /// Get or assign an inode number for an ObjectId.
    pub fn get_or_assign(&self, id: ObjectId) -> Ino {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&ino) = inner.id_to_ino.get(&id) {
            return ino;
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.id_to_ino.insert(id, ino);
        inner.ino_to_id.insert(ino, id);
        ino
    }

    /// Look up ObjectId by inode number.
    pub fn lookup_ino(&self, ino: Ino) -> Option<ObjectId> {
        let inner = self.inner.lock().unwrap();
        inner.ino_to_id.get(&ino).copied()
    }

    /// Look up inode number by ObjectId.
    pub fn lookup_id(&self, id: &ObjectId) -> Option<Ino> {
        let inner = self.inner.lock().unwrap();
        inner.id_to_ino.get(id).copied()
    }

    /// Remove a mapping (when node is deleted).
    pub fn remove_id(&self, id: &ObjectId) -> Option<Ino> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ino) = inner.id_to_ino.remove(id) {
            inner.ino_to_id.remove(&ino);
            Some(ino)
        } else {
            None
        }
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}
