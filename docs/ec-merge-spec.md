# EC Merge Implementation Specification

## Purpose

Implement equivalence class (EC) merge for AssFS — Episode's adaptive
transaction batching mechanism. When two concurrent transactions conflict
on a graph node, they merge into an equivalence class and share a single
commit point. Under light load, transactions run independently (max
parallelism). Under heavy load with contention, they merge into larger
ECs that commit atomically, converging toward checkpoint speed.

This requires switching from single-threaded to multi-threaded FUSE.

## Background

In Episode (source: `logbuf/tran.c`), EC merge works as follows:
1. Transaction locks (tlocks) live on anodes (storage objects)
2. When a transaction acquires a tlock already held by another transaction,
   the two are merged into an equivalence class
3. Merge uses union-find: smaller EC folded into larger
4. EC members end with BEND records; the EC head ends with END
5. The LAST transaction to call EndTran triggers the atomic commit
6. Merge is triggered at lock RELEASE time, not acquire time, to avoid
   spurious merges from brief lock holders

## Implementation Plan

### Phase 1: Multi-threaded FUSE

**`src/main.rs`**: Switch from `fuser::mount2` to `fuser::spawn_mount2`.

`spawn_mount2` returns a `BackgroundSession` guard. The main thread waits
for it (or for a signal). Multi-threaded mode means multiple FUSE ops
execute concurrently on different threads.

**Note**: `fuser::Filesystem` trait methods take `&mut self`, but
`spawn_mount2` requires `Send`. With fuser 0.14, the session handles
dispatch. Check the fuser API — may need `Arc<AssFs>` instead of owned,
or we may need to use the lower-level `Session` API.

**IMPORTANT**: fuser 0.14 calls Filesystem methods with `&mut self`.
For multi-threaded operation, this means fuser serializes calls internally.
To get ACTUAL concurrent execution, we may need to change `AssFs` to use
interior mutability (all fields behind Arc/Mutex) so we can implement
Filesystem for `&AssFs` or use a wrapper. Research the fuser 0.14 API
carefully before implementing.

**Alternative**: If fuser 0.14 doesn't support true multi-threaded dispatch
with `&mut self`, we can simulate concurrent transactions using a thread
pool WITHIN single-threaded FUSE dispatch. Each FUSE op spawns its
transaction work onto the pool and the FUSE thread waits for completion.
This gives us concurrent transactions (and thus EC merge triggers) without
requiring fuser multi-threading.

The simplest approach that demonstrates EC merge may be: keep single-threaded
FUSE but allow transactions to overlap. Currently each FUSE op does
start_txn → work → commit_txn atomically. If we instead allow a FUSE op
to start a transaction and release the txn_mgr lock while doing graph
operations, a subsequent FUSE op could start its own transaction before
the first commits. This is the minimum change to enable EC merge.

### Phase 2: Fine-Grained Locking

Current pattern in every FUSE op:
```rust
let mut txn_mgr = self.txn_mgr.lock().unwrap();
let txn_id = txn_mgr.start_txn()?;
// ... all work done with txn_mgr lock held ...
txn_mgr.commit_txn(txn_id)?;
```

This serializes everything. For EC merge, we need:
```rust
let txn_id = {
    let mut txn_mgr = self.txn_mgr.lock().unwrap();
    txn_mgr.start_txn()?
};
// txn_mgr lock RELEASED — other threads can start transactions
// Acquire tlocks on graph nodes (may trigger EC merge)
// Do graph and store operations
// Log operations (briefly reacquire txn_mgr lock for each log call)
{
    let mut txn_mgr = self.txn_mgr.lock().unwrap();
    txn_mgr.end_txn(txn_id)?;  // may or may not trigger commit
}
```

Key changes to `TransactionManager`:
- `start_txn()` and `end_txn()` are brief operations (acquire/release global lock)
- Log operations (`log_add_node`, etc.) acquire the lock briefly per call
- The txn_mgr lock is NOT held during graph/store work

### Phase 3: Transaction Locks (TlockTable)

New module: `src/txn/tlock.rs`

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use crate::{ObjectId, TransactionId};

/// A transaction lock on a graph node.
struct TlockEntry {
    /// Transaction that currently "owns" this lock (last writer).
    holder: Option<TransactionId>,
}

/// Table of transaction locks, one per graph node that has been accessed.
pub struct TlockTable {
    locks: Mutex<HashMap<ObjectId, TlockEntry>>,
}

