//! End-to-end FUSE test: mount AssFS, do real file operations, verify results.
//!
//! This test spawns a real FUSE mount, exercises it through the kernel VFS
//! (actual syscalls, not mocked), and verifies the filesystem works.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Build the binary if needed, return path to it.
fn assfs_binary() -> PathBuf {
    let status = Command::new("cargo")
        .args(["build"])
        .env("PATH", format!("{}/.cargo/bin:{}", env!("HOME"), std::env::var("PATH").unwrap_or_default()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("cargo build failed");
    assert!(status.success(), "cargo build failed");

    let binary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("assfs");
    assert!(binary.exists(), "binary not found at {:?}", binary);
    binary
}

/// Wait for the mount to appear (poll /proc/mounts).
fn wait_for_mount(mountpoint: &std::path::Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            if mounts.contains(mountpoint.to_str().unwrap()) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Unmount the filesystem.
fn unmount(mountpoint: &std::path::Path) {
    let _ = Command::new("fusermount3")
        .args(["-u", mountpoint.to_str().unwrap()])
        .status();
    // Give it a moment to clean up.
    thread::sleep(Duration::from_millis(500));
}

struct MountGuard {
    mountpoint: PathBuf,
    _data_dir: tempfile::TempDir, // held to keep tempdir alive
    child: Option<std::process::Child>,
}

impl MountGuard {
    fn mount() -> Self {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let mountpoint = data_dir.path().join("mnt");
        fs::create_dir_all(&mountpoint).expect("create mountpoint");

        let binary = assfs_binary();
        let data_path = data_dir.path().join("data");

        let child = Command::new(&binary)
            .args([
                mountpoint.to_str().unwrap(),
                "--data-dir",
                data_path.to_str().unwrap(),
            ])
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn assfs");

        let mut guard = MountGuard {
            mountpoint: mountpoint.clone(),
            _data_dir: data_dir,
            child: Some(child),
        };

        if !wait_for_mount(&mountpoint, Duration::from_secs(5)) {
            // Check if process died.
            if let Some(ref mut child) = guard.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut stderr = String::new();
                        if let Some(ref mut err) = child.stderr {
                            use std::io::Read;
                            let _ = err.read_to_string(&mut stderr);
                        }
                        panic!(
                            "assfs exited with {} before mount appeared. stderr:\n{}",
                            status, stderr
                        );
                    }
                    _ => {}
                }
            }
            panic!("mount did not appear at {:?} within 5 seconds", mountpoint);
        }

        // Give the fs a moment to stabilize.
        thread::sleep(Duration::from_millis(200));
        guard
    }

    fn path(&self) -> &std::path::Path {
        &self.mountpoint
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        unmount(&self.mountpoint);
        if let Some(ref mut child) = self.child {
            let _ = child.wait();
        }
    }
}

#[test]
fn test_mount_and_list_empty_root() {
    let guard = MountGuard::mount();

    // ls should show empty directory (just . and ..).
    let entries: Vec<_> = fs::read_dir(guard.path())
        .expect("read_dir on mount")
        .collect();
    assert_eq!(entries.len(), 0, "root should be empty, got {:?}", entries);

    // stat the root.
    let meta = fs::metadata(guard.path()).expect("stat root");
    assert!(meta.is_dir());
}

#[test]
fn test_create_and_read_file() {
    let guard = MountGuard::mount();
    let file_path = guard.path().join("hello.txt");

    // Create and write.
    fs::write(&file_path, b"hello, assfs!\n").expect("write file");

    // Read back.
    let content = fs::read_to_string(&file_path).expect("read file");
    assert_eq!(content, "hello, assfs!\n");

    // Stat.
    let meta = fs::metadata(&file_path).expect("stat file");
    assert!(meta.is_file());
    assert_eq!(meta.len(), 14);
}

#[test]
fn test_mkdir_and_nested_create() {
    let guard = MountGuard::mount();

    // mkdir.
    let dir_path = guard.path().join("subdir");
    fs::create_dir(&dir_path).expect("mkdir");
    assert!(fs::metadata(&dir_path).expect("stat dir").is_dir());

    // Create file inside.
    let file_path = dir_path.join("nested.txt");
    fs::write(&file_path, b"nested content").expect("write nested");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read nested"),
        "nested content"
    );

    // readdir on subdir.
    let entries: Vec<String> = fs::read_dir(&dir_path)
        .expect("readdir")
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["nested.txt"]);
}

#[test]
fn test_unlink_and_rmdir() {
    let guard = MountGuard::mount();

    // Create file, then unlink.
    let file_path = guard.path().join("ephemeral.txt");
    fs::write(&file_path, b"gone soon").expect("write");
    assert!(file_path.exists());
    fs::remove_file(&file_path).expect("unlink");
    assert!(!file_path.exists());

    // Create dir, then rmdir.
    let dir_path = guard.path().join("tmpdir");
    fs::create_dir(&dir_path).expect("mkdir");
    assert!(dir_path.exists());
    fs::remove_dir(&dir_path).expect("rmdir");
    assert!(!dir_path.exists());
}

#[test]
fn test_hard_link_shares_content() {
    let guard = MountGuard::mount();

    let original = guard.path().join("original.txt");
    let link = guard.path().join("hardlink.txt");

    fs::write(&original, b"shared data").expect("write original");
    fs::hard_link(&original, &link).expect("hard_link");

    // Both should have same content.
    assert_eq!(
        fs::read_to_string(&original).expect("read original"),
        "shared data"
    );
    assert_eq!(
        fs::read_to_string(&link).expect("read link"),
        "shared data"
    );

    // Both should have same inode.
    let meta_orig = fs::metadata(&original).expect("stat original");
    let meta_link = fs::metadata(&link).expect("stat link");
    assert_eq!(meta_orig.ino(), meta_link.ino(), "hardlinks should share inode");
    assert_eq!(meta_orig.nlink(), 2, "nlink should be 2");
}

#[test]
fn test_rename_file() {
    let guard = MountGuard::mount();

    let old_path = guard.path().join("before.txt");
    let new_path = guard.path().join("after.txt");

    fs::write(&old_path, b"moving").expect("write");
    fs::rename(&old_path, &new_path).expect("rename");

    assert!(!old_path.exists());
    assert_eq!(
        fs::read_to_string(&new_path).expect("read"),
        "moving"
    );
}

#[test]
fn test_overwrite_file() {
    let guard = MountGuard::mount();
    let path = guard.path().join("overwrite.txt");

    fs::write(&path, b"version 1").expect("write v1");
    assert_eq!(fs::read_to_string(&path).unwrap(), "version 1");

    fs::write(&path, b"version 2").expect("write v2");
    assert_eq!(fs::read_to_string(&path).unwrap(), "version 2");
}

#[test]
fn test_multiple_files_readdir() {
    let guard = MountGuard::mount();

    for i in 0..5 {
        let path = guard.path().join(format!("file_{}.txt", i));
        fs::write(&path, format!("content {}", i)).expect("write");
    }

    let mut entries: Vec<String> = fs::read_dir(guard.path())
        .expect("readdir")
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["file_0.txt", "file_1.txt", "file_2.txt", "file_3.txt", "file_4.txt"]
    );
}
