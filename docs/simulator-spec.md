# Metadata Cache Simulator Specification

## Purpose

Compare two buffer management policies for a transactional filesystem's
metadata cache, to quantify the advantage of UNDO/REDO logging (STEAL +
NO-FORCE) over REDO-only logging (NO-STEAL + NO-FORCE) under memory pressure.

## Location

`src/bin/cache_sim.rs` — a standalone binary in the existing AssFS crate.
Uses `clap` for CLI argument parsing. Add `clap = { version = "4", features = ["derive"] }` to `[dependencies]` in Cargo.toml.

## Core Model

### Pages

The simulated filesystem has `N` metadata pages (configurable, default 10000).
Each page has:
- `page_id: u64`
- `dirty: bool` — modified since last flush
- `pinned_by: Option<TxnId>` — which transaction dirtied this page (Policy A only)

### Cache

A fixed-size cache holding `C` pages (configurable, default 256).
Uses **LRU eviction** with one constraint under Policy A:
- **Policy A (NO-STEAL)**: Pages where `pinned_by` is an active (uncommitted)
  transaction CANNOT be evicted. If all candidates are pinned, the operation
  stalls (counted as a "pin stall").
- **Policy B (STEAL)**: Any page can be evicted. If a dirty page from an
  uncommitted transaction is evicted, it's a "stolen write" (counted).

On cache miss: load the page (counts as a "disk read"). On eviction of a
dirty page: counts as a "disk write". Eviction of a clean page is free.

### Transactions

Each transaction:
- Has an ID and a status (active, committed, aborted).
- Touches a sequence of pages (reads and writes). The page set is determined
  by the workload generator.
- Has a duration in "ticks" (simulated time steps). During this time it holds
  its pages dirty.
- Commits after its duration expires: the log record is "written" (simulated
  latency of `L` ticks for fsync), and after that, the transaction's pages
  are unpinned (Policy A) or simply marked committed.

### Log

The log is not a real log — it's a simulated latency. When a transaction
commits:
- A log write is initiated (takes `L` ticks to complete, configurable).
- Under NO-FORCE: the commit returns immediately, log completes in background.
  Pages are unpinned when the log write completes (Policy A).
- Under FORCE (not simulated — both policies use NO-FORCE for the log itself,
  the difference is in page pinning).

### Workload Generator

Generate a stream of transactions. Each transaction:
1. Starts at a random tick.
2. Reads `R` pages and writes `W` pages (configurable).
3. Page selection follows a **Zipfian distribution** with configurable skew
   (default 0.8) to create hot spots — realistic for metadata workloads
   where some directories/inodes are accessed far more than others.
4. Duration is drawn from a uniform distribution [min_duration, max_duration].

Concurrency: at any tick, up to `T` transactions may be active simultaneously
(configurable, default 8).

### Simulation Loop

```
for tick in 0..total_ticks:
    1. Start new transactions if concurrency < T and workload has more.
    2. For each active transaction, process next operation:
       - Read page: bring into cache if missing (cache miss → evict if full).
       - Write page: bring into cache, mark dirty, set pinned_by (Policy A).
    3. Check for transactions whose duration has expired → initiate commit.
    4. Check for log writes that have completed → unpin pages (Policy A),
       mark transaction committed.
    5. Record metrics for this tick.
```

## CLI Interface

```
cache_sim [OPTIONS]

Options:
  --policy <A|B>       Eviction policy: A = no-steal (pin), B = steal [default: both]
  --cache-size <N>     Cache capacity in pages [default: 256]
  --num-pages <N>      Total metadata pages in the filesystem [default: 10000]
  --num-txns <N>       Number of transactions to simulate [default: 5000]
  --concurrency <N>    Max concurrent active transactions [default: 8]
  --reads <N>          Pages read per transaction [default: 4]
  --writes <N>         Pages written per transaction [default: 2]
  --zipf-skew <F>      Zipfian skew parameter (0 = uniform, 1 = very skewed) [default: 0.8]
  --log-latency <N>    Ticks for log write to complete [default: 10]
  --min-duration <N>   Minimum transaction duration in ticks [default: 5]
  --max-duration <N>   Maximum transaction duration in ticks [default: 20]
  --seed <N>           RNG seed for reproducibility [default: 42]
  --json               Output results as JSON
  --sweep              Run a parameter sweep (varies cache size)
```

