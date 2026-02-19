//! Metadata Cache Simulator
//!
//! Compares two buffer management policies for a transactional filesystem's
//! metadata cache:
//!   Policy A (NO-STEAL): pages in an active transaction's working set cannot
//!                        be evicted until that transaction's log write completes
//!   Policy B (STEAL):    any page can be evicted (undo/redo logging)
//!
//! Under Policy A, every page touched (read or written) by an active
//! transaction is pinned in the cache until that transaction commits and
//! its log write completes.  This models the NO-STEAL constraint.
//! Under Policy B, any page can be evicted at any time — if it was dirty
//! from an uncommitted transaction, that is a "stolen write" requiring
//! undo capability.
//!
//! Pin stalls (Policy A) are counted as failed accesses — the page could
//! not be brought into the cache because all eviction candidates are pinned.
//! When deadlock occurs (all active txns stalled, no pending unpins), the
//! least-progressed transaction is aborted to free pages.
//!
//! Unified mode (--unified): models a shared page cache where metadata pages
//! and data pages compete for the same LRU slots.  Pinned metadata under
//! Policy A displaces data pages, degrading user workload performance.
//!
//! See docs/simulator-spec.md and docs/unified-cache-sim-spec.md for details.

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "cache_sim", about = "Metadata cache simulator for transactional filesystem policies")]
struct Cli {
    /// Eviction policy: A = no-steal (pin), B = steal, both = run both
    #[arg(long, default_value = "both")]
    policy: String,

    /// Cache capacity in pages
    #[arg(long, default_value_t = 256)]
    cache_size: usize,

    /// Total metadata pages in the filesystem
    #[arg(long, default_value_t = 10000)]
    num_pages: u64,

    /// Number of transactions to simulate
    #[arg(long, default_value_t = 5000)]
    num_txns: usize,

    /// Max concurrent active transactions
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Pages read per transaction
    #[arg(long, default_value_t = 4)]
    reads: usize,

    /// Pages written per transaction
    #[arg(long, default_value_t = 2)]
    writes: usize,

    /// Zipfian skew parameter (0 = uniform, 1 = very skewed)
    #[arg(long, default_value_t = 0.8)]
    zipf_skew: f64,

    /// Ticks for log write to complete
    #[arg(long, default_value_t = 10)]
    log_latency: u64,

    /// Minimum transaction duration in ticks
    #[arg(long, default_value_t = 5)]
    min_duration: u64,

    /// Maximum transaction duration in ticks
    #[arg(long, default_value_t = 20)]
    max_duration: u64,

    /// RNG seed for reproducibility
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Output results as JSON
    #[arg(long, default_value_t = false)]
    json: bool,

    /// Run a parameter sweep (varies cache size)
    #[arg(long, default_value_t = false)]
    sweep: bool,

    // -----------------------------------------------------------------------
    // Unified cache mode options
    // -----------------------------------------------------------------------

    /// Run unified cache simulation with data workload
    #[arg(long, default_value_t = false)]
    unified: bool,

    /// Data workload type: streaming, database, build
    #[arg(long, default_value = "streaming")]
    data_workload: String,

    /// Streaming: number of concurrent streams
    #[arg(long, default_value_t = 4)]
    num_streams: usize,

    /// Streaming: pages per stream
    #[arg(long, default_value_t = 5000)]
    stream_length: u64,

    /// Streaming: pages prefetched ahead per stream
    #[arg(long, default_value_t = 8)]
    read_ahead: usize,

    /// Database/build: total data pages in address space
    #[arg(long, default_value_t = 50000)]
    data_pages: u64,

    /// Database: reads per tick
    #[arg(long, default_value_t = 4)]
    data_reads_per_tick: usize,

    /// Database: Zipfian skew for data reads
    #[arg(long, default_value_t = 0.8)]
    data_zipf_skew: f64,

    /// Build: pages per burst
    #[arg(long, default_value_t = 6)]
    burst_size: usize,

    /// Run unified parameter sweep (varies cache size)
    #[arg(long, default_value_t = false)]
    unified_sweep: bool,
}

// ---------------------------------------------------------------------------
// Workload generation
// ---------------------------------------------------------------------------

/// Generate a Zipfian page ID using the power-law mapping from the spec:
///   page_id = floor(N * u^(1/(1-s)))
fn zipf_page(rng: &mut StdRng, num_pages: u64, skew: f64) -> u64 {
    let u: f64 = rng.gen_range(0.0001_f64..1.0_f64);
    let exponent = 1.0 / (1.0 - skew);
    let page = (num_pages as f64 * u.powf(exponent)).floor() as u64;
    page.min(num_pages - 1)
}

#[derive(Debug, Clone)]
enum TxnOp {
    Read(u64),
    Write(u64),
}

#[derive(Debug, Clone)]
struct TxnTemplate {
    ops: Vec<TxnOp>,
    duration: u64,
}

fn generate_workload(
    num_txns: usize,
    num_pages: u64,
    reads_per_txn: usize,
    writes_per_txn: usize,
    zipf_skew: f64,
    min_duration: u64,
    max_duration: u64,
    seed: u64,
) -> Vec<TxnTemplate> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut templates = Vec::with_capacity(num_txns);

    for _ in 0..num_txns {
        let mut ops = Vec::with_capacity(reads_per_txn + writes_per_txn);
        for _ in 0..reads_per_txn {
            ops.push(TxnOp::Read(zipf_page(&mut rng, num_pages, zipf_skew)));
        }
        for _ in 0..writes_per_txn {
            ops.push(TxnOp::Write(zipf_page(&mut rng, num_pages, zipf_skew)));
        }
        let duration = rng.gen_range(min_duration..=max_duration);
        templates.push(TxnTemplate { ops, duration });
    }
    templates
}

// ---------------------------------------------------------------------------
// Cache simulation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    A,
    B,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    dirty: bool,
    pinners: HashSet<u64>,
}