impl TlockTable {
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Record that a transaction is accessing a graph node.
    /// Returns Some(other_txn_id) if a different transaction already holds
    /// the tlock — the caller should trigger EC merge.
    ///
    /// Following Episode's design: the merge check happens at RELEASE time
    /// (when we assign the new holder), not at acquire time.
    pub fn touch(
        &self,
        node_id: ObjectId,
        txn_id: TransactionId,
    ) -> Option<TransactionId> {
        let mut locks = self.locks.lock().unwrap();
        let entry = locks.entry(node_id).or_insert(TlockEntry {
            holder: None,
        });
        let prev_holder = entry.holder;
        entry.holder = Some(txn_id);

        match prev_holder {
            Some(prev) if prev != txn_id => Some(prev),
            _ => None,
        }
    }

    /// Clear all tlocks held by a transaction (called on commit/abort).
    pub fn release_all(&self, txn_id: TransactionId) {
        let mut locks = self.locks.lock().unwrap();
        for entry in locks.values_mut() {
            if entry.holder == Some(txn_id) {
                entry.holder = None;
            }
        }
    }
}
```

Add `TlockTable` to `AssFs`:
```rust
pub struct AssFs {
    // ... existing fields ...
    pub tlocks: TlockTable,
}
```

In FUSE ops, after starting a transaction, touch tlocks for each graph
node accessed:
```rust
if let Some(other_txn) = self.tlocks.touch(parent_id, txn_id) {
    let mut txn_mgr = self.txn_mgr.lock().unwrap();
    txn_mgr.ec_merge(txn_id, other_txn);
}
if let Some(other_txn) = self.tlocks.touch(file_id, txn_id) {
    let mut txn_mgr = self.txn_mgr.lock().unwrap();
    txn_mgr.ec_merge(txn_id, other_txn);
}
```

### Phase 4: EC Merge Algorithm

Changes to `Transaction` struct:
```rust
pub struct Transaction {
    pub id: TransactionId,
    pub status: TransactionStatus,
    pub reservation: Reservation,
    pub first_lsn: Option<LogSequenceNumber>,
    pub last_lsn: Option<LogSequenceNumber>,
    pub undo_log: Vec<UndoEntry>,
    pub pending_writes: Vec<(ObjectId, Vec<u8>)>,
    // --- EC merge fields ---
    /// Points to EC head transaction ID. Self if this is the head.
    pub ec_head: TransactionId,
    /// Number of active members in this EC (only meaningful on head).
    pub ec_size: u32,
    /// Member list (only on head). Does NOT include the head itself.
    pub ec_members: Vec<TransactionId>,
}
```

Initialize in `start_txn()`:
```rust
ec_head: id,     // self = singleton EC
ec_size: 1,      // just us
ec_members: Vec::new(),
```

**EC Merge function** on `TransactionManager`:
```rust
/// Merge two transactions into one equivalence class.
/// Follows Episode's algorithm: fold smaller EC into larger.
pub fn ec_merge(&mut self, txn1: TransactionId, txn2: TransactionId) {
    if txn1 == txn2 { return; }

    // Find EC heads (walk chain).
    let head1 = self.find_ec_head(txn1);
    let head2 = self.find_ec_head(txn2);
    if head1 == head2 { return; }  // already in same EC

    // Verify both are still active.
    let size1 = self.active.get(&head1).map_or(0, |t| t.ec_size);
    let size2 = self.active.get(&head2).map_or(0, |t| t.ec_size);
    if size1 == 0 || size2 == 0 { return; }  // already committed

    // Fold smaller into larger.
    let (small_head, large_head) = if size1 <= size2 {
        (head1, head2)
    } else {
        (head2, head1)
    };

    // Move members from small to large.
    let small_members: Vec<TransactionId> = {
        let small = self.active.get_mut(&small_head).unwrap();
        small.ec_head = large_head;
        let members = std::mem::take(&mut small.ec_members);
        small.ec_size = 0;
        members
    };

    // Redirect all small EC members to point to large head.
    for &member_id in &small_members {
        if let Some(member) = self.active.get_mut(&member_id) {
            member.ec_head = large_head;
        }
    }

    // Add small head + its members to large EC.
    let large = self.active.get_mut(&large_head).unwrap();
    large.ec_size += if small_members.is_empty() { 1 } else { small_members.len() as u32 + 1 };
    large.ec_members.push(small_head);
    large.ec_members.extend(small_members);

    log::debug!(
        "EC merge: txn {} + txn {} → head {} (size {})",
        txn1, txn2, large_head, large.ec_size
    );
}

