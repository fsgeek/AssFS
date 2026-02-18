//! Recovery engine — redo committed, undo uncommitted.
//!
//! On mount, scan the log to find committed vs uncommitted transactions.
//! Redo committed: replay all operations (content changes AND graph mutations).
//! Undo uncommitted: reverse all operations.
//!
//! Both the object index and the graph state are corrected.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::graph::node::GraphNode;
use crate::graph::operations::GraphNamespace;
use crate::store::content::ContentStore;
use crate::TransactionId;

use super::log::{LogManager, LogPayload, LogRecordType};

/// Result of running recovery.
#[derive(Debug)]
pub struct RecoveryResult {
    pub committed_count: usize,
    pub uncommitted_count: usize,
    pub max_txn_id: TransactionId,
}

/// Run recovery: scan log, redo committed, undo uncommitted.
pub fn recover(
    log_path: &Path,
    store: &ContentStore,
    graph: &GraphNamespace,
) -> io::Result<RecoveryResult> {
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

    let uncommitted: HashSet<TransactionId> =
        all_txn_ids.difference(&committed).copied().collect();

    log::info!(
        "Recovery: {} committed, {} uncommitted transactions",
        committed.len(),
        uncommitted.len()
    );

    // Phase 2: Redo committed transactions (in LSN order).
    for record in &records {
        if record.record_type != LogRecordType::Data {
            continue;
        }
        if !committed.contains(&record.transaction_id) {
            continue;
        }
        if let Some(ref payload) = record.payload {
            redo_payload(payload, store, graph);
        }
    }

    // Phase 3: Undo uncommitted transactions (in reverse LSN order).
    let mut uncommitted_records: Vec<_> = records
        .iter()
        .filter(|r| {
            r.record_type == LogRecordType::Data && uncommitted.contains(&r.transaction_id)
        })
        .collect();
    uncommitted_records.sort_by(|a, b| b.lsn.cmp(&a.lsn));

    for record in &uncommitted_records {
        if let Some(ref payload) = record.payload {
            undo_payload(payload, store, graph);
        }
    }

    // Phase 4: Flush corrected state.
    store.flush_index()?;
    graph.flush()?;

    Ok(RecoveryResult {
        committed_count: committed.len(),
        uncommitted_count: uncommitted.len(),
        max_txn_id,
    })
}

/// Apply a payload forward (redo).
fn redo_payload(payload: &LogPayload, store: &ContentStore, graph: &GraphNamespace) {
    match payload {
        LogPayload::ContentChange {
            object_id,
            new_hash,
            ..
        } => {
            if let Some(hash) = new_hash {
                store.set_hash(*object_id, *hash);
            } else {
                store.remove_object(object_id);
            }
        }
        LogPayload::AddNode { node_data, node_id } => {
            if let Ok(node) = bincode::deserialize::<GraphNode>(node_data) {
                graph.add_node(node);
            } else {
                log::error!("Recovery: failed to deserialize AddNode for {}", node_id);
            }
        }
        LogPayload::RemoveNode { node_id, .. } => {
            graph.remove_node(node_id);
        }
        LogPayload::AddEdge {
            source,
            target,
            edge_type,
            label,
        } => {
            let _ = graph.add_edge(*source, *target, *edge_type, label.clone());
        }
        LogPayload::RemoveEdge {
            source,
            edge_type,
            label,
            ..
        } => {
            graph.remove_edge(source, *edge_type, label);
        }
    }
}

/// Reverse a payload (undo). Each operation is the inverse of redo.
fn undo_payload(payload: &LogPayload, store: &ContentStore, graph: &GraphNamespace) {
    match payload {
        LogPayload::ContentChange {
            object_id,
            old_hash,
            ..
        } => {
            if let Some(hash) = old_hash {
                store.set_hash(*object_id, *hash);
            } else {
                store.remove_object(object_id);
            }
        }
        // Undo AddNode = remove it.
        LogPayload::AddNode { node_id, .. } => {
            graph.remove_node(node_id);
        }
        // Undo RemoveNode = re-add it.
        LogPayload::RemoveNode { node_data, node_id } => {
            if let Ok(node) = bincode::deserialize::<GraphNode>(node_data) {
                graph.add_node(node);
            } else {
                log::error!("Recovery: failed to deserialize RemoveNode for {}", node_id);
            }
        }
        // Undo AddEdge = remove it.
        LogPayload::AddEdge {
            source,
            edge_type,
            label,
            ..
        } => {
            graph.remove_edge(source, *edge_type, label);
        }
        // Undo RemoveEdge = re-add it.
        LogPayload::RemoveEdge {
            source,
            target,
            edge_type,
            label,
        } => {
            let _ = graph.add_edge(*source, *target, *edge_type, label.clone());
        }
    }
}
