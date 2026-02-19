# --no-steal Flag Implementation Specification

## Purpose

Add a `--no-steal` mode to AssFS that defers object store writes until
transaction commit. This creates a measurable NO-STEAL constraint: uncommitted
data is pinned in process memory, increasing RSS and reducing memory available
for the kernel page cache.

Combined with cgroup memory limits and a concurrent data workload, this
enables a real A/B experiment comparing STEAL vs NO-STEAL on the actual
filesystem, not just in simulation.

## Design

### Current Behavior (STEAL — default)

When a FUSE operation mutates content:
1. `store.put_object(id, data)` writes the blob to disk immediately
2. `store.register_empty(id)` updates the in-memory index immediately
3. The transaction log records the mutation (old_hash, new_hash)
4. On commit, the log END record is synced
5. On abort, the undo log reverts the index entries

Content blobs reach disk before commit — this is the STEAL property. The
process memory for the data can be reclaimed by the OS after the write.

### New Behavior (NO-STEAL — `--no-steal` flag)

When `--no-steal` is active, content store writes are buffered in the
transaction until commit:

1. `store.put_object(id, data)` does NOT write to disk. Instead, the data
   is held in a per-transaction buffer: `pending_writes: Vec<(ObjectId, Vec<u8>)>`.
2. The object index is updated in-memory as before (reads within the
   transaction must see their own writes).
3. The transaction log records the mutation as before.
4. On commit: drain `pending_writes` to the real content store backend,
   then sync the log END record.
5. On abort: discard `pending_writes`, revert the index.

The graph state (GraphNamespace) is already fully in-memory, so it's
effectively "pinned" in both modes. The difference is in the content store:
under NO-STEAL, blob data stays in process memory until commit.

### Where the Memory Pressure Comes From

Under NO-STEAL, every file write within an uncommitted transaction keeps
the written data in `pending_writes` (process heap). This increases the
process RSS. In a cgroup with a fixed memory limit, higher RSS means less
memory available for the kernel page cache, which means data pages from
concurrent workloads (e.g., streaming reads) get evicted.

Under STEAL (default), blob data is written to disk immediately. The
process memory is freed. The kernel page cache has more room.

## Implementation

### Changes to `Transaction` (`src/txn/transaction.rs`)

Add a field to hold pending content writes:

```rust
pub struct Transaction {
    pub id: TransactionId,
    pub status: TransactionStatus,
    pub reservation: Reservation,
    pub first_lsn: Option<LogSequenceNumber>,
    pub last_lsn: Option<LogSequenceNumber>,
    pub undo_log: Vec<UndoEntry>,
    /// NO-STEAL: content data buffered until commit.
    pub pending_writes: Vec<(ObjectId, Vec<u8>)>,
}
```

Initialize `pending_writes: Vec::new()` in `start_txn()`.

### Changes to `ContentStore` (`src/store/content.rs`)

Add a method that buffers instead of writing:

```rust
/// Buffer a write for deferred flush (NO-STEAL mode).
/// Updates the in-memory index so reads see the new data,
/// but does NOT write the blob to disk.
pub fn put_object_buffered(
    &self,
    object_id: ObjectId,
    data: &[u8],
) -> io::Result<ContentHash> {
    let hash = crate::content_hash(data);
    let mut idx = self.index.lock().unwrap();
    idx.entries.insert(object_id, hash);
    Ok(hash)
}

/// Flush buffered writes to disk. Called on commit in NO-STEAL mode.
pub fn flush_pending(&self, pending: &[(ObjectId, Vec<u8>)]) -> io::Result<()> {
    for (_object_id, data) in pending {
        self.backend.put(data)?;
    }
    Ok(())
}
```

### Changes to `TransactionManager` (`src/txn/transaction.rs`)

`commit_txn` needs access to pending writes for NO-STEAL flush. Two options:

**Option A (simpler)**: Return pending_writes from commit_txn and let the
caller (fuse_ops) flush them. This keeps the TxnMgr unaware of the store.

```rust
pub fn commit_txn(&mut self, txn_id: TransactionId) -> io::Result<Vec<(ObjectId, Vec<u8>)>> {
    let lsn = self.log.log_end(txn_id)?;
    let txn = self.active.get_mut(&txn_id).ok_or_else(|| ...)?;
    txn.last_lsn = Some(lsn);
    txn.status = TransactionStatus::Committed;
    self.admission.release(&txn.reservation);
    let txn = self.active.remove(&txn_id).unwrap();
    Ok(txn.pending_writes)
}
```

**Option B**: Pass a reference to the ContentStore into commit_txn.