/// Walk the ec_head chain to find the actual head.
fn find_ec_head(&self, txn_id: TransactionId) -> TransactionId {
    let mut current = txn_id;
    loop {
        match self.active.get(&current) {
            Some(txn) if txn.ec_head == current => return current,
            Some(txn) => current = txn.ec_head,
            None => return txn_id,  // transaction already committed
        }
    }
}
```

### Phase 5: Commit Protocol (BEND/END)

Add new log record types:
```rust
pub enum LogRecordType {
    Data,
    End,
    /// Batch end — member of an EC that has finished.
    BatchEnd,
    Checkpoint,
    Restart,
}
```

New method on `TransactionManager`:
```rust
/// End a transaction. If this is the last member of its EC,
/// triggers the atomic commit (BEND for members, END for head).
/// Returns the pending_writes from ALL transactions in the EC.
pub fn end_txn(&mut self, txn_id: TransactionId) -> io::Result<Vec<(ObjectId, Vec<u8>)>> {
    let head_id = self.find_ec_head(txn_id);

    // Decrement EC size on the head.
    let ec_size = {
        let head = self.active.get_mut(&head_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "EC head not found"))?;
        head.ec_size -= 1;
        head.ec_size
    };

    if ec_size > 0 {
        // Not the last member. Mark as pending, don't commit yet.
        let txn = self.active.get_mut(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "txn not found"))?;
        txn.status = TransactionStatus::Pending;
        return Ok(Vec::new());  // No pending_writes returned until EC commits
    }

    // Last member — commit the entire EC atomically.
    let head = self.active.get(&head_id).unwrap();
    let member_ids: Vec<TransactionId> = head.ec_members.clone();

    // Collect all pending_writes from all EC members.
    let mut all_pending = Vec::new();

    // Write BEND for each non-head member.
    for &member_id in &member_ids {
        self.log.log_batch_end(member_id, head_id)?;
        if let Some(member) = self.active.get_mut(&member_id) {
            all_pending.extend(std::mem::take(&mut member.pending_writes));
            member.status = TransactionStatus::Committed;
            self.admission.release(&member.reservation);
        }
    }

    // Write END for the head.
    self.log.log_end(head_id)?;

    // Mark head as committed, collect its pending_writes.
    let head = self.active.get_mut(&head_id).unwrap();
    all_pending.extend(std::mem::take(&mut head.pending_writes));
    head.status = TransactionStatus::Committed;
    self.admission.release(&head.reservation);

    // Remove all EC members from active set.
    for &member_id in &member_ids {
        self.active.remove(&member_id);
    }
    self.active.remove(&head_id);

    Ok(all_pending)
}
```

Add `log_batch_end` to `LogManager`:
```rust
pub fn log_batch_end(
    &mut self,
    member_id: TransactionId,
    head_id: TransactionId,
) -> io::Result<LogSequenceNumber> {
    // Write a BEND record that references the EC head.
    let record = LogRecord {
        record_type: LogRecordType::BatchEnd,
        txn_id: member_id,
        lsn: self.next_lsn,
        payload: LogPayload::BatchEnd { ec_head: head_id },
    };
    self.write_record(&record)?;
    let lsn = self.next_lsn;
    self.next_lsn += 1;
    Ok(lsn)
}
```

Add `BatchEnd` to `LogPayload`:
```rust
pub enum LogPayload {
    ContentChange { ... },
    AddNode { ... },
    RemoveNode { ... },
    AddEdge { ... },
    RemoveEdge { ... },
    /// Batch end — this member is part of an EC headed by ec_head.
    BatchEnd { ec_head: TransactionId },
}
```

### Phase 6: FUSE Op Restructuring

The critical change: FUSE ops must not hold the txn_mgr lock during
graph operations. Example for `create`:

```rust
fn create(&mut self, req: &Request, parent: u64, name: &OsStr, ...) {
    // ... validate inputs ...

    // 1. Start transaction (brief lock).
    let txn_id = {
        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        txn_mgr.start_txn()?
    };

    // 2. Touch tlocks (may trigger EC merge).
    if let Some(other) = self.tlocks.touch(parent_id, txn_id) {
        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        txn_mgr.ec_merge(txn_id, other);
    }

    // 3. Create node, prepare data.
    let file_id = uuid::Uuid::new_v4();
    let file_node = GraphNode::new_file(file_id, ...);
    let node_data = bincode::serialize(&file_node).unwrap_or_default();

    // 4. Touch tlock for the new node.
    self.tlocks.touch(file_id, txn_id);  // no conflict possible, new node

    // 5. Log operations (brief lock per call).
    {
        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        txn_mgr.log_add_node(txn_id, file_id, node_data)?;
        txn_mgr.log_add_edge(txn_id, parent_id, file_id, ...)?;
        txn_mgr.log_content_change(txn_id, file_id, None, Some(EMPTY_HASH))?;
    }

    // 6. Apply to in-memory state.
    self.graph.add_node(file_node);
    self.graph.add_edge(parent_id, file_id, ...)?;
    self.store.register_empty(file_id);

    // 7. End transaction (may trigger EC commit if last in EC).
    let pending = {
        let mut txn_mgr = self.txn_mgr.lock().unwrap();
        txn_mgr.end_txn(txn_id)?
    };

    // 8. Flush pending writes if NO-STEAL and EC committed.
    if self.no_steal && !pending.is_empty() {
        self.store.flush_pending(&pending)?;
    }

    // 9. Release tlocks.
    self.tlocks.release_all(txn_id);

    reply.created(...);
}
```

### Phase 7: Waiting for EC Commit

When `end_txn` returns early (ec_size > 0), the current transaction's
FUSE op must WAIT for the EC to commit before replying. We need a
notification mechanism:

```rust
pub struct Transaction {
    // ... existing fields ...
    /// Signaled when the EC this transaction belongs to commits.
    pub commit_signal: Option<Arc<(Mutex<bool>, Condvar)>>,
}
```

When `end_txn` returns with ec_size > 0:
- Create a `commit_signal` on the transaction
- Return a `Waiter` that the FUSE op can block on
- When `CompleteEC` fires (last member ends), signal all waiters

```rust
pub enum EndTxnResult {
    /// This transaction (or its EC) committed. Here are the pending writes.
    Committed(Vec<(ObjectId, Vec<u8>)>),
    /// This transaction is waiting for EC commit. Block on the waiter.
    Waiting(Arc<(Mutex<bool>, Condvar)>),
}
```

The FUSE op then:
```rust
match txn_mgr.end_txn(txn_id)? {
    EndTxnResult::Committed(pending) => {
        // handle pending writes, reply
    }
    EndTxnResult::Waiting(signal) => {
        // Release txn_mgr lock, wait for signal
        drop(txn_mgr);
        let (lock, cvar) = &*signal;
        let mut committed = lock.lock().unwrap();
        while !*committed {
            committed = cvar.wait(committed).unwrap();
        }
        // EC committed, reply
    }
}
```

### Recovery Changes

Recovery must handle BEND records:
- BEND records indicate the transaction is part of an EC
- The EC is committed only if the head's END record is present
- If BEND exists but no END for the EC head → undo the member
- If BEND exists and END for the EC head → redo the member

In `src/txn/recovery.rs`:
```rust
// During log scan:
LogRecordType::BatchEnd => {
    if let LogPayload::BatchEnd { ec_head } = &record.payload {
        // Link this transaction to the EC head.
        ec_membership.insert(record.txn_id, *ec_head);
    }
}

