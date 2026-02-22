//! Concurrent transaction test harness for EC merge.
//!
//! Spawns N worker threads, each running transactions against a shared
//! TransactionManager, ContentStore, and GraphNamespace. Creates tlock
//! conflicts on "hot" directory nodes that trigger EC merge.
//!
//! This bypasses FUSE entirely — it exercises the transaction layer
//! directly to demonstrate and measure EC merge under contention.
//!
//! Usage:
//!   cargo run --bin concurrent_harness -- --workers 4 --txns-per-worker 50 --hot-dirs 3
//!
//! Concurrency design — three-phase protocol per transaction:
//!
//!   Phase 1 (acquire txn_mgr lock):
//!     start_txn -> release lock
//!     Multiple transactions now active concurrently.
//!
//!   Phase 2 (NO txn_mgr lock — concurrent with other threads):
//!     Apply graph mutations (graph has internal Mutex).
//!     Register content store entries (store has internal Mutex).
//!     Touch tlock on parent directory (tlock has internal Mutex).
//!     The tlock records our txn_id as the current holder and returns
//!     any previous holder — this is the conflict signal.
//!
//!   Phase 3 (acquire txn_mgr lock):
//!     Log mutations (AddNode, AddEdge).
//!     If tlock returned a conflict AND the conflicting txn is still active,
//!       call ec_merge.
//!     Call end_txn (EC-aware commit).
//!     Release lock.
//!     Release tlock.
//!
//!   EC merge triggers because between Phase 1 and Phase 3, multiple threads
//!   can touch the same tlock. Thread A touches dir_0 (becomes holder).
//!   Thread B touches dir_0 (sees A as previous holder → conflict).
//!   Thread B enters Phase 3, merges with A if A hasn't committed yet.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::Parser;
use rand::Rng;

use assfs::graph::node::GraphNode;
use assfs::graph::operations::GraphNamespace;
use assfs::store::content::ContentStore;
use assfs::txn::tlock::TlockTable;
use assfs::txn::transaction::TransactionManager;
use assfs::{EdgeType, ObjectId};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "concurrent-harness",
    about = "Concurrent transaction test harness for EC merge"
)]
struct Cli {
    /// Number of worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Transactions per worker.
    #[arg(long, default_value_t = 100)]
    txns_per_worker: usize,

    /// Number of hot directories (contention targets).
    #[arg(long, default_value_t = 3)]
    hot_dirs: usize,

    /// Print per-transaction details.
    #[arg(short, long)]
    verbose: bool,

    /// Disable fsync on log writes (faster, for measurement).
    #[arg(long)]
    no_sync: bool,
}

// ---------------------------------------------------------------------------
// Shared counters
// ---------------------------------------------------------------------------

struct SharedCounters {
    total_txns: AtomicU64,
    ec_merges: AtomicU64,
    max_ec_size: AtomicU64,
}

impl SharedCounters {
    fn new() -> Self {
        Self {
            total_txns: AtomicU64::new(0),
            ec_merges: AtomicU64::new(0),
            max_ec_size: AtomicU64::new(1),
        }
    }

    fn record_merge(&self) {
        self.ec_merges.fetch_add(1, Ordering::Relaxed);
    }

