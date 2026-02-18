# AssFS MVP — Scope and Roadmap

## What the MVP Does

Mount it, and you can: create, read, write, and delete files. Make directories.
Hard-link files (demonstrating the associative property through POSIX semantics).
Rename files and directories. Kill the process and remount — crash recovery
restores consistency by redoing committed transactions and undoing uncommitted
ones.

## What's In vs. Deferred

| Component | MVP | Phase 2 | Phase 3 | Future |
|---|---|---|---|---|
| Content store (file-per-object) | Yes | | | |
| Log manager (sync writes) | Yes | Async + io_window | | |
| Transaction manager (single-threaded) | Yes | EC merge, BEND records | | |
| Recovery (redo/undo on mount) | Yes | | | |
| Graph namespace (core ops) | Yes | | | |
| Reason-tagged reference counting | Yes | | | |
| Hierarchical projection (single) | Yes | | Multiple projections | |
| FUSE adapter (15 ops) | Yes | | | |
| Undo-zero transactions | | Yes | | |
| Transparent encryption | | | Yes | |
| Finesse bypass channel | | | Yes | |
| Predictive prefetch | | | | Yes |
| Block-level COW for large files | | | | Yes |

## Project Structure

```
assfs/
  Cargo.toml
  src/
    main.rs                  # CLI, FUSE session setup
    lib.rs                   # Core types: ContentHash, ObjectId, EdgeType, NodeType
    fuse_ops/
      mod.rs                 # Filesystem impl: 15 FUSE ops wired to graph + store
    graph/
      mod.rs
      node.rs                # GraphNode, NodeMetadata
      edge.rs                # GraphEdge, EdgeIndex (BTreeMap)
      operations.rs          # GraphNamespace: add/remove nodes+edges, resolve, neighbors
    store/
      mod.rs
      backend.rs             # StorageBackend trait
      content.rs             # ContentStore: object index + blob storage
      file_backend.rs        # File-per-object with two-level hex dirs
    txn/
      mod.rs
      log.rs                 # LogManager: framed sequential append, sync writes
      transaction.rs         # TransactionManager: start/commit/abort, admission control
      recovery.rs            # Redo committed, undo uncommitted
      reservation.rs         # AdmissionController, 70KB max transaction size
    refcount/
      mod.rs                 # Reason-tagged refcounts, RwLock for 1->0 transition
    projection/
      mod.rs                 # InodeTable: UUID <-> inode bidirectional mapping
  tests/
    integration.rs           # Layer tests: log, txn, recovery, graph, content store
    fuse_e2e.rs              # Real FUSE mount tests: create, read, write, mkdir, link, rename
  docs/
    architecture.md          # Full vision, layer diagram
    design.md                # Data structures, algorithms, operation mapping
    mvp.md                   # This file
```

## Implementation Phases

### Phase 1: MVP (current)

Single-threaded, sync writes, simplified transactions. All six layers
implemented and tested through real FUSE mounts.

**Test coverage:**
- 8 unit tests (refcount, file backend)
- 18 layer-integration tests (log, txn, recovery, graph, content store,
  graph recovery with committed/uncommitted/mixed scenarios)
- 8 end-to-end FUSE tests (mount, create, read, write, mkdir, unlink, rmdir,
  hardlink, rename, readdir)
- 4 crash injection tests (SIGKILL during file creation, overwrite, directory
  ops, repeated crash/recovery cycles — verifies committed data survives,
  no partial state, filesystem remounts correctly)

**Known limitations:**
- Entire file content is read-modify-written on each write (no block-level COW).
- Graph state and object index are serialized entirely on flush (no incremental
  persistence).
- No fsync support (the FUSE `fsync` callback is a no-op).
- Single-threaded FUSE dispatch.
- Tests written by the same agent that wrote the code (no independent test
  oracle yet).

### Phase 2: Transaction Sophistication

- **EC merge**: when concurrent transactions touch the same graph node, merge
  their equivalence classes. Requires transaction locks on edge indices,
  the EC merge algorithm (fold smaller into larger), and BEND records for
  batch commit.
- **Async log writes**: buffer log records and write in batches. Track
  io_window for consistent-cut recovery.
- **Undo-zero transactions**: nested transactions that only generate undo
  records. Used for cache flush, GC, and other "undo on crash but don't redo
  on recovery" operations. Massive performance win because they require only
  ~512 bytes of log reservation.
- **Multi-threaded FUSE dispatch**.

### Phase 3: Extended Namespace

- **Multiple projections**: mount the same graph as different directory trees
  simultaneously, filtered by edge type.
- **Finesse bypass**: shared-memory IPC for graph-native operations that
  bypass the kernel VFS. Traverse, associate, query without POSIX overhead.
- **Transparent encryption**: encrypt content blobs and edge labels at rest.
  Key management TBD.

## Evaluation Plan

Claims to evaluate for publication:

1. **Transactional correctness** — crash injection (kill -9 during writes),
   verify recovery restores consistency.
2. **Deduplication** — storage savings on overlapping content vs. traditional
   filesystem.
3. **Graph expressiveness** — single graph operations vs. multi-step POSIX
   equivalents (e.g., tagging a file = adding an edge, vs. creating a
   symlink in a tag directory).
4. **EC merge scaling** — throughput under increasing contention (Phase 2).
5. **Recovery time** — mount-to-ready vs. fsck or journal replay.

Benchmarks: Filebench, mdtest, custom graph workload, crash injection via
kill -9, pjdfstest for POSIX compliance.

## Key Episode Source Files

For anyone extending the transaction layer, these are the files to study in
`~/projects/dce/dce/src/file/episode/`:

| File | What to extract |
|---|---|
| `logbuf/tran.c` | EC merge algorithm (lines 652-765), transaction lifecycle state machine, CompleteEC |
| `logbuf/logrec.h` | Log record format, OLDDATA/NEWDATA flags, logPageHdr with io_window |
| `logbuf/log.c` | Three-tier reservation (Full/End/Released), adaptive maxTranSize, admission control |
| `logbuf/recover.c` | Five-phase recovery: read log, hash by txn, mark committed, handle BEND, apply to disk |
| `anode/tlock.h` | Transaction lock -> EC merge trigger |
