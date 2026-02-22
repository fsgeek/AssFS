//! Integration tests for AssFS core layers.

use std::io;

use assfs::txn::log::{LogManager, LogPayload, LogRecordType};
use assfs::txn::recovery;
use assfs::txn::tlock::TlockTable;
use assfs::txn::transaction::{TransactionManager, TransactionStatus, UndoEntry};
use assfs::graph::operations::GraphNamespace;
use assfs::graph::node::GraphNode;
use assfs::store::content::ContentStore;
use assfs::{content_hash, EdgeType, EMPTY_HASH};

// --- Log Manager Tests ---

#[test]
fn test_log_write_and_read_back() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"version A");
    let hash_b = content_hash(b"version B");

    // Write some records.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj_id,
                old_hash: None,
                new_hash: Some(hash_a),
            },
        )?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj_id,
                old_hash: Some(hash_a),
                new_hash: Some(hash_b),
            },
        )?;
        log.log_end(1)?;
    }

    // Read them back.
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].record_type, LogRecordType::Data);
    assert_eq!(records[0].transaction_id, 1);

    // Check payload of first record.
    match &records[0].payload {
        Some(LogPayload::ContentChange {
            object_id,
            old_hash,
            new_hash,
        }) => {
            assert_eq!(*object_id, obj_id);
            assert_eq!(*old_hash, None);
            assert_eq!(*new_hash, Some(hash_a));
        }
        other => panic!("expected ContentChange, got {:?}", other),
    }

    // Check payload of second record.
    match &records[1].payload {
        Some(LogPayload::ContentChange { new_hash, .. }) => {
            assert_eq!(*new_hash, Some(hash_b));
        }
        other => panic!("expected ContentChange, got {:?}", other),
    }

    assert_eq!(records[2].record_type, LogRecordType::End);

    Ok(())
}

#[test]
fn test_log_survives_reopen() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    // Write in first session.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: uuid::Uuid::new_v4(),
                old_hash: None,
                new_hash: Some(EMPTY_HASH),
            },
        )?;
        log.log_end(1)?;
    }

    // Write more in second session.
    {
        let mut log = LogManager::open(&log_path)?;
        assert_eq!(log.next_lsn(), 3); // should continue from where we left off
        log.log_data(
            2,
            LogPayload::ContentChange {
                object_id: uuid::Uuid::new_v4(),
                old_hash: None,
                new_hash: Some(EMPTY_HASH),
            },
        )?;
        log.log_end(2)?;
    }

    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 4);
    assert_eq!(records[2].transaction_id, 2);

    Ok(())
}

// --- Transaction Manager Tests ---

#[test]
fn test_transaction_commit() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let obj_id = uuid::Uuid::new_v4();
    let hash = content_hash(b"hello");

    let txn_id = txn_mgr.start_txn()?;
    txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash))?;
    let _pending = txn_mgr.commit_txn(txn_id)?;

    assert!(!txn_mgr.has_active_transactions());

    // Verify log has Data + End records.
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].record_type, LogRecordType::Data);
    assert_eq!(records[1].record_type, LogRecordType::End);

    Ok(())
}

#[test]
fn test_transaction_abort() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let obj_id = uuid::Uuid::new_v4();
    let hash = content_hash(b"data");

    let txn_id = txn_mgr.start_txn()?;
    txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash))?;
    let undo_log = txn_mgr.abort_txn(txn_id)?;

    // Undo log should have one ContentRevert entry with old_hash = None.
    assert_eq!(undo_log.len(), 1);
    match &undo_log[0] {
        UndoEntry::ContentRevert {
            object_id,
            old_hash,
        } => {
            assert_eq!(*object_id, obj_id);
            assert_eq!(*old_hash, None);
        }
        other => panic!("expected ContentRevert, got {:?}", other),
    }
    assert!(!txn_mgr.has_active_transactions());

    Ok(())
}

// --- Recovery Tests ---

#[test]
fn test_recovery_redo_committed() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;
    let graph = GraphNamespace::open(dir.path())?;

    let obj_id = uuid::Uuid::new_v4();
    let hash = content_hash(b"committed data");

    // Register obj in store with empty content.
    store.register_empty(obj_id);

    // Write committed transaction to log.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj_id,
                old_hash: Some(EMPTY_HASH),
                new_hash: Some(hash),
            },
        )?;
        log.log_end(1)?;
    }

    // Store the actual content.
    store.put_object(obj_id, b"committed data")?;

    // Simulate crash: clear the index (as if it wasn't flushed).
    store.set_hash(obj_id, EMPTY_HASH);

    // Run recovery — should redo committed txn.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 1);
    assert_eq!(result.uncommitted_count, 0);

    // Verify the index now points to the new hash.
    assert_eq!(store.get_hash(&obj_id), Some(hash));

    Ok(())
}

#[test]
fn test_recovery_undo_uncommitted() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;
    let graph = GraphNamespace::open(dir.path())?;

    let obj_id = uuid::Uuid::new_v4();
    let old_hash = content_hash(b"original");
    let new_hash = content_hash(b"modified");

    // Set up: object exists with original content.
    store.put_object(obj_id, b"original")?;

    // Write uncommitted transaction (no End record).
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj_id,
                old_hash: Some(old_hash),
                new_hash: Some(new_hash),
            },
        )?;
        // No log_end — simulating crash before commit.
    }

    // Simulate: index was updated to new_hash before crash.
    store.set_hash(obj_id, new_hash);

    // Run recovery — should undo uncommitted txn.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 0);
    assert_eq!(result.uncommitted_count, 1);

    // Verify the index reverted to old_hash.
    assert_eq!(store.get_hash(&obj_id), Some(old_hash));

    Ok(())
}

#[test]
fn test_recovery_mixed_committed_and_uncommitted() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;
    let graph = GraphNamespace::open(dir.path())?;

    let obj_a = uuid::Uuid::new_v4();
    let obj_b = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"A committed");
    let hash_b_old = content_hash(b"B original");
    let hash_b_new = content_hash(b"B modified");

    store.register_empty(obj_a);
    store.put_object(obj_b, b"B original")?;

    // Write log: txn 1 committed, txn 2 uncommitted.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj_a,
                old_hash: None,
                new_hash: Some(hash_a),
            },
        )?;
        log.log_end(1)?;
        log.log_data(
            2,
            LogPayload::ContentChange {
                object_id: obj_b,
                old_hash: Some(hash_b_old),
                new_hash: Some(hash_b_new),
            },
        )?;
        // No End for txn 2.
    }

    // Simulate crash state.
    store.set_hash(obj_a, EMPTY_HASH); // committed txn not yet in index
    store.set_hash(obj_b, hash_b_new); // uncommitted txn was in index

    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 1);
    assert_eq!(result.uncommitted_count, 1);

    // obj_a should have hash_a (redo).
    assert_eq!(store.get_hash(&obj_a), Some(hash_a));
    // obj_b should have hash_b_old (undo).
    assert_eq!(store.get_hash(&obj_b), Some(hash_b_old));

    Ok(())
}

// --- Graph Recovery Tests ---

