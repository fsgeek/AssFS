//! Transaction manager — single-threaded MVP.
//!
//! Lifecycle: StartTran → Operations → EndTran → Commit (log sync).
//! All mutations (content changes AND graph changes) go through here
//! so they're logged and recoverable.
//! EC merge deferred to Phase 2.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::{ContentHash, EdgeType, LogSequenceNumber, ObjectId, TransactionId};

use super::log::{LogManager, LogPayload};
use super::reservation::{AdmissionController, Reservation, MAX_TRANSACTION_SIZE};

/// Transaction status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    Active,
    /// Transaction has finished its work but is waiting for EC commit.
    Pending,
    Committed,
    Aborted,
}

/// Undo entry — one step to reverse on abort or recovery.
#[derive(Debug, Clone)]
pub enum UndoEntry {
    /// Revert object_id's content hash to old_hash.
    ContentRevert {
        object_id: ObjectId,
        old_hash: Option<ContentHash>,
    },
    /// Remove a node that was added.
    RemoveNode { node_id: ObjectId },
    /// Re-add a node that was removed (serialized GraphNode).
    RestoreNode { node_data: Vec<u8>, node_id: ObjectId },
    /// Remove an edge that was added.
    RemoveEdge {
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    },
    /// Re-add an edge that was removed.
    RestoreEdge {
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    },
}

/// A single transaction.
#[derive(Debug)]
pub struct Transaction {
    pub id: TransactionId,
    pub status: TransactionStatus,
    pub reservation: Reservation,
    pub first_lsn: Option<LogSequenceNumber>,
    pub last_lsn: Option<LogSequenceNumber>,
    /// Ordered list of undo entries (applied in reverse on abort).
    pub undo_log: Vec<UndoEntry>,
    /// NO-STEAL: content data buffered until commit.
    pub pending_writes: Vec<(ObjectId, Vec<u8>)>,
    // --- EC merge fields ---
    /// Points to EC head transaction ID. Self if this is the head.
    pub ec_head: TransactionId,
    /// Number of active members in this EC (only meaningful on head).
    pub ec_size: u32,
    /// Member list (only on head). Does NOT include the head itself.
    pub ec_members: Vec<TransactionId>,
}

/// The transaction manager.
pub struct TransactionManager {
    pub log: LogManager,
    pub admission: AdmissionController,
    next_txn_id: TransactionId,
    pub active: HashMap<TransactionId, Transaction>,
}

impl TransactionManager {
    pub fn open(log_path: &Path) -> io::Result<Self> {
        let log = LogManager::open(log_path)?;
        Ok(Self {
            log,
            admission: AdmissionController::new(),
            next_txn_id: 1,
            active: HashMap::new(),
        })
    }

    /// Restore state after recovery (set next txn id past any recovered ones).
    pub fn set_next_txn_id(&mut self, id: TransactionId) {
        if id >= self.next_txn_id {
            self.next_txn_id = id + 1;
        }
    }

