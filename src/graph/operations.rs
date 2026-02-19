//! Graph namespace operations.
//!
//! The graph is the storage namespace. Nodes are objects with stable UUIDs.
//! Edges are typed associations. All mutations go through the transaction
//! manager so they're logged and recoverable.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::graph::edge::EdgeIndex;
use crate::graph::node::GraphNode;
use crate::{EdgeType, ObjectId};

/// Persistent graph state: nodes and edge indices.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GraphState {
    pub nodes: HashMap<ObjectId, GraphNode>,
    pub edges: HashMap<ObjectId, EdgeIndex>,
    /// Reverse index: target → set of (source, edge_type, label).
    pub reverse: HashMap<ObjectId, Vec<(ObjectId, EdgeType, String)>>,
}

/// The graph namespace manager.
pub struct GraphNamespace {
    pub state: Mutex<GraphState>,
    state_path: std::path::PathBuf,
}

impl GraphNamespace {
    /// Open or create the graph namespace.
    pub fn open(root: &Path) -> io::Result<Self> {
        let state_path = root.join("graph_state.bin");

        let state = if state_path.exists() {
            let data = std::fs::read(&state_path)?;
            bincode::deserialize(&data)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            GraphState::default()
        };

        Ok(Self {
            state: Mutex::new(state),
            state_path,
        })
    }

    /// Initialize a fresh filesystem with a root directory node.
    pub fn init_root(&self, uid: u32, gid: u32) -> ObjectId {
        let mut state = self.state.lock().unwrap();
        // Check if root already exists.
        if let Some((_id, _node)) = state.nodes.iter().next() {
            // Find the root — it's the directory with no incoming Child edges,
            // or just return the first node for now. We'll track root explicitly.
        }

        let root_id = uuid::Uuid::new_v4();
        let root_node = GraphNode::new_dir(root_id, uid, gid, 0o755);
        state.nodes.insert(root_id, root_node);
        state.edges.insert(root_id, EdgeIndex::new(root_id));
        root_id
    }

    /// Add a node to the graph.
    pub fn add_node(&self, node: GraphNode) {
        let mut state = self.state.lock().unwrap();
        let id = node.id;
        state.nodes.insert(id, node);
        state.edges.entry(id).or_insert_with(|| EdgeIndex::new(id));
    }

    /// Remove a node from the graph.
    /// Also cleans up reverse index entries for any outgoing edges
    /// (e.g., tag edges from a file being deleted).
    pub fn remove_node(&self, id: &ObjectId) -> Option<GraphNode> {
        let mut state = self.state.lock().unwrap();
        // Collect targets of outgoing edges, then clean up their reverse indices.
        let targets: Vec<ObjectId> = state
            .edges
            .get(id)
            .map(|idx| idx.outgoing.values().copied().collect())
            .unwrap_or_default();
        for target in &targets {
            if let Some(rev) = state.reverse.get_mut(target) {
                rev.retain(|(s, _, _)| s != id);
            }
        }
        state.edges.remove(id);
        state.reverse.remove(id);
        state.nodes.remove(id)
    }

    /// Get a node by ID.
    pub fn get_node(&self, id: &ObjectId) -> Option<GraphNode> {
        let state = self.state.lock().unwrap();
        state.nodes.get(id).cloned()
    }

    /// Update a node's metadata.
    pub fn update_node<F>(&self, id: &ObjectId, f: F) -> bool
    where
        F: FnOnce(&mut GraphNode),
    {
        let mut state = self.state.lock().unwrap();
        if let Some(node) = state.nodes.get_mut(id) {
            f(node);
            true
        } else {
            false
        }
    }

    /// Add a directed edge.
    pub fn add_edge(
        &self,
        source: ObjectId,
        target: ObjectId,
        edge_type: EdgeType,
        label: String,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();

        // Verify both nodes exist.
        if !state.nodes.contains_key(&source) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("source node {} not found", source),
            ));
        }
        if !state.nodes.contains_key(&target) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("target node {} not found", target),
            ));
        }

        // Add to forward index.
        state
            .edges
            .entry(source)
            .or_insert_with(|| EdgeIndex::new(source))
            .add(edge_type, label.clone(), target);

        // Add to reverse index.
        state
            .reverse
            .entry(target)
            .or_default()
            .push((source, edge_type, label));

        Ok(())
    }

    /// Remove a directed edge. Returns the target node ID if the edge existed.
    pub fn remove_edge(
        &self,
        source: &ObjectId,
        edge_type: EdgeType,
        label: &str,
    ) -> Option<ObjectId> {
        let mut state = self.state.lock().unwrap();

        let target = state
            .edges
            .get_mut(source)?
            .remove(edge_type, label)?;

        // Remove from reverse index.
        if let Some(rev) = state.reverse.get_mut(&target) {
            rev.retain(|(s, et, l)| !(s == source && *et == edge_type && l == label));
        }

        Some(target)
    }

    /// Resolve a name: given a source node and label, find the target.
    pub fn resolve(
        &self,
        source: &ObjectId,
        edge_type: EdgeType,
        label: &str,
    ) -> Option<ObjectId> {
        let state = self.state.lock().unwrap();
        state.edges.get(source)?.resolve(edge_type, label)
    }

    /// Get all outgoing edges from a node (for readdir).
    pub fn neighbors(&self, source: &ObjectId) -> Vec<(EdgeType, String, ObjectId)> {
        let state = self.state.lock().unwrap();
        match state.edges.get(source) {
            Some(idx) => idx
                .all_outgoing()
                .map(|((et, label), target)| (*et, label.clone(), *target))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Get all edges pointing TO a node (reverse lookup).
    pub fn reverse_lookup(&self, target: &ObjectId) -> Vec<(ObjectId, EdgeType, String)> {
        let state = self.state.lock().unwrap();
        state
            .reverse
            .get(target)
            .cloned()
            .unwrap_or_default()
    }

    /// Persist graph state to disk.
    pub fn flush(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        let data =
            bincode::serialize(&*state).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let tmp_path = self.state_path.with_extension("tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.state_path)?;
        Ok(())
    }

    /// Check if a node exists.
    pub fn node_exists(&self, id: &ObjectId) -> bool {
        let state = self.state.lock().unwrap();
        state.nodes.contains_key(id)
    }

    /// Count of outgoing edges for a node.
    pub fn edge_count(&self, source: &ObjectId) -> usize {
        let state = self.state.lock().unwrap();
        state.edges.get(source).map_or(0, |e| e.count())
    }
}