#[test]
fn test_recovery_redo_committed_graph_ops() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;

    let parent_id = uuid::Uuid::new_v4();
    let file_id = uuid::Uuid::new_v4();

    let parent_node = GraphNode::new_dir(parent_id, 1000, 1000, 0o755);
    let file_node = GraphNode::new_file(file_id, 1000, 1000, 0o644);
    let parent_data = bincode::serialize(&parent_node).unwrap();
    let file_data = bincode::serialize(&file_node).unwrap();

    // Write committed transaction with graph operations.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(1, LogPayload::AddNode { node_data: parent_data, node_id: parent_id })?;
        log.log_data(1, LogPayload::AddNode { node_data: file_data, node_id: file_id })?;
        log.log_data(
            1,
            LogPayload::AddEdge {
                source: parent_id,
                target: file_id,
                edge_type: EdgeType::Child,
                label: "hello.txt".to_string(),
            },
        )?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: file_id,
                old_hash: None,
                new_hash: Some(EMPTY_HASH),
            },
        )?;
        log.log_end(1)?;
    }

    // Simulate crash: graph state is empty (wasn't flushed).
    let graph = GraphNamespace::open(dir.path())?;
    assert!(!graph.node_exists(&parent_id));
    assert!(!graph.node_exists(&file_id));

    // Run recovery — should redo the committed graph ops.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 1);
    assert_eq!(result.uncommitted_count, 0);

    // Verify graph state was restored.
    assert!(graph.node_exists(&parent_id));
    assert!(graph.node_exists(&file_id));
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "hello.txt"),
        Some(file_id)
    );

    Ok(())
}

#[test]
fn test_recovery_undo_uncommitted_graph_ops() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;

    let parent_id = uuid::Uuid::new_v4();
    let file_id = uuid::Uuid::new_v4();

    let parent_node = GraphNode::new_dir(parent_id, 1000, 1000, 0o755);
    let file_node = GraphNode::new_file(file_id, 1000, 1000, 0o644);
    let parent_data = bincode::serialize(&parent_node).unwrap();
    let file_data = bincode::serialize(&file_node).unwrap();

    // Write uncommitted transaction (no End record).
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(1, LogPayload::AddNode { node_data: parent_data, node_id: parent_id })?;
        log.log_data(1, LogPayload::AddNode { node_data: file_data, node_id: file_id })?;
        log.log_data(
            1,
            LogPayload::AddEdge {
                source: parent_id,
                target: file_id,
                edge_type: EdgeType::Child,
                label: "orphan.txt".to_string(),
            },
        )?;
        // No log_end — simulating crash before commit.
    }

    // Simulate crash: graph state was partially written before crash.
    let graph = GraphNamespace::open(dir.path())?;
    graph.add_node(parent_node);
    graph.add_node(file_node);
    graph.add_edge(parent_id, file_id, EdgeType::Child, "orphan.txt".to_string())?;

    // Verify the dirty state exists.
    assert!(graph.node_exists(&parent_id));
    assert!(graph.node_exists(&file_id));

    // Run recovery — should undo the uncommitted graph ops.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 0);
    assert_eq!(result.uncommitted_count, 1);

    // Verify graph state was cleaned up (undo removes nodes and edges).
    assert!(!graph.node_exists(&parent_id));
    assert!(!graph.node_exists(&file_id));
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "orphan.txt"),
        None
    );

    Ok(())
}

#[test]
fn test_recovery_redo_edge_removal() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;

    let parent_id = uuid::Uuid::new_v4();
    let file_id = uuid::Uuid::new_v4();

    let file_node = GraphNode::new_file(file_id, 1000, 1000, 0o644);
    let file_data = bincode::serialize(&file_node).unwrap();

    // Set up: pre-existing graph state with parent→file edge.
    let graph = GraphNamespace::open(dir.path())?;
    graph.add_node(GraphNode::new_dir(parent_id, 1000, 1000, 0o755));
    graph.add_node(file_node);
    graph.add_edge(parent_id, file_id, EdgeType::Child, "doomed.txt".to_string())?;
    graph.flush()?;

    // Write committed transaction that removes the edge and node.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::RemoveEdge {
                source: parent_id,
                target: file_id,
                edge_type: EdgeType::Child,
                label: "doomed.txt".to_string(),
            },
        )?;
        log.log_data(
            1,
            LogPayload::RemoveNode { node_data: file_data, node_id: file_id },
        )?;
        log.log_end(1)?;
    }

    // Simulate crash: graph still has the old state (removal wasn't flushed).
    // Reopen graph to get the persisted (pre-removal) state.
    let graph = GraphNamespace::open(dir.path())?;
    assert!(graph.node_exists(&file_id));
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "doomed.txt"),
        Some(file_id)
    );

    // Run recovery — should redo the removal.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 1);

    // Verify the edge and node are gone.
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "doomed.txt"),
        None
    );
    assert!(!graph.node_exists(&file_id));

    Ok(())
}

#[test]
fn test_recovery_undo_edge_removal() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;

    let parent_id = uuid::Uuid::new_v4();
    let file_id = uuid::Uuid::new_v4();

    let file_node = GraphNode::new_file(file_id, 1000, 1000, 0o644);
    let file_data = bincode::serialize(&file_node).unwrap();

    // Write uncommitted transaction that removes an edge and node.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::RemoveEdge {
                source: parent_id,
                target: file_id,
                edge_type: EdgeType::Child,
                label: "saved.txt".to_string(),
            },
        )?;
        log.log_data(
            1,
            LogPayload::RemoveNode { node_data: file_data, node_id: file_id },
        )?;
        // No log_end — crash before commit.
    }

    // Simulate crash: the removals were applied in-memory before crash.
    let graph = GraphNamespace::open(dir.path())?;
    graph.add_node(GraphNode::new_dir(parent_id, 1000, 1000, 0o755));
    // Node and edge already removed in the "crash" state — they're gone.

    // Run recovery — should undo the removal (re-add node and edge).
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.uncommitted_count, 1);

    // Verify the node was restored (undo of RemoveNode).
    assert!(graph.node_exists(&file_id));

    // Verify the edge was restored (undo of RemoveEdge).
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "saved.txt"),
        Some(file_id)
    );

    Ok(())
}

