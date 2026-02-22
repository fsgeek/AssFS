//! Episode-style crash-after-every-mutation test harness.
//!
//! Runs a workload against AssFS at the library level (no FUSE).
//! After each committed mutation, snapshots the data directory,
//! runs recovery + fsck on the snapshot. If fsck fails, preserves
//! the snapshot with failure details.
//!
//! This is the pattern Tony used to test Episode: stop after every
//! single update, snapshot, verify the snapshot is recoverable and
//! passes fsck. Preserve failures, continue the workload.
//!
//! Usage:
//!   cargo run --bin crash-harness [--preserve-dir <path>] [--workload <name>]
//!
//! Workloads: basic, deep-tree, rename-heavy, mixed

use std::io;
use std::path::{Path, PathBuf};

use clap::Parser;

use std::process::{Command, Stdio};

use assfs::graph::node::GraphNode;
use assfs::graph::operations::GraphNamespace;
use assfs::store::content::ContentStore;
use assfs::txn::recovery;
use assfs::txn::transaction::TransactionManager;
use assfs::{content_hash, EdgeType, ObjectId, EMPTY_HASH};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "crash-harness", about = "Episode-style crash-after-every-mutation test harness")]
struct Cli {
    /// Directory to preserve failed snapshots. If not set, uses a tempdir.
    #[arg(long)]
    preserve_dir: Option<PathBuf>,

    /// Workload to run.
    #[arg(long, default_value = "mixed")]
    workload: String,

    /// Stop after this many mutations (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    limit: usize,

    /// Print verbose output for each mutation.
    #[arg(short, long)]
    verbose: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the fsck binary (if needed) and return its path.
fn fsck_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = manifest_dir.join("target").join("debug").join("fsck");
    // If it exists already, use it. Otherwise build it.
    if !binary.exists() {
        let status = Command::new("cargo")
            .args(["build", "--bin", "fsck"])
            .env(
                "PATH",
                format!(
                    "{}/.cargo/bin:{}",
                    std::env::var("HOME").unwrap_or_default(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("cargo build fsck failed");
        assert!(status.success(), "cargo build --bin fsck failed");
    }
    binary
}

// ---------------------------------------------------------------------------
// Snapshot + verify infrastructure
// ---------------------------------------------------------------------------

struct HarnessState {
    /// Working data directory where the "live" filesystem state lives.
    data_dir: PathBuf,
    /// Graph namespace (live).
    graph: GraphNamespace,
    /// Content store (live).
    store: ContentStore,
    /// Transaction manager (live).
    txn_mgr: TransactionManager,
    /// Root node ID.
    root_id: ObjectId,
    /// Tracking: known directory nodes for workload navigation.
    directories: Vec<ObjectId>,
    /// Tracking: known file nodes for workload navigation.
    files: Vec<(ObjectId, String)>, // (id, name — for display/debug)
    /// Mutation counter.
    mutation_count: u64,
    /// Failure counter.
    failure_count: u64,
    /// Where to preserve failed snapshots.
    preserve_dir: PathBuf,
    /// Verbose output.
    verbose: bool,
}

impl HarnessState {
    fn new(preserve_dir: PathBuf, verbose: bool) -> io::Result<Self> {
        let temp = tempfile::tempdir().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("tempdir: {}", e))
        })?;
        let data_dir = temp.path().to_path_buf();
        let _keep = temp.keep(); // prevent cleanup on drop
        std::fs::create_dir_all(&data_dir)?;

        let store = ContentStore::open(&data_dir)?;
        let graph = GraphNamespace::open(&data_dir)?;
        let log_path = data_dir.join("assfs.log");
        let recovery_result = recovery::recover(&log_path, &store, &graph)?;
        let mut txn_mgr = TransactionManager::open(&log_path)?;
        txn_mgr.set_next_txn_id(recovery_result.max_txn_id);

        // Create root node.
        let root_id = graph.init_root(unsafe { libc::getuid() }, unsafe { libc::getgid() });
        store.register_empty(root_id);
        let root_bytes = bincode::serialize(&root_id)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(data_dir.join("root_id"), &root_bytes)?;

        // Flush initial state.
        graph.flush()?;
        store.flush_index()?;

        std::fs::create_dir_all(&preserve_dir)?;

        Ok(Self {
            data_dir,
            graph,
            store,
            txn_mgr,
            root_id,
            directories: vec![root_id],
            files: Vec::new(),
            mutation_count: 0,
            failure_count: 0,
            preserve_dir,
            verbose,
        })
    }

