//! File-per-object storage backend.
//!
//! Each content blob is stored as a file named by its hex-encoded BLAKE3 hash.
//! Two-level directory structure: first 2 hex chars as subdirectory.
//!   e.g., ab/abcdef0123456789...

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{content_hash, hash_hex, ContentHash};

use super::backend::StorageBackend;

pub struct FileBackend {
    root: PathBuf,
}

impl FileBackend {
    pub fn new(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Path for a given content hash: root/ab/abcdef...
    fn blob_path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash_hex(hash);
        self.root.join(&hex[..2]).join(&hex)
    }
}

impl StorageBackend for FileBackend {
    fn put(&self, data: &[u8]) -> io::Result<ContentHash> {
        let hash = content_hash(data);
        let path = self.blob_path(&hash);

        // Content-addressed: if it exists, it's already correct.
        if path.exists() {
            return Ok(hash);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write to temp file, then rename for atomicity.
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, data)?;
        fs::rename(&tmp_path, &path)?;

        Ok(hash)
    }

    fn get(&self, hash: &ContentHash) -> io::Result<Vec<u8>> {
        let path = self.blob_path(hash);
        fs::read(&path)
    }

    fn exists(&self, hash: &ContentHash) -> io::Result<bool> {
        let path = self.blob_path(hash);
        Ok(path.exists())
    }

    fn delete(&self, hash: &ContentHash) -> io::Result<bool> {
        let path = self.blob_path(hash);
        if path.exists() {
            fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::backend::StorageBackend;
    use std::io;

    fn temp_backend() -> io::Result<(FileBackend, tempfile::TempDir)> {
        let dir = tempfile::tempdir()?;
        let backend = FileBackend::new(dir.path())?;
        Ok((backend, dir))
    }

    #[test]
    fn test_put_get_roundtrip() -> io::Result<()> {
        let (backend, _dir) = temp_backend()?;
        let data = b"hello, assfs";
        let hash = backend.put(data)?;
        let retrieved = backend.get(&hash)?;
        assert_eq!(retrieved, data);
        Ok(())
    }

    #[test]
    fn test_put_idempotent() -> io::Result<()> {
        let (backend, _dir) = temp_backend()?;
        let data = b"duplicate content";
        let h1 = backend.put(data)?;
        let h2 = backend.put(data)?;
        assert_eq!(h1, h2);
        Ok(())
    }

    #[test]
    fn test_exists() -> io::Result<()> {
        let (backend, _dir) = temp_backend()?;
        let data = b"exists test";
        let hash = backend.put(data)?;
        assert!(backend.exists(&hash)?);
        assert!(!backend.exists(&[0u8; 32])?);
        Ok(())
    }

    #[test]
    fn test_delete() -> io::Result<()> {
        let (backend, _dir) = temp_backend()?;
        let data = b"delete me";
        let hash = backend.put(data)?;
        assert!(backend.delete(&hash)?);
        assert!(!backend.exists(&hash)?);
        assert!(!backend.delete(&hash)?);
        Ok(())
    }

    #[test]
    fn test_get_nonexistent() {
        let (backend, _dir) = temp_backend().unwrap();
        let result = backend.get(&[0u8; 32]);
        assert!(result.is_err());
    }
}