#[test]
fn test_recovery_mixed_graph_and_content() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;

    let parent_id = uuid::Uuid::new_v4();
    let committed_id = uuid::Uuid::new_v4();
    let uncommitted_id = uuid::Uuid::new_v4();

    let committed_node = GraphNode::new_file(committed_id, 1000, 1000, 0o644);
    let uncommitted_node = GraphNode::new_file(uncommitted_id, 1000, 1000, 0o644);
    let committed_data = bincode::serialize(&committed_node).unwrap();
    let uncommitted_data = bincode::serialize(&uncommitted_node).unwrap();

    let hash_c = content_hash(b"committed content");

    // Set up graph with just the parent.
    let graph = GraphNamespace::open(dir.path())?;
    graph.add_node(GraphNode::new_dir(parent_id, 1000, 1000, 0o755));

    // Write log: txn 1 committed (creates file + content), txn 2 uncommitted (creates file).
    {
        let mut log = LogManager::open(&log_path)?;
        // Txn 1: committed create.
        log.log_data(1, LogPayload::AddNode { node_data: committed_data, node_id: committed_id })?;
        log.log_data(
            1,
            LogPayload::AddEdge {
                source: parent_id,
                target: committed_id,
                edge_type: EdgeType::Child,
                label: "good.txt".to_string(),
            },
        )?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: committed_id,
                old_hash: None,
                new_hash: Some(hash_c),
            },
        )?;
        log.log_end(1)?;

        // Txn 2: uncommitted create.
        log.log_data(2, LogPayload::AddNode { node_data: uncommitted_data, node_id: uncommitted_id })?;
        log.log_data(
            2,
            LogPayload::AddEdge {
                source: parent_id,
                target: uncommitted_id,
                edge_type: EdgeType::Child,
                label: "bad.txt".to_string(),
            },
        )?;
        // No End for txn 2.
    }

    // Simulate crash: uncommitted node was partially in graph.
    graph.add_node(uncommitted_node);
    graph.add_edge(
        parent_id,
        uncommitted_id,
        EdgeType::Child,
        "bad.txt".to_string(),
    )?;

    // Run recovery.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 1);
    assert_eq!(result.uncommitted_count, 1);

    // Committed file should exist.
    assert!(graph.node_exists(&committed_id));
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "good.txt"),
        Some(committed_id)
    );
    assert_eq!(store.get_hash(&committed_id), Some(hash_c));

    // Uncommitted file should be gone.
    assert!(!graph.node_exists(&uncommitted_id));
    assert_eq!(
        graph.resolve(&parent_id, EdgeType::Child, "bad.txt"),
        None
    );

    Ok(())
}

// --- Graph Namespace Tests ---

#[test]
fn test_graph_add_and_resolve() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let graph = GraphNamespace::open(dir.path())?;

    let parent = uuid::Uuid::new_v4();
    let child = uuid::Uuid::new_v4();

    let parent_node = GraphNode::new_dir(parent, 1000, 1000, 0o755);
    let child_node = GraphNode::new_file(child, 1000, 1000, 0o644);

    graph.add_node(parent_node);
    graph.add_node(child_node);
    graph.add_edge(parent, child, EdgeType::Child, "hello.txt".to_string())?;

    // Resolve.
    let resolved = graph.resolve(&parent, EdgeType::Child, "hello.txt");
    assert_eq!(resolved, Some(child));

    // Non-existent.
    assert_eq!(graph.resolve(&parent, EdgeType::Child, "nope"), None);

    // Neighbors.
    let neighbors = graph.neighbors(&parent);
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].1, "hello.txt");
    assert_eq!(neighbors[0].2, child);

    Ok(())
}

#[test]
fn test_graph_remove_edge() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let graph = GraphNamespace::open(dir.path())?;

    let parent = uuid::Uuid::new_v4();
    let child = uuid::Uuid::new_v4();

    graph.add_node(GraphNode::new_dir(parent, 0, 0, 0o755));
    graph.add_node(GraphNode::new_file(child, 0, 0, 0o644));
    graph.add_edge(parent, child, EdgeType::Child, "file.txt".to_string())?;

    let removed = graph.remove_edge(&parent, EdgeType::Child, "file.txt");
    assert_eq!(removed, Some(child));
    assert_eq!(graph.resolve(&parent, EdgeType::Child, "file.txt"), None);

    // Reverse index should be updated.
    let reverse = graph.reverse_lookup(&child);
    assert!(reverse.is_empty());

    Ok(())
}

#[test]
fn test_graph_multiple_edges_same_target() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let graph = GraphNamespace::open(dir.path())?;

    let dir_a = uuid::Uuid::new_v4();
    let dir_b = uuid::Uuid::new_v4();
    let file = uuid::Uuid::new_v4();

    graph.add_node(GraphNode::new_dir(dir_a, 0, 0, 0o755));
    graph.add_node(GraphNode::new_dir(dir_b, 0, 0, 0o755));
    graph.add_node(GraphNode::new_file(file, 0, 0, 0o644));

    // Two edges to same file (hard link pattern).
    graph.add_edge(dir_a, file, EdgeType::Child, "link1.txt".to_string())?;
    graph.add_edge(dir_b, file, EdgeType::Child, "link2.txt".to_string())?;

    assert_eq!(graph.resolve(&dir_a, EdgeType::Child, "link1.txt"), Some(file));
    assert_eq!(graph.resolve(&dir_b, EdgeType::Child, "link2.txt"), Some(file));

    // Reverse lookup should show both.
    let reverse = graph.reverse_lookup(&file);
    assert_eq!(reverse.len(), 2);

    // Remove one link — file still referenced.
    graph.remove_edge(&dir_a, EdgeType::Child, "link1.txt");
    let reverse = graph.reverse_lookup(&file);
    assert_eq!(reverse.len(), 1);

    Ok(())
}

// --- Content Store Tests ---

#[test]
fn test_content_store_dedup() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = ContentStore::open(dir.path())?;

    let obj_a = uuid::Uuid::new_v4();
    let obj_b = uuid::Uuid::new_v4();

    // Two different objects with identical content.
    let hash_a = store.put_object(obj_a, b"same content")?;
    let hash_b = store.put_object(obj_b, b"same content")?;

    // Content hashes should be identical (dedup).
    assert_eq!(hash_a, hash_b);

    // Both objects should resolve to the same content.
    assert_eq!(store.get_content(&obj_a)?, b"same content");
    assert_eq!(store.get_content(&obj_b)?, b"same content");

    Ok(())
}

#[test]
fn test_content_store_persistence() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let obj_id = uuid::Uuid::new_v4();
    let expected_hash = content_hash(b"persistent data");

    // Write and flush.
    {
        let store = ContentStore::open(dir.path())?;
        store.put_object(obj_id, b"persistent data")?;
        store.flush_index()?;
    }

    // Reopen and verify.
    {
        let store = ContentStore::open(dir.path())?;
        assert_eq!(store.get_hash(&obj_id), Some(expected_hash));
        assert_eq!(store.get_content(&obj_id)?, b"persistent data");
    }

    Ok(())
}

// --- Graph Persistence Tests ---

#[test]
fn test_graph_persistence() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let parent = uuid::Uuid::new_v4();
    let child = uuid::Uuid::new_v4();

    // Create and flush.
    {
        let graph = GraphNamespace::open(dir.path())?;
        graph.add_node(GraphNode::new_dir(parent, 0, 0, 0o755));
        graph.add_node(GraphNode::new_file(child, 0, 0, 0o644));
        graph.add_edge(parent, child, EdgeType::Child, "test.txt".to_string())?;
        graph.flush()?;
    }

    // Reopen and verify.
    {
        let graph = GraphNamespace::open(dir.path())?;
        assert!(graph.node_exists(&parent));
        assert!(graph.node_exists(&child));
        assert_eq!(graph.resolve(&parent, EdgeType::Child, "test.txt"), Some(child));
    }

    Ok(())
}