// When determining committed vs uncommitted:
// A transaction is committed if:
//   - It has an END record (singleton or EC head), OR
//   - It has a BEND record and its EC head has an END record
```

## Stats

Add EC merge counters to `FsStats`:
```rust
pub ec_merges: AtomicU64,       // number of EC merge operations
pub ec_max_size: AtomicU64,     // largest EC seen
pub ec_commits: AtomicU64,      // number of EC group commits (vs singleton)
```

## Testing

### Unit tests
1. Two transactions, same node conflict → they merge
2. Three transactions, chain conflict → all in one EC
3. Singleton transaction (no conflict) → commits normally
4. EC size tracking is correct after merges
5. find_ec_head walks chain correctly

### Integration tests
1. Concurrent mkdir in same directory → EC merge triggers
2. Concurrent file creates in same directory → EC merge
3. Mixed operations: create + unlink in same dir → EC merge
4. Verify all committed data survives (same as existing crash tests)

### Stress test
1. 8 threads, 100 file creates each, all in same directory
2. Verify all 800 files exist after completion
3. Verify EC merge stats show merges occurred
4. Compare throughput: single-threaded vs multi-threaded with EC merge

## Implementation Order

1. Add EC fields to Transaction struct + ec_merge/find_ec_head/end_txn
2. Add BatchEnd log record type and log_batch_end
3. Add TlockTable
4. Restructure ONE FUSE op (create) as proof of concept
5. Verify existing tests still pass
6. Add the waiting mechanism (Condvar)
7. Restructure remaining FUSE ops
8. Update recovery to handle BEND records
9. Add stats counters
10. Write new tests

## Success Criteria

1. All 44 existing tests pass (they exercise single-threaded paths, which
   are singleton ECs that commit immediately)
2. New concurrent test shows EC merges occurring under directory contention
3. EC merge stats are reported on unmount
4. Recovery correctly handles BEND records
5. No deadlocks under concurrent operation
