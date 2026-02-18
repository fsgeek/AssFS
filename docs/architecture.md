# AssFS Architecture

This document describes the full vision. The MVP implements a subset (see
[mvp.md](mvp.md) for what's in and what's deferred).

## Layer Diagram

```
 +-------------------------------------------------------------------+
 |                    CLIENT INTERFACE LAYER                          |
 |  +---------------------------+  +------------------------------+  |
 |  |   FUSE Low-Level API      |  |  Finesse Bypass Channel      |  |
 |  |   (POSIX tree projection) |  |  (graph ops, assoc. queries) |  |
 |  +-------------+-------------+  +--------------+---------------+  |
 +-------------------------------------------------------------------+
                   |                               |
 +-------------------------------------------------------------------+
 |                  HIERARCHICAL PROJECTION LAYER                     |
 |  Spanning tree extraction  |  Inode mapping  |  Multiple views     |
 +-------------------------------------------------------------------+
                   |
 +-------------------------------------------------------------------+
 |                    GRAPH NAMESPACE LAYER                           |
 |  Nodes = object IDs    |  Edges = typed associations               |
 |  Adjacency ops  |  Traversal  |  Reverse lookups                   |
 +-------------------------------------------------------------------+
                   |
 +-------------------------------------------------------------------+
 |              TRANSACTIONAL OBJECT STORE (TOS)                     |
 |  Transaction Mgr (EC merge, 3-tier reservation, admission ctrl)   |
 |  Log Manager (old/new tuples, async writes, io_window)            |
 |  Recovery Engine (redo committed, undo uncommitted)               |
 |  Content Store (hash-addressed, dedup, COW)                       |
 |  Reference Counting (reason-tagged, atomic, 1->0 exclusive)       |
 +-------------------------------------------------------------------+
                   |
 +-------------------------------------------------------------------+
 |                    STORAGE BACKEND LAYER                           |
 |  Pluggable: file-per-object, block device, KV engine              |
 |  Encryption transform (content + edge names)                      |
 +-------------------------------------------------------------------+
```

## Component Responsibilities

### Client Interface Layer

Two paths in. FUSE provides POSIX compatibility via `fuser` crate low-level
API. Finesse bypass (future) provides graph-native operations — traverse,
associate, query, project — via shared-memory IPC. Legacy apps see a tree;
graph-aware apps see the full namespace.

### Hierarchical Projection Layer

Bridges POSIX tree semantics and graph namespace. A "projection" is a spanning
tree extracted from the graph, defined by a root node and a set of eligible edge
types. Multiple projections yield multiple views of the same data. Manages inode
number assignment (synthetic, per-mount, assigned on first reference).

### Graph Namespace Layer

The storage namespace is a labeled directed graph:

- **Nodes** are objects with stable UUIDs, a type (File, Directory, Symlink),
  a content hash, and POSIX metadata.
- **Edges** are first-class associations with source, target, label, and type
  (Child, Hardlink, Symlink, and extensible to Tag, Dependency, etc.).
- **Edge index**: per-node BTreeMap keyed by `(EdgeType, label)` for O(log n)
  lookup and ordered iteration (readdir).
- **Reverse index**: target -> list of (source, type, label) for reverse
  lookups and nlink/deletion tracking.

Operations: `add_edge`, `remove_edge`, `resolve(parent, name)`,
`neighbors(node, types)`, `reverse_lookup(target)`.

Transaction locks on edge indices trigger EC merge on conflict (Phase 2).

### Transactional Object Store (TOS)

The heart. Adapts Episode's transaction/logging to content-addressed KV:

- **Log records as tuples**: `<operation, object_id, old_content_hash,
  new_content_hash>`. Old hash = undo, new hash = redo.
- **EC merge**: overlapping transactions merge under contention. Adaptive
  parallelism collapses to checkpoint under heavy load. (Phase 2.)
- **Three-tier reservation**: Full (active transaction) -> End (pending commit)
  -> Released (on disk). Admission control, no GC, bounded transaction size.
- **Async log writes** with io_window for consistent-cut recovery. (Phase 2;
  MVP uses sync writes.)
- **Content-addressed**: dedup free, integrity free, COW trivial. Objects are
  immutable blobs identified by BLAKE3 hash. The mutable binding is the object
  index: UUID -> ContentHash.
- **Reason-tagged refcounts**: atomic inc/dec under shared lock, exclusive lock
  only on 1-to-0 transition with re-check after acquisition.

### Storage Backend Layer

Pluggable via the `StorageBackend` trait:

- **MVP**: file-per-object in a two-level hex directory (`ab/abcdef01...`).
  Atomic writes via temp file + rename.
- **Future**: raw block device, custom KV engine.
- **Encryption**: composable transform layer on content and edge labels (Phase 3).

## How Episode's Insights Map to AssFS

| Episode Source | AssFS Component | Adaptation |
|---|---|---|
| `logbuf/tran.c` EC merge | TOS Transaction Manager | EC merge on key conflicts |
| `logbuf/logrec.h` old/new data | TOS Log Manager | old_hash/new_hash tuples |
| `logbuf/log.c` 3-tier reservation | TOS Transaction Manager | full/undo/end per transaction |
| `logbuf/log.c` admission control | TOS Transaction Manager | no GC, bounded txn size |
| `logbuf/log.c` async + io_window | TOS Log Manager | consistent-cut recovery (Phase 2) |
| `logbuf/recover.c` undo/redo | TOS Recovery Engine | reinstate old_hash or new_hash |
| `anode/tlock.h` transaction locks | TOS EC trigger | graph-node locks trigger EC merge |
| `anode/anode.p.h` storage polymorphism | Content Store | inline/block tiers (future) |
| `anode/volume.c` filesets | Graph Namespace | projections generalize filesets |
| `vnops/` VFS interface | Hierarchical Projection | FUSE low-level ops |
| Bitbucket `object.c` refcounts | TOS Reference Counting | directly inherited |
| Bitbucket `bitbucket.c` FUSE skeleton | Client Interface | starting point |
| Finesse IPC | Bypass Channel | future graph-native API |

## Open Decisions

**Encryption scope.** Content blobs are naturally encryptable (keyed by hash,
encrypted at rest). Edge labels carry semantic information (file names) that
also needs encryption. The question is granularity: per-object keys, per-user
keys, or a key hierarchy. Deferred to Phase 3.

**Inline small objects.** Episode's anode layer stores small data inline in the
anode itself, avoiding indirection. AssFS could do the same — store small
objects (< 256 bytes) directly in the log record or graph node rather than as
separate blobs. Not yet implemented.

**Block-level COW.** Currently the entire object is replaced on write. For large
files, block-level COW (splitting content into fixed-size chunks, each
content-addressed) would avoid rewriting unchanged portions. This interacts
with the log tuple format — the "old_hash/new_hash" would become per-chunk
rather than per-object.