// --- EC Merge Tests ---

#[test]
fn test_ec_merge_two_transactions() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;

    // Merge them.
    txn_mgr.ec_merge(txn1, txn2);

    // They should share an EC head.
    let head1 = txn_mgr.find_ec_head(txn1);
    let head2 = txn_mgr.find_ec_head(txn2);
    assert_eq!(head1, head2);

    // The EC head should have size 2.
    let head = txn_mgr.active.get(&head1).unwrap();
    assert_eq!(head.ec_size, 2);

    Ok(())
}

#[test]
fn test_ec_merge_three_transactions_chain() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;
    let txn3 = txn_mgr.start_txn()?;

    // Chain merge: txn1+txn2, then txn2+txn3.
    txn_mgr.ec_merge(txn1, txn2);
    txn_mgr.ec_merge(txn2, txn3);

    // All should share the same EC head.
    let head1 = txn_mgr.find_ec_head(txn1);
    let head2 = txn_mgr.find_ec_head(txn2);
    let head3 = txn_mgr.find_ec_head(txn3);
    assert_eq!(head1, head2);
    assert_eq!(head2, head3);

    // EC size should be 3.
    let head = txn_mgr.active.get(&head1).unwrap();
    assert_eq!(head.ec_size, 3);

    Ok(())
}

#[test]
fn test_ec_singleton_commits_normally() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let obj_id = uuid::Uuid::new_v4();
    let hash = content_hash(b"solo");

    let txn_id = txn_mgr.start_txn()?;
    txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash))?;

    // end_txn on a singleton should commit immediately.
    let pending = txn_mgr.end_txn(txn_id)?;
    // Pending writes are returned (empty in this case since we didn't buffer).
    assert!(pending.is_empty());
    assert!(!txn_mgr.has_active_transactions());

    // Verify log has Data + End records.
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].record_type, LogRecordType::Data);
    assert_eq!(records[1].record_type, LogRecordType::End);

    Ok(())
}

#[test]
fn test_ec_size_tracking_after_merges() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;
    let txn3 = txn_mgr.start_txn()?;
    let txn4 = txn_mgr.start_txn()?;

    // Two separate ECs: {txn1, txn2} and {txn3, txn4}.
    txn_mgr.ec_merge(txn1, txn2);
    txn_mgr.ec_merge(txn3, txn4);

    let head_a = txn_mgr.find_ec_head(txn1);
    let head_b = txn_mgr.find_ec_head(txn3);
    assert_eq!(txn_mgr.active.get(&head_a).unwrap().ec_size, 2);
    assert_eq!(txn_mgr.active.get(&head_b).unwrap().ec_size, 2);

    // Now merge the two ECs.
    txn_mgr.ec_merge(txn1, txn3);
    let final_head = txn_mgr.find_ec_head(txn1);
    assert_eq!(txn_mgr.active.get(&final_head).unwrap().ec_size, 4);

    // All four should resolve to the same head.
    assert_eq!(txn_mgr.find_ec_head(txn2), final_head);
    assert_eq!(txn_mgr.find_ec_head(txn3), final_head);
    assert_eq!(txn_mgr.find_ec_head(txn4), final_head);

    Ok(())
}

#[test]
fn test_find_ec_head_walks_chain() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;
    let txn3 = txn_mgr.start_txn()?;

    // Initially each is its own head.
    assert_eq!(txn_mgr.find_ec_head(txn1), txn1);
    assert_eq!(txn_mgr.find_ec_head(txn2), txn2);
    assert_eq!(txn_mgr.find_ec_head(txn3), txn3);

    // Merge txn1 into txn2 (txn2 is larger or equal, becomes head).
    txn_mgr.ec_merge(txn1, txn2);
    let head_after_first = txn_mgr.find_ec_head(txn1);
    assert_eq!(txn_mgr.find_ec_head(txn2), head_after_first);

    // Now merge txn3 with txn1 — should follow chain txn1 -> head_after_first.
    txn_mgr.ec_merge(txn3, txn1);
    let final_head = txn_mgr.find_ec_head(txn3);
    assert_eq!(txn_mgr.find_ec_head(txn1), final_head);
    assert_eq!(txn_mgr.find_ec_head(txn2), final_head);

    Ok(())
}

#[test]
fn test_end_txn_ec_non_last_returns_empty_last_returns_all() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let obj1 = uuid::Uuid::new_v4();
    let obj2 = uuid::Uuid::new_v4();

    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;

    // Log some operations.
    txn_mgr.log_content_change(txn1, obj1, None, Some(EMPTY_HASH))?;
    txn_mgr.log_content_change(txn2, obj2, None, Some(EMPTY_HASH))?;

    // Add pending writes to each transaction.
    txn_mgr.active.get_mut(&txn1).unwrap()
        .pending_writes.push((obj1, b"data1".to_vec()));
    txn_mgr.active.get_mut(&txn2).unwrap()
        .pending_writes.push((obj2, b"data2".to_vec()));

    // Merge them.
    txn_mgr.ec_merge(txn1, txn2);

    // First to end: non-last member returns empty pending_writes.
    let pending1 = txn_mgr.end_txn(txn1)?;
    assert!(pending1.is_empty());
    // Transaction should be marked Pending (still in active set).
    assert_eq!(
        txn_mgr.active.get(&txn1).map(|t| t.status),
        Some(TransactionStatus::Pending)
    );

    // Last to end: returns ALL pending_writes from the entire EC.
    let pending2 = txn_mgr.end_txn(txn2)?;
    assert_eq!(pending2.len(), 2);
    assert!(!txn_mgr.has_active_transactions());

    // Verify log has BEND for the non-head member and END for the head.
    let records = LogManager::read_all_records(&log_path)?;
    let bend_records: Vec<_> = records.iter()
        .filter(|r| r.record_type == LogRecordType::BatchEnd)
        .collect();
    let end_records: Vec<_> = records.iter()
        .filter(|r| r.record_type == LogRecordType::End)
        .collect();
    assert_eq!(bend_records.len(), 1);
    assert_eq!(end_records.len(), 1);

    Ok(())
}

#[test]
fn test_recovery_bend_committed_ec() -> io::Result<()> {
    // An EC with BEND + END should be committed (all members redone).
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;
    let graph = GraphNamespace::open(dir.path())?;

    let obj1 = uuid::Uuid::new_v4();
    let obj2 = uuid::Uuid::new_v4();
    let hash1 = content_hash(b"ec-member1");
    let hash2 = content_hash(b"ec-member2");

    store.register_empty(obj1);
    store.register_empty(obj2);

    // Write log: txn 1 (member, BEND) and txn 2 (head, END).
    {
        let mut log = LogManager::open(&log_path)?;
        // Txn 1 data.
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj1,
                old_hash: None,
                new_hash: Some(hash1),
            },
        )?;
        // Txn 2 data.
        log.log_data(
            2,
            LogPayload::ContentChange {
                object_id: obj2,
                old_hash: None,
                new_hash: Some(hash2),
            },
        )?;
        // BEND for txn 1, ec_head = 2.
        log.log_batch_end(1, 2)?;
        // END for txn 2 (the head).
        log.log_end(2)?;
    }

    // Simulate crash: neither hash is in the index.
    store.set_hash(obj1, EMPTY_HASH);
    store.set_hash(obj2, EMPTY_HASH);

    // Run recovery -- both should be redone.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 2);
    assert_eq!(result.uncommitted_count, 0);

    assert_eq!(store.get_hash(&obj1), Some(hash1));
    assert_eq!(store.get_hash(&obj2), Some(hash2));

    Ok(())
}