Prefer **Option A** — it preserves the layering (TxnMgr doesn't know about
the store). The caller already has access to the store.

### Changes to `AssFs` (`src/fuse_ops/mod.rs`)

Add a `no_steal: bool` field to `AssFs`.

In every FUSE operation that writes content (write, create, setxattr, etc.):

```rust
if self.no_steal {
    // Buffer the data in the transaction
    let hash = self.store.put_object_buffered(object_id, &data)?;
    let mut txn_mgr = self.txn_mgr.lock().unwrap();
    // ... log the content change as before ...
    // Stash the data in pending_writes
    txn_mgr.active.get_mut(&txn_id).unwrap()
        .pending_writes.push((object_id, data.to_vec()));
} else {
    // Current behavior: write through immediately
    let hash = self.store.put_object(object_id, &data)?;
}
```

On commit:
```rust
let pending = {
    let mut txn_mgr = self.txn_mgr.lock().unwrap();
    txn_mgr.commit_txn(txn_id)?
};
if self.no_steal && !pending.is_empty() {
    self.store.flush_pending(&pending)?;
}
```

On abort: pending_writes are dropped with the transaction (no flush needed).

### Changes to `main.rs`

Add `--no-steal` to argument parsing (same pattern as `--no-sync`):

```rust
if args.iter().any(|a| a == "--no-steal") {
    fs.no_steal = true;
    log::warn!("NO-STEAL mode: content writes buffered until commit (for measurement)");
}
```

Update the usage string.

### Changes to `AssFs::open()`

Initialize `no_steal: false` in the struct construction.

Add a setter: `pub fn set_no_steal(&mut self, v: bool) { self.no_steal = v; }`

## Behavioral Notes

- **Reads within a transaction**: The in-memory object index is updated
  immediately in both modes, so `get_content()` on a just-written object
  will fail in NO-STEAL mode (the blob isn't on disk yet). For the MVP
  this is acceptable — single-threaded FUSE ops don't read-after-write
  within the same transaction. If needed, add a check: look in
  pending_writes before falling through to the backend.

  Actually, on reflection: FUSE write() followed by read() on the same
  file IS possible (different FUSE requests, different transactions in
  our current model). Each FUSE op is its own transaction currently.
  So pending_writes are flushed at the end of each FUSE op's commit.
  The window where data is only in pending_writes is very short.

  BUT: if we later batch multiple FUSE ops into one transaction (EC merge),
  then reads within the batch could hit unflushed data. Add the fallback:

```rust
pub fn get_content_or_pending(
    &self,
    object_id: &ObjectId,
    pending: &[(ObjectId, Vec<u8>)],
) -> io::Result<Vec<u8>> {
    // Check pending writes first.
    for (id, data) in pending.iter().rev() {
        if id == object_id {
            return Ok(data.clone());
        }
    }
    // Fall through to on-disk content.
    self.get_content(object_id)
}
```

- **Graph state**: Already in-memory in both modes. No change needed.
  The graph state flush (`graph.flush()`) can happen in either mode
  because graph state is not content — it's metadata index. Both modes
  update it the same way.

- **Recovery**: No change needed. The log records are identical in both
  modes. Recovery replays from the log regardless of whether content
  was written early (STEAL) or late (NO-STEAL). If we crash in NO-STEAL
  mode before commit, the pending_writes are lost — but so is the
  transaction (no END record), so recovery undoes it correctly.

- **Stats**: Add a counter for pending_writes bytes (tracks memory pressure).

## Testing

### Unit test: NO-STEAL buffers correctly
1. Open AssFS with no_steal=true
2. Create a file, write content
3. Verify content is readable (committed, flushed)
4. Create a file in a new transaction — before explicit commit, verify
   the blob file does NOT exist on disk (buffered)

### E2E test: NO-STEAL mode works end-to-end
1. Mount with `--no-steal`
2. Run the existing FUSE e2e tests
3. All should pass (the flush happens at commit, which is at the end
   of each FUSE op)

### Measurement test (manual, with cgroup)
1. Create a cgroup with 64MB memory limit
2. Mount AssFS in STEAL mode inside the cgroup
3. Run a metadata workload (many file creates) + a streaming read workload
4. Record data page cache stats from /proc/vmstat
5. Unmount, remount with --no-steal
6. Run the same workloads
7. Compare page cache stats

## Success Criteria

1. `--no-steal` flag accepted, logs a warning about measurement mode
2. All existing tests pass in both modes
3. In NO-STEAL mode, content blobs are NOT written to disk until commit
4. In STEAL mode (default), behavior is unchanged
5. The commit_txn return type changes to include pending_writes (Option A)
6. Recovery works correctly in both modes