impl CacheEntry {
    fn is_pinned(&self) -> bool {
        !self.pinners.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ActiveTxn {
    txn_id: u64,
    remaining_ops: VecDeque<TxnOp>,
    start_tick: u64,
    duration: u64,
    ops_complete: bool,
    stalled: bool,
}

#[derive(Debug, Clone)]
struct PendingCommit {
    txn_id: u64,
    complete_tick: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
struct Metrics {
    total_accesses: u64,
    cache_hits: u64,
    cache_misses: u64,
    evictions: u64,
    dirty_evictions: u64,
    pin_stalls: u64,
    stolen_writes: u64,
    aborted_txns: u64,
    pinned_pages_sum: u64,
    max_pinned_pages: usize,
    ticks_counted: u64,
}

struct Simulator {
    policy: Policy,
    cache_size: usize,
    cache: HashMap<u64, CacheEntry>,
    lru_order: VecDeque<u64>,
    active_txns: Vec<ActiveTxn>,
    pending_commits: Vec<PendingCommit>,
    metrics: Metrics,
    next_txn_id: u64,
    log_latency: u64,
}

impl Simulator {
    fn new(policy: Policy, cache_size: usize, log_latency: u64) -> Self {
        Self {
            policy,
            cache_size,
            cache: HashMap::with_capacity(cache_size),
            lru_order: VecDeque::with_capacity(cache_size),
            active_txns: Vec::new(),
            pending_commits: Vec::new(),
            metrics: Metrics::default(),
            next_txn_id: 0,
            log_latency,
        }
    }

    fn touch_lru(&mut self, page_id: u64) {
        if let Some(pos) = self.lru_order.iter().position(|&p| p == page_id) {
            self.lru_order.remove(pos);
        }
        self.lru_order.push_back(page_id);
    }

    fn try_evict(&mut self) -> Option<u64> {
        match self.policy {
            Policy::A => {
                let evict_pos = self
                    .lru_order
                    .iter()
                    .position(|pid| self.cache.get(pid).map_or(true, |e| !e.is_pinned()));
                match evict_pos {
                    Some(pos) => {
                        let page_id = self.lru_order.remove(pos).unwrap();
                        let entry = self.cache.remove(&page_id).unwrap();
                        self.metrics.evictions += 1;
                        if entry.dirty {
                            self.metrics.dirty_evictions += 1;
                        }
                        Some(page_id)
                    }
                    None => None,
                }
            }
            Policy::B => {
                if let Some(page_id) = self.lru_order.pop_front() {
                    let entry = self.cache.remove(&page_id).unwrap();
                    self.metrics.evictions += 1;
                    if entry.dirty {
                        self.metrics.dirty_evictions += 1;
                        if entry.is_pinned() {
                            self.metrics.stolen_writes += 1;
                        }
                    }
                    Some(page_id)
                } else {
                    None
                }
            }
        }
    }

    /// Access a page. Returns Some(true) on hit, Some(false) on miss+load,
    /// None on pin stall (Policy A only — counted as a failed access).
    fn access_page(&mut self, page_id: u64, is_write: bool, txn_id: u64) -> Option<bool> {
        self.metrics.total_accesses += 1;

        if self.cache.contains_key(&page_id) {
            self.metrics.cache_hits += 1;
            self.touch_lru(page_id);
            let entry = self.cache.get_mut(&page_id).unwrap();
            if is_write {
                entry.dirty = true;
            }
            entry.pinners.insert(txn_id);
            return Some(true);
        }

        // Cache miss — need to load from disk
        self.metrics.cache_misses += 1;

        if self.cache.len() >= self.cache_size {
            if self.try_evict().is_none() {
                // Pin stall: all pages are pinned, cannot evict.
                // This counts as a miss (total_accesses and cache_misses
                // are NOT undone — the access attempt happened and failed).
                self.metrics.pin_stalls += 1;
                return None;
            }
        }

        let mut pinners = HashSet::new();
        pinners.insert(txn_id);
        self.cache.insert(
            page_id,
            CacheEntry {
                dirty: is_write,
                pinners,
            },
        );
        self.lru_order.push_back(page_id);
        Some(false)
    }

    /// Access a data page (no pinning, no transaction association).
    /// Returns (hit: bool, evicted_page: Option<u64>).
    /// On pin stall (Policy A, all pages pinned), returns None.
    fn access_data_page(&mut self, page_id: u64) -> Option<(bool, Option<u64>)> {
        if self.cache.contains_key(&page_id) {
            self.touch_lru(page_id);
            return Some((true, None));
        }

        // Miss — need to load
        let mut evicted = None;
        if self.cache.len() >= self.cache_size {
            match self.try_evict() {
                Some(eid) => evicted = Some(eid),
                None => return None, // pin stall
            }
        }

        self.cache.insert(
            page_id,
            CacheEntry {
                dirty: false,
                pinners: HashSet::new(),
            },
        );
        self.lru_order.push_back(page_id);
        Some((false, evicted))
    }

    /// Check whether a page is currently in the cache.
    fn page_in_cache(&self, page_id: u64) -> bool {
        self.cache.contains_key(&page_id)
    }

    fn count_pinned(&self) -> usize {
        self.cache.values().filter(|e| e.is_pinned()).count()
    }

    /// Count data pages currently in cache (pages >= data_base_id).
    fn count_data_pages(&self, data_base_id: u64) -> usize {
        self.cache.keys().filter(|&&k| k >= data_base_id).count()
    }

    fn unpin_txn(&mut self, txn_id: u64) {
        for entry in self.cache.values_mut() {
            entry.pinners.remove(&txn_id);
        }
    }

    fn abort_txn(&mut self, idx: usize) {
        let txn = self.active_txns.remove(idx);
        self.unpin_txn(txn.txn_id);
        self.metrics.aborted_txns += 1;
    }

    fn run(&mut self, workload: &[TxnTemplate], concurrency: usize) {
        let mut txn_queue: VecDeque<(usize, &TxnTemplate)> =
            workload.iter().enumerate().collect();
        let mut tick: u64 = 0;

        loop {
            // 1. Start new transactions.
            while self.active_txns.len() < concurrency {
                if let Some((_idx, template)) = txn_queue.pop_front() {
                    let txn_id = self.next_txn_id;
                    self.next_txn_id += 1;
                    self.active_txns.push(ActiveTxn {
                        txn_id,
                        remaining_ops: template.ops.iter().cloned().collect(),
                        start_tick: tick,
                        duration: template.duration,
                        ops_complete: false,
                        stalled: false,
                    });
                } else {
                    break;
                }
            }

            if self.active_txns.is_empty() && self.pending_commits.is_empty() {
                break;
            }

            // 2. Each active transaction processes its next operation.
            for i in 0..self.active_txns.len() {
                self.active_txns[i].stalled = false;
                if self.active_txns[i].ops_complete {
                    continue;
                }
                if let Some(op) = self.active_txns[i].remaining_ops.front().cloned() {
                    let txn_id = self.active_txns[i].txn_id;
                    let result = match op {
                        TxnOp::Read(page_id) => self.access_page(page_id, false, txn_id),
                        TxnOp::Write(page_id) => self.access_page(page_id, true, txn_id),
                    };
                    if result.is_some() {
                        self.active_txns[i].remaining_ops.pop_front();
                    } else {
                        self.active_txns[i].stalled = true;
                    }
                } else {
                    self.active_txns[i].ops_complete = true;
                }
            }

            // 3. Expired duration → initiate commit.
            let log_latency = self.log_latency;
            let mut to_commit = Vec::new();
            self.active_txns.retain(|txn| {
                if txn.ops_complete && tick >= txn.start_tick + txn.duration {
                    to_commit.push(PendingCommit {
                        txn_id: txn.txn_id,
                        complete_tick: tick + log_latency,
                    });
                    false
                } else {
                    true
                }
            });
            self.pending_commits.extend(to_commit);

            // 4. Completed log writes → unpin.
            let completed: Vec<PendingCommit> = self
                .pending_commits
                .iter()
                .filter(|pc| tick >= pc.complete_tick)
                .cloned()
                .collect();
            self.pending_commits.retain(|pc| tick < pc.complete_tick);
            for pc in &completed {
                self.unpin_txn(pc.txn_id);
            }

            // 5. Deadlock detection (Policy A only).
            if self.policy == Policy::A && !self.active_txns.is_empty() {
                let all_stuck = self
                    .active_txns
                    .iter()
                    .all(|t| t.stalled || t.ops_complete);
                let any_completing = self.active_txns.iter().any(|t| t.ops_complete);
                let has_pending = !self.pending_commits.is_empty();

                if all_stuck && !any_completing && !has_pending {
                    // True deadlock: abort the txn with most remaining ops
                    if let Some(victim) = self
                        .active_txns
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.stalled)
                        .max_by_key(|(_, t)| t.remaining_ops.len())
                        .map(|(i, _)| i)
                    {
                        self.abort_txn(victim);
                    }
                }
            }

            // 6. Record per-tick metrics.
            let pinned = if self.policy == Policy::A {
                self.count_pinned()
            } else {
                0
            };
            self.metrics.pinned_pages_sum += pinned as u64;
            if pinned > self.metrics.max_pinned_pages {
                self.metrics.max_pinned_pages = pinned;
            }
            self.metrics.ticks_counted += 1;

            tick += 1;
            if tick > 2_000_000 {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct SimResult {
    policy: String,
    policy_description: String,
    cache_size: usize,
    total_accesses: u64,
    cache_hits: u64,
    cache_misses: u64,
    hit_rate_pct: f64,
    evictions: u64,
    dirty_evictions: u64,
    pin_stalls: u64,
    stolen_writes: u64,
    aborted_txns: u64,
    avg_pinned_pages: f64,
    max_pinned_pages: usize,
}

impl SimResult {
    fn from_metrics(metrics: &Metrics, policy: Policy, cache_size: usize) -> Self {
        let hit_rate = if metrics.total_accesses > 0 {
            (metrics.cache_hits as f64 / metrics.total_accesses as f64) * 100.0
        } else {
            0.0
        };
        let avg_pinned = if metrics.ticks_counted > 0 {
            metrics.pinned_pages_sum as f64 / metrics.ticks_counted as f64
        } else {
            0.0
        };
        SimResult {
            policy: match policy {
                Policy::A => "A".into(),
                Policy::B => "B".into(),
            },
            policy_description: match policy {
                Policy::A => "NO-STEAL / pin-until-commit".into(),
                Policy::B => "STEAL / undo-redo".into(),
            },
            cache_size,
            total_accesses: metrics.total_accesses,
            cache_hits: metrics.cache_hits,
            cache_misses: metrics.cache_misses,
            hit_rate_pct: hit_rate,
            evictions: metrics.evictions,
            dirty_evictions: metrics.dirty_evictions,
            pin_stalls: metrics.pin_stalls,
            stolen_writes: metrics.stolen_writes,
            aborted_txns: metrics.aborted_txns,
            avg_pinned_pages: avg_pinned,
            max_pinned_pages: metrics.max_pinned_pages,
        }
    }
}

fn run_simulation(
    policy: Policy,
    cache_size: usize,
    log_latency: u64,
    concurrency: usize,
    workload: &[TxnTemplate],
) -> SimResult {
    let mut sim = Simulator::new(policy, cache_size, log_latency);
    sim.run(workload, concurrency);
    SimResult::from_metrics(&sim.metrics, policy, cache_size)
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn print_single_result(r: &SimResult, cache_size: usize) {
    println!("  Policy {} ({}):", r.policy, r.policy_description);
    println!(
        "    Cache hits:      {} / {} ({:.1}%)",
        r.cache_hits, r.total_accesses, r.hit_rate_pct
    );
    println!("    Cache misses:    {}", r.cache_misses);
    println!("    Evictions:       {}", r.evictions);
    println!("    Dirty evictions: {} (disk writes)", r.dirty_evictions);
    if r.policy == "A" {
        println!("    Pin stalls:      {}", r.pin_stalls);
        println!("    Aborted txns:    {}", r.aborted_txns);
        println!(
            "    Avg pinned pages: {:.1} / {} ({:.1}%)",
            r.avg_pinned_pages,
            cache_size,
            (r.avg_pinned_pages / cache_size as f64) * 100.0
        );
        println!(
            "    Max pinned pages: {} / {} ({:.1}%)",
            r.max_pinned_pages,
            cache_size,
            (r.max_pinned_pages as f64 / cache_size as f64) * 100.0
        );
    } else {
        println!("    Stolen writes:   {}", r.stolen_writes);
        println!("    Pin stalls:      0");
        println!("    Avg pinned pages: 0 / {} (0.0%)", cache_size);
    }
}

fn print_comparison(a: &SimResult, b: &SimResult) {
    let advantage = b.hit_rate_pct - a.hit_rate_pct;
    let extra_dirty = b.dirty_evictions as i64 - a.dirty_evictions as i64;
    println!();
    println!("  Comparison:");
    println!("    Hit rate advantage (B over A): {:+.1}%", advantage);
    println!("    Pin stalls eliminated:         {}", a.pin_stalls);
    println!(
        "    Additional dirty evictions:    {:+} (cost of STEAL)",
        extra_dirty
    );
}

fn print_header(cli: &Cli, cache_size: usize) {
    println!("=== Metadata Cache Simulator ===");
    println!(
        "  Pages: {}, Cache: {}, Transactions: {}",
        cli.num_pages, cache_size, cli.num_txns
    );
    println!(
        "  Concurrency: {}, Reads/txn: {}, Writes/txn: {}",
        cli.concurrency, cli.reads, cli.writes
    );
    println!(
        "  Zipf skew: {:.2}, Log latency: {} ticks",
        cli.zipf_skew, cli.log_latency
    );
    println!();
}

fn print_sweep_table(results: &[(usize, SimResult, SimResult)]) {
    println!(
        "| {:>10} | {:>8} | {:>8} | {:>14} | {:>10} | {:>9} |",
        "Cache Size", "Hit% (A)", "Hit% (B)", "Pin Stalls (A)", "Stolen (B)", "Advantage"
    );
    println!(
        "|{:-<12}|{:-<10}|{:-<10}|{:-<16}|{:-<12}|{:-<11}|",
        "", "", "", "", "", ""
    );
    for (size, a, b) in results {
        let advantage = b.hit_rate_pct - a.hit_rate_pct;
        println!(
            "| {:>10} | {:>7.1}% | {:>7.1}% | {:>14} | {:>10} | {:>+8.1}% |",
            size, a.hit_rate_pct, b.hit_rate_pct, a.pin_stalls, b.stolen_writes, advantage
        );
    }
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonOutput {
    parameters: JsonParams,
    results: Vec<SimResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparison: Option<JsonComparison>,
}

#[derive(Serialize)]
struct JsonParams {
    num_pages: u64,
    cache_size: usize,
    num_txns: usize,
    concurrency: usize,
    reads_per_txn: usize,
    writes_per_txn: usize,
    zipf_skew: f64,
    log_latency: u64,
    seed: u64,
}

#[derive(Serialize)]
struct JsonComparison {
    hit_rate_advantage_pct: f64,
    pin_stalls_eliminated: u64,
    additional_dirty_evictions: i64,
}

#[derive(Serialize)]
struct JsonSweepOutput {
    parameters: JsonSweepParams,
    sweep: Vec<JsonSweepRow>,
}

#[derive(Serialize)]
struct JsonSweepParams {
    num_pages: u64,
    num_txns: usize,
    concurrency: usize,
    reads_per_txn: usize,
    writes_per_txn: usize,
    zipf_skew: f64,
    log_latency: u64,
    seed: u64,
    cache_sizes: Vec<usize>,
}

#[derive(Serialize)]
struct JsonSweepRow {
    cache_size: usize,
    policy_a: SimResult,
    policy_b: SimResult,
    advantage_pct: f64,
}

// ===========================================================================
// Unified cache simulation — data workload sharing cache with metadata
// ===========================================================================

// ---------------------------------------------------------------------------
// Data workload profiles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataWorkloadType {
    Streaming,
    Database,
    Build,
}

impl DataWorkloadType {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "streaming" => DataWorkloadType::Streaming,
            "database" => DataWorkloadType::Database,
            "build" => DataWorkloadType::Build,
            other => {
                eprintln!("Unknown data workload '{}'. Use streaming, database, or build.", other);
                std::process::exit(1);
            }
        }
    }

    fn label(&self) -> &'static str {
        match self {
            DataWorkloadType::Streaming => "streaming",
            DataWorkloadType::Database => "database",
            DataWorkloadType::Build => "build",
        }
    }
}

/// Per-stream state for the streaming workload.
#[derive(Debug, Clone)]
struct StreamState {
    /// Current read position (next page this stream will consume).
    position: u64,
    /// Total pages this stream will read.
    length: u64,
    /// Base page ID for this stream's address space.
    base_page: u64,
    /// Set of page IDs that have been prefetched but not yet consumed.
    prefetched: HashSet<u64>,
}

/// State for the build workload: burst reading.
#[derive(Debug, Clone)]
struct BuildState {
    /// Pages remaining in the current burst.
    burst_remaining: usize,
    /// Pages to read in the current burst (pre-generated).
    burst_pages: VecDeque<u64>,
    /// Ticks to wait before next burst (gap counter).
    gap_remaining: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DataMetrics {
    total_data_accesses: u64,
    data_cache_hits: u64,
    data_cache_misses: u64,
    data_evictions: u64,
    wasted_read_ahead: u64,
    data_pages_in_cache_sum: u64,
    min_data_pages_in_cache: usize,
    max_data_pages_in_cache: usize,
    ticks_counted: u64,
}

// ---------------------------------------------------------------------------
// Unified simulation result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct UnifiedResult {
    policy: String,
    policy_description: String,
    cache_size: usize,
    // Metadata metrics (same as SimResult)
    meta_total_accesses: u64,
    meta_cache_hits: u64,
    meta_cache_misses: u64,
    meta_hit_rate_pct: f64,
    meta_evictions: u64,
    meta_pin_stalls: u64,
    meta_stolen_writes: u64,
    meta_aborted_txns: u64,
    avg_pinned_pages: f64,
    max_pinned_pages: usize,
    // Data metrics
    data_total_accesses: u64,
    data_cache_hits: u64,
    data_cache_misses: u64,
    data_hit_rate_pct: f64,
    data_evictions: u64,
    wasted_read_ahead: u64,
    avg_data_pages_in_cache: f64,
    min_data_pages_in_cache: usize,
    max_data_pages_in_cache: usize,
    effective_data_cache: f64,
    effective_data_cache_pct: f64,
}

// ---------------------------------------------------------------------------
// Unified simulator
// ---------------------------------------------------------------------------

/// Run a unified simulation for a single policy.
fn run_unified_simulation(
    policy: Policy,
    cache_size: usize,
    log_latency: u64,
    concurrency: usize,
    workload: &[TxnTemplate],
    data_workload_type: DataWorkloadType,
    cli: &Cli,
) -> UnifiedResult {
    let mut sim = Simulator::new(policy, cache_size, log_latency);
    let mut data_metrics = DataMetrics {
        min_data_pages_in_cache: usize::MAX,
        ..Default::default()
    };

    // Data page IDs start after metadata pages to avoid collision.
    let data_base_id = cli.num_pages;

    // Use a separate RNG for data workload, seeded deterministically.
    let mut data_rng = StdRng::seed_from_u64(cli.seed.wrapping_add(1000));

    // Initialize data workload state.
    let mut streams: Vec<StreamState> = Vec::new();
    let mut build_state: Option<BuildState> = None;

    match data_workload_type {
        DataWorkloadType::Streaming => {
            // Each stream gets a disjoint portion of the data page space.
            let pages_per_stream = cli.stream_length;
            for s in 0..cli.num_streams {
                streams.push(StreamState {
                    position: 0,
                    length: pages_per_stream,
                    base_page: data_base_id + (s as u64) * pages_per_stream,
                    prefetched: HashSet::new(),
                });
            }
        }
        DataWorkloadType::Build => {
            build_state = Some(BuildState {
                burst_remaining: 0,
                burst_pages: VecDeque::new(),
                gap_remaining: 0,
            });
        }
        DataWorkloadType::Database => {
            // No special state needed; random reads each tick.
        }
    }

    // --- Main simulation loop (mirrors Simulator::run but interleaves data) ---
    let mut txn_queue: VecDeque<(usize, &TxnTemplate)> =
        workload.iter().enumerate().collect();
    let mut tick: u64 = 0;

    loop {
        // 1. Start new transactions.
        while sim.active_txns.len() < concurrency {
            if let Some((_idx, template)) = txn_queue.pop_front() {
                let txn_id = sim.next_txn_id;
                sim.next_txn_id += 1;
                sim.active_txns.push(ActiveTxn {
                    txn_id,
                    remaining_ops: template.ops.iter().cloned().collect(),
                    start_tick: tick,
                    duration: template.duration,
                    ops_complete: false,
                    stalled: false,
                });
            } else {
                break;
            }
        }

        if sim.active_txns.is_empty() && sim.pending_commits.is_empty() {
            break;
        }

        // 2. Metadata workload step.
        for i in 0..sim.active_txns.len() {
            sim.active_txns[i].stalled = false;
            if sim.active_txns[i].ops_complete {
                continue;
            }
            if let Some(op) = sim.active_txns[i].remaining_ops.front().cloned() {
                let txn_id = sim.active_txns[i].txn_id;
                let result = match op {
                    TxnOp::Read(page_id) => sim.access_page(page_id, false, txn_id),
                    TxnOp::Write(page_id) => sim.access_page(page_id, true, txn_id),
                };
                if result.is_some() {
                    sim.active_txns[i].remaining_ops.pop_front();
                } else {
                    sim.active_txns[i].stalled = true;
                }
            } else {
                sim.active_txns[i].ops_complete = true;
            }
        }

        // 3. Data workload step.
        let data_page_accesses = generate_data_accesses(
            data_workload_type,
            &mut streams,
            &mut build_state,
            &mut data_rng,
            data_base_id,
            cli,
        );

        for page_id in &data_page_accesses {
            data_metrics.total_data_accesses += 1;
            match sim.access_data_page(*page_id) {
                Some((true, _)) => {
                    data_metrics.data_cache_hits += 1;
                    // If this was a prefetched page being consumed, remove from prefetch set.
                    if data_workload_type == DataWorkloadType::Streaming {
                        for stream in &mut streams {
                            stream.prefetched.remove(page_id);
                        }
                    }
                }
                Some((false, evicted)) => {
                    data_metrics.data_cache_misses += 1;
                    // Track data-page evictions (evicted page was a data page).
                    if let Some(eid) = evicted {
                        if eid >= data_base_id {
                            data_metrics.data_evictions += 1;
                            // Check if evicted page was a prefetched-but-unconsumed page.
                            if data_workload_type == DataWorkloadType::Streaming {
                                for stream in &mut streams {
                                    if stream.prefetched.remove(&eid) {
                                        data_metrics.wasted_read_ahead += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                None => {
                    // Pin stall on data page access (Policy A, cache full of pinned metadata).
                    data_metrics.data_cache_misses += 1;
                    // A prefetched page that could not even be loaded is wasted intent.
                }
            }
        }

        // Handle streaming prefetch: insert prefetch pages into cache.
        if data_workload_type == DataWorkloadType::Streaming {
            // Collect prefetch requests first (page_id, stream_index).
            let mut prefetch_requests: Vec<(u64, usize)> = Vec::new();
            for (si, stream) in streams.iter().enumerate() {
                if stream.position > stream.length {
                    continue;
                }
                let prefetch_start = stream.position;
                let prefetch_end =
                    (stream.position + cli.read_ahead as u64).min(stream.length);
                for offset in prefetch_start..prefetch_end {
                    let pf_page = stream.base_page + offset;
                    if !stream.prefetched.contains(&pf_page) && !sim.page_in_cache(pf_page) {
                        prefetch_requests.push((pf_page, si));
                    }
                }
            }

            // Execute prefetch requests.
            for (pf_page, si) in prefetch_requests {
                match sim.access_data_page(pf_page) {
                    Some((false, evicted)) => {
                        streams[si].prefetched.insert(pf_page);
                        if let Some(eid) = evicted {
                            if eid >= data_base_id {
                                data_metrics.data_evictions += 1;
                                // Check if evicted page was prefetched by any stream.
                                for stream in &mut streams {
                                    if stream.prefetched.remove(&eid) {
                                        data_metrics.wasted_read_ahead += 1;
                                    }
                                }
                            }
                        }
                    }
                    Some((true, _)) => {
                        // Already in cache.
                        streams[si].prefetched.insert(pf_page);
                    }
                    None => {
                        // Pin stall — cannot prefetch.
                    }
                }
            }

            // Clean up evicted prefetch pages across all streams.
            for stream in &mut streams {
                let still_present: HashSet<u64> = stream
                    .prefetched
                    .iter()
                    .copied()
                    .filter(|p| sim.page_in_cache(*p))
                    .collect();
                let evicted_count = stream.prefetched.len() - still_present.len();
                data_metrics.wasted_read_ahead += evicted_count as u64;
                stream.prefetched = still_present;
            }

            // Advance stream positions (one page per tick per stream).
            for stream in &mut streams {
                if stream.position < stream.length {
                    stream.position += 1;
                }
            }
        }

        // 4. Expired duration -> initiate commit.
        let log_latency = sim.log_latency;
        let mut to_commit = Vec::new();
        sim.active_txns.retain(|txn| {
            if txn.ops_complete && tick >= txn.start_tick + txn.duration {
                to_commit.push(PendingCommit {
                    txn_id: txn.txn_id,
                    complete_tick: tick + log_latency,
                });
                false
            } else {
                true
            }
        });
        sim.pending_commits.extend(to_commit);

        // 5. Completed log writes -> unpin.
        let completed: Vec<PendingCommit> = sim
            .pending_commits
            .iter()
            .filter(|pc| tick >= pc.complete_tick)
            .cloned()
            .collect();
        sim.pending_commits.retain(|pc| tick < pc.complete_tick);
        for pc in &completed {
            sim.unpin_txn(pc.txn_id);
        }

        // 6. Deadlock detection (Policy A only).
        if sim.policy == Policy::A && !sim.active_txns.is_empty() {
            let all_stuck = sim
                .active_txns
                .iter()
                .all(|t| t.stalled || t.ops_complete);
            let any_completing = sim.active_txns.iter().any(|t| t.ops_complete);
            let has_pending = !sim.pending_commits.is_empty();

            if all_stuck && !any_completing && !has_pending {
                if let Some(victim) = sim
                    .active_txns
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.stalled)
                    .max_by_key(|(_, t)| t.remaining_ops.len())
                    .map(|(i, _)| i)
                {
                    sim.abort_txn(victim);
                }
            }
        }

        // 7. Record per-tick metrics.
        let pinned = if sim.policy == Policy::A {
            sim.count_pinned()
        } else {
            0
        };
        sim.metrics.pinned_pages_sum += pinned as u64;
        if pinned > sim.metrics.max_pinned_pages {
            sim.metrics.max_pinned_pages = pinned;
        }
        sim.metrics.ticks_counted += 1;

        let data_in_cache = sim.count_data_pages(data_base_id);
        data_metrics.data_pages_in_cache_sum += data_in_cache as u64;
        if data_in_cache < data_metrics.min_data_pages_in_cache {
            data_metrics.min_data_pages_in_cache = data_in_cache;
        }
        if data_in_cache > data_metrics.max_data_pages_in_cache {
            data_metrics.max_data_pages_in_cache = data_in_cache;
        }
        data_metrics.ticks_counted += 1;

        tick += 1;
        if tick > 2_000_000 {
            break;
        }
    }

    // Fix min if never updated (no ticks).
    if data_metrics.min_data_pages_in_cache == usize::MAX {
        data_metrics.min_data_pages_in_cache = 0;
    }

    build_unified_result(policy, cache_size, &sim.metrics, &data_metrics)
}

/// Generate data page accesses for a single tick.
fn generate_data_accesses(
    workload_type: DataWorkloadType,
    streams: &mut Vec<StreamState>,
    build_state: &mut Option<BuildState>,
    rng: &mut StdRng,
    data_base_id: u64,
    cli: &Cli,
) -> Vec<u64> {
    match workload_type {
        DataWorkloadType::Streaming => {
            // Each stream reads one page at its current position.
            let mut accesses = Vec::new();
            for stream in streams.iter() {
                if stream.position < stream.length {
                    accesses.push(stream.base_page + stream.position);
                }
            }
            accesses
        }
        DataWorkloadType::Database => {
            let mut accesses = Vec::with_capacity(cli.data_reads_per_tick);
            for _ in 0..cli.data_reads_per_tick {
                let page = zipf_page(rng, cli.data_pages, cli.data_zipf_skew);
                accesses.push(data_base_id + page);
            }
            accesses
        }
        DataWorkloadType::Build => {
            let bs = build_state.as_mut().unwrap();
            let mut accesses = Vec::new();

            if bs.gap_remaining > 0 {
                bs.gap_remaining -= 1;
                return accesses;
            }

            if bs.burst_remaining == 0 {
                // Start a new burst.
                let burst_size = cli.burst_size;
                // Generate burst_size pages with moderate locality (Zipfian 0.5).
                let data_pages = if cli.data_pages > 0 {
                    cli.data_pages
                } else {
                    20000
                };
                for _ in 0..burst_size {
                    let page = zipf_page(rng, data_pages, 0.5);
                    bs.burst_pages.push_back(data_base_id + page);
                }
                bs.burst_remaining = burst_size;
            }

            // Read all remaining burst pages this tick (burst = quick succession).
            while bs.burst_remaining > 0 {
                if let Some(page) = bs.burst_pages.pop_front() {
                    accesses.push(page);
                }
                bs.burst_remaining -= 1;
            }

            // After burst completes, set a gap before next burst (3-8 ticks).
            if bs.burst_remaining == 0 && bs.burst_pages.is_empty() {
                bs.gap_remaining = rng.gen_range(3..=8);
            }

            accesses
        }
    }
}

fn build_unified_result(
    policy: Policy,
    cache_size: usize,
    meta_metrics: &Metrics,
    data_metrics: &DataMetrics,
) -> UnifiedResult {
    let meta_hit_rate = if meta_metrics.total_accesses > 0 {
        (meta_metrics.cache_hits as f64 / meta_metrics.total_accesses as f64) * 100.0
    } else {
        0.0
    };
    let data_hit_rate = if data_metrics.total_data_accesses > 0 {
        (data_metrics.data_cache_hits as f64 / data_metrics.total_data_accesses as f64) * 100.0
    } else {
        0.0
    };
    let avg_pinned = if meta_metrics.ticks_counted > 0 {
        meta_metrics.pinned_pages_sum as f64 / meta_metrics.ticks_counted as f64
    } else {
        0.0
    };
    let avg_data_in_cache = if data_metrics.ticks_counted > 0 {
        data_metrics.data_pages_in_cache_sum as f64 / data_metrics.ticks_counted as f64
    } else {
        0.0
    };
    let effective_data_cache = cache_size as f64 - avg_pinned;
    let effective_data_cache_pct = if cache_size > 0 {
        (effective_data_cache / cache_size as f64) * 100.0
    } else {
        0.0
    };

    UnifiedResult {
        policy: match policy {
            Policy::A => "A".into(),
            Policy::B => "B".into(),
        },
        policy_description: match policy {
            Policy::A => "NO-STEAL".into(),
            Policy::B => "STEAL".into(),
        },
        cache_size,
        meta_total_accesses: meta_metrics.total_accesses,
        meta_cache_hits: meta_metrics.cache_hits,
        meta_cache_misses: meta_metrics.cache_misses,
        meta_hit_rate_pct: meta_hit_rate,
        meta_evictions: meta_metrics.evictions,
        meta_pin_stalls: meta_metrics.pin_stalls,
        meta_stolen_writes: meta_metrics.stolen_writes,
        meta_aborted_txns: meta_metrics.aborted_txns,
        avg_pinned_pages: avg_pinned,
        max_pinned_pages: meta_metrics.max_pinned_pages,
        data_total_accesses: data_metrics.total_data_accesses,
        data_cache_hits: data_metrics.data_cache_hits,
        data_cache_misses: data_metrics.data_cache_misses,
        data_hit_rate_pct: data_hit_rate,
        data_evictions: data_metrics.data_evictions,
        wasted_read_ahead: data_metrics.wasted_read_ahead,
        avg_data_pages_in_cache: avg_data_in_cache,
        min_data_pages_in_cache: data_metrics.min_data_pages_in_cache,
        max_data_pages_in_cache: data_metrics.max_data_pages_in_cache,
        effective_data_cache,
        effective_data_cache_pct,
    }
}

// ---------------------------------------------------------------------------
// Unified output formatting
// ---------------------------------------------------------------------------

fn print_unified_header(cli: &Cli, cache_size: usize, workload_type: DataWorkloadType) {
    println!("=== Unified Cache Simulator ===");
    println!(
        "  Total cache: {} pages (metadata + data shared)",
        cache_size
    );
    println!(
        "  Metadata: {} pages, {} txns, concurrency {}",
        cli.num_pages, cli.num_txns, cli.concurrency
    );
    match workload_type {
        DataWorkloadType::Streaming => {
            println!(
                "  Data workload: streaming ({} streams, {} pages each, read-ahead {})",
                cli.num_streams, cli.stream_length, cli.read_ahead
            );
        }
        DataWorkloadType::Database => {
            println!(
                "  Data workload: database ({} pages, {} reads/tick, zipf {:.1})",
                cli.data_pages, cli.data_reads_per_tick, cli.data_zipf_skew
            );
        }
        DataWorkloadType::Build => {
            println!(
                "  Data workload: build ({} pages, burst size {})",
                cli.data_pages, cli.burst_size
            );
        }
    }
    println!();
}

fn print_unified_result(r: &UnifiedResult, workload_type: DataWorkloadType) {
    println!("  Policy {} ({}):", r.policy, r.policy_description);
    println!(
        "    Metadata hit rate:   {:.1}%  ({} / {})",
        r.meta_hit_rate_pct, r.meta_cache_hits, r.meta_total_accesses
    );
    println!(
        "    Data hit rate:       {:.1}%  ({} / {})",
        r.data_hit_rate_pct, r.data_cache_hits, r.data_total_accesses
    );
    println!("    Pin stalls:          {}", r.meta_pin_stalls);
    println!(
        "    Avg pinned pages:    {:.1} / {} ({:.1}%)",
        r.avg_pinned_pages, r.cache_size,
        if r.cache_size > 0 {
            (r.avg_pinned_pages / r.cache_size as f64) * 100.0
        } else {
            0.0
        }
    );
    println!(
        "    Effective data cache: {:.0} / {} ({:.1}%)",
        r.effective_data_cache, r.cache_size, r.effective_data_cache_pct
    );
    if workload_type == DataWorkloadType::Streaming {
        println!("    Wasted read-ahead:   {}", r.wasted_read_ahead);
    }
    if r.policy == "B" {
        println!("    Stolen writes:       {}", r.meta_stolen_writes);
    }
}

fn print_unified_comparison(
    a: &UnifiedResult,
    b: &UnifiedResult,
    workload_type: DataWorkloadType,
) {
    let meta_advantage = b.meta_hit_rate_pct - a.meta_hit_rate_pct;
    let data_advantage = b.data_hit_rate_pct - a.data_hit_rate_pct;
    let cache_recovered = b.effective_data_cache - a.effective_data_cache;
    let cache_recovered_pct = if a.cache_size > 0 {
        (cache_recovered / a.cache_size as f64) * 100.0
    } else {
        0.0
    };

    println!();
    println!("  Comparison:");
    println!("    Metadata advantage:      {:+.1}%", meta_advantage);
    println!(
        "    Data hit rate advantage:  {:+.1}%  <-- THE NUMBER THAT MATTERS",
        data_advantage
    );
    if workload_type == DataWorkloadType::Streaming {
        let wasted_saved = a.wasted_read_ahead as i64 - b.wasted_read_ahead as i64;
        println!("    Wasted read-ahead saved: {}", wasted_saved);
    }
    println!(
        "    Effective cache recovered: {:.0} pages ({:.1}%)",
        cache_recovered, cache_recovered_pct
    );
}

// ---------------------------------------------------------------------------
// Unified JSON output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonUnifiedOutput {
    parameters: JsonUnifiedParams,
    results: Vec<UnifiedResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_comparison: Option<JsonUnifiedComparison>,
}

#[derive(Serialize)]
struct JsonUnifiedParams {
    num_pages: u64,
    cache_size: usize,
    num_txns: usize,
    concurrency: usize,
    reads_per_txn: usize,
    writes_per_txn: usize,
    zipf_skew: f64,
    log_latency: u64,
    seed: u64,
    data_workload: String,
}

#[derive(Serialize)]
struct JsonUnifiedComparison {
    meta_hit_rate_advantage_pct: f64,
    data_hit_rate_advantage_pct: f64,
    effective_cache_recovered: f64,
    effective_cache_recovered_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    wasted_read_ahead_saved: Option<i64>,
}

#[derive(Serialize)]
struct JsonUnifiedSweepOutput {
    parameters: JsonUnifiedSweepParams,
    sweep: Vec<JsonUnifiedSweepRow>,
}

#[derive(Serialize)]
struct JsonUnifiedSweepParams {
    num_pages: u64,
    num_txns: usize,
    concurrency: usize,
    reads_per_txn: usize,
    writes_per_txn: usize,
    zipf_skew: f64,
    log_latency: u64,
    seed: u64,
    data_workload: String,
    cache_sizes: Vec<usize>,
}

#[derive(Serialize)]
struct JsonUnifiedSweepRow {
    cache_size: usize,
    policy_a: UnifiedResult,
    policy_b: UnifiedResult,
    meta_advantage_pct: f64,
    data_advantage_pct: f64,
    pinned_pct: f64,
}

// ---------------------------------------------------------------------------
// Unified sweep table
// ---------------------------------------------------------------------------

fn print_unified_sweep_table(results: &[(usize, UnifiedResult, UnifiedResult)]) {
    println!(
        "| {:>10} | {:>12} | {:>12} | {:>12} | {:>12} | {:>14} | {:>9} |",
        "Cache Size",
        "Meta Hit%(A)",
        "Meta Hit%(B)",
        "Data Hit%(A)",
        "Data Hit%(B)",
        "Data Advantage",
        "Pinned %"
    );
    println!(
        "|{:-<12}|{:-<14}|{:-<14}|{:-<14}|{:-<14}|{:-<16}|{:-<11}|",
        "", "", "", "", "", "", ""
    );
    for (size, a, b) in results {
        let data_advantage = b.data_hit_rate_pct - a.data_hit_rate_pct;
        let pinned_pct = if *size > 0 {
            (a.avg_pinned_pages / *size as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "| {:>10} | {:>11.1}% | {:>11.1}% | {:>11.1}% | {:>11.1}% | {:>+13.1}% | {:>8.1}% |",
            size,
            a.meta_hit_rate_pct,
            b.meta_hit_rate_pct,
            a.data_hit_rate_pct,
            b.data_hit_rate_pct,
            data_advantage,
            pinned_pct
        );
    }
}

// ---------------------------------------------------------------------------
// Unified run functions
// ---------------------------------------------------------------------------

fn run_unified_single(cli: &Cli, workload: &[TxnTemplate]) {
    let workload_type = DataWorkloadType::from_str(&cli.data_workload);

    let a = run_unified_simulation(
        Policy::A,
        cli.cache_size,
        cli.log_latency,
        cli.concurrency,
        workload,
        workload_type,
        cli,
    );
    let b = run_unified_simulation(
        Policy::B,
        cli.cache_size,
        cli.log_latency,
        cli.concurrency,
        workload,
        workload_type,
        cli,
    );

    if cli.json {
        let wasted_saved = if workload_type == DataWorkloadType::Streaming {
            Some(a.wasted_read_ahead as i64 - b.wasted_read_ahead as i64)
        } else {
            None
        };
        let cache_recovered = b.effective_data_cache - a.effective_data_cache;
        let cache_recovered_pct = if cli.cache_size > 0 {
            (cache_recovered / cli.cache_size as f64) * 100.0
        } else {
            0.0
        };
        print_json(&JsonUnifiedOutput {
            parameters: JsonUnifiedParams {
                num_pages: cli.num_pages,
                cache_size: cli.cache_size,
                num_txns: cli.num_txns,
                concurrency: cli.concurrency,
                reads_per_txn: cli.reads,
                writes_per_txn: cli.writes,
                zipf_skew: cli.zipf_skew,
                log_latency: cli.log_latency,
                seed: cli.seed,
                data_workload: workload_type.label().into(),
            },
            results: vec![a.clone(), b.clone()],
            data_comparison: Some(JsonUnifiedComparison {
                meta_hit_rate_advantage_pct: b.meta_hit_rate_pct - a.meta_hit_rate_pct,
                data_hit_rate_advantage_pct: b.data_hit_rate_pct - a.data_hit_rate_pct,
                effective_cache_recovered: cache_recovered,
                effective_cache_recovered_pct: cache_recovered_pct,
                wasted_read_ahead_saved: wasted_saved,
            }),
        });
    } else {
        print_unified_header(cli, cli.cache_size, workload_type);
        print_unified_result(&a, workload_type);
        println!();
        print_unified_result(&b, workload_type);
        print_unified_comparison(&a, &b, workload_type);
    }
}

fn run_unified_sweep(cli: &Cli, workload: &[TxnTemplate]) {
    let workload_type = DataWorkloadType::from_str(&cli.data_workload);
    let cache_sizes = vec![32, 64, 128, 256, 512, 1024, 2048];
    let mut results: Vec<(usize, UnifiedResult, UnifiedResult)> = Vec::new();

    for &size in &cache_sizes {
        let a = run_unified_simulation(
            Policy::A,
            size,
            cli.log_latency,
            cli.concurrency,
            workload,
            workload_type,
            cli,
        );
        let b = run_unified_simulation(
            Policy::B,
            size,
            cli.log_latency,
            cli.concurrency,
            workload,
            workload_type,
            cli,
        );
        results.push((size, a, b));
    }

    if cli.json {
        let sweep_output = JsonUnifiedSweepOutput {
            parameters: JsonUnifiedSweepParams {
                num_pages: cli.num_pages,
                num_txns: cli.num_txns,
                concurrency: cli.concurrency,
                reads_per_txn: cli.reads,
                writes_per_txn: cli.writes,
                zipf_skew: cli.zipf_skew,
                log_latency: cli.log_latency,
                seed: cli.seed,
                data_workload: workload_type.label().into(),
                cache_sizes: cache_sizes.clone(),
            },
            sweep: results
                .iter()
                .map(|(size, a, b)| {
                    let pinned_pct = if *size > 0 {
                        (a.avg_pinned_pages / *size as f64) * 100.0
                    } else {
                        0.0
                    };
                    JsonUnifiedSweepRow {
                        cache_size: *size,
                        policy_a: a.clone(),
                        policy_b: b.clone(),
                        meta_advantage_pct: b.meta_hit_rate_pct - a.meta_hit_rate_pct,
                        data_advantage_pct: b.data_hit_rate_pct - a.data_hit_rate_pct,
                        pinned_pct,
                    }
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&sweep_output).unwrap());
    } else {
        println!("=== Unified Cache Simulator — Parameter Sweep ===");
        println!(
            "  Metadata: {} pages, {} txns, concurrency {}",
            cli.num_pages, cli.num_txns, cli.concurrency
        );
        println!(
            "  Reads/txn: {}, Writes/txn: {}, Zipf skew: {:.2}",
            cli.reads, cli.writes, cli.zipf_skew
        );
        println!(
            "  Log latency: {} ticks, Seed: {}",
            cli.log_latency, cli.seed
        );
        println!("  Data workload: {}", workload_type.label());
        println!();
        print_unified_sweep_table(&results);
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    let workload = generate_workload(
        cli.num_txns,
        cli.num_pages,
        cli.reads,
        cli.writes,
        cli.zipf_skew,
        cli.min_duration,
        cli.max_duration,
        cli.seed,
    );

    if cli.unified_sweep {
        run_unified_sweep(&cli, &workload);
    } else if cli.unified {
        run_unified_single(&cli, &workload);
    } else if cli.sweep {
        run_sweep(&cli, &workload);
    } else {
        run_single(&cli, &workload);
    }
}

fn run_single(cli: &Cli, workload: &[TxnTemplate]) {
    let policy_str = cli.policy.to_lowercase();
    match policy_str.as_str() {
        "a" => {
            let r = run_simulation(Policy::A, cli.cache_size, cli.log_latency, cli.concurrency, workload);
            if cli.json {
                print_json(&JsonOutput { parameters: make_json_params(cli), results: vec![r], comparison: None });
            } else {
                print_header(cli, cli.cache_size);
                print_single_result(&r, cli.cache_size);
            }
        }
        "b" => {
            let r = run_simulation(Policy::B, cli.cache_size, cli.log_latency, cli.concurrency, workload);
            if cli.json {
                print_json(&JsonOutput { parameters: make_json_params(cli), results: vec![r], comparison: None });
            } else {
                print_header(cli, cli.cache_size);
                print_single_result(&r, cli.cache_size);
            }
        }
        "both" => {
            let a = run_simulation(Policy::A, cli.cache_size, cli.log_latency, cli.concurrency, workload);
            let b = run_simulation(Policy::B, cli.cache_size, cli.log_latency, cli.concurrency, workload);
            if cli.json {
                print_json(&JsonOutput {
                    parameters: make_json_params(cli),
                    results: vec![a.clone(), b.clone()],
                    comparison: Some(JsonComparison {
                        hit_rate_advantage_pct: b.hit_rate_pct - a.hit_rate_pct,
                        pin_stalls_eliminated: a.pin_stalls,
                        additional_dirty_evictions: b.dirty_evictions as i64 - a.dirty_evictions as i64,
                    }),
                });
            } else {
                print_header(cli, cli.cache_size);
                print_single_result(&a, cli.cache_size);
                println!();
                print_single_result(&b, cli.cache_size);
                print_comparison(&a, &b);
            }
        }
        other => {
            eprintln!("Unknown policy '{}'. Use A, B, or both.", other);
            std::process::exit(1);
        }
    }
}

fn run_sweep(cli: &Cli, workload: &[TxnTemplate]) {
    let cache_sizes = vec![32, 64, 128, 256, 512, 1024, 2048];
    let mut results: Vec<(usize, SimResult, SimResult)> = Vec::new();

    for &size in &cache_sizes {
        let a = run_simulation(Policy::A, size, cli.log_latency, cli.concurrency, workload);
        let b = run_simulation(Policy::B, size, cli.log_latency, cli.concurrency, workload);
        results.push((size, a, b));
    }

    if cli.json {
        let sweep_output = JsonSweepOutput {
            parameters: JsonSweepParams {
                num_pages: cli.num_pages,
                num_txns: cli.num_txns,
                concurrency: cli.concurrency,
                reads_per_txn: cli.reads,
                writes_per_txn: cli.writes,
                zipf_skew: cli.zipf_skew,
                log_latency: cli.log_latency,
                seed: cli.seed,
                cache_sizes: cache_sizes.clone(),
            },
            sweep: results
                .iter()
                .map(|(size, a, b)| JsonSweepRow {
                    cache_size: *size,
                    policy_a: a.clone(),
                    policy_b: b.clone(),
                    advantage_pct: b.hit_rate_pct - a.hit_rate_pct,
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&sweep_output).unwrap());
    } else {
        println!("=== Metadata Cache Simulator — Parameter Sweep ===");
        println!(
            "  Pages: {}, Transactions: {}, Concurrency: {}",
            cli.num_pages, cli.num_txns, cli.concurrency
        );
        println!(
            "  Reads/txn: {}, Writes/txn: {}, Zipf skew: {:.2}",
            cli.reads, cli.writes, cli.zipf_skew
        );
        println!(
            "  Log latency: {} ticks, Seed: {}",
            cli.log_latency, cli.seed
        );
        println!();
        print_sweep_table(&results);
    }
}

fn print_json<T: Serialize>(val: &T) {
    println!("{}", serde_json::to_string_pretty(val).unwrap());
}

fn make_json_params(cli: &Cli) -> JsonParams {
    JsonParams {
        num_pages: cli.num_pages,
        cache_size: cli.cache_size,
        num_txns: cli.num_txns,
        concurrency: cli.concurrency,
        reads_per_txn: cli.reads,
        writes_per_txn: cli.writes,
        zipf_skew: cli.zipf_skew,
        log_latency: cli.log_latency,
        seed: cli.seed,
    }
}