#[test]
fn test_recovery_bend_uncommitted_ec() -> io::Result<()> {
    // An EC with BEND but no END for its head should be uncommitted (all members undone).
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;
    let graph = GraphNamespace::open(dir.path())?;

    let obj1 = uuid::Uuid::new_v4();
    let obj2 = uuid::Uuid::new_v4();
    let old_hash1 = content_hash(b"original1");
    let old_hash2 = content_hash(b"original2");
    let new_hash1 = content_hash(b"modified1");
    let new_hash2 = content_hash(b"modified2");

    store.put_object(obj1, b"original1")?;
    store.put_object(obj2, b"original2")?;

    // Write log: txn 1 (member, BEND) and txn 2 (head, NO END -- crash).
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj1,
                old_hash: Some(old_hash1),
                new_hash: Some(new_hash1),
            },
        )?;
        log.log_data(
            2,
            LogPayload::ContentChange {
                object_id: obj2,
                old_hash: Some(old_hash2),
                new_hash: Some(new_hash2),
            },
        )?;
        // BEND for txn 1, ec_head = 2.
        log.log_batch_end(1, 2)?;
        // No END for txn 2 -- simulating crash before EC commit.
    }

    // Simulate crash: both were partially updated.
    store.set_hash(obj1, new_hash1);
    store.set_hash(obj2, new_hash2);

    // Run recovery -- both should be undone.
    let result = recovery::recover(&log_path, &store, &graph)?;
    assert_eq!(result.committed_count, 0);
    assert_eq!(result.uncommitted_count, 2);

    assert_eq!(store.get_hash(&obj1), Some(old_hash1));
    assert_eq!(store.get_hash(&obj2), Some(old_hash2));

    Ok(())
}

#[test]
fn test_tlock_touch_and_release() {
    let tlocks = TlockTable::new();
    let node_id = uuid::Uuid::new_v4();

    // First touch: no conflict.
    assert_eq!(tlocks.touch(node_id, 1), None);

    // Same transaction touches again: no conflict.
    assert_eq!(tlocks.touch(node_id, 1), None);

    // Different transaction: conflict.
    assert_eq!(tlocks.touch(node_id, 2), Some(1));

    // Release txn 2's locks.
    tlocks.release_all(2);

    // Now txn 3 touches: no conflict (previous holder was released).
    assert_eq!(tlocks.touch(node_id, 3), None);
}

#[test]
fn test_ec_merge_idempotent() -> io::Result<()> {
    // Merging the same pair twice should be harmless.
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;

    txn_mgr.ec_merge(txn1, txn2);
    let head_after_first = txn_mgr.find_ec_head(txn1);
    let size_after_first = txn_mgr.active.get(&head_after_first).unwrap().ec_size;

    // Merge again -- should be a no-op (same EC head).
    txn_mgr.ec_merge(txn1, txn2);
    let head_after_second = txn_mgr.find_ec_head(txn1);
    let size_after_second = txn_mgr.active.get(&head_after_second).unwrap().ec_size;

    assert_eq!(head_after_first, head_after_second);
    assert_eq!(size_after_first, size_after_second);
    assert_eq!(size_after_second, 2);

    Ok(())
}

// --- Deferred Sync / IO Window Tests ---
//
// These tests verify that:
// 1. Data/BatchEnd records do NOT trigger fsync (io_window stays put)
// 2. End/Checkpoint/Restart records DO trigger fsync (io_window advances)
// 3. sync_pending() flushes everything and advances io_window
// 4. EC commit batches all BENDs + END into a single fsync
// 5. Deferred sync measurably reduces sync overhead

#[test]
fn test_io_window_advances_on_end_record() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"alpha");

    let mut log = LogManager::open(&log_path)?;

    // io_window starts at 0 for a fresh log.
    assert_eq!(log.io_window(), 0);

    // Data record: io_window should NOT advance (deferred sync).
    let data_lsn = log.log_data(
        1,
        LogPayload::ContentChange {
            object_id: obj_id,
            old_hash: None,
            new_hash: Some(hash_a),
        },
    )?;
    assert_eq!(data_lsn, 1);
    assert_eq!(log.io_window(), 0, "io_window must not advance on Data record");

    // End record: io_window MUST advance (commit-point sync).
    let end_lsn = log.log_end(1)?;
    assert_eq!(end_lsn, 2);
    assert_eq!(
        log.io_window(),
        end_lsn,
        "io_window must advance to END record LSN"
    );

    Ok(())
}

#[test]
fn test_io_window_advances_on_checkpoint() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut log = LogManager::open(&log_path)?;
    assert_eq!(log.io_window(), 0);

    let ckpt_lsn = log.log_checkpoint()?;
    assert_eq!(
        log.io_window(),
        ckpt_lsn,
        "io_window must advance on Checkpoint"
    );

    Ok(())
}

#[test]
fn test_io_window_advances_on_restart() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut log = LogManager::open(&log_path)?;
    let rst_lsn = log.log_restart()?;
    assert_eq!(
        log.io_window(),
        rst_lsn,
        "io_window must advance on Restart"
    );

    Ok(())
}

#[test]
fn test_batch_end_does_not_advance_io_window() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut log = LogManager::open(&log_path)?;
    assert_eq!(log.io_window(), 0);

    // BatchEnd should NOT advance io_window (it's committed by the head's END).
    let _bend_lsn = log.log_batch_end(1, 2)?;
    assert_eq!(
        log.io_window(),
        0,
        "io_window must not advance on BatchEnd"
    );

    Ok(())
}

#[test]
fn test_sync_pending_advances_io_window() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"beta");

    let mut log = LogManager::open(&log_path)?;

    // Write a Data record (no sync, io_window stays at 0).
    let data_lsn = log.log_data(
        1,
        LogPayload::ContentChange {
            object_id: obj_id,
            old_hash: None,
            new_hash: Some(hash_a),
        },
    )?;
    assert_eq!(log.io_window(), 0);

    // Explicit sync_pending() should advance io_window.
    log.sync_pending()?;
    assert_eq!(
        log.io_window(),
        data_lsn,
        "sync_pending must advance io_window to cover all written records"
    );

    Ok(())
}

