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
//! See docs/simulator-spec.md for full specification.

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

    fn try_evict(&mut self) -> bool {
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
                        true
                    }
                    None => false,
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
                    true
                } else {
                    false
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
            if !self.try_evict() {
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

    fn count_pinned(&self) -> usize {
        self.cache.values().filter(|e| e.is_pinned()).count()
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

    if cli.sweep {
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
