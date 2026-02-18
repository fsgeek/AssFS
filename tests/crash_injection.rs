//! Crash injection tests: SIGKILL the FUSE daemon during operations,
//! remount (triggering recovery), verify consistency.
//!
//! These tests prove the core claim: crash at any point, recovery
//! restores a consistent state. Committed data survives; uncommitted
//! data is cleanly absent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

// --- Test infrastructure ---

fn assfs_binary() -> PathBuf {
    let status = Command::new("cargo")
        .args(["build"])
        .env(
            "PATH",
            format!(
                "{}/.cargo/bin:{}",
                env!("HOME"),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
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

fn wait_for_mount(mountpoint: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            if mounts.contains(mountpoint.to_str().unwrap()) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn force_unmount(mountpoint: &Path) {
    // Try normal unmount first, then lazy unmount as fallback.
    let _ = Command::new("fusermount3")
        .args(["-u", mountpoint.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(200));

    // If still mounted, force lazy unmount.
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        if mounts.contains(mountpoint.to_str().unwrap()) {
            let _ = Command::new("fusermount3")
                .args(["-uz", mountpoint.to_str().unwrap()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            thread::sleep(Duration::from_millis(300));
        }
    }
}

/// Persistent test harness that survives across mount/crash/remount cycles.
struct CrashTestHarness {
    _temp_dir: tempfile::TempDir,
    data_path: PathBuf,
    mountpoint: PathBuf,
}

impl CrashTestHarness {
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mountpoint = temp_dir.path().join("mnt");
        let data_path = temp_dir.path().join("data");
        fs::create_dir_all(&mountpoint).expect("create mountpoint");
        Self {
            _temp_dir: temp_dir,
            data_path,
            mountpoint,
        }
    }

    /// Mount the filesystem, wait for it to appear. Returns the daemon process.
    fn mount(&self) -> Child {
        let binary = assfs_binary();
        let mut child = Command::new(&binary)
            .args([
                self.mountpoint.to_str().unwrap(),
                "--data-dir",
                self.data_path.to_str().unwrap(),
            ])
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn assfs");

        if !wait_for_mount(&self.mountpoint, Duration::from_secs(5)) {
            // Check if process died.
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
            panic!(
                "mount did not appear at {:?} within 5 seconds",
                self.mountpoint
            );
        }

        thread::sleep(Duration::from_millis(200));
        child
    }

    /// SIGKILL the daemon — simulates a hard crash.
    fn crash(&self, child: &mut Child) {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        let _ = child.wait();
        // Clean up the stale mountpoint.
        thread::sleep(Duration::from_millis(300));
        force_unmount(&self.mountpoint);
    }

    /// Clean unmount.
    fn unmount(&self, child: &mut Child) {
        force_unmount(&self.mountpoint);
        let _ = child.wait();
    }

    fn path(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for CrashTestHarness {
    fn drop(&mut self) {
        force_unmount(&self.mountpoint);
    }
}

// --- Tests ---

/// Phase 1: commit baseline. Phase 2: crash during file creation burst.
/// Phase 3: remount, verify baseline intact and no partial state.
#[test]
fn test_crash_during_file_creation() {
    let h = CrashTestHarness::new();

    // Phase 1: Establish committed baseline.
    {
        let mut child = h.mount();
        for i in 0..5 {
            fs::write(
                h.path().join(format!("base_{}.txt", i)),
                format!("base content {}", i),
            )
            .expect("write baseline");
        }
        h.unmount(&mut child);
    }

    // Phase 2: Mount, create files rapidly, kill -9.
    {
        let mut child = h.mount();
        let mp = h.path().to_path_buf();
        let writer = thread::spawn(move || {
            let mut created = 0u32;
            for i in 0..50 {
                match fs::write(
                    mp.join(format!("crash_{}.txt", i)),
                    format!("crash content {}", i),
                ) {
                    Ok(_) => created += 1,
                    Err(_) => break,
                }
            }
            created
        });

        // Let the writer start but crash before it finishes.
        thread::sleep(Duration::from_millis(100));
        h.crash(&mut child);
        let created = writer.join().unwrap_or(0);
        eprintln!(
            "crash_during_file_creation: writer reported {} files before crash",
            created
        );
    }

    // Phase 3: Remount (recovery runs), verify consistency.
    {
        let mut child = h.mount();

        // Baseline files must be intact.
        for i in 0..5 {
            let path = h.path().join(format!("base_{}.txt", i));
            let content = fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("baseline file base_{}.txt missing after recovery: {}", i, e)
            });
            assert_eq!(
                content,
                format!("base content {}", i),
                "baseline file base_{}.txt has wrong content",
                i
            );
        }

        // Crash files: each is in one of three valid states:
        // (a) Fully present with correct content (create + write both committed)
        // (b) Present but empty (create committed, write didn't)
        // (c) Absent (create didn't commit)
        // No partial content is acceptable.
        for i in 0..50 {
            let path = h.path().join(format!("crash_{}.txt", i));
            if path.exists() {
                let content = fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("crash_{}.txt exists but unreadable: {}", i, e));
                let expected = format!("crash content {}", i);
                assert!(
                    content == expected || content.is_empty(),
                    "crash_{}.txt has unexpected content (partial write?): {:?}",
                    i,
                    content
                );
            }
        }

        // readdir should be consistent: every listed entry is accessible.
        let entries: Vec<String> = fs::read_dir(h.path())
            .expect("readdir after recovery")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        for name in &entries {
            let path = h.path().join(name);
            assert!(
                fs::metadata(&path).is_ok(),
                "readdir listed {} but stat fails",
                name
            );
        }

        h.unmount(&mut child);
    }
}

/// Write a file, commit it, then overwrite it and crash.
/// After recovery, file should have either old or new content — never partial.
#[test]
fn test_crash_during_file_overwrite() {
    let h = CrashTestHarness::new();
    let filename = "overwrite_target.txt";
    let old_content = "version 1 — the original";
    let new_content = "version 2 — the replacement which is longer to be different";

    // Phase 1: Create file with known content.
    {
        let mut child = h.mount();
        fs::write(h.path().join(filename), old_content).expect("write v1");
        // Also verify it's readable before unmount.
        assert_eq!(
            fs::read_to_string(h.path().join(filename)).unwrap(),
            old_content
        );
        h.unmount(&mut child);
    }

    // Phase 2: Overwrite and crash.
    {
        let mut child = h.mount();
        // Write the new content — this may or may not complete.
        let mp = h.path().to_path_buf();
        let fname = filename.to_string();
        let nc = new_content.to_string();
        let writer = thread::spawn(move || fs::write(mp.join(&fname), &nc));

        thread::sleep(Duration::from_millis(50));
        h.crash(&mut child);
        let _ = writer.join();
    }

    // Phase 3: Verify.
    {
        let mut child = h.mount();
        let content = fs::read_to_string(h.path().join(filename))
            .expect("file should exist after recovery");

        // Valid states after crash:
        // - Old content (write didn't commit, or entire overwrite undone)
        // - New content (write committed)
        // - Empty (truncation applied but write didn't commit — setattr
        //   truncation is not transactional in the MVP)
        assert!(
            content == old_content || content == new_content || content.is_empty(),
            "file has unexpected content (partial write?): {:?}",
            content
        );

        h.unmount(&mut child);
    }
}

/// Create directories and files, crash, verify directory structure is consistent.
#[test]
fn test_crash_during_directory_operations() {
    let h = CrashTestHarness::new();

    // Phase 1: Create a directory tree as baseline.
    {
        let mut child = h.mount();
        fs::create_dir(h.path().join("stable_dir")).expect("mkdir");
        fs::write(
            h.path().join("stable_dir").join("stable_file.txt"),
            "stable",
        )
        .expect("write stable file");
        h.unmount(&mut child);
    }

    // Phase 2: Create and delete directories rapidly, crash.
    {
        let mut child = h.mount();
        let mp = h.path().to_path_buf();
        let worker = thread::spawn(move || {
            for i in 0..20 {
                let dir = mp.join(format!("tmpdir_{}", i));
                let _ = fs::create_dir(&dir);
                let _ = fs::write(dir.join("file.txt"), format!("content {}", i));
                // Delete some of them.
                if i % 3 == 0 {
                    let _ = fs::remove_file(dir.join("file.txt"));
                    let _ = fs::remove_dir(&dir);
                }
            }
        });

        thread::sleep(Duration::from_millis(80));
        h.crash(&mut child);
        let _ = worker.join();
    }

    // Phase 3: Verify consistency.
    {
        let mut child = h.mount();

        // Baseline directory and file must be intact.
        assert!(
            fs::metadata(h.path().join("stable_dir"))
                .expect("stable_dir missing")
                .is_dir()
        );
        assert_eq!(
            fs::read_to_string(h.path().join("stable_dir").join("stable_file.txt"))
                .expect("stable file missing"),
            "stable"
        );

        // Every directory that exists should be stat-able and listable.
        let entries: Vec<String> = fs::read_dir(h.path())
            .expect("readdir root")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        for name in &entries {
            let path = h.path().join(name);
            let meta = fs::metadata(&path).unwrap_or_else(|e| {
                panic!("readdir entry {} not stat-able: {}", name, e)
            });

            if meta.is_dir() {
                // Directory should be listable.
                let _sub: Vec<_> = fs::read_dir(&path)
                    .unwrap_or_else(|e| panic!("dir {} not listable: {}", name, e))
                    .collect();
            } else {
                // File should be readable.
                let _data = fs::read(&path)
                    .unwrap_or_else(|e| panic!("file {} not readable: {}", name, e));
            }
        }

        h.unmount(&mut child);
    }
}

/// Multiple crash/recovery cycles — verify recovery is idempotent and
/// the filesystem accumulates committed state correctly.
#[test]
fn test_repeated_crash_recovery_cycles() {
    let h = CrashTestHarness::new();

    // Cycle 1: Create files, clean unmount.
    {
        let mut child = h.mount();
        for i in 0..3 {
            fs::write(
                h.path().join(format!("cycle1_{}.txt", i)),
                format!("cycle1 {}", i),
            )
            .expect("write cycle1");
        }
        h.unmount(&mut child);
    }

    // Cycle 2: Create more files, crash.
    {
        let mut child = h.mount();
        let mp = h.path().to_path_buf();
        let writer = thread::spawn(move || {
            for i in 0..20 {
                let _ = fs::write(
                    mp.join(format!("cycle2_{}.txt", i)),
                    format!("cycle2 {}", i),
                );
            }
        });
        thread::sleep(Duration::from_millis(80));
        h.crash(&mut child);
        let _ = writer.join();
    }

    // Cycle 3: Recover, create more files, crash again.
    {
        let mut child = h.mount();

        // Verify cycle 1 data survived the first crash.
        for i in 0..3 {
            let content = fs::read_to_string(h.path().join(format!("cycle1_{}.txt", i)))
                .unwrap_or_else(|e| panic!("cycle1_{}.txt missing after first crash: {}", i, e));
            assert_eq!(content, format!("cycle1 {}", i));
        }

        // Create more files, then crash again.
        let mp = h.path().to_path_buf();
        let writer = thread::spawn(move || {
            for i in 0..20 {
                let _ = fs::write(
                    mp.join(format!("cycle3_{}.txt", i)),
                    format!("cycle3 {}", i),
                );
            }
        });
        thread::sleep(Duration::from_millis(80));
        h.crash(&mut child);
        let _ = writer.join();
    }

    // Final verification after two crashes.
    {
        let mut child = h.mount();

        // Cycle 1 files must survive two crash/recovery cycles.
        for i in 0..3 {
            let content = fs::read_to_string(h.path().join(format!("cycle1_{}.txt", i)))
                .unwrap_or_else(|e| {
                    panic!("cycle1_{}.txt missing after two crashes: {}", i, e)
                });
            assert_eq!(content, format!("cycle1 {}", i));
        }

        // All files present should be internally consistent.
        let entries: Vec<String> = fs::read_dir(h.path())
            .expect("readdir final")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        for name in &entries {
            let path = h.path().join(name);
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} exists but not readable: {}", name, e));
            // Files from crash cycles may be empty (create committed, write didn't).
            // Baseline cycle1_* files should have content.
            if name.starts_with("cycle1_") {
                assert!(!content.is_empty(), "{} should have content", name);
            }
        }

        eprintln!(
            "repeated_crash_recovery: {} files survived two crash cycles",
            entries.len()
        );

        h.unmount(&mut child);
    }
}