    fn update_max_ec_size(&self, size: u64) {
        let mut current = self.max_ec_size.load(Ordering::Relaxed);
        while size > current {
            match self.max_ec_size.compare_exchange_weak(
                current,
                size,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    // Create a temporary directory for the filesystem state.
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let root = tmp_dir.path().to_path_buf();

    // Ensure blobs directory exists for ContentStore.
    std::fs::create_dir_all(root.join("blobs")).expect("failed to create blobs dir");

    println!("=== AssFS Concurrent Transaction Harness ===");
    println!(
        "  Workers: {}, Txns/worker: {}, Hot dirs: {}",
        cli.workers, cli.txns_per_worker, cli.hot_dirs
    );
    println!("  Data dir: {}", root.display());
    println!();

    // Initialize shared state.
    let log_path = root.join("txn.log");
    let mut txn_mgr =
        TransactionManager::open(&log_path).expect("failed to open transaction manager");
    if cli.no_sync {
        txn_mgr.log.no_sync = true;
    }
    let txn_mgr = Arc::new(Mutex::new(txn_mgr));
    let graph = Arc::new(GraphNamespace::open(&root).expect("failed to open graph namespace"));
    let store = Arc::new(ContentStore::open(&root).expect("failed to open content store"));
    let tlocks = Arc::new(TlockTable::new());
    let counters = Arc::new(SharedCounters::new());

    // Create root directory and hot directory nodes.
    let root_id = graph.init_root(0, 0);
    store.register_empty(root_id);

    let mut hot_dir_ids: Vec<ObjectId> = Vec::with_capacity(cli.hot_dirs);
    {
        let mut tm = txn_mgr.lock().unwrap();
        for i in 0..cli.hot_dirs {
            let dir_id = uuid::Uuid::new_v4();
            let dir_node = GraphNode::new_dir(dir_id, 0, 0, 0o755);
            let node_data =
                bincode::serialize(&dir_node).expect("failed to serialize directory node");

            let txn_id = tm.start_txn().expect("failed to start setup txn");
            tm.log_add_node(txn_id, dir_id, node_data)
                .expect("failed to log add node");
            tm.log_add_edge(
                txn_id,
                root_id,
                dir_id,
                EdgeType::Child,
                format!("hotdir_{}", i),
            )
            .expect("failed to log add edge");
            tm.end_txn(txn_id).expect("failed to end setup txn");

            graph.add_node(dir_node);
            graph
                .add_edge(root_id, dir_id, EdgeType::Child, format!("hotdir_{}", i))
                .expect("failed to add edge to graph");
            store.register_empty(dir_id);

            hot_dir_ids.push(dir_id);
            println!("  Created hot dir {}: {}", i, dir_id);
        }
    }
    println!();

    // Spawn workers.
    let start = Instant::now();
    let verbose = cli.verbose;
    let txns_per_worker = cli.txns_per_worker;

    let handles: Vec<_> = (0..cli.workers)
        .map(|worker_id| {
            let txn_mgr = Arc::clone(&txn_mgr);
            let graph = Arc::clone(&graph);
            let store = Arc::clone(&store);
            let tlocks = Arc::clone(&tlocks);
            let counters = Arc::clone(&counters);
            let hot_dirs = hot_dir_ids.clone();

            std::thread::spawn(move || {
                let mut rng = rand::thread_rng();
                let mut local_merges = 0u64;
                let mut local_max_ec = 1u64;

                for txn_num in 0..txns_per_worker {
                    // Pick a random hot directory.
                    let dir_idx = rng.gen_range(0..hot_dirs.len());
                    let parent_id = hot_dirs[dir_idx];

                    // Create a new file node.
                    let file_id = uuid::Uuid::new_v4();
                    let file_node = GraphNode::new_file(file_id, 0, 0, 0o644);
                    let node_data = bincode::serialize(&file_node)
                        .expect("failed to serialize file node");
                    let label = format!("w{}_f{}", worker_id, txn_num);

                    // ===== Phase 1: Start transaction =====
                    let txn_id = {
                        let mut tm = txn_mgr.lock().unwrap();
                        tm.start_txn().expect("failed to start txn")
                    };
                    // Lock released. Multiple transactions are now active.

                    // ===== Phase 2: Graph/store mutations + tlock (concurrent) =====
                    // STEAL semantics: mutations visible before commit.
                    graph.add_node(file_node);
                    graph
                        .add_edge(parent_id, file_id, EdgeType::Child, label.clone())
                        .expect("failed to add edge to graph");
                    store.register_empty(file_id);

                    // Touch tlock on parent directory. If another active txn
                    // already holds this tlock, we get its txn_id back.
                    let conflict = tlocks.touch(parent_id, txn_id);

                    // ===== Phase 3: Log + conflict resolution + commit =====
                    {
                        let mut tm = txn_mgr.lock().unwrap();

                        // Log the graph mutations.
                        tm.log_add_node(txn_id, file_id, node_data)
                            .expect("failed to log add node");
                        tm.log_add_edge(
                            txn_id,
                            parent_id,
                            file_id,
                            EdgeType::Child,
                            label.clone(),
                        )
                        .expect("failed to log add edge");

                        // If we detected a conflict, try to merge.
                        if let Some(other_txn_id) = conflict {
                            // Both must still be active for merge to succeed.
                            if tm.active.contains_key(&txn_id)
                                && tm.active.contains_key(&other_txn_id)
                            {
                                let head_before = tm.find_ec_head(txn_id);
                                tm.ec_merge(txn_id, other_txn_id);
                                let head_after = tm.find_ec_head(txn_id);
                                let ec_size = tm
                                    .active
                                    .get(&head_after)
                                    .map_or(1, |t| t.ec_size as u64);

                                local_merges += 1;
                                if ec_size > local_max_ec {
                                    local_max_ec = ec_size;
                                }
                                counters.record_merge();
                                counters.update_max_ec_size(ec_size);

                                if verbose {
                                    println!(
                                        "  [W{}] txn {} conflict with txn {} on dir {} -> EC merge (head {} -> {}, size {})",
                                        worker_id, txn_id, other_txn_id, dir_idx,
                                        head_before, head_after, ec_size
                                    );
                                }
                            }
                        }

                        // End transaction (EC-aware commit).
                        match tm.end_txn(txn_id) {
                            Ok(pending_writes) => {
                                if !pending_writes.is_empty() {
                                    store
                                        .flush_pending(&pending_writes)
                                        .expect("failed to flush pending writes");
                                }
                            }
                            Err(e) => {
                                // Transaction was already committed as part of
                                // an EC by another thread's end_txn call.
                                if verbose {
                                    println!(
                                        "  [W{}] txn {} already committed by EC ({})",
                                        worker_id, txn_id, e
                                    );
                                }
                            }
                        }
                    }

                    // Release tlock after commit.
                    tlocks.release_all(txn_id);

                    counters.total_txns.fetch_add(1, Ordering::Relaxed);
                }

                if verbose {
                    println!(
                        "  [W{}] Done: {} txns, {} merges, max EC size {}",
                        worker_id, txns_per_worker, local_merges, local_max_ec
                    );
                }
            })
        })
        .collect();

    // Wait for all workers.
    for h in handles {
        h.join().expect("worker thread panicked");
    }
    let elapsed = start.elapsed();

    // Summary.
    let total = counters.total_txns.load(Ordering::Relaxed);
    let merges = counters.ec_merges.load(Ordering::Relaxed);
    let max_ec = counters.max_ec_size.load(Ordering::Relaxed);
    let tps = total as f64 / elapsed.as_secs_f64();

    println!();
    println!("=== Results ===");
    println!("  Total transactions:  {}", total);
    println!("  EC merges triggered: {}", merges);
    println!("  Max EC size:         {}", max_ec);
    println!("  Elapsed:             {:.3}s", elapsed.as_secs_f64());
    println!("  Throughput:          {:.1} txns/sec", tps);
    println!();

    // Tlock access distribution.
    let summary = tlocks.access_summary();
    println!("=== Tlock Access Distribution ===");
    print!("{}", summary);
    println!();

    // Detailed distribution.
    let dist = tlocks.access_distribution();
    println!("=== Per-Node Touch Counts ===");
    for (node_id, count) in &dist {
        let label = if *node_id == root_id {
            "root".to_string()
        } else if let Some(pos) = hot_dir_ids.iter().position(|id| id == node_id) {
            format!("hotdir_{}", pos)
        } else {
            format!("file {}", &node_id.to_string()[..8])
        };
        println!("  {}: {} touches", label, count);
    }

    // Verify graph integrity.
    println!();
    let expected_files = cli.workers * cli.txns_per_worker;
    let total_nodes = {
        let state = graph.state.lock().unwrap();
        state.nodes.len()
    };
    let expected_nodes = 1 + cli.hot_dirs + expected_files;
    println!("=== Graph Integrity ===");
    println!(
        "  Expected nodes: {} (1 root + {} dirs + {} files)",
        expected_nodes, cli.hot_dirs, expected_files
    );
    println!("  Actual nodes:   {}", total_nodes);
    if total_nodes == expected_nodes {
        println!("  Status: OK");
    } else {
        println!("  Status: MISMATCH");
    }

    // Check remaining active transactions.
    {
        let tm = txn_mgr.lock().unwrap();
        let active = tm.active.len();
        if active > 0 {
            println!();
            println!(
                "  WARNING: {} transactions still active (leaked by EC commit protocol)",
                active
            );
            for (txn_id, txn) in &tm.active {
                println!(
                    "    txn {}: status={:?}, ec_head={}, ec_size={}",
                    txn_id, txn.status, txn.ec_head, txn.ec_size
                );
            }
            println!();
            println!("  NOTE: This indicates a race in the EC merge/commit protocol.");
            println!("  When end_txn for a member runs concurrently with ec_merge");
            println!("  that folds additional transactions in, the ec_size accounting");
            println!("  can become inconsistent. This is a known challenge.");
        } else {
            println!("  Active transactions remaining: 0 (clean shutdown)");
        }
    }

    // Clean shutdown.
    drop(tmp_dir);
}
