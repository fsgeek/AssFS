# AssFS Research Questions

## Context

These questions emerged after completing the MVP prototype (38 tests passing,
including crash injection). The prototype is untuned — no optimization work
has been done. Claims should be carefully scoped: we can demonstrate mechanisms
work, but performance claims require tuning and fair comparison baselines.

## Q1: Old/New Value Logging with EC Merge vs WAL Under Contention

Episode's transaction model uses old/new value logging (not write-ahead logging),
meaning metadata can be written before transaction commits — undo is the safety
net, not a gate. Equivalence class merge allows overlapping transactions to
proceed without blocking, collapsing to checkpoint behavior under heavy load.

**Claim to test:** EC merge provides adaptive concurrency that matches or exceeds
WAL-based journaling under varying contention levels, without the complexity of
lock-based concurrency control.

**What's needed:** EC merge implementation (Phase 2), contention benchmark
(concurrent mkdir/create workloads), comparison against ext4/xfs journal behavior.

**Caution:** AssFS is untuned. Fair comparison requires identifying what we're
measuring (mechanism overhead, not implementation maturity).

## Q2: Graph Namespace Cost and Expressiveness

The storage namespace is a labeled directed graph. POSIX tree semantics are a
projection (spanning tree extraction). This adds overhead for traditional
workloads but enables operations that are awkward or impossible in a tree.

**Claim to test:** The graph namespace overhead is bounded and acceptable for
POSIX workloads, and for graph-native workloads (tagging, multi-parent files,
cross-referencing), the expressiveness gain is significant (fewer operations
per task, simpler application logic).

**What's needed:** POSIX benchmark (mdtest, filebench) for overhead measurement,
custom graph workload (tag-based file organization, dependency tracking) for
expressiveness measurement, comparison against symlink/hardlink-based equivalents.

**Caution:** "Expressiveness" is qualitative unless we define concrete tasks
and measure operation counts / code complexity.

## Q3: Content Addressing Composing with Transactions

Content-addressed storage provides automatic dedup and integrity. Combined
with old/new value logging, the log tuples become
`<operation, old_hash, new_hash>` — undo reinstates old_hash, redo reinstates
new_hash. Content is never overwritten, only the index (ObjectId → ContentHash)
is mutable.

**Claim to test:** Content addressing simplifies recovery (only the index needs
fixing, not the content), provides measurable storage savings on workloads with
redundant content, and composes cleanly with the transaction model.

**What's needed:** Storage savings measurement on real-world workloads (source
trees, document collections), recovery correctness already demonstrated by crash
injection tests, comparison of recovery complexity vs traditional journaling.

**Caution:** Dedup savings are workload-dependent. Need representative workloads,
not cherry-picked ones.

## Q4: Mount-to-Ready Recovery Time

Traditional fsck is O(filesystem size). Journal replay is O(journal size).
AssFS recovery is currently O(total log size) because it replays the entire log.
With checkpoints (Restart records marking persisted state), recovery becomes
O(log since last checkpoint).

**Claim to test:** Recovery time scales with uncommitted work, not filesystem
size, and is competitive with or faster than journal-based recovery for
equivalent workloads.

**What's needed:** Checkpoint implementation, timing harness that measures
mount-to-ready under varying log sizes and filesystem sizes, comparison against
ext4/xfs mount times after crash.

**Caution:** Current recovery replays the entire log — checkpoint implementation
is required before this claim can be evaluated.

## Additional Questions to Explore

- **Q5: Write structure preservation.** Royal/BitRaider insight: preserving
  object boundaries enables predictive prefetch. Can we demonstrate this with
  content-addressed storage?

- **Q6: Reason-tagged reference counting.** Does tracking *why* a reference
  exists (vs just counting) provide debugging/diagnostic value? Does the
  exclusive-lock-on-1→0 pattern perform well under load?

- **Q7: Projection multiplicity.** Can multiple simultaneous projections
  (different spanning trees of the same graph) provide useful views without
  data duplication? What's the overhead of maintaining multiple inode tables?

## Evaluation Pipeline (To Be Designed)

Modeled on yanantin's approach: automated, reproducible, with clear
before/after comparison points.

- Benchmark harness (filebench, mdtest, custom graph workloads)
- Crash injection suite (already started — 4 tests)
- Recovery timing harness
- Storage efficiency measurement
- Contention scaling measurement (post EC merge)
- Comparison baselines (ext4, xfs, btrfs for POSIX; application-level
  alternatives for graph workloads)
