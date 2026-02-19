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
}

/// The transaction manager.
pub struct TransactionManager {
    pub log: LogManager,
    admission: AdmissionController,
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

    /// Write a checkpoint record.
    pub fn checkpoint(&mut self) -> io::Result<LogSequenceNumber> {
        self.log.log_checkpoint()
    }

    /// Check if any transactions are active.
    pub fn has_active_transactions(&self) -> bool {
        !self.active.is_empty()
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
