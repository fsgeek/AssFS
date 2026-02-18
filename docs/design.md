# AssFS Design — Data Structures and Algorithms

## Identity Model

AssFS uses a two-layer identity scheme:

- **ObjectId** (UUID v4): stable mutable identity. A file keeps the same UUID
  across content updates. Used in graph edges, log records, inode table.
- **ContentHash** (BLAKE3, 32 bytes): immutable content identity. Every
  distinct byte sequence has exactly one hash. Used in the blob store.

The **object index** (`ObjectId -> ContentHash`) is the mutable binding between
the two layers. This is the only metadata that the transaction log protects.
Content blobs themselves are immutable and never overwritten.

This split means:
- Deduplication is free (same content = same hash, stored once).
- Integrity checking is free (hash the blob, compare).
- The log is compact (records UUIDs and hashes, not data).
- Recovery only needs to fix the object index, not content.

## Log Tuple Format

Every data modification produces a log record:

```
LogRecord {
    lsn:            u64         // monotonically increasing
    record_type:    Data | End | Checkpoint | Restart
    transaction_id: u64
    object_id:      UUID        // which object changed
    old_hash:       Option<[u8; 32]>  // undo value (what to revert to)
    new_hash:       Option<[u8; 32]>  // redo value (what was committed)
}
```

- `old_hash = None`: object didn't exist before this operation (create).
- `new_hash = None`: object was deleted by this operation.
- Both present: content was modified.
- `End` record: marks transaction commit. When this reaches stable storage,
  the transaction is committed. This is the single commit point.

Records are serialized with bincode, framed with a 4-byte little-endian length
prefix, and appended sequentially. Each append is followed by `fsync` (MVP;
async writes in Phase 2).

## Transaction Lifecycle

```
StartTran
  |-- admission control: reserve log space (70KB max)
  |-- allocate TransactionId
  |
  v
[Operations]  -- each produces a LogRecord
  |-- log_write(txn_id, object_id, old_hash, new_hash)
  |-- undo_map tracks first old_hash per object (for abort)
  |
  v
CommitTran
  |-- write End record
  |-- fsync (commit point)
  |-- release reservation
  |
  v
(transaction complete)
```

Abort: release reservation, return undo_map to caller for index reversion.

### Admission Control

From Episode: no garbage collection on the log. Instead:

- Maximum transaction size: 70KB (invariant-tested in Episode).
- `AdmissionController` tracks total reserved bytes across all active transactions.
- `try_reserve()` fails if total would exceed max log size.
- Upper layer must checkpoint before new transactions can start.

### Three-Tier Reservation (Phase 2)

Episode's full model has three reservation levels per transaction:

1. **Full**: reserved at StartTran. Pessimistic — assumes max usage.
2. **End**: after PendTran (EC member done, waiting for EC head). Only needs
   space for the End record (~64 bytes).
3. **Released**: after CompleteTran (End record on stable storage).

The MVP uses a simplified model: full reservation at start, full release at
commit/abort.

## Recovery Algorithm

On mount, before the filesystem is available:

1. **Read log**: parse all framed records from the log file. Truncated records
   at the end (partial write before crash) are silently ignored.

2. **Classify transactions**: scan for End records. Transactions with an End
   record are committed; those without are uncommitted.

3. **Redo committed**: for each committed transaction's Data records (in LSN
   order), set `object_index[object_id] = new_hash`. Later writes win.

4. **Undo uncommitted**: for each uncommitted transaction's Data records (in
   reverse LSN order), set `object_index[object_id] = old_hash`. If
   `old_hash` is None, remove the object from the index.

5. **Flush index**: write the corrected object index to disk.

This is simpler than Episode's recovery because content blobs are never
overwritten — only the object index (a key-value map) needs to be corrected.
Episode must reconstruct the actual disk blocks by applying old/new data
byte-ranges.

## Graph Namespace

### Node