    /// Flush all live state to disk, then snapshot and verify.
    fn snapshot_and_verify(&mut self, label: &str) -> io::Result<bool> {
        // 1. Flush live state to disk.
        self.graph.flush()?;
        self.store.flush_index()?;

        // 2. Create a temporary snapshot directory.
        let snap_dir = tempfile::tempdir()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("snap tempdir: {}", e)))?;
        copy_dir_recursive(&self.data_dir, snap_dir.path())?;

        // 3. Run recovery + fsck on the snapshot via the fsck binary.
        let fsck_binary = fsck_binary();
        let output = Command::new(&fsck_binary)
            .args(["--after-recovery", snap_dir.path().to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("fsck spawn: {}", e)))?;

        let stderr_str = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            // Exit 0 = no errors.
            if self.verbose {
                eprintln!("  OK   mutation #{} ({})", self.mutation_count, label);
            }
            Ok(true)
        } else {
            self.failure_count += 1;
            let exit_code = output.status.code().unwrap_or(-1);
            eprintln!(
                "  FAIL after mutation #{} ({}) — fsck exit {}",
                self.mutation_count, label, exit_code
            );

            // Preserve the failed snapshot.
            let fail_name = format!(
                "failure_{:06}_{}", self.mutation_count,
                label.replace(' ', "_").replace('/', "_")
            );
            let fail_dir = self.preserve_dir.join(&fail_name);
            if let Err(e) = copy_dir_recursive(snap_dir.path(), &fail_dir) {
                eprintln!("    WARNING: could not preserve snapshot: {}", e);
            } else {
                let details = format!(
                    "Mutation: #{}\nLabel: {}\nFsck exit code: {}\nStderr:\n{}\n",
                    self.mutation_count, label, exit_code, stderr_str
                );
                let _ = std::fs::write(fail_dir.join("FAILURE_DETAILS.txt"), &details);
                eprintln!("    Preserved: {}", fail_dir.display());
            }
            Ok(false)
        }
    }

    // --- Mutation operations (each one does the mutation, commits, snapshots) ---

    /// Create a file in a given parent directory.
    fn do_create_file(&mut self, parent_id: ObjectId, name: &str) -> io::Result<ObjectId> {
        self.mutation_count += 1;
        let label = format!("create_file({})", name);

        let txn_id = self.txn_mgr.start_txn()?;

        let file_id = uuid::Uuid::new_v4();
        let file_node = GraphNode::new_file(
            file_id,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
            0o644,
        );
        let node_data = bincode::serialize(&file_node).unwrap_or_default();

        self.txn_mgr.log_add_node(txn_id, file_id, node_data)?;
        self.txn_mgr.log_add_edge(
            txn_id, parent_id, file_id, EdgeType::Child, name.to_string(),
        )?;
        self.txn_mgr
            .log_content_change(txn_id, file_id, None, Some(EMPTY_HASH))?;

        // Apply to in-memory state.
        self.graph.add_node(file_node);
        self.graph
            .add_edge(parent_id, file_id, EdgeType::Child, name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        self.store.register_empty(file_id);

        // Commit.
        self.txn_mgr.end_txn(txn_id)?;

        // Snapshot and verify.
        self.snapshot_and_verify(&label)?;

        self.files.push((file_id, name.to_string()));
        Ok(file_id)
    }

    /// Write content to an existing file.
    fn do_write_file(&mut self, file_id: ObjectId, data: &[u8]) -> io::Result<()> {
        self.mutation_count += 1;
        let label = format!("write_file({} bytes)", data.len());

        let txn_id = self.txn_mgr.start_txn()?;

        let old_hash = self.store.get_hash(&file_id);
        let new_hash = content_hash(data);
        self.store.put_object(file_id, data)?;

        self.txn_mgr
            .log_content_change(txn_id, file_id, old_hash, Some(new_hash))?;

        // Update metadata.
        self.graph.update_node(&file_id, |node| {
            node.metadata.size = data.len() as u64;
        });

        // Commit.
        self.txn_mgr.end_txn(txn_id)?;

        // Snapshot and verify.
        self.snapshot_and_verify(&label)?;
        Ok(())
    }

    /// Create a directory.
    fn do_mkdir(&mut self, parent_id: ObjectId, name: &str) -> io::Result<ObjectId> {
        self.mutation_count += 1;
        let label = format!("mkdir({})", name);

        let txn_id = self.txn_mgr.start_txn()?;

        let dir_id = uuid::Uuid::new_v4();
        let dir_node = GraphNode::new_dir(
            dir_id,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
            0o755,
        );
        let node_data = bincode::serialize(&dir_node).unwrap_or_default();

        self.txn_mgr.log_add_node(txn_id, dir_id, node_data)?;
        self.txn_mgr.log_add_edge(
            txn_id, parent_id, dir_id, EdgeType::Child, name.to_string(),
        )?;
        self.txn_mgr
            .log_content_change(txn_id, dir_id, None, Some(EMPTY_HASH))?;

        self.graph.add_node(dir_node);
        self.graph
            .add_edge(parent_id, dir_id, EdgeType::Child, name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        self.store.register_empty(dir_id);

        self.txn_mgr.end_txn(txn_id)?;

        self.snapshot_and_verify(&label)?;

        self.directories.push(dir_id);
        Ok(dir_id)
    }

    /// Unlink a file (remove edge; remove node only if last link).
    fn do_unlink(&mut self, parent_id: ObjectId, file_id: ObjectId, name: &str) -> io::Result<()> {
        self.mutation_count += 1;
        let label = format!("unlink({})", name);

        let txn_id = self.txn_mgr.start_txn()?;

        self.txn_mgr.log_remove_edge(
            txn_id, parent_id, file_id, EdgeType::Child, name.to_string(),
        )?;

        // Remove the edge from graph.
        self.graph.remove_edge(&parent_id, EdgeType::Child, name);

        // Check if there are remaining incoming edges (hard links).
        let remaining_links = {
            let state = self.graph.state.lock().unwrap();
            state.reverse.get(&file_id)
                .map(|refs| refs.len())
                .unwrap_or(0)
        };

        if remaining_links == 0 {
            // Last link — remove the node.
            self.txn_mgr.log_remove_node(txn_id, file_id, Vec::new())?;
            self.graph.remove_node(&file_id);
        } else {
            // Update nlink.
            self.graph.update_node(&file_id, |node| {
                node.metadata.nlink = remaining_links as u32;
            });
        }

        self.txn_mgr.end_txn(txn_id)?;

        self.snapshot_and_verify(&label)?;

        if remaining_links == 0 {
            self.files.retain(|(id, _)| *id != file_id);
        }
        Ok(())
    }

    /// Remove an empty directory.
    fn do_rmdir(&mut self, parent_id: ObjectId, dir_id: ObjectId, name: &str) -> io::Result<()> {
        self.mutation_count += 1;
        let label = format!("rmdir({})", name);

        let txn_id = self.txn_mgr.start_txn()?;

        self.txn_mgr.log_remove_edge(
            txn_id, parent_id, dir_id, EdgeType::Child, name.to_string(),
        )?;
        self.txn_mgr.log_remove_node(txn_id, dir_id, Vec::new())?;

        self.graph.remove_edge(&parent_id, EdgeType::Child, name);
        self.graph.remove_node(&dir_id);

        self.txn_mgr.end_txn(txn_id)?;

        self.snapshot_and_verify(&label)?;

        self.directories.retain(|id| *id != dir_id);
        Ok(())
    }

    /// Hard link a file into another directory.
    fn do_link(
        &mut self,
        target_id: ObjectId,
        new_parent: ObjectId,
        name: &str,
    ) -> io::Result<()> {
        self.mutation_count += 1;
        let label = format!("link({})", name);

        let txn_id = self.txn_mgr.start_txn()?;

        self.txn_mgr.log_add_edge(
            txn_id, new_parent, target_id, EdgeType::Child, name.to_string(),
        )?;

        self.graph
            .add_edge(new_parent, target_id, EdgeType::Child, name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // Increment nlink.
        self.graph.update_node(&target_id, |node| {
            node.metadata.nlink += 1;
        });

        self.txn_mgr.end_txn(txn_id)?;

        self.snapshot_and_verify(&label)?;
        Ok(())
    }

    /// Rename a file from one parent/name to another.
    fn do_rename(
        &mut self,
        old_parent: ObjectId,
        old_name: &str,
        new_parent: ObjectId,
        new_name: &str,
        target_id: ObjectId,
    ) -> io::Result<()> {
        self.mutation_count += 1;
        let label = format!("rename({} -> {})", old_name, new_name);

        let txn_id = self.txn_mgr.start_txn()?;

        self.txn_mgr.log_remove_edge(
            txn_id, old_parent, target_id, EdgeType::Child, old_name.to_string(),
        )?;
        self.txn_mgr.log_add_edge(
            txn_id, new_parent, target_id, EdgeType::Child, new_name.to_string(),
        )?;

        self.graph.remove_edge(&old_parent, EdgeType::Child, old_name);
        self.graph
            .add_edge(new_parent, target_id, EdgeType::Child, new_name.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        self.txn_mgr.end_txn(txn_id)?;

        self.snapshot_and_verify(&label)?;

        // Update tracking.
        for (id, n) in self.files.iter_mut() {
            if *id == target_id {
                *n = new_name.to_string();
            }
        }
        Ok(())
    }
}

impl Drop for HarnessState {
    fn drop(&mut self) {
        // Clean up the working data directory.
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// Basic workload: create files, write content, remove some.
fn workload_basic(h: &mut HarnessState, limit: usize) -> io::Result<()> {
    let root = h.root_id;

    // Create 10 files.
    let mut created_files: Vec<(ObjectId, String)> = Vec::new();
    for i in 0..10 {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let name = format!("file_{}.txt", i);
        let id = h.do_create_file(root, &name)?;
        created_files.push((id, name));
    }

    // Write content to each file.
    for (id, name) in &created_files {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let content = format!("Hello from {}! This is test content #{}", name, id);
        h.do_write_file(*id, content.as_bytes())?;
    }

    // Overwrite half the files.
    for (id, name) in created_files.iter().step_by(2) {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let content = format!("Updated content for {}", name);
        h.do_write_file(*id, content.as_bytes())?;
    }

    // Unlink every third file.
    let to_unlink: Vec<(ObjectId, String)> = created_files
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 3 == 0)
        .map(|(_, f)| f.clone())
        .collect();
    for (id, name) in &to_unlink {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        h.do_unlink(root, *id, name)?;
    }

    Ok(())
}

/// Deep tree workload: nested directories, files at various levels.
fn workload_deep_tree(h: &mut HarnessState, limit: usize) -> io::Result<()> {
    let root = h.root_id;

    // Create a chain of directories 5 levels deep.
    let mut current_parent = root;
    let mut parents: Vec<(ObjectId, String)> = Vec::new();
    for depth in 0..5 {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let name = format!("level_{}", depth);
        let dir_id = h.do_mkdir(current_parent, &name)?;
        parents.push((dir_id, name));
        current_parent = dir_id;
    }

    // Create files at each level.
    for (parent_id, _) in &parents {
        for i in 0..3 {
            if limit > 0 && h.mutation_count >= limit as u64 {
                return Ok(());
            }
            let name = format!("file_{}.txt", i);
            let id = h.do_create_file(*parent_id, &name)?;
            let content = format!("Content at depth, file {}", i);
            if limit > 0 && h.mutation_count >= limit as u64 {
                return Ok(());
            }
            h.do_write_file(id, content.as_bytes())?;
        }
    }

    // Remove deepest directory's files, then the directory itself.
    let (deepest_id, deepest_name) = parents.last().unwrap().clone();
    let deepest_parent = parents[parents.len() - 2].0;

    for i in 0..3 {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let name = format!("file_{}.txt", i);
        if let Some(file_id) = h.graph.resolve(&deepest_id, EdgeType::Child, &name) {
            h.do_unlink(deepest_id, file_id, &name)?;
        }
    }

    if limit == 0 || (h.mutation_count as usize) < limit {
        h.do_rmdir(deepest_parent, deepest_id, &deepest_name)?;
    }

    Ok(())
}

/// Rename-heavy workload: create files, rename them around.
fn workload_rename_heavy(h: &mut HarnessState, limit: usize) -> io::Result<()> {
    let root = h.root_id;

    // Create two directories.
    if limit > 0 && h.mutation_count >= limit as u64 {
        return Ok(());
    }
    let dir_a = h.do_mkdir(root, "dir_a")?;
    if limit > 0 && h.mutation_count >= limit as u64 {
        return Ok(());
    }
    let dir_b = h.do_mkdir(root, "dir_b")?;

    // Create files in dir_a.
    let mut files_in_a: Vec<(ObjectId, String)> = Vec::new();
    for i in 0..8 {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let name = format!("file_{}.txt", i);
        let id = h.do_create_file(dir_a, &name)?;
        files_in_a.push((id, name));
    }

    // Rename half to dir_b.
    for (i, (id, old_name)) in files_in_a.iter().enumerate() {
        if i % 2 != 0 {
            continue;
        }
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let new_name = format!("moved_{}.txt", i);
        h.do_rename(dir_a, old_name, dir_b, &new_name, *id)?;
    }

    // Rename some within dir_b.
    let dir_b_children: Vec<(String, ObjectId)> = h.graph.neighbors(&dir_b)
        .into_iter()
        .filter(|(et, _, _)| *et == EdgeType::Child)
        .map(|(_, label, id)| (label, id))
        .collect();
    for (i, (name, id)) in dir_b_children.iter().enumerate() {
        if limit > 0 && h.mutation_count >= limit as u64 {
            return Ok(());
        }
        let new_name = format!("renamed_{}.txt", i);
        h.do_rename(dir_b, name, dir_b, &new_name, *id)?;
    }

    Ok(())
}

/// Mixed workload: combination of all operations.
fn workload_mixed(h: &mut HarnessState, limit: usize) -> io::Result<()> {
    let root = h.root_id;

    // Phase 1: Create structure.
    let dir1 = h.do_mkdir(root, "alpha")?;
    if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
    let dir2 = h.do_mkdir(root, "beta")?;
    if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
    let sub = h.do_mkdir(dir1, "sub")?;
    if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }

    // Phase 2: Create files in various directories.
    let mut all_files: Vec<(ObjectId, ObjectId, String)> = Vec::new(); // (file_id, parent_id, name)
    for (parent, prefix) in [(root, "root"), (dir1, "alpha"), (dir2, "beta"), (sub, "sub")] {
        for i in 0..4 {
            if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
            let name = format!("{}_{}.txt", prefix, i);
            let id = h.do_create_file(parent, &name)?;
            all_files.push((id, parent, name));
        }
    }

    // Phase 3: Write content.
    for (id, _, name) in &all_files {
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        let content = format!("Content of {} — written by crash harness", name);
        h.do_write_file(*id, content.as_bytes())?;
    }

    // Phase 4: Hard link some files.
    if all_files.len() >= 4 {
        let (f0, _, _) = all_files[0];
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        h.do_link(f0, dir2, "linked_from_root.txt")?;

        let (f4, _, _) = all_files[4];
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        h.do_link(f4, sub, "linked_from_alpha.txt")?;
    }

    // Phase 5: Rename files between directories.
    if all_files.len() >= 8 {
        let (f2, p2, n2) = all_files[2].clone();
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        h.do_rename(p2, &n2, dir1, "moved_here.txt", f2)?;

        let (f6, p6, n6) = all_files[6].clone();
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        h.do_rename(p6, &n6, root, "promoted.txt", f6)?;
    }

    // Phase 6: Overwrite files.
    for (id, _, name) in all_files.iter().step_by(3) {
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        let content = format!("OVERWRITTEN: {} — second write", name);
        h.do_write_file(*id, content.as_bytes())?;
    }

    // Phase 7: Unlink some files.
    let to_unlink: Vec<_> = all_files.iter()
        .enumerate()
        .filter(|(i, _)| i % 5 == 0)
        .map(|(_, f)| f.clone())
        .collect();
    for (id, parent, name) in &to_unlink {
        if limit > 0 && h.mutation_count >= limit as u64 { return Ok(()); }
        // Only unlink if the edge still exists (may have been renamed).
        if h.graph.resolve(parent, EdgeType::Child, name).is_some() {
            h.do_unlink(*parent, *id, name)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Recursively copy a directory. Files only (no symlinks, no special files).
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ty.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();

    let preserve_dir = cli.preserve_dir.unwrap_or_else(|| {
        let dir = std::env::temp_dir().join("assfs-crash-failures");
        eprintln!("Preserving failures to: {}", dir.display());
        dir
    });

    eprintln!("=== AssFS Crash-After-Every-Mutation Harness ===");
    eprintln!("  Workload: {}", cli.workload);
    if cli.limit > 0 {
        eprintln!("  Limit: {} mutations", cli.limit);
    }
    eprintln!();

    let mut h = match HarnessState::new(preserve_dir, cli.verbose) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("FATAL: Failed to initialize harness: {}", e);
            std::process::exit(2);
        }
    };

    let result = match cli.workload.as_str() {
        "basic" => workload_basic(&mut h, cli.limit),
        "deep-tree" => workload_deep_tree(&mut h, cli.limit),
        "rename-heavy" => workload_rename_heavy(&mut h, cli.limit),
        "mixed" => workload_mixed(&mut h, cli.limit),
        other => {
            eprintln!("Unknown workload: {}. Available: basic, deep-tree, rename-heavy, mixed", other);
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("\nWorkload error: {}", e);
    }

    eprintln!("\n=== Results ===");
    eprintln!("  Total mutations: {}", h.mutation_count);
    eprintln!("  Failures: {}", h.failure_count);
    if h.failure_count > 0 {
        eprintln!("  SOME CHECKS FAILED — see preserved snapshots");
        std::process::exit(1);
    } else {
        eprintln!("  All snapshots passed recovery + fsck");
    }
}
