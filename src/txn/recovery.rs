//! Recovery engine — redo committed, undo uncommitted.
//!
//! On mount, scan the log to find committed vs uncommitted transactions.
//! Redo committed: replay all operations (content changes AND graph mutations).
//! Undo uncommitted: reverse all operations.
//!
//! Both the object index and the graph state are corrected.

use std::collections::{HashMap, HashSet};
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

    // Phase 1: Identify committed transactions.
    // A transaction is committed if:
    //   - It has an END record (singleton or EC head), OR
    //   - It has a BEND record and its EC head has an END record.
    let mut committed: HashSet<TransactionId> = HashSet::new();
    let mut all_txn_ids: HashSet<TransactionId> = HashSet::new();
    let mut max_txn_id: TransactionId = 0;
    // EC membership: member -> ec_head (from BEND records).
    let mut ec_membership: HashMap<TransactionId, TransactionId> = HashMap::new();

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
        if record.record_type == LogRecordType::BatchEnd {
            if let Some(LogPayload::BatchEnd { ec_head }) = &record.payload {
                ec_membership.insert(record.transaction_id, *ec_head);
            }
        }
    }

    // A transaction with BEND is committed if its ec_head has an END record.
    for (&member_id, &head_id) in &ec_membership {
        if committed.contains(&head_id) {
            committed.insert(member_id);
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

    // Phase 4: Reconcile file sizes with actual blob sizes.
    // Write operations update node metadata (size, mtime) in-memory but don't
    // log these changes. After redo/undo, the graph node may have stale size
    // from the original AddNode log record. Fix this by stat-ing each blob.
    {
        let file_node_ids: Vec<crate::ObjectId> = {
            let state = graph.state.lock().unwrap();
            state
                .nodes
                .iter()
                .filter(|(_, n)| n.node_type == crate::NodeType::File)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in file_node_ids {
            if let Ok(size) = store.get_content_size(&id) {
                graph.update_node(&id, |node| {
                    node.metadata.size = size;
                });
            }
        }
    }

    // Phase 5: Flush corrected state.
    store.flush_index()?;
    graph.flush()?;

    Ok(RecoveryResult {
        committed_count: committed.len(),
        uncommitted_count: uncommitted.len(),
        max_txn_id,
    })
}

/// Apply a payload forward (redo).
///
/// Redo must be idempotent: the persisted graph/index state may already
/// reflect this operation (from a prior flush). We guard AddNode and
/// AddEdge with existence checks so replaying old log records doesn't
/// stomp on more-recent persisted metadata (e.g., a node's size updated
/// by a later write that only logged a ContentChange, not a full node
/// snapshot). ContentChange and Remove* are naturally idempotent.
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
            // Skip if node already exists — persisted state is at least as recent
            // as this log record, and may have metadata updates (size, mtime) that
            // the log record's creation-time snapshot doesn't capture.
            if !graph.node_exists(node_id) {
                if let Ok(node) = bincode::deserialize::<GraphNode>(node_data) {
                    graph.add_node(node);
                } else {
                    log::error!("Recovery: failed to deserialize AddNode for {}", node_id);
                }
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
            // Skip if edge already exists — avoids duplicate reverse-index entries
            // and preserves any metadata updates the persisted state may have.
            if graph.resolve(source, *edge_type, label).is_none() {
                let _ = graph.add_edge(*source, *target, *edge_type, label.clone());
            }
        }
        LogPayload::RemoveEdge {
            source,
            edge_type,
            label,
            ..
        } => {
            graph.remove_edge(source, *edge_type, label);
        }
        // BatchEnd payloads carry no mutation data — EC membership is handled
        // at the transaction level, not per-record.
        LogPayload::BatchEnd { .. } => {}
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
        // BatchEnd payloads carry no mutation data.
        LogPayload::BatchEnd { .. } => {}
    }
}
