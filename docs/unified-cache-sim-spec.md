# Unified Cache Simulator Extension

## Purpose

Extend `src/bin/cache_sim.rs` to model a **unified page cache** where metadata
pages and data pages share the same LRU cache. This captures the real-world
effect: pinned metadata pages under NO-STEAL displace data pages, degrading
user workload performance.

The existing simulator measures direct metadata cost. This extension measures
**collateral damage** — the impact on user data workloads caused by metadata
pinning.

## What Changes

The extension adds a **data workload** that runs concurrently with the metadata
transaction workload, competing for the same shared cache. A new `--unified`
flag activates this mode.

### Shared Cache Model

The cache now holds two kinds of pages:
- **Metadata pages**: IDs in range `0..num_pages` (same as before)
- **Data pages**: IDs in range `num_pages..(num_pages + num_data_pages)`

Both use the same LRU eviction. Under Policy A, pinned metadata pages cannot
be evicted, so the eviction pressure falls entirely on data pages (and unpinned
metadata pages). Under Policy B, everything is evictable by LRU.

### Data Workload Profiles

Three configurable profiles, selected by `--data-workload`:

**`streaming`** (default): Models a video/media server.
- Sequential scan through a large address space.
- Multiple concurrent streams, each reading sequentially.
- Read-ahead: each stream prefetches `P` pages ahead of its current position.
- A prefetched page evicted before it's consumed counts as **wasted read-ahead**.
- Parameters: `--num-streams` (default 4), `--stream-length` (default 5000 pages),
  `--read-ahead` (default 8 pages).

**`database`**: Models random-read OLTP.
- Random reads with Zipfian distribution (reuses `zipf_page()`).
- Separate data page space, separate skew parameter.
- Parameters: `--data-pages` (default 50000), `--data-zipf-skew` (default 0.8),
  `--data-reads-per-tick` (default 4).

**`build`**: Models a build server (many small file reads).
- Burst reads: groups of 4-8 pages read in quick succession, then a gap.
- Moderate locality (Zipfian skew 0.5).
- Parameters: `--data-pages` (default 20000), `--burst-size` (default 6).

### Simulation Loop Changes

Each tick now processes BOTH workloads:
```
for tick in 0..total_ticks:
    1. Metadata workload step (same as existing code)
    2. Data workload step:
       - Generate data page accesses for this tick (profile-dependent)
       - For each data page access:
         - If in cache: hit (touch LRU)
         - If not in cache: miss, evict if full (same LRU, same pin rules)
    3. Record separate metrics for metadata and data pages
```

### New Metrics

In addition to existing metadata metrics, track:

```rust
struct DataMetrics {
    total_data_accesses: u64,
    data_cache_hits: u64,
    data_cache_misses: u64,
    data_evictions: u64,           // data pages evicted to make room
    wasted_read_ahead: u64,        // streaming: prefetched but evicted before use
    data_pages_in_cache: u64,      // sum per tick (for averaging)
    min_data_pages_in_cache: usize,
    max_data_pages_in_cache: usize,
}
```

Key derived metric: **effective cache fraction** — what fraction of the cache
is actually available for data pages? Under Policy A with 22% pinned metadata,
the effective data cache is 78% of nominal. Under Policy B it's ~100%.

### CLI Additions

```
  --unified                 Run unified cache simulation with data workload
  --data-workload <TYPE>    Data workload: streaming, database, build [default: streaming]
  --num-streams <N>         Streaming: concurrent streams [default: 4]
  --stream-length <N>       Streaming: pages per stream [default: 5000]
  --read-ahead <N>          Streaming: pages prefetched ahead [default: 8]
  --data-pages <N>          Database/build: total data pages [default: 50000]
  --data-reads-per-tick <N> Database: reads per tick [default: 4]
  --data-zipf-skew <F>      Database: skew for data reads [default: 0.8]
  --burst-size <N>          Build: pages per burst [default: 6]
  --unified-sweep           Sweep cache size with unified model
```

### Output Format

#### Human-readable (--unified without --json)

```
=== Unified Cache Simulator ===
  Total cache: 256 pages (metadata + data shared)
  Metadata: 10000 pages, 5000 txns, concurrency 8
  Data workload: streaming (4 streams, 5000 pages each, read-ahead 8)

  Policy A (NO-STEAL):
    Metadata hit rate:   35.3%  (10599 / 30000)
    Data hit rate:       62.1%  (12420 / 20000)
    Pin stalls:          0
    Avg pinned pages:    57.3 / 256 (22.4%)
    Effective data cache: 199 / 256 (77.6%)
    Wasted read-ahead:   1823

  Policy B (STEAL):
    Metadata hit rate:   35.3%  (10599 / 30000)
    Data hit rate:       71.8%  (14360 / 20000)
    Pin stalls:          0
    Stolen writes:       0
    Effective data cache: 256 / 256 (100.0%)
    Wasted read-ahead:   412

  Comparison:
    Metadata advantage:      +0.0%
    Data hit rate advantage:  +9.7%  ← THE NUMBER THAT MATTERS
    Wasted read-ahead saved: 1411
    Effective cache recovered: 57 pages (22.4%)
```

#### Unified sweep (--unified-sweep)

```
| Cache Size | Meta Hit% (A) | Meta Hit% (B) | Data Hit% (A) | Data Hit% (B) | Data Advantage | Pinned % |
```

This table shows that even when metadata hit rates are identical (generous
cache), the data hit rate suffers under Policy A because pinned pages reduce
the effective data cache.

### JSON output

Same structure as existing JSON but with additional `data_metrics` and
`data_comparison` fields in the results.

## Implementation Notes

- The existing `CacheEntry` struct needs a `is_data: bool` field (or use
  the page ID range to distinguish).
- The `access_page` method works unchanged — data pages are just pages with
  IDs in the data range. No pinning on data pages (they're not part of
  transactions).
- For streaming: maintain per-stream state (current position, prefetch buffer).
  Each tick, each stream advances one page and prefetches `P` ahead.
- Wasted read-ahead: track which pages were prefetched. If a prefetched page
  is evicted before the stream reaches it, increment the counter.
- The unified sweep should use the same cache sizes as the existing sweep:
  [32, 64, 128, 256, 512, 1024, 2048].
- Keep the existing non-unified mode working. The `--unified` flag activates
  the new behavior.

## Success Criteria

1. Under tight unified cache (32-64 pages), Policy B shows substantially
   higher data hit rates than Policy A, even if metadata hit rates are similar.
2. For streaming workloads, wasted read-ahead is dramatically higher under
   Policy A because prefetched data pages are evicted to make room for
   pinned metadata.
3. Under generous cache, the difference shrinks but does NOT vanish as
   quickly as in the metadata-only simulation — because pinned metadata
   still consumes a fixed fraction of the cache.
4. The streaming workload produces the most dramatic results (confirming
   Tony's video server intuition).
5. All existing (non-unified) functionality continues to work unchanged.
