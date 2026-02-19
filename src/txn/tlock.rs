//! Transaction locks (tlocks) — per-node conflict detection.
//!
//! When a transaction touches a graph node, we record it. If a different
//! transaction already holds the tlock, the caller should trigger EC merge.
//!
//! Following Episode's design: the merge check happens at touch time
//! (when we assign the new holder).
//!
//! Access counters: every touch increments a per-node counter, giving us
//! the directory mutation frequency distribution (Pareto characterization).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{ObjectId, TransactionId};

/// A transaction lock on a graph node.
struct TlockEntry {
    /// Transaction that currently "owns" this lock (last writer).
    holder: Option<TransactionId>,
    /// Total number of times this node has been touched (mutation frequency).
    touch_count: u64,
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
        let entry = locks.entry(node_id).or_insert(TlockEntry {
            holder: None,
            touch_count: 0,
        });
        entry.touch_count += 1;
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

    /// Snapshot of per-node touch counts, sorted descending by frequency.
    /// Returns (node_id, touch_count) pairs.
    pub fn access_distribution(&self) -> Vec<(ObjectId, u64)> {
        let locks = self.locks.lock().unwrap();
        let mut dist: Vec<(ObjectId, u64)> = locks
            .iter()
            .map(|(id, entry)| (*id, entry.touch_count))
            .collect();
        dist.sort_by(|a, b| b.1.cmp(&a.1));
        dist
    }

    /// Summary statistics: total touches, unique nodes, and top-N concentration.
    pub fn access_summary(&self) -> AccessSummary {
        let dist = self.access_distribution();
        let total: u64 = dist.iter().map(|(_, c)| c).sum();
        let unique = dist.len();

        // Compute cumulative concentration at 1%, 5%, 10% of nodes.
        let concentration = [0.01, 0.05, 0.10]
            .iter()
            .map(|frac| {
                let top_n = ((unique as f64 * frac).ceil() as usize).max(1).min(unique);
                let top_sum: u64 = dist[..top_n].iter().map(|(_, c)| c).sum();
                (*frac, top_sum as f64 / total.max(1) as f64)
            })
            .collect();

        AccessSummary {
            total_touches: total,
            unique_nodes: unique,
            concentration,
        }
    }
}

/// Summary of tlock access distribution.
pub struct AccessSummary {
    pub total_touches: u64,
    pub unique_nodes: usize,
    /// (fraction_of_nodes, fraction_of_touches) — e.g., (0.05, 0.93) means
    /// 5% of nodes account for 93% of touches.
    pub concentration: Vec<(f64, f64)>,
}

impl std::fmt::Display for AccessSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "  Tlock touches: {} across {} nodes",
            self.total_touches, self.unique_nodes
        )?;
        for (frac, conc) in &self.concentration {
            writeln!(
                f,
                "    Top {:>4.0}% of nodes: {:>5.1}% of touches",
                frac * 100.0,
                conc * 100.0
            )?;
        }
        Ok(())
    }
}
