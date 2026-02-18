//! Transaction manager — single-threaded MVP.
//!
//! Lifecycle: StartTran → Operations → EndTran → Commit (log sync).
//! EC merge deferred to Phase 2.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::{ContentHash, LogSequenceNumber, ObjectId, TransactionId};

use super::log::LogManager;
use super::reservation::{AdmissionController, Reservation, MAX_TRANSACTION_SIZE};

/// Transaction status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    Active,
    Committed,
    Aborted,
}

/// A single transaction.
#[derive(Debug)]
pub struct Transaction {
    pub id: TransactionId,
    pub status: TransactionStatus,
    pub reservation: Reservation,
    pub first_lsn: Option<LogSequenceNumber>,
    pub last_lsn: Option<LogSequenceNumber>,
    /// Undo information: object_id → old_hash (for abort).
    pub undo_map: HashMap<ObjectId, Option<ContentHash>>,
}

/// The transaction manager.
pub struct TransactionManager {
    pub log: LogManager,
    admission: AdmissionController,
    next_txn_id: TransactionId,
    /// Active transactions by id.
    active: HashMap<TransactionId, Transaction>,
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
            undo_map: HashMap::new(),
        };

        self.active.insert(id, txn);
        Ok(id)
    }

    /// Log a data modification within a transaction.
    pub fn log_write(
        &mut self,
        txn_id: TransactionId,
        object_id: ObjectId,
        old_hash: Option<ContentHash>,
        new_hash: Option<ContentHash>,
    ) -> io::Result<LogSequenceNumber> {
        let lsn = self.log.log_data(txn_id, object_id, old_hash, new_hash)?;

        let txn = self
            .active
            .get_mut(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;

        if txn.first_lsn.is_none() {
            txn.first_lsn = Some(lsn);
        }
        txn.last_lsn = Some(lsn);

        // Record undo info (only first write per object matters for undo).
        txn.undo_map.entry(object_id).or_insert(old_hash);

        // Track reservation usage (approximate — record size varies).
        txn.reservation.use_bytes(128); // conservative estimate per record

        Ok(lsn)
    }

    /// Commit a transaction: write END record, sync, mark committed.
    pub fn commit_txn(&mut self, txn_id: TransactionId) -> io::Result<()> {
        // Write END record — when this hits stable storage, txn is committed.
        let lsn = self.log.log_end(txn_id)?;

        let txn = self
            .active
            .get_mut(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;

        txn.last_lsn = Some(lsn);
        txn.status = TransactionStatus::Committed;

        // Release reservation.
        self.admission.release(&txn.reservation);

        // Remove from active set.
        self.active.remove(&txn_id);

        Ok(())
    }

    /// Abort a transaction. Returns the undo map so the caller can revert.
    pub fn abort_txn(
        &mut self,
        txn_id: TransactionId,
    ) -> io::Result<HashMap<ObjectId, Option<ContentHash>>> {
        let txn = self
            .active
            .remove(&txn_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transaction not found"))?;

        // Release reservation.
        self.admission.release(&txn.reservation);

        // Return undo map — caller reverts the object index.
        Ok(txn.undo_map)
    }

    /// Write a checkpoint record.
    pub fn checkpoint(&mut self) -> io::Result<LogSequenceNumber> {
        self.log.log_checkpoint()
    }

    /// Check if any transactions are active.
    pub fn has_active_transactions(&self) -> bool {
        !self.active.is_empty()
    }

    /// Get undo map for a specific transaction (for testing/debugging).
    pub fn get_undo_map(
        &self,
        txn_id: TransactionId,
    ) -> Option<&HashMap<ObjectId, Option<ContentHash>>> {
        self.active.get(&txn_id).map(|t| &t.undo_map)
    }
}