    /// Start a new transaction.
    pub fn start_txn(&mut self) -> io::Result<TransactionId> {
        let reservation = self
            .admission
            .try_reserve(MAX_TRANSACTION_SIZE)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "log space exhausted, checkpoint needed",
                )
            })?;

        let id = self.next_txn_id;
        self.next_txn_id += 1;

        let txn = Transaction {
            id,
            status: TransactionStatus::Active,
            reservation,
            first_lsn: None,
            last_lsn: None,
            undo_log: Vec::new(),
            pending_writes: Vec::new(),
            ec_head: id,
            ec_size: 1,
            ec_members: Vec::new(),
        };

        self.active.insert(id, txn);
        Ok(id)
    }

    // --- Content operations ---

    /// Log a content hash change.
    pub fn log_content_change(
        &mut self,
        txn_id: TransactionId,
        object_id: ObjectId,
        old_hash: Option<ContentHash>,
        new_hash: Option<ContentHash>,
    ) -> io::Result<LogSequenceNumber> {
        let payload = LogPayload::ContentChange {
            object_id,
            old_hash,
            new_hash,
        };
        let lsn = self.log.log_data(txn_id, payload)?;
        self.record_lsn(txn_id, lsn)?;

        let txn = self.active.get_mut(&txn_id).unwrap();
        txn.undo_log.push(UndoEntry::ContentRevert {
            object_id,
            old_hash,
        });
        txn.reservation.use_bytes(128);

        Ok(lsn)
    }

    // --- Graph node operations ---

    /// Log a node addition. `node_data` is the serialized GraphNode.
    pub fn log_add_node(
        &mut self,
        txn_id: TransactionId,
        node_id: ObjectId,
        node_data: Vec<u8>,
    ) -> io::Result<LogSequenceNumber> {
        let payload = LogPayload::AddNode {
            node_data: node_data.clone(),
            node_id,
        };
        let lsn = self.log.log_data(txn_id, payload)?;
        self.record_lsn(txn_id, lsn)?;

        let txn = self.active.get_mut(&txn_id).unwrap();
        txn.undo_log.push(UndoEntry::RemoveNode { node_id });
        txn.reservation.use_bytes(node_data.len() as u32 + 64);

        Ok(lsn)
    }

    /// Log a node removal. `node_data` is the serialized GraphNode (for undo).
    pub fn log_remove_node(
        &mut self,
        txn_id: TransactionId,
        node_id: ObjectId,
        node_data: Vec<u8>,
    ) -> io::Result<LogSequenceNumber> {
        let payload = LogPayload::RemoveNode {
            node_data: node_data.clone(),
            node_id,
        };
        let lsn = self.log.log_data(txn_id, payload)?;
        self.record_lsn(txn_id, lsn)?;

        let txn = self.active.get_mut(&txn_id).unwrap();
        txn.undo_log.push(UndoEntry::RestoreNode {
            node_data,
            node_id,
        });
        txn.reservation.use_bytes(128);

        Ok(lsn)
    }

    // --- Graph edge operations ---

    /// Log an edge addition.
    pub fn log_add_edge(
        &mut self,
        txn_id: TransactionId,
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    ) -> io::Result<LogSequenceNumber> {
        let payload = LogPayload::AddEdge {
            source,
            target,
            edge_type,
            label: label.clone(),
        };
        let lsn = self.log.log_data(txn_id, payload)?;
        self.record_lsn(txn_id, lsn)?;

        let txn = self.active.get_mut(&txn_id).unwrap();
        txn.undo_log.push(UndoEntry::RemoveEdge {
            source,
            target,
            edge_type,
            label,
        });
        txn.reservation.use_bytes(128);

        Ok(lsn)
    }

    /// Log an edge removal.
    pub fn log_remove_edge(
        &mut self,
        txn_id: TransactionId,
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    ) -> io::Result<LogSequenceNumber> {
        let payload = LogPayload::RemoveEdge {
            source,
            target,
            edge_type,
            label: label.clone(),
        };
        let lsn = self.log.log_data(txn_id, payload)?;
        self.record_lsn(txn_id, lsn)?;

        let txn = self.active.get_mut(&txn_id).unwrap();
        txn.undo_log.push(UndoEntry::RestoreEdge {
            source,
            target,
            edge_type,
            label,
        });
        txn.reservation.use_bytes(128);

        Ok(lsn)
    }

    // --- Transaction lifecycle ---

    /// Commit a transaction: write END record, sync, mark committed.
    /// Returns any pending writes (NO-STEAL mode) so the caller can flush them.
    pub fn commit_txn(&mut self, txn_id: TransactionId) -> io::Result<Vec<(ObjectId, Vec<u8>)>> {
        // Progressive release: tighten reservation before writing END.
        // This releases unused data space back to the global pool,
        // allowing other transactions to start sooner.
        {
            let txn = self
                .active
                .get_mut(&txn_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;
            let released = txn.reservation.tighten();
            if released > 0 {
                self.admission.release_partial(released);
            }
        }

        let lsn = self.log.log_end(txn_id)?;

        let txn = self
            .active
            .get_mut(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;

        txn.last_lsn = Some(lsn);
        txn.status = TransactionStatus::Committed;
        self.admission.release(&txn.reservation);
        let txn = self.active.remove(&txn_id).unwrap();

        Ok(txn.pending_writes)
    }

    /// Abort a transaction. Returns the undo log so the caller can revert
    /// both content and graph state.
    pub fn abort_txn(&mut self, txn_id: TransactionId) -> io::Result<Vec<UndoEntry>> {
        let txn = self
            .active
            .remove(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;

        self.admission.release(&txn.reservation);
        Ok(txn.undo_log)
    }

    /// Write a checkpoint record (without truncation).
    pub fn log_checkpoint(&mut self) -> io::Result<LogSequenceNumber> {
        self.log.log_checkpoint()
    }

    /// Full checkpoint: truncate the log and reset admission control.
    ///
    /// Precondition: the caller MUST flush graph state and content index
    /// to disk BEFORE calling this. This method only handles the log and
    /// admission controller — it doesn't know about graph/store state.
    ///
    /// Postcondition: the log contains only a restart sentinel, and
    /// admission control is fully reset (all space reclaimed).
    pub fn checkpoint(&mut self) -> io::Result<()> {
        if !self.active.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "checkpoint with {} active transactions",
                    self.active.len()
                ),
            ));
        }

        // Truncate the log and write a restart sentinel.
        self.log.truncate_and_restart()?;

        // Reset admission controller — all log space is reclaimed.
        self.admission.reset();

        log::info!("Checkpoint complete, admission control reset");
        Ok(())
    }

    /// Check if any transactions are active.
    pub fn has_active_transactions(&self) -> bool {
        !self.active.is_empty()
    }

    // --- EC merge ---

    /// Walk the ec_head chain to find the actual head of the equivalence class.
    pub fn find_ec_head(&self, txn_id: TransactionId) -> TransactionId {
        let mut current = txn_id;
        loop {
            match self.active.get(&current) {
                Some(txn) if txn.ec_head == current => return current,
                Some(txn) => current = txn.ec_head,
                None => return txn_id, // transaction already committed
            }
        }
    }

    /// Merge two transactions into one equivalence class.
    /// Follows Episode's algorithm: fold smaller EC into larger.
    ///
    /// Concurrency note: members of the small EC may have already called
    /// end_txn (status=Pending), which decremented the old small head's
    /// ec_size. We must only count truly active (non-Pending) members
    /// when computing the new ec_size on the large head, otherwise the
    /// EC will never reach ec_size=0 and leak transactions.
    pub fn ec_merge(&mut self, txn1: TransactionId, txn2: TransactionId) {
        if txn1 == txn2 {
            return;
        }

        // Find EC heads (walk chain).
        let head1 = self.find_ec_head(txn1);
        let head2 = self.find_ec_head(txn2);
        if head1 == head2 {
            return; // already in same EC
        }

        // Verify both are still active.
        let size1 = self.active.get(&head1).map_or(0, |t| t.ec_size);
        let size2 = self.active.get(&head2).map_or(0, |t| t.ec_size);
        if size1 == 0 || size2 == 0 {
            return; // already committed
        }

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

        // Count only active (non-Pending) members being transferred.
        // Members that already called end_txn decremented the old head's
        // ec_size, so they should not inflate the new head's ec_size.
        let small_head_active = self.active.get(&small_head)
            .map_or(false, |t| t.status != TransactionStatus::Pending);
        let active_member_count: u32 = small_members.iter()
            .filter(|mid| {
                self.active.get(mid)
                    .map_or(false, |t| t.status != TransactionStatus::Pending)
            })
            .count() as u32
            + if small_head_active { 1 } else { 0 };

        // Add small head + its members to large EC.
        let large = self.active.get_mut(&large_head).unwrap();
        large.ec_size += active_member_count;
        large.ec_members.push(small_head);
        large.ec_members.extend(small_members);

        log::debug!(
            "EC merge: txn {} + txn {} -> head {} (size {}, transferred {} active)",
            txn1,
            txn2,
            large_head,
            large.ec_size,
            active_member_count
        );
    }

    /// End a transaction. If this is the last member of its EC,
    /// triggers the atomic commit (BEND for members, END for head).
    /// Returns the pending_writes from ALL transactions in the EC.
    pub fn end_txn(&mut self, txn_id: TransactionId) -> io::Result<Vec<(ObjectId, Vec<u8>)>> {
        let head_id = self.find_ec_head(txn_id);

        // Decrement EC size on the head.
        let ec_size = {
            let head = self.active.get_mut(&head_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "EC head not found")
            })?;
            head.ec_size -= 1;
            head.ec_size
        };

        if ec_size > 0 {
            // Not the last member. Tighten reservation and mark as pending.
            let txn = self.active.get_mut(&txn_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "txn not found")
            })?;
            let released = txn.reservation.tighten();
            if released > 0 {
                self.admission.release_partial(released);
            }
            txn.status = TransactionStatus::Pending;
            return Ok(Vec::new()); // No pending_writes returned until EC commits
        }

        // Last member -- commit the entire EC atomically.
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

    // --- Internal ---

    fn record_lsn(&mut self, txn_id: TransactionId, lsn: LogSequenceNumber) -> io::Result<()> {
        let txn = self
            .active
            .get_mut(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;
        if txn.first_lsn.is_none() {
            txn.first_lsn = Some(lsn);
        }
        txn.last_lsn = Some(lsn);
        Ok(())
    }
}