#[test]
fn test_deferred_sync_reduces_sync_count() -> io::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;

    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"gamma");
    let hash_b = content_hash(b"delta");

    let mut log = LogManager::open(&log_path)?;

    // Write 3 Data records + 1 End = 4 records total.
    // With deferred sync, only the End triggers fsync.
    log.log_data(
        1,
        LogPayload::ContentChange {
            object_id: obj_id,
            old_hash: None,
            new_hash: Some(hash_a),
        },
    )?;
    log.log_data(
        1,
        LogPayload::AddNode {
            node_id: obj_id,
            node_data: vec![1, 2, 3],
        },
    )?;
    log.log_data(
        1,
        LogPayload::ContentChange {
            object_id: obj_id,
            old_hash: Some(hash_a),
            new_hash: Some(hash_b),
        },
    )?;

    // Check that sync time is 0 after Data records (no fsync happened).
    let sync_before_end = log.timings.sync_ns.load(Relaxed);
    assert_eq!(
        sync_before_end, 0,
        "sync_ns must be 0 after Data-only records"
    );

    // Now write End record — this triggers the one and only fsync.
    log.log_end(1)?;

    let sync_after_end = log.timings.sync_ns.load(Relaxed);
    assert!(
        sync_after_end > 0,
        "sync_ns must be > 0 after End record (fsync happened)"
    );

    // Verify all records are readable (they were all flushed by the End sync).
    drop(log);
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 4, "all 4 records must be durable after End sync");
    assert_eq!(records[0].record_type, LogRecordType::Data);
    assert_eq!(records[1].record_type, LogRecordType::Data);
    assert_eq!(records[2].record_type, LogRecordType::Data);
    assert_eq!(records[3].record_type, LogRecordType::End);

    Ok(())
}

#[test]
fn test_io_window_restored_on_reopen() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"epsilon");

    // Write and commit a transaction.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(
            1,
            LogPayload::ContentChange {
                object_id: obj_id,
                old_hash: None,
                new_hash: Some(hash_a),
            },
        )?;
        log.log_end(1)?;
    }

    // Reopen — io_window should reflect on-disk records.
    let log = LogManager::open(&log_path)?;
    assert_eq!(
        log.io_window(),
        2,
        "io_window must be initialized from on-disk records on reopen"
    );

    Ok(())
}

#[test]
fn test_ec_commit_single_sync() -> io::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;

    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_a = uuid::Uuid::new_v4();
    let obj_b = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"zeta");
    let hash_b = content_hash(b"eta");

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    // Start two transactions, merge them into one EC.
    let txn1 = txn_mgr.start_txn()?;
    let txn2 = txn_mgr.start_txn()?;

    txn_mgr.log_content_change(txn1, obj_a, None, Some(hash_a))?;
    txn_mgr.log_content_change(txn2, obj_b, None, Some(hash_b))?;
    txn_mgr.ec_merge(txn1, txn2);

    // Verify no sync has happened yet (only Data records written).
    let sync_before = txn_mgr.log.timings.sync_ns.load(Relaxed);
    assert_eq!(sync_before, 0, "no sync before EC commit");

    // End first member — not the last, so no commit yet.
    let pending1 = txn_mgr.end_txn(txn1)?;
    assert!(pending1.is_empty());
    let sync_after_first = txn_mgr.log.timings.sync_ns.load(Relaxed);
    assert_eq!(
        sync_after_first, 0,
        "BEND for non-last member must not trigger sync"
    );

    // End second member — last member, triggers BEND + END + sync.
    let pending2 = txn_mgr.end_txn(txn2)?;
    assert!(pending2.is_empty()); // no NO-STEAL writes in this test

    let sync_after_commit = txn_mgr.log.timings.sync_ns.load(Relaxed);
    assert!(
        sync_after_commit > 0,
        "EC commit (last member END) must trigger sync"
    );

    // All records should be durable: 2 Data + 1 BEND + 1 END = 4 records.
    drop(txn_mgr);
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 4);
    assert_eq!(records[2].record_type, LogRecordType::BatchEnd);
    assert_eq!(records[3].record_type, LogRecordType::End);

    Ok(())
}

// --- Checkpoint / Log Truncation Tests ---

#[test]
fn test_checkpoint_truncates_log() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"pre-checkpoint");

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    // Write and commit a transaction.
    let txn_id = txn_mgr.start_txn()?;
    txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash_a))?;
    txn_mgr.commit_txn(txn_id)?;

    // Log should have 2 records (Data + End).
    let records_before = LogManager::read_all_records(&log_path)?;
    assert_eq!(records_before.len(), 2);

    // Checkpoint: truncate log and reset admission.
    txn_mgr.checkpoint()?;

    // Log should now have only the restart sentinel.
    let records_after = LogManager::read_all_records(&log_path)?;
    assert_eq!(records_after.len(), 1);
    assert_eq!(records_after[0].record_type, LogRecordType::Restart);

    Ok(())
}

#[test]
fn test_checkpoint_resets_admission() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    // Fill up admission control with many transactions.
    for _ in 0..100 {
        let txn_id = txn_mgr.start_txn()?;
        let obj_id = uuid::Uuid::new_v4();
        let hash = content_hash(b"data");
        txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash))?;
        txn_mgr.commit_txn(txn_id)?;
    }

    // Checkpoint: reset admission control.
    txn_mgr.checkpoint()?;

    // Should be able to start new transactions after checkpoint.
    let txn_id = txn_mgr.start_txn()?;
    txn_mgr.commit_txn(txn_id)?;

    Ok(())
}

#[test]
fn test_checkpoint_with_active_transactions_fails() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    // Start a transaction but don't commit.
    let _txn_id = txn_mgr.start_txn()?;

    // Checkpoint should fail with active transactions.
    let result = txn_mgr.checkpoint();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("active transactions"));

    Ok(())
}

#[test]
fn test_recovery_after_checkpoint() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let store_dir = dir.path().join("store");
    let graph_dir = dir.path().join("graph");
    std::fs::create_dir_all(&store_dir)?;
    std::fs::create_dir_all(&graph_dir)?;
    let log_path = dir.path().join("test.log");

    let obj_id = uuid::Uuid::new_v4();
    let hash_a = content_hash(b"pre-checkpoint data");
    let hash_b = content_hash(b"post-checkpoint data");

    let store = ContentStore::open(&store_dir)?;
    let graph = GraphNamespace::open(&graph_dir)?;

    // Pre-checkpoint: create a node and commit.
    {
        let mut txn_mgr = TransactionManager::open(&log_path)?;
        let txn_id = txn_mgr.start_txn()?;

        let node = GraphNode::new_file(obj_id, 0, 0, 0o644);
        let node_data = bincode::serialize(&node).unwrap();
        txn_mgr.log_add_node(txn_id, obj_id, node_data)?;
        graph.add_node(node);

        store.put_object(obj_id, b"pre-checkpoint data")?;
        txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash_a))?;

        txn_mgr.commit_txn(txn_id)?;
    }

    // Flush state and checkpoint (simulates what unmount does).
    store.flush_index()?;
    graph.flush()?;
    {
        let mut txn_mgr = TransactionManager::open(&log_path)?;
        txn_mgr.checkpoint()?;
    }

    // Post-checkpoint: update content and commit.
    {
        let mut txn_mgr = TransactionManager::open(&log_path)?;
        let txn_id = txn_mgr.start_txn()?;

        store.put_object(obj_id, b"post-checkpoint data")?;
        txn_mgr.log_content_change(txn_id, obj_id, Some(hash_a), Some(hash_b))?;

        txn_mgr.commit_txn(txn_id)?;
    }

    // Simulate crash: reload graph and store from disk (checkpoint state).
    let store2 = ContentStore::open(&store_dir)?;
    let graph2 = GraphNamespace::open(&graph_dir)?;

    // Recovery should redo the post-checkpoint transaction.
    let result = recovery::recover(&log_path, &store2, &graph2)?;
    assert_eq!(result.committed_count, 1, "one post-checkpoint txn committed");
    assert_eq!(result.uncommitted_count, 0);

    // Node should exist (from checkpoint state).
    assert!(graph2.node_exists(&obj_id));

    // Content should reflect the post-checkpoint update (redo applied it).
    let hash = store2.get_hash(&obj_id);
    assert_eq!(hash, Some(hash_b), "redo should apply post-checkpoint content change");

    Ok(())
}

