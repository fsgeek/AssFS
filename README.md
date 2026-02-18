# AssFS — Associative File System

AssFS is a FUSE filesystem that replaces the traditional directory tree with a
graph namespace backed by a transactional, content-addressed object store. Legacy
applications see a POSIX tree; graph-aware applications see the full associative
namespace.

The design draws on three decades of file system work — particularly the
transaction and recovery architecture of the Episode file system (DCE/DFS,
Transarc, 1990s) and the FUSE infrastructure of the Finesse/Bitbucket project.

## Core Principles

**The fastest writes are the ones you never write to disk.**
Content-addressing gives us deduplication and integrity for free. Copy-on-write
is trivial. The log records what changed, not the data itself.

**Old/new value logging, not write-ahead.**
Every log record carries both the old value (undo) and the new value (redo).
This means metadata can be written to disk *before* a transaction commits —
undo is the safety net, not a gate. This is the lesson Mike Young brought from
Camelot at CMU: new-value-only logging "saves space" but constrains everything.

**Reservation, not garbage collection.**
Each transaction reserves log space pessimistically at start, releases
progressively as it commits. The log never needs sweeping. Admission control
bounds transaction size (~70KB). No GC means no GC pauses.

**The namespace is a graph, not a tree.**
Objects have stable UUID identities. Names are typed, directed edges between
objects — not path components in a hierarchy. A POSIX directory tree is one
*projection* of this graph: a spanning tree defined by a root node and eligible
edge types. Multiple projections can expose the same data as different
hierarchies simultaneously.

**Reason-tagged reference counting.**
Every reference carries a tag explaining *why* it exists (graph edge, open file
descriptor, transaction hold, cache entry, ...). Only the 1-to-0 transition
requires an exclusive lock; all other reference operations use shared access.
This means traversals stay lock-free and deletion only contends when it must.

**Equivalence classes for adaptive concurrency.** *(Phase 2)*
When concurrent transactions touch overlapping objects, they merge into an
equivalence class and commit together. Under light load, transactions run in
full parallel. Under heavy contention, the system collapses gracefully to
checkpoint behavior. This is *faster* than traditional locking, not slower.

## Architecture

```
 CLIENT INTERFACE          FUSE (POSIX tree)  |  Finesse bypass (graph ops)
 HIERARCHICAL PROJECTION   spanning tree extraction, inode mapping
 GRAPH NAMESPACE            nodes + typed edges, resolve, traverse, reverse lookup
 TRANSACTIONAL OBJECT STORE log manager, transaction manager, recovery, content store
 STORAGE BACKEND            pluggable: file-per-object (MVP), block device, KV engine
```

See [docs/architecture.md](docs/architecture.md) for the full layer diagram and
component responsibilities, and [docs/design.md](docs/design.md) for data
structures and algorithms.

## Status

**MVP implemented.** The filesystem mounts, creates/reads/writes/deletes files,
supports directories, hard links, rename, and crash recovery via redo/undo on
the transaction log.

What works now:
- Graph namespace with typed edges (Child, Hardlink, Symlink)
- Content-addressed blob store with BLAKE3 hashing and automatic dedup
- Transaction manager with admission control and bounded log
- Recovery engine (redo committed, undo uncommitted) on mount
- Reason-tagged reference counting
- Hierarchical projection (UUID-to-inode mapping)
- FUSE adapter (15 operations)
- End-to-end tests that mount the real filesystem

What's deferred:
- Equivalence class merge (Phase 2)
- Async log writes with io_window (Phase 2)
- Undo-zero transactions (Phase 2)
- Transparent encryption (Phase 3)
- Finesse bypass channel for graph-native ops (Phase 3)
- Multiple simultaneous projections (Phase 3)

## Building and Running

```
cargo build
cargo test -- --test-threads=1

# Mount the filesystem
mkdir -p /tmp/assfs-mnt
./target/debug/assfs /tmp/assfs-mnt --data-dir /tmp/assfs-data

# Use it
echo "hello" > /tmp/assfs-mnt/hello.txt
cat /tmp/assfs-mnt/hello.txt
ln /tmp/assfs-mnt/hello.txt /tmp/assfs-mnt/link.txt
ls -li /tmp/assfs-mnt/

# Unmount
fusermount3 -u /tmp/assfs-mnt
```

Requires: Rust 1.75+, FUSE3 (`libfuse3-dev` on Debian/Ubuntu).

## Lineage

| Concept | Origin | AssFS Adaptation |
|---|---|---|
| Old/new value logging | Episode `logbuf/logrec.h` | Log tuples: `<op, object_id, old_hash, new_hash>` |
| Admission control, no GC | Episode `logbuf/log.c` | Bounded transaction size, reservation tracking |
| EC merge | Episode `logbuf/tran.c` | Graph-node locks trigger EC merge (Phase 2) |
| Five-phase recovery | Episode `logbuf/recover.c` | Simplified: content never overwritten, only index needs fixing |
| Reason-tagged refcounts | Bitbucket `object.c` / FSDK | 8 reason tags, RwLock for 1-to-0 transition |
| FUSE skeleton | Bitbucket `bitbucket.c` | 15 low-level ops via `fuser` crate |
| Write structure preservation | BitRaider | Content-addressed objects expose natural boundaries |

## Target Venues

FAST, Systor, Transactions on Storage.
