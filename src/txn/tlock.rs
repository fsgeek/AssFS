//! Transaction locks (tlocks) — per-node conflict detection.
//!
//! When a transaction touches a graph node, we record it. If a different
//! transaction already holds the tlock, the caller should trigger EC merge.
//!
//! Following Episode's design: the merge check happens at touch time
//! (when we assign the new holder).

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
    /// the tlock -- the caller should trigger EC merge.
    pub fn touch(
        &self,
        node_id: ObjectId,
        txn_id: TransactionId,
    ) -> Option<TransactionId> {
        let mut locks = self.locks.lock().unwrap();
        let entry = locks.entry(node_id).or_insert(TlockEntry { holder: None });
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