#[test]
fn test_post_checkpoint_transactions_work() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    // Pre-checkpoint work.
    let txn1 = txn_mgr.start_txn()?;
    let obj1 = uuid::Uuid::new_v4();
    txn_mgr.log_content_change(txn1, obj1, None, Some(content_hash(b"alpha")))?;
    txn_mgr.commit_txn(txn1)?;

    // Checkpoint.
    txn_mgr.checkpoint()?;

    // Post-checkpoint: new transactions should work normally.
    let txn2 = txn_mgr.start_txn()?;
    let obj2 = uuid::Uuid::new_v4();
    txn_mgr.log_content_change(txn2, obj2, None, Some(content_hash(b"beta")))?;
    txn_mgr.commit_txn(txn2)?;

    // Log should have: Restart + Data + End = 3 records.
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].record_type, LogRecordType::Restart);
    assert_eq!(records[1].record_type, LogRecordType::Data);
    assert_eq!(records[2].record_type, LogRecordType::End);

    Ok(())
}

// --- Three-Tier Reservation Tests ---

#[test]
fn test_progressive_reservation_release() -> io::Result<()> {
    use assfs::txn::reservation::MAX_TRANSACTION_SIZE;

    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    // Start a transaction — reserves MAX_TRANSACTION_SIZE.
    let txn_id = txn_mgr.start_txn()?;
    let initial_reserved = txn_mgr.admission.total_reserved;
    assert_eq!(initial_reserved, MAX_TRANSACTION_SIZE as u64);

    // Write a small data record (uses ~128 bytes of the 70KB reservation).
    let obj_id = uuid::Uuid::new_v4();
    let hash = content_hash(b"small data");
    txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash))?;

    // Before commit, reservation is still full.
    assert_eq!(txn_mgr.admission.total_reserved, MAX_TRANSACTION_SIZE as u64);

    // Commit triggers tightening: unused portion released before END.
    txn_mgr.commit_txn(txn_id)?;

    // After commit, all reservation is released.
    assert_eq!(txn_mgr.admission.total_reserved, 0);

    Ok(())
}

#[test]
fn test_tightening_increases_concurrent_capacity() -> io::Result<()> {
    use assfs::txn::reservation::MAX_TRANSACTION_SIZE;

    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");

    let mut txn_mgr = TransactionManager::open(&log_path)?;
    // Shrink max log size to make the test meaningful.
    txn_mgr.admission.max_log_size = (MAX_TRANSACTION_SIZE * 3) as u64;

    // Start txn 1, write a small record, then commit.
    let txn1 = txn_mgr.start_txn()?;
    let obj1 = uuid::Uuid::new_v4();
    txn_mgr.log_content_change(txn1, obj1, None, Some(content_hash(b"a")))?;

    // Start txn 2 while txn 1 is still active.
    let txn2 = txn_mgr.start_txn()?;
    let obj2 = uuid::Uuid::new_v4();
    txn_mgr.log_content_change(txn2, obj2, None, Some(content_hash(b"b")))?;

    // Both active: 2 * MAX_TRANSACTION_SIZE reserved.
    // With max_log_size = 3 * MAX_TRANSACTION_SIZE, we can still start one more.
    let txn3 = txn_mgr.start_txn()?;
    let obj3 = uuid::Uuid::new_v4();
    txn_mgr.log_content_change(txn3, obj3, None, Some(content_hash(b"c")))?;

    // 3 * MAX_TRANSACTION_SIZE = max. No more room.
    assert!(txn_mgr.start_txn().is_err());

    // Commit txn1 — tightening releases most of its 70KB reservation.
    txn_mgr.commit_txn(txn1)?;

    // After txn1 commits: its full reservation is released.
    // Total reserved = 2 * MAX_TRANSACTION_SIZE.
    // Now we can start another.
    let txn4 = txn_mgr.start_txn()?;

    // Clean up.
    txn_mgr.commit_txn(txn2)?;
    txn_mgr.commit_txn(txn3)?;
    txn_mgr.commit_txn(txn4)?;

    Ok(())
}

#[test]
fn test_tighten_is_idempotent() {
    use assfs::txn::reservation::Reservation;

    let mut r = Reservation::new(70 * 1024);
    r.use_bytes(500);

    let first = r.tighten();
    assert!(first > 0, "first tighten should release bytes");

    let second = r.tighten();
    assert_eq!(second, 0, "second tighten should be a no-op");
}

/// Timing benchmark: measure how many fsyncs happen for N transactions.
/// With deferred sync, N transactions of M records each should produce
/// exactly N fsyncs (one per END), not N*M fsyncs.
#[test]
fn test_deferred_sync_timing_benchmark() -> io::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Instant;

    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("bench.log");

    let num_txns = 10;
    let records_per_txn = 5; // 5 Data + 1 End = 6 records per txn

    let mut txn_mgr = TransactionManager::open(&log_path)?;

    let start = Instant::now();

    for t in 0..num_txns {
        let txn_id = txn_mgr.start_txn()?;
        for r in 0..records_per_txn {
            let obj_id = uuid::Uuid::new_v4();
            let data = format!("txn-{}-rec-{}", t, r);
            let hash = content_hash(data.as_bytes());
            txn_mgr.log_content_change(txn_id, obj_id, None, Some(hash))?;
        }
        txn_mgr.commit_txn(txn_id)?;
    }

    let elapsed = start.elapsed();
    let total_records = num_txns * (records_per_txn + 1); // +1 for End
    let sync_ns = txn_mgr.log.timings.sync_ns.load(Relaxed);
    let record_count = txn_mgr.log.timings.record_count.load(Relaxed);

    // Verify record count.
    assert_eq!(record_count, total_records as u64);

    // Verify all records are on disk.
    drop(txn_mgr);
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), total_records);

    // Print benchmark results (visible with cargo test -- --nocapture).
    eprintln!(
        "\n=== Deferred Sync Benchmark ===\n\
         Transactions: {}\n\
         Records/txn:  {} data + 1 end = {}\n\
         Total records: {}\n\
         Total time:   {:?}\n\
         Sync time:    {:.3} ms (fsyncs happen only on End records)\n\
         Per-txn:      {:.3} ms\n\
         Per-record:   {:.3} us\n\
         ===============================\n",
        num_txns,
        records_per_txn,
        records_per_txn + 1,
        total_records,
        elapsed,
        sync_ns as f64 / 1_000_000.0,
        elapsed.as_micros() as f64 / num_txns as f64 / 1000.0,
        elapsed.as_micros() as f64 / total_records as f64,
    );

    Ok(())
}

