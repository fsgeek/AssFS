//! Content store — manages content-addressed object storage.
//!
//! Objects are identified by UUID (stable mutable identity) but their
//! content is stored by BLAKE3 hash (immutable content addressing).
//! The object_index maps ObjectId → ContentHash (the mutable binding).
//! The content blobs are stored in the backend by hash (immutable).

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::{ContentHash, ObjectId, EMPTY_HASH};

use super::backend::StorageBackend;
use super::file_backend::FileBackend;

/// Persistent mapping: ObjectId → ContentHash.
/// This is the mutable layer — the content itself is immutable.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ObjectIndex {
    pub entries: HashMap<ObjectId, ContentHash>,
}

/// The content store combines the object index with a storage backend.
pub struct ContentStore {
    pub index: Mutex<ObjectIndex>,
    pub backend: FileBackend,
    index_path: std::path::PathBuf,
}

impl ContentStore {
    /// Open or create a content store at the given root directory.
    pub fn open(root: &Path) -> io::Result<Self> {
        let blob_dir = root.join("blobs");
        let backend = FileBackend::new(&blob_dir)?;
        let index_path = root.join("object_index.bin");

        let index = if index_path.exists() {
            let data = std::fs::read(&index_path)?;
            bincode::deserialize(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            ObjectIndex::default()
        };

        Ok(Self {
            index: Mutex::new(index),
            backend,
            index_path,
        })
    }

    /// Store content for an object. Returns the content hash.
    /// Updates the object_index to point ObjectId → new hash.
    pub fn put_object(&self, object_id: ObjectId, data: &[u8]) -> io::Result<ContentHash> {
        let hash = self.backend.put(data)?;
        let mut idx = self.index.lock().unwrap();
        idx.entries.insert(object_id, hash);
        Ok(hash)
    }

    /// Get the current content hash for an object.
    pub fn get_hash(&self, object_id: &ObjectId) -> Option<ContentHash> {
        let idx = self.index.lock().unwrap();
        idx.entries.get(object_id).copied()
    }

    /// Get the content for an object.
    pub fn get_content(&self, object_id: &ObjectId) -> io::Result<Vec<u8>> {
        let hash = {
            let idx = self.index.lock().unwrap();
            match idx.entries.get(object_id) {
                Some(h) if *h == EMPTY_HASH => return Ok(Vec::new()),
                Some(h) => *h,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("object {} not in index", object_id),
                    ))
                }
            }
        };
        self.backend.get(&hash)
    }

    /// Register an object with empty content (e.g., new directory or empty file).
    pub fn register_empty(&self, object_id: ObjectId) {
        let mut idx = self.index.lock().unwrap();
        idx.entries.insert(object_id, EMPTY_HASH);
    }

    /// Remove an object from the index.
    /// Does NOT delete the content blob (dedup means others might reference it).
    pub fn remove_object(&self, object_id: &ObjectId) -> Option<ContentHash> {
        let mut idx = self.index.lock().unwrap();
        idx.entries.remove(object_id)
    }

    /// Update the index entry directly (used by recovery).
    pub fn set_hash(&self, object_id: ObjectId, hash: ContentHash) {
        let mut idx = self.index.lock().unwrap();
        idx.entries.insert(object_id, hash);
    }

    /// Persist the object index to disk.
    pub fn flush_index(&self) -> io::Result<()> {
        let idx = self.index.lock().unwrap();
        let data =
            bincode::serialize(&*idx).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let tmp_path = self.index_path.with_extension("tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.index_path)?;
        Ok(())
    }
}
