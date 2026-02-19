# STEAL/NO-FORCE Analysis: Why UNDO/REDO Logging Matters for File Systems

## The Steal/Force Matrix

The classical database recovery taxonomy (from ARIES and predecessors) maps
logging approaches onto two orthogonal buffer management policies:

|  | **FORCE** | **NO-FORCE** |
|---|---|---|
| **NO-STEAL** | Shadow paging (no undo, no redo) | Redo only |
| **STEAL** | Undo only | **Both undo + redo** (ARIES, Episode, AssFS) |

- **STEAL**: Dirty pages from uncommitted transactions may be written to disk
  (evicted from the buffer cache) before the transaction commits.
- **NO-STEAL**: Dirty pages must remain pinned until the transaction commits.
- **FORCE**: All dirty pages must reach disk before commit returns.
- **NO-FORCE**: Commit returns when the log record is stable; dirty pages
  can be written later.

**STEAL + NO-FORCE** is the performance-optimal quadrant: the pager is free
to evict any page at any time (pure caching decisions), and commit doesn't
wait for data I/O — only for log I/O. But it requires both UNDO (to revert
stolen pages from uncommitted transactions) and REDO (to re-apply committed
changes whose data pages haven't reached disk yet).

## Where Modern Filesystems Sit

| Filesystem | Quadrant | Log I/O | Why |
|---|---|---|---|
| **Episode/AssFS** | STEAL + NO-FORCE | Async, callback-driven | Old/new value logging enables both undo and redo |
| **ext4/JBD2** | NO-STEAL + FORCE | Synchronous (submit-wait) | Redo-only WAL; metadata pinned until journal committed |
| **XFS** | NO-STEAL + NO-FORCE | Async (closest to Episode) | Redo-only; metadata pinned via AIL until log LSN stable |
| **btrfs** | NO-STEAL + FORCE | Synchronous | COW = shadow paging; no in-place writes |
| **ZFS** | NO-STEAL + FORCE | Sync (ZIL) / background (TXG) | COW; ZIL exists for sync ops |
| **F2FS** | NO-STEAL + FORCE | Fully synchronous checkpoint | Log-structured; checkpoint freezes FS |

**XFS is the closest** to Episode's model: it submits log I/O asynchronously,
tracks stable LSN via `l_last_sync_lsn` and the iclog state machine, and
bypasses the page cache for log writes. But without UNDO capability, XFS
cannot STEAL — it must pin dirty metadata in the Active Item List (AIL) until
the covering log record reaches stable storage. Dave Chinner got XFS most of
the way there, but the lack of undo prevents the last degree of freedom.

## The Core Insight: Metadata Pages as "Just Cache"

With UNDO/REDO logging (STEAL + NO-FORCE):
- Metadata pages are **pure cache**. The OS pager can evict or flush them
  at any time based on memory pressure, access patterns, or any other policy.
- If a crash occurs, UNDO reverts uncommitted changes (from stolen pages)
  and REDO re-applies committed changes (whose pages hadn't reached disk).
- The only ordering constraint is WAL: the log record must be stable before
  the *commit point*, not before every metadata page write.
- This **decouples the pager from the transaction manager**.

With REDO-only logging (NO-STEAL):
- Dirty metadata pages from active transactions must be **pinned** in the
  buffer cache until the transaction commits and the log record stabilizes.
- Under memory pressure, the pager cannot evict pinned pages, so it evicts
  data pages instead — hurting read performance to protect transaction
  correctness.
- The pinned working set grows with transaction concurrency and duration.
- The pager is **subordinate to the transaction manager**.

## The Camelot Connection

Mike Young (Mach external pager, Encina team) consulted deliberately on
Episode's log design. His regret from Camelot (CMU): new-value-only logging
"saves space but constrains everything." The Camelot distributed logging
paper (Eppinger et al.) discusses redo and sometimes undo components but
the system as built did not fully exploit old/new logging everywhere.

Reference: https://cs-people.bu.edu/mathan/reading-groups/papers-classics/distributed-logging.pdf

Episode's designers made the deliberate choice to log both old and new values
for all metadata mutations. This choice, informed by Mike Young's Camelot
experience, is what enables the STEAL + NO-FORCE policy.

## Simulation Proposal

Compare two policies under controlled conditions:

**Policy A (redo-only + pin)**: XFS-like. Dirty metadata pages are pinned
until the covering log record reaches stable storage. The pager cannot evict
pinned pages.

**Policy B (undo/redo + STEAL)**: Episode/AssFS. Any dirty metadata page can
be evicted at any time. Recovery uses old values from the log to undo
uncommitted changes.

### Parameters to vary:
- **Memory size** (metadata cache capacity)
- **Fsync frequency** (how often durability is forced)
- **Transaction concurrency** (number of overlapping transactions)
- **Transaction duration** (how long pages stay dirty before commit)
- **Workload skew** (uniform vs. hot-spot access patterns)

### Metrics to measure:
- Cache hit rate (overall and for data vs. metadata)
- Fraction of cache consumed by pinned metadata
- Eviction pressure (pages evicted per operation)
- Fsync latency (average and tail)
- Journal traffic and write amplification
- Effective throughput under memory pressure

### Expected results:
Under low memory pressure, the two policies are equivalent. Under high
memory pressure with concurrent transactions, Policy B wins because:
1. No metadata pages are pinned, so the full cache is available for LRU
2. The pager makes pure performance decisions, not correctness decisions
3. Data pages are not evicted to make room for pinned metadata

The crossover point — where STEAL + NO-FORCE starts to dominate — reveals
the practical value of old/new value logging in a file system context.

## Implication for AssFS

AssFS already has the machinery:
- Old/new value logging on all mutations (ContentChange, AddNode, RemoveNode,
  AddEdge, RemoveEdge all carry full state for both undo and redo)
- Transaction manager with undo log entries
- Recovery engine that does both redo (committed) and undo (uncommitted)
- The STEAL + NO-FORCE policy is architecturally available

What we haven't done yet:
- Exploited the STEAL freedom (currently single-threaded, no memory pressure)
- Built the simulator to quantify the advantage
- Compared against a redo-only+pin baseline

## Framing for Paper

"Importing DB-style logging semantics into the FS specifically to improve
OS-level paging behavior" — the novelty is not "we added transactions to a
filesystem" but "we freed the OS pager from being subordinate to the
transaction manager." The ARIES insight (1992) has been standard in databases
for 30+ years but has never been fully absorbed by filesystem designers.

The evidence:
- ext4: synchronous journal, pinned metadata (2006-present)
- XFS: async log but still pins metadata (2010 delayed logging redesign)
- btrfs/ZFS: adopted COW to sidestep the issue entirely
- Episode: had the right answer in 1991, but the codebase was proprietary
  and the insight was never published in a form the FS community absorbed
