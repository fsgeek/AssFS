//! AssFS consistency checker (fsck equivalent).
//!
//! Validates structural invariants of an AssFS data directory without
//! mounting it. Operates on persisted state only. Optionally runs
//! recovery first (--after-recovery) before checking.
//!
//! Exit codes:
//!   0 = no errors
//!   1 = errors found
//!   2 = usage error or I/O failure

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process;

use clap::Parser;

use assfs::graph::operations::GraphNamespace;
use assfs::store::content::ContentStore;
use assfs::txn::log::{LogManager, LogPayload, LogRecordType};
use assfs::txn::recovery;
use assfs::{content_hash, hash_hex, ContentHash, EdgeType, NodeType, ObjectId, EMPTY_HASH};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "assfs-fsck", about = "AssFS consistency checker")]
struct Cli {
    /// Path to the AssFS data directory.
    data_dir: PathBuf,

    /// Run recovery before checking (useful for the crash-test harness).
    #[arg(long)]
    after_recovery: bool,

    /// Print verbose per-check details even for passing checks.
    #[arg(short, long)]
    verbose: bool,
}

// ---------------------------------------------------------------------------
// Result tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub severity: Severity,
    pub message: String,
}

/// Accumulator for all check results.
pub struct Checker {
    pub results: Vec<CheckResult>,
}

impl Checker {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
        }
    }

    pub fn pass(&mut self, name: &str, msg: impl Into<String>) {
        self.results.push(CheckResult {
            name: name.to_string(),
            severity: Severity::Pass,
            message: msg.into(),
        });
    }

    pub fn warn(&mut self, name: &str, msg: impl Into<String>) {
        self.results.push(CheckResult {
            name: name.to_string(),
            severity: Severity::Warn,
            message: msg.into(),
        });
    }

    pub fn fail(&mut self, name: &str, msg: impl Into<String>) {
        self.results.push(CheckResult {
            name: name.to_string(),
            severity: Severity::Fail,
            message: msg.into(),
        });
    }

    pub fn error_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity == Severity::Fail)
            .count()
    }

    pub fn warn_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity == Severity::Warn)
            .count()
    }

    pub fn print_results(&self) {
        for r in &self.results {
            let tag = match r.severity {
                Severity::Pass => "[PASS]",
                Severity::Warn => "[WARN]",
                Severity::Fail => "[FAIL]",
            };
            eprintln!("    {} {}", tag, r.message);
        }
    }
}

// ---------------------------------------------------------------------------
// Graph invariant checks
// ---------------------------------------------------------------------------