When `--policy` is omitted or set to `both`, run both policies with the same
workload (same RNG seed) and print a comparison.

When `--sweep` is specified, run both policies for cache sizes
[32, 64, 128, 256, 512, 1024, 2048] and output a table showing how the
gap changes with memory pressure.

## Output

### Human-readable (default)

```
=== Metadata Cache Simulator ===
  Pages: 10000, Cache: 256, Transactions: 5000
  Concurrency: 8, Reads/txn: 4, Writes/txn: 2
  Zipf skew: 0.80, Log latency: 10 ticks

  Policy A (NO-STEAL / pin-until-commit):
    Cache hits:      38421 / 45000 (85.4%)
    Cache misses:    6579
    Evictions:       6323
    Dirty evictions: 2891 (disk writes)
    Pin stalls:      347
    Avg pinned pages: 12.4 / 256 (4.8%)
    Max pinned pages: 31 / 256 (12.1%)

  Policy B (STEAL / undo-redo):
    Cache hits:      39102 / 45000 (86.9%)
    Cache misses:    5898
    Evictions:       5642
    Dirty evictions: 3210 (disk writes)
    Stolen writes:   418
    Pin stalls:      0
    Avg pinned pages: 0 / 256 (0.0%)

  Comparison:
    Hit rate advantage (B over A): +1.5%
    Pin stalls eliminated:         347
    Additional dirty evictions:    +319 (cost of STEAL)
```

### JSON (--json flag)

Emit the same data as a JSON object for programmatic consumption.
Include all raw counters plus derived metrics.

### Sweep output

A markdown table:

```
| Cache Size | Hit% (A) | Hit% (B) | Pin Stalls (A) | Stolen (B) | Advantage |
|------------|----------|----------|----------------|------------|-----------|
|         32 |    52.1% |    61.3% |           2341 |        891 |    +9.2%  |
|         64 |    68.4% |    73.1% |            982 |        654 |    +4.7%  |
|        128 |    79.2% |    81.8% |            412 |        503 |    +2.6%  |
|        256 |    85.4% |    86.9% |            347 |        418 |    +1.5%  |
|        512 |    92.1% |    92.6% |             89 |        287 |    +0.5%  |
|       1024 |    96.3% |    96.4% |             12 |        134 |    +0.1%  |
|       2048 |    98.8% |    98.8% |              0 |         61 |    +0.0%  |
```

## Dependencies to Add

```toml
clap = { version = "4", features = ["derive"] }
rand = "0.8"
rand_distr = "0.4"   # for Zipf distribution
```

## Implementation Notes

- Use a `HashMap<u64, CacheEntry>` for the cache, with a `VecDeque<u64>` for
  LRU ordering. On access, move to back. On eviction, scan from front,
  skipping pinned entries (Policy A).
- The Zipfian distribution can be approximated: generate a random float `u`
  in (0,1), compute `page_id = floor(N * u^(1/(1-s)))` where `s` is the
  skew parameter. Or use `rand_distr::Zipf`.
- Keep the simulation deterministic: use `rand::SeedableRng` with the
  configured seed.
- The workload should be generated in advance (or lazily with a seeded RNG)
  so both policies see identical page access sequences.
- This is a simulator, not production code. Correctness of the model matters
  more than performance. Aim for clarity.

## What This Does NOT Simulate

- Actual disk I/O latency (we use abstract "ticks")
- Log space management / admission control
- Recovery (we're measuring steady-state cache behavior, not crash recovery)
- Data pages (only metadata pages are modeled)
- Multi-threaded contention (the simulation is single-threaded and tick-based)

## Success Criteria

The simulator builds, runs, and produces reproducible results showing:
1. Under tight cache (small cache size), Policy B (STEAL) has measurably
   higher hit rates than Policy A (NO-STEAL) due to pin stalls.
2. Under generous cache (large cache size), the difference vanishes.
3. The sweep output clearly shows the crossover point.
4. Results are reproducible given the same seed.
