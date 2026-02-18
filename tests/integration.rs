//! Integration tests for AssFS core layers.

use std::io;

use assfs::txn::log::{LogManager, LogRecordType};
use assfs::txn::recovery;
use assfs::txn::transaction::TransactionManager;
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
        log.log_data(1, obj_id, None, Some(hash_a))?;
        log.log_data(1, obj_id, Some(hash_a), Some(hash_b))?;
        log.log_end(1)?;
    }

    // Read them back.
    let records = LogManager::read_all_records(&log_path)?;
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].record_type, LogRecordType::Data);
    assert_eq!(records[0].transaction_id, 1);
    assert_eq!(records[0].object_id, obj_id);
    assert_eq!(records[0].old_hash, None);
    assert_eq!(records[0].new_hash, Some(hash_a));
    assert_eq!(records[1].new_hash, Some(hash_b));
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
        log.log_data(1, uuid::Uuid::new_v4(), None, Some(EMPTY_HASH))?;
        log.log_end(1)?;
    }

    // Write more in second session.
    {
        let mut log = LogManager::open(&log_path)?;
        assert_eq!(log.next_lsn(), 3); // should continue from where we left off
        log.log_data(2, uuid::Uuid::new_v4(), None, Some(EMPTY_HASH))?;
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
    txn_mgr.log_write(txn_id, obj_id, None, Some(hash))?;
    txn_mgr.commit_txn(txn_id)?;

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
    txn_mgr.log_write(txn_id, obj_id, None, Some(hash))?;
    let undo_map = txn_mgr.abort_txn(txn_id)?;

    // Undo map should have the object with old_hash = None (didn't exist before).
    assert_eq!(undo_map.get(&obj_id), Some(&None));
    assert!(!txn_mgr.has_active_transactions());

    Ok(())
}

// --- Recovery Tests ---

#[test]
fn test_recovery_redo_committed() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("test.log");
    let store = ContentStore::open(dir.path())?;

    let obj_id = uuid::Uuid::new_v4();
    let hash = content_hash(b"committed data");

    // Register obj in store with empty content.
    store.register_empty(obj_id);

    // Write committed transaction to log.
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(1, obj_id, Some(EMPTY_HASH), Some(hash))?;
        log.log_end(1)?;
    }

    // Store the actual content.
    store.put_object(obj_id, b"committed data")?;

    // Simulate crash: clear the index (as if it wasn't flushed).
    store.set_hash(obj_id, EMPTY_HASH);

    // Run recovery — should redo committed txn.
    let result = recovery::recover(&log_path, &store)?;
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

    let obj_id = uuid::Uuid::new_v4();
    let old_hash = content_hash(b"original");
    let new_hash = content_hash(b"modified");

    // Set up: object exists with original content.
    store.put_object(obj_id, b"original")?;

    // Write uncommitted transaction (no End record).
    {
        let mut log = LogManager::open(&log_path)?;
        log.log_data(1, obj_id, Some(old_hash), Some(new_hash))?;
        // No log_end — simulating crash before commit.
    }

    // Simulate: index was updated to new_hash before crash.
    store.set_hash(obj_id, new_hash);

    // Run recovery — should undo uncommitted txn.
    let result = recovery::recover(&log_path, &store)?;
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
        log.log_data(1, obj_a, None, Some(hash_a))?;
        log.log_end(1)?;
        log.log_data(2, obj_b, Some(hash_b_old), Some(hash_b_new))?;
        // No End for txn 2.
    }

    // Simulate crash state.
    store.set_hash(obj_a, EMPTY_HASH); // committed txn not yet in index
    store.set_hash(obj_b, hash_b_new); // uncommitted txn was in index

    let result = recovery::recover(&log_path, &store)?;
    assert_eq!(result.committed_count, 1);
    assert_eq!(result.uncommitted_count, 1);

    // obj_a should have hash_a (redo).
    assert_eq!(store.get_hash(&obj_a), Some(hash_a));
    // obj_b should have hash_b_old (undo).
    assert_eq!(store.get_hash(&obj_b), Some(hash_b_old));

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
