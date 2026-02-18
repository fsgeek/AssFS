//! Recovery engine — redo committed, undo uncommitted.
//!
//! On mount, scan the log to find committed vs uncommitted transactions.
//! Redo committed: ensure object_id → new_content_hash in the index.
//! Undo uncommitted: revert object_id → old_content_hash in the index.
//!
//! Simpler than Episode: content blobs are never overwritten, only the
//! object index (ObjectId → ContentHash mapping) needs fixing.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::store::content::ContentStore;
use crate::TransactionId;

use super::log::{LogManager, LogRecord, LogRecordType};

/// Result of running recovery.
#[derive(Debug)]
pub struct RecoveryResult {
    /// Number of committed transactions replayed.
    pub committed_count: usize,
    /// Number of uncommitted transactions rolled back.
    pub uncommitted_count: usize,
    /// Highest transaction ID seen (for TransactionManager restart).
    pub max_txn_id: TransactionId,
}

/// Run recovery: scan log, redo committed, undo uncommitted.
pub fn recover(log_path: &Path, store: &ContentStore) -> io::Result<RecoveryResult> {
    let records = LogManager::read_all_records(log_path)?;

    if records.is_empty() {
        log::info!("Recovery: empty log, nothing to do");
        return Ok(RecoveryResult {
            committed_count: 0,
            uncommitted_count: 0,
            max_txn_id: 0,
        });
    }

    // Phase 1: Identify committed transactions (those with an End record).
    let mut committed: HashSet<TransactionId> = HashSet::new();
    let mut all_txn_ids: HashSet<TransactionId> = HashSet::new();
    let mut max_txn_id: TransactionId = 0;

    for record in &records {
        if record.transaction_id > 0 {
            all_txn_ids.insert(record.transaction_id);
            if record.transaction_id > max_txn_id {
                max_txn_id = record.transaction_id;
            }
        }
        if record.record_type == LogRecordType::End {
            committed.insert(record.transaction_id);
        }
    }

    let uncommitted: HashSet<TransactionId> = all_txn_ids
        .difference(&committed)
        .copied()
        .collect();

    log::info!(
        "Recovery: {} committed, {} uncommitted transactions",
        committed.len(),
        uncommitted.len()
    );

    // Phase 2: Redo committed transactions.
    // For each committed txn, ensure the object index reflects new_hash.
    // Process in LSN order so later writes win.
    for record in &records {
        if record.record_type != LogRecordType::Data {
            continue;
        }
        if !committed.contains(&record.transaction_id) {
            continue;
        }
        if let Some(new_hash) = record.new_hash {
            store.set_hash(record.object_id, new_hash);
        } else {
            // new_hash is None → object was deleted by this txn.
            store.remove_object(&record.object_id);
        }
    }

    // Phase 3: Undo uncommitted transactions.
    // For each uncommitted txn, revert the object index to old_hash.
    // Process in reverse LSN order so we undo most recent first.
    let mut uncommitted_records: Vec<&LogRecord> = records
        .iter()
        .filter(|r| {
            r.record_type == LogRecordType::Data && uncommitted.contains(&r.transaction_id)
        })
        .collect();
    uncommitted_records.sort_by(|a, b| b.lsn.cmp(&a.lsn)); // reverse LSN order

    for record in &uncommitted_records {
        if let Some(old_hash) = record.old_hash {
            store.set_hash(record.object_id, old_hash);
        } else {
            // old_hash is None → object didn't exist before this txn.
            store.remove_object(&record.object_id);
        }
    }

    // Phase 4: Flush the corrected index to disk.
    store.flush_index()?;

    Ok(RecoveryResult {
        committed_count: committed.len(),
        uncommitted_count: uncommitted.len(),
        max_txn_id,
    })
}