// --- Proactive Checkpoint Tests ---

#[test]
fn test_proactive_checkpoint_on_admission_exhaustion() -> io::Result<()> {
    use assfs::fuse_ops::AssFs;
    use assfs::txn::reservation::MAX_TRANSACTION_SIZE;
    use std::sync::atomic::Ordering::Relaxed;

    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");

    let fs = AssFs::open(&data_dir)?;

    // Shrink max_log_size so admission control exhausts quickly.
    // Allow exactly 2 transactions worth of space: after the first two
    // transactions commit (releasing their reservations), the cumulative
    // log bytes are still tracked by admission until checkpoint.
    // With 3 * MAX_TRANSACTION_SIZE, we can fit 3 concurrent reservations.
    // We'll fill it by starting+committing transactions (which release
    // reservation but the log grows), then check that the 4th triggers
    // a checkpoint.
    //
    // Actually, AdmissionController tracks total_reserved (not total log
    // bytes). When a transaction commits, its reservation is released.
    // So we need to make the max small enough that we can't even reserve
    // one more transaction.
    //
    // The trick: commit_txn releases reservation via admission.release(),
    // which decrements total_reserved. So total_reserved returns to 0
    // after each commit. The log file grows but admission doesn't track
    // that. We need a different approach.
    //
    // Let's set max_log_size to something tiny — less than
    // MAX_TRANSACTION_SIZE — so start_txn always fails. Then
    // start_txn_or_checkpoint should checkpoint and retry.
    //
    // But wait: after checkpoint, admission.reset() sets total_reserved=0,
    // and max_log_size is still tiny. The retry will fail again.
    //
    // Real approach: set max_log_size to just barely more than
    // MAX_TRANSACTION_SIZE (so one txn fits). Fill it up with a committed
    // transaction, then try to start another. The committed txn released
    // its reservation, so total_reserved is 0, and start_txn should
    // succeed... unless admission tracks cumulative log bytes differently.
    //
    // Reading the code: try_reserve checks total_reserved + bytes > max_log_size.
    // release() decrements total_reserved. So after commit, total_reserved=0
    // and the next start_txn should succeed regardless. This means the
    // "checkpoint needed" error only fires when there are enough CONCURRENT
    // active transactions to exhaust the space.
    //
    // For the proactive checkpoint test, we need concurrent active
    // transactions. But FUSE is single-threaded in the MVP. So the test
    // scenario is: we manually leave reservations in place (e.g., by not
    // committing) to exhaust space, then the next FUSE op should checkpoint.
    //
    // Simplest approach: exhaust admission by adjusting max_log_size, then
    // call start_txn_or_checkpoint and verify it succeeds.

    {
        let mut txn_mgr = fs.txn_mgr.lock().unwrap();

        // Set max_log_size so exactly one transaction can be reserved.
        txn_mgr.admission.max_log_size = MAX_TRANSACTION_SIZE as u64;

        // Start and commit a transaction so the graph/store have some state.
        let txn1 = txn_mgr.start_txn()?;
        let obj1 = uuid::Uuid::new_v4();
        txn_mgr.log_content_change(txn1, obj1, None, Some(content_hash(b"data1")))?;
        txn_mgr.commit_txn(txn1)?;
        // total_reserved is now 0, but the log contains records.

        // Artificially inflate total_reserved to simulate exhaustion.
        // This mimics the state where the log has grown large enough
        // that admission control would reject new transactions.
        txn_mgr.admission.total_reserved = MAX_TRANSACTION_SIZE as u64;

        // Verify direct start_txn fails.
        assert!(
            txn_mgr.start_txn().is_err(),
            "start_txn should fail when admission is exhausted"
        );
    }

    // Now call start_txn_or_checkpoint — it should:
    // 1. Try start_txn -> fails
    // 2. Flush store + graph
    // 3. Checkpoint (resets admission to 0)
    // 4. Retry start_txn -> succeeds
    {
        let mut txn_mgr = fs.txn_mgr.lock().unwrap();
        let txn_id = fs.start_txn_or_checkpoint(&mut txn_mgr)?;

        // Verify we got a valid transaction.
        assert!(txn_mgr.active.contains_key(&txn_id));

        // Clean up.
        txn_mgr.commit_txn(txn_id)?;
    }

    // Verify the checkpoint counter was incremented.
    assert_eq!(
        fs.stats.checkpoint_count.load(Relaxed),
        1,
        "exactly one proactive checkpoint should have been triggered"
    );

    Ok(())
}

#[test]
fn test_proactive_checkpoint_counter_accumulates() -> io::Result<()> {
    use assfs::fuse_ops::AssFs;
    use assfs::txn::reservation::MAX_TRANSACTION_SIZE;
    use std::sync::atomic::Ordering::Relaxed;

    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");

    let fs = AssFs::open(&data_dir)?;

    // Trigger proactive checkpoint twice.
    for round in 0..2 {
        {
            let mut txn_mgr = fs.txn_mgr.lock().unwrap();
            txn_mgr.admission.max_log_size = MAX_TRANSACTION_SIZE as u64;
            // Inflate reservation to simulate exhaustion.
            txn_mgr.admission.total_reserved = MAX_TRANSACTION_SIZE as u64;
        }

        {
            let mut txn_mgr = fs.txn_mgr.lock().unwrap();
            let txn_id = fs.start_txn_or_checkpoint(&mut txn_mgr)?;
            // Do some work so the log has content for next checkpoint.
            let obj_id = uuid::Uuid::new_v4();
            txn_mgr.log_content_change(
                txn_id,
                obj_id,
                None,
                Some(content_hash(format!("round-{}", round).as_bytes())),
            )?;
            txn_mgr.commit_txn(txn_id)?;
        }
    }

    assert_eq!(
        fs.stats.checkpoint_count.load(Relaxed),
        2,
        "two proactive checkpoints should have been triggered"
    );

    Ok(())
}

#[test]
fn test_proactive_checkpoint_no_false_trigger() -> io::Result<()> {
    use assfs::fuse_ops::AssFs;
    use std::sync::atomic::Ordering::Relaxed;

    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");

    let fs = AssFs::open(&data_dir)?;

    // Normal operation: admission has plenty of space.
    // start_txn_or_checkpoint should succeed without checkpoint.
    {
        let mut txn_mgr = fs.txn_mgr.lock().unwrap();
        let txn_id = fs.start_txn_or_checkpoint(&mut txn_mgr)?;
        txn_mgr.commit_txn(txn_id)?;
    }

    assert_eq!(
        fs.stats.checkpoint_count.load(Relaxed),
        0,
        "no checkpoint should be triggered when admission has space"
    );

    Ok(())
}