pub fn check_graph_invariants(
    graph: &GraphNamespace,
    root_id: Option<ObjectId>,
    checker: &mut Checker,
) {
    let state = graph.state.lock().unwrap();

    let node_count = state.nodes.len();
    let edge_count: usize = state.edges.values().map(|idx| idx.outgoing.len()).sum();
    eprintln!("  Graph: {} nodes, {} edges", node_count, edge_count);

    // 1. Every edge's target node exists.
    {
        let mut missing_targets = Vec::new();
        for (source_id, edge_idx) in &state.edges {
            for ((et, label), target_id) in &edge_idx.outgoing {
                if !state.nodes.contains_key(target_id) {
                    missing_targets.push((*source_id, *et, label.clone(), *target_id));
                }
            }
        }
        if missing_targets.is_empty() {
            checker.pass("edge_targets_exist", "All edge targets exist");
        } else {
            for (src, et, label, tgt) in &missing_targets {
                checker.fail(
                    "edge_targets_exist",
                    format!(
                        "Edge target missing: {}--{:?}:{}-->{} (target not in graph)",
                        src, et, label, tgt
                    ),
                );
            }
        }
    }

    // 2. Every edge's source node exists.
    {
        let mut missing_sources = Vec::new();
        for (source_id, _edge_idx) in &state.edges {
            if !state.nodes.contains_key(source_id) {
                missing_sources.push(*source_id);
            }
        }
        if missing_sources.is_empty() {
            checker.pass("edge_sources_exist", "All edge sources exist");
        } else {
            for src in &missing_sources {
                checker.fail(
                    "edge_sources_exist",
                    format!("Edge index exists for non-existent source node {}", src),
                );
            }
        }
    }

    // 3. Reverse index consistency.
    {
        let mut inconsistencies = Vec::new();

        // Build expected reverse index from forward edges.
        let mut expected_reverse: HashMap<ObjectId, Vec<(ObjectId, EdgeType, String)>> =
            HashMap::new();
        for (source_id, edge_idx) in &state.edges {
            for ((et, label), target_id) in &edge_idx.outgoing {
                expected_reverse
                    .entry(*target_id)
                    .or_default()
                    .push((*source_id, *et, label.clone()));
            }
        }

        // Check each expected entry exists in the actual reverse index.
        for (target_id, expected_entries) in &expected_reverse {
            let actual = state.reverse.get(target_id);
            for (src, et, label) in expected_entries {
                let found = actual.map_or(false, |entries| {
                    entries
                        .iter()
                        .any(|(s, e, l)| s == src && *e == *et && l == label)
                });
                if !found {
                    inconsistencies.push(format!(
                        "Missing reverse entry: {} should have reverse ({}, {:?}, {})",
                        target_id, src, et, label
                    ));
                }
            }
        }

        // Check for stale reverse entries (in reverse but not in forward).
        for (target_id, rev_entries) in &state.reverse {
            for (src, et, label) in rev_entries {
                let found = state
                    .edges
                    .get(src)
                    .map_or(false, |idx| {
                        idx.outgoing
                            .get(&(*et, label.clone()))
                            .map_or(false, |t| t == target_id)
                    });
                if !found {
                    inconsistencies.push(format!(
                        "Stale reverse entry: {} has reverse ({}, {:?}, {}) but no forward edge",
                        target_id, src, et, label
                    ));
                }
            }
        }

        if inconsistencies.is_empty() {
            checker.pass("reverse_index_consistent", "Reverse index consistent");
        } else {
            for msg in &inconsistencies {
                checker.fail("reverse_index_consistent", msg.clone());
            }
        }
    }

    // 4. No duplicate edges (same source, edge_type, label).
    // BTreeMap keys are unique by construction, so duplicates are impossible
    // in the current data structure. Check anyway for completeness.
    {
        checker.pass(
            "no_duplicate_edges",
            "No duplicate edges (enforced by BTreeMap)",
        );
    }

    // 5. nlink count matches actual incoming edge count.
    {
        let mut mismatches = Vec::new();

        // Count actual incoming Child + Hardlink edges per node.
        let mut incoming_counts: HashMap<ObjectId, u32> = HashMap::new();
        for (_source_id, edge_idx) in &state.edges {
            for ((et, _label), target_id) in &edge_idx.outgoing {
                if *et == EdgeType::Child || *et == EdgeType::Hardlink {
                    *incoming_counts.entry(*target_id).or_insert(0) += 1;
                }
            }
        }

        for (node_id, node) in &state.nodes {
            let actual_incoming = incoming_counts.get(node_id).copied().unwrap_or(0);

            // For directories, nlink = 2 (self + parent "..") + number of
            // child subdirectories. The "2" base includes the implicit "."
            // and the parent's Child edge. We check that nlink >= actual
            // incoming for files (where nlink should equal incoming edges).
            // For directories, nlink tracking is more complex (includes
            // subdirectory count), so we only flag if nlink < actual_incoming.
            match node.node_type {
                NodeType::File | NodeType::Symlink => {
                    if node.metadata.nlink != actual_incoming {
                        mismatches.push(format!(
                            "nlink mismatch: node {} ({:?}) has nlink={} but {} incoming Child/Hardlink edges",
                            node_id, node.node_type, node.metadata.nlink, actual_incoming
                        ));
                    }
                }
                NodeType::Directory => {
                    // Directory nlink = 2 + number of child subdirectories.
                    // We only flag if nlink < actual_incoming (definitely wrong).
                    if (node.metadata.nlink as u64) < actual_incoming as u64 {
                        mismatches.push(format!(
                            "nlink mismatch: directory {} has nlink={} but {} incoming edges (should be >= incoming)",
                            node_id, node.metadata.nlink, actual_incoming
                        ));
                    }
                }
                NodeType::TagAnchor => {
                    // Tag anchors don't participate in POSIX nlink.
                }
            }
        }

        if mismatches.is_empty() {
            checker.pass("nlink_consistent", "nlink counts consistent");
        } else {
            for msg in &mismatches {
                checker.fail("nlink_consistent", msg.clone());
            }
        }
    }

    // 6. Root node exists.
    {
        match root_id {
            Some(rid) => {
                if state.nodes.contains_key(&rid) {
                    checker.pass("root_exists", format!("Root node {} exists", rid));
                } else {
                    checker.fail(
                        "root_exists",
                        format!("Root node {} referenced by root_id file but not in graph", rid),
                    );
                }
            }
            None => {
                checker.warn("root_exists", "No root_id file found (fresh filesystem?)");
            }
        }
    }

    // 7. Reachability from root.
    {
        if let Some(rid) = root_id {
            if state.nodes.contains_key(&rid) {
                // BFS from root following Child and Hardlink edges.
                let mut reachable: HashSet<ObjectId> = HashSet::new();
                let mut queue = vec![rid];
                while let Some(current) = queue.pop() {
                    if !reachable.insert(current) {
                        continue;
                    }
                    if let Some(edge_idx) = state.edges.get(&current) {
                        for ((et, _label), target_id) in &edge_idx.outgoing {
                            if *et == EdgeType::Child || *et == EdgeType::Hardlink {
                                if !reachable.contains(target_id) {
                                    queue.push(*target_id);
                                }
                            }
                        }
                    }
                }

                let mut orphans = Vec::new();
                for (node_id, node) in &state.nodes {
                    if !reachable.contains(node_id) {
                        // TagAnchor nodes are not expected to be reachable
                        // from root via Child/Hardlink edges.
                        if node.node_type == NodeType::TagAnchor {
                            continue;
                        }
                        orphans.push((*node_id, node.node_type));
                    }
                }

                if orphans.is_empty() {
                    checker.pass(
                        "reachability",
                        "All File/Directory nodes reachable from root",
                    );
                } else {
                    for (oid, nt) in &orphans {
                        checker.warn(
                            "reachability",
                            format!(
                                "Orphaned node {} ({:?}) not reachable from root (zero-link-count cleanup candidate)",
                                oid, nt
                            ),
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Content store invariant checks
// ---------------------------------------------------------------------------

pub fn check_content_invariants(
    store: &ContentStore,
    graph: &GraphNamespace,
    data_dir: &Path,
    checker: &mut Checker,
) {
    let index = store.index.lock().unwrap();
    let blob_dir = data_dir.join("blobs");

    let mut total_size: u64 = 0;
    let object_count = index.entries.len();

    // 8. Every indexed entry has a corresponding blob file (unless EMPTY_HASH).
    {
        let mut missing_blobs = Vec::new();
        for (obj_id, hash) in &index.entries {
            if *hash == EMPTY_HASH {
                continue;
            }
            let path = blob_path(&blob_dir, hash);
            if !path.exists() {
                missing_blobs.push((*obj_id, *hash));
            } else {
                if let Ok(meta) = std::fs::metadata(&path) {
                    total_size += meta.len();
                }
            }
        }
        if missing_blobs.is_empty() {
            checker.pass("indexed_blobs_exist", "All indexed blobs exist on disk");
        } else {
            for (oid, hash) in &missing_blobs {
                checker.fail(
                    "indexed_blobs_exist",
                    format!(
                        "Missing blob: object {} references hash {} but file not found",
                        oid,
                        hash_hex(hash)
                    ),
                );
            }
        }
    }

    eprintln!(
        "  Content: {} objects, {} on disk",
        object_count,
        format_size(total_size)
    );

    // 9. No orphaned blobs on disk (warning, not error).
    {
        let indexed_hashes: HashSet<ContentHash> = index
            .entries
            .values()
            .filter(|h| **h != EMPTY_HASH)
            .copied()
            .collect();

        let mut orphaned_count = 0;
        if blob_dir.exists() {
            if let Ok(subdirs) = std::fs::read_dir(&blob_dir) {
                for subdir_entry in subdirs.flatten() {
                    if !subdir_entry.file_type().map_or(false, |ft| ft.is_dir()) {
                        continue;
                    }
                    if let Ok(blobs) = std::fs::read_dir(subdir_entry.path()) {
                        for blob_entry in blobs.flatten() {
                            let fname = blob_entry.file_name();
                            let fname_str = fname.to_string_lossy();
                            // Skip tmp files.
                            if fname_str.ends_with(".tmp") {
                                continue;
                            }
                            // Parse hex filename back to hash.
                            if let Some(hash) = hex_to_hash(&fname_str) {
                                if !indexed_hashes.contains(&hash) {
                                    orphaned_count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        if orphaned_count == 0 {
            checker.pass("no_orphaned_blobs", "No orphaned blob files");
        } else {
            checker.warn(
                "no_orphaned_blobs",
                format!(
                    "{} orphaned blob file(s) (no index entry)",
                    orphaned_count
                ),
            );
        }
    }

    // 10. Content integrity: actual hash matches stored hash.
    {
        let mut verified = 0u64;
        let mut mismatches = Vec::new();
        for (obj_id, hash) in &index.entries {
            if *hash == EMPTY_HASH {
                verified += 1;
                continue;
            }
            let path = blob_path(&blob_dir, hash);
            match std::fs::read(&path) {
                Ok(data) => {
                    let actual_hash = content_hash(&data);
                    if actual_hash != *hash {
                        mismatches.push((*obj_id, *hash, actual_hash));
                    }
                    verified += 1;
                }
                Err(_) => {
                    // Already flagged by check 8.
                }
            }
        }
        if mismatches.is_empty() {
            checker.pass(
                "content_integrity",
                format!(
                    "Content integrity verified ({}/{} hashes match)",
                    verified, object_count
                ),
            );
        } else {
            for (oid, expected, actual) in &mismatches {
                checker.fail(
                    "content_integrity",
                    format!(
                        "Hash mismatch: object {} expected {} but blob hashes to {}",
                        oid,
                        hash_hex(expected),
                        hash_hex(actual)
                    ),
                );
            }
        }
    }

    // 11. File node metadata.size matches actual blob size.
    {
        let graph_state = graph.state.lock().unwrap();
        let mut mismatches = Vec::new();
        for (node_id, node) in &graph_state.nodes {
            if node.node_type != NodeType::File {
                continue;
            }
            let hash = match index.entries.get(node_id) {
                Some(h) => *h,
                None => continue, // No index entry; orphaned node (different check).
            };
            let expected_size = if hash == EMPTY_HASH {
                0u64
            } else {
                let path = blob_path(&blob_dir, &hash);
                match std::fs::metadata(&path) {
                    Ok(meta) => meta.len(),
                    Err(_) => continue, // Already flagged by check 8.
                }
            };
            if node.metadata.size != expected_size {
                mismatches.push((*node_id, node.metadata.size, expected_size));
            }
        }
        if mismatches.is_empty() {
            checker.pass("size_consistent", "File sizes match blob sizes");
        } else {
            for (nid, meta_size, blob_size) in &mismatches {
                checker.fail(
                    "size_consistent",
                    format!(
                        "Size mismatch: node {} metadata.size={} but blob is {} bytes",
                        nid, meta_size, blob_size
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Log invariant checks
// ---------------------------------------------------------------------------

pub fn check_log_invariants(log_path: &Path, checker: &mut Checker) {
    let records = match LogManager::read_all_records(log_path) {
        Ok(r) => r,
        Err(e) => {
            checker.fail("log_readable", format!("Failed to read log: {}", e));
            return;
        }
    };

    if records.is_empty() {
        eprintln!("  Log: 0 records, 0 transactions");
        checker.pass("log_readable", "Log is empty (no records)");
        return;
    }

    // Gather transaction IDs.
    let mut txn_ids: HashSet<u64> = HashSet::new();
    for r in &records {
        if r.transaction_id > 0 {
            txn_ids.insert(r.transaction_id);
        }
    }

    eprintln!(
        "  Log: {} records, {} transactions",
        records.len(),
        txn_ids.len()
    );

    // 12. All records are well-formed (they deserialized, so pass).
    checker.pass(
        "log_well_formed",
        format!("All {} records well-formed (deserializable)", records.len()),
    );

    // 13. LSNs monotonically increasing.
    {
        let mut non_monotonic = Vec::new();
        for i in 1..records.len() {
            if records[i].lsn <= records[i - 1].lsn {
                non_monotonic.push((i, records[i - 1].lsn, records[i].lsn));
            }
        }
        if non_monotonic.is_empty() {
            checker.pass("lsn_monotonic", "LSNs monotonically increasing");
        } else {
            for (idx, prev, curr) in &non_monotonic {
                checker.fail(
                    "lsn_monotonic",
                    format!(
                        "LSN not monotonic at record {}: prev={}, curr={}",
                        idx, prev, curr
                    ),
                );
            }
        }
    }

    // 14. Every End record has at least one preceding Data record for that txn.
    {
        let mut data_txns: HashSet<u64> = HashSet::new();
        let mut orphan_ends = Vec::new();
        for r in &records {
            if r.record_type == LogRecordType::Data {
                data_txns.insert(r.transaction_id);
            }
            if r.record_type == LogRecordType::End {
                if !data_txns.contains(&r.transaction_id) {
                    orphan_ends.push(r.transaction_id);
                }
            }
        }
        if orphan_ends.is_empty() {
            checker.pass(
                "end_has_data",
                "Every End record has preceding Data records",
            );
        } else {
            for tid in &orphan_ends {
                checker.fail(
                    "end_has_data",
                    format!(
                        "End record for txn {} has no preceding Data records",
                        tid
                    ),
                );
            }
        }
    }

    // 15. BatchEnd records reference a valid transaction ID as ec_head.
    {
        let mut invalid_bend = Vec::new();
        for r in &records {
            if r.record_type == LogRecordType::BatchEnd {
                if let Some(LogPayload::BatchEnd { ec_head }) = &r.payload {
                    if !txn_ids.contains(ec_head) {
                        invalid_bend.push((r.transaction_id, *ec_head));
                    }
                } else {
                    invalid_bend.push((r.transaction_id, 0));
                }
            }
        }
        if invalid_bend.is_empty() {
            checker.pass("bend_valid_head", "BatchEnd records reference valid ec_head");
        } else {
            for (member, head) in &invalid_bend {
                checker.fail(
                    "bend_valid_head",
                    format!(
                        "BatchEnd for txn {} references ec_head {} which has no records in log",
                        member, head
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Replicate FileBackend::blob_path without requiring a FileBackend instance.
fn blob_path(blob_dir: &Path, hash: &ContentHash) -> PathBuf {
    let hex = hash_hex(hash);
    blob_dir.join(&hex[..2]).join(&hex)
}

/// Parse a 64-char hex string into a ContentHash.
fn hex_to_hash(hex: &str) -> Option<ContentHash> {
    if hex.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for i in 0..32 {
        hash[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

/// Human-readable size.
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Load the root_id from the data directory (if present).
fn load_root_id(data_dir: &Path) -> Option<ObjectId> {
    let root_id_path = data_dir.join("root_id");
    if !root_id_path.exists() {
        return None;
    }
    let bytes = std::fs::read(&root_id_path).ok()?;
    bincode::deserialize(&bytes).ok()
}

// ---------------------------------------------------------------------------
// Public API for use from tests
// ---------------------------------------------------------------------------

/// Run all consistency checks against a data directory.
/// Returns the checker with all results.
pub fn run_fsck(data_dir: &Path, after_recovery: bool) -> Result<Checker, String> {
    if !data_dir.exists() {
        return Err(format!("Data directory does not exist: {}", data_dir.display()));
    }

    let graph = GraphNamespace::open(data_dir)
        .map_err(|e| format!("Failed to open graph: {}", e))?;
    let store = ContentStore::open(data_dir)
        .map_err(|e| format!("Failed to open content store: {}", e))?;
    let log_path = data_dir.join("assfs.log");

    if after_recovery && log_path.exists() {
        recovery::recover(&log_path, &store, &graph)
            .map_err(|e| format!("Recovery failed: {}", e))?;
    }

    let root_id = load_root_id(data_dir);

    let mut checker = Checker::new();

    eprintln!("\n  Checking graph invariants...");
    check_graph_invariants(&graph, root_id, &mut checker);

    eprintln!("\n  Checking content invariants...");
    check_content_invariants(&store, &graph, data_dir, &mut checker);

    eprintln!("\n  Checking log invariants...");
    if log_path.exists() {
        check_log_invariants(&log_path, &mut checker);
    } else {
        checker.pass("log_exists", "No log file (fresh filesystem)");
    }

    Ok(checker)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let data_dir = &cli.data_dir;

    eprintln!(
        "AssFS Consistency Check: {}",
        data_dir.display()
    );

    match run_fsck(data_dir, cli.after_recovery) {
        Ok(checker) => {
            checker.print_results();
            let errors = checker.error_count();
            let warnings = checker.warn_count();
            eprintln!(
                "\n  Summary: {} error(s), {} warning(s)",
                errors, warnings
            );
            if errors > 0 {
                process::exit(1);
            }
        }
        Err(msg) => {
            eprintln!("FATAL: {}", msg);
            process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use assfs::graph::node::GraphNode;
    use std::io;

    /// Helper: create a minimal AssFS data directory with root, a file,
    /// a directory, and some content.
    fn setup_test_dir() -> io::Result<(tempfile::TempDir, ObjectId)> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();

        // Open graph and store.
        let graph = GraphNamespace::open(data_dir)?;
        let store = ContentStore::open(data_dir)?;
        let log_path = data_dir.join("assfs.log");

        // Create root node.
        let root_id = graph.init_root(1000, 1000);
        store.register_empty(root_id);

        // Persist root_id.
        let root_id_bytes = bincode::serialize(&root_id).unwrap();
        std::fs::write(data_dir.join("root_id"), &root_id_bytes)?;

        // Create a subdirectory.
        let subdir_id = uuid::Uuid::new_v4();
        let subdir_node = GraphNode::new_dir(subdir_id, 1000, 1000, 0o755);
        graph.add_node(subdir_node);
        graph.add_edge(root_id, subdir_id, EdgeType::Child, "subdir".to_string())?;
        store.register_empty(subdir_id);

        // Increment root nlink for subdirectory.
        graph.update_node(&root_id, |n| {
            n.metadata.nlink += 1;
        });

        // Create a file in root.
        let file_id = uuid::Uuid::new_v4();
        let mut file_node = GraphNode::new_file(file_id, 1000, 1000, 0o644);
        let content = b"hello, fsck";
        let hash = store.put_object(file_id, content)?;
        file_node.content_hash = hash;
        file_node.metadata.size = content.len() as u64;
        graph.add_node(file_node);
        graph.add_edge(root_id, file_id, EdgeType::Child, "test.txt".to_string())?;

        // Create a file in subdir.
        let file2_id = uuid::Uuid::new_v4();
        let mut file2_node = GraphNode::new_file(file2_id, 1000, 1000, 0o644);
        let content2 = b"nested file";
        let hash2 = store.put_object(file2_id, content2)?;
        file2_node.content_hash = hash2;
        file2_node.metadata.size = content2.len() as u64;
        graph.add_node(file2_node);
        graph.add_edge(
            subdir_id,
            file2_id,
            EdgeType::Child,
            "nested.txt".to_string(),
        )?;

        // Write a simple committed transaction to the log.
        {
            let parent_data = bincode::serialize(
                &graph.get_node(&root_id).unwrap(),
            )
            .unwrap();
            let mut log = LogManager::open(&log_path)?;
            log.log_data(
                1,
                LogPayload::AddNode {
                    node_data: parent_data,
                    node_id: root_id,
                },
            )?;
            log.log_data(
                1,
                LogPayload::ContentChange {
                    object_id: file_id,
                    old_hash: None,
                    new_hash: Some(hash),
                },
            )?;
            log.log_end(1)?;
        }

        // Flush everything.
        graph.flush()?;
        store.flush_index()?;

        Ok((dir, root_id))
    }

    #[test]
    fn test_fsck_clean_filesystem() -> io::Result<()> {
        let (dir, _root_id) = setup_test_dir()?;
        let checker = run_fsck(dir.path(), false).expect("fsck should succeed");
        checker.print_results();

        assert_eq!(
            checker.error_count(),
            0,
            "Clean filesystem should have no errors"
        );
        Ok(())
    }

    #[test]
    fn test_fsck_after_recovery() -> io::Result<()> {
        let (dir, _root_id) = setup_test_dir()?;
        let checker = run_fsck(dir.path(), true).expect("fsck should succeed");
        checker.print_results();

        assert_eq!(
            checker.error_count(),
            0,
            "Filesystem after recovery should have no errors"
        );
        Ok(())
    }

    #[test]
    fn test_fsck_empty_data_dir() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let checker = run_fsck(dir.path(), false).expect("fsck should succeed on empty dir");
        checker.print_results();

        // Empty dir = no nodes, no content, no log. Should pass.
        assert_eq!(checker.error_count(), 0);
        Ok(())
    }

    #[test]
    fn test_fsck_nonexistent_dir() {
        let result = run_fsck(Path::new("/nonexistent/assfs/data"), false);
        assert!(result.is_err(), "Should error on nonexistent directory");
    }

    #[test]
    fn test_hex_to_hash_roundtrip() {
        let data = b"roundtrip test";
        let hash = content_hash(data);
        let hex = hash_hex(&hash);
        let parsed = hex_to_hash(&hex);
        assert_eq!(parsed, Some(hash));
    }

    #[test]
    fn test_hex_to_hash_invalid() {
        assert_eq!(hex_to_hash("too_short"), None);
        assert_eq!(hex_to_hash("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"), None);
    }
}