```
GraphNode {
    id:           UUID
    node_type:    File | Directory | Symlink
    content_hash: [u8; 32]
    metadata:     NodeMetadata {
        mode, uid, gid, size,
        atime, mtime, ctime (with nanoseconds),
        nlink
    }
}
```

### Edge

Edges are stored in a per-node `EdgeIndex`:

```
EdgeIndex {
    node_id:  UUID
    outgoing: BTreeMap<(EdgeType, String), UUID>
}
```

`EdgeType` is `Child | Hardlink | Symlink` (extensible to `Tag`, `Dependency`,
etc.).

The BTreeMap gives O(log n) lookup by `(type, name)` and ordered iteration
for readdir.

A reverse index (`target -> Vec<(source, type, label)>`) supports:
- Determining whether an object is still referenced (for deletion).
- Computing nlink.
- Graph queries that traverse edges backwards.

### FUSE Operation Mapping

| FUSE Op | Graph Op |
|---|---|
| `lookup(parent, name)` | `resolve(parent, Child, name)` |
| `readdir(ino)` | `neighbors(node)` |
| `create(parent, name)` | txn { `add_node`; `add_edge(parent, Child, name, node)` } |
| `unlink(parent, name)` | txn { `remove_edge(parent, Child, name)`; if unreferenced: `remove_node` } |
| `read(ino, off, size)` | `get_content(node)` + slice |
| `write(ino, buf, off)` | txn { read-modify-write content; `put_object`; log tuple } |
| `link(ino, newparent, name)` | txn { `add_edge(newparent, Child, name, node)` } |
| `rename(old_p, old_n, new_p, new_n)` | txn { `remove_edge(old)`; `add_edge(new)` } |
| `mkdir(parent, name)` | txn { `add_node(Dir)`; `add_edge(parent, Child, name, node)` } |
| `rmdir(parent, name)` | txn { assert empty; `remove_edge`; `remove_node` } |

## Hierarchical Projection

A projection maps the graph to a POSIX tree:

```
Projection {
    root_node:          UUID
    eligible_edge_types: {Child, Hardlink}  // controls what's visible
}
```

The **inode table** is a bidirectional map (`UUID <-> u64`) that assigns
synthetic inode numbers on first reference. Numbers are stable for the lifetime
of the mount but not across mounts. Root is always inode 1.

Path resolution: walk path components left to right, at each step calling
`resolve(current_node, eligible_type, component)` on the edge index.

Multiple projections (Phase 3) would allow the same graph to appear as different
directory trees simultaneously — for example, one organized by date, another by
project, another by tag.

## Reason-Tagged Reference Counting

```
RefReason: GraphEdge | TransactionHold | CacheEntry | OpenFileDesc
         | TraversalCursor | LogReference | SnapshotPin | InternalTemp
```

Protocol:
- **Increment**: shared (read) lock on transition_lock. Atomic fetch_add on
  the per-reason counter.
- **Decrement**: shared lock, atomic fetch_sub. If the reason count was 1
  (now 0), check total. If total might be 0, upgrade to exclusive (write)
  lock and re-check. Only return `Unreferenced` under exclusive lock with
  confirmed zero total.

This means concurrent increments and decrements from different reasons never
block each other. Only the final decrement — the 1-to-0 transition — takes
the exclusive lock, and it re-checks to handle races.

## Content Store

Two sub-components:

### Blob Store (FileBackend)

Content-addressed. Each blob is stored as a file named by its hex-encoded
BLAKE3 hash, in a two-level directory structure (`ab/abcdef01...`).

- `put(data)`: hash, check if exists (idempotent), write to temp file, rename.
- `get(hash)`: read file.
- Deduplication is automatic: identical content produces the same hash and
  path.

### Object Index

`HashMap<UUID, ContentHash>`, serialized to disk with bincode. This is the
mutable layer. The transaction log protects it; recovery corrects it.

- `put_object(uuid, data)`: store blob, update index.
- `get_content(uuid)`: look up hash in index, read blob.
- `EMPTY_HASH` (all zeros): sentinel for empty files and directories.
