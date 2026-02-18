//! Graph edges — typed associations between nodes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{EdgeType, ObjectId};

/// A directed edge in the graph namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub id: ObjectId,
    pub source: ObjectId,
    pub target: ObjectId,
    pub label: String,
    pub edge_type: EdgeType,
}

/// Edge index for a single node — outgoing edges indexed by (type, label).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EdgeIndex {
    pub node_id: ObjectId,
    /// (EdgeType, label) → target ObjectId.
    pub outgoing: BTreeMap<(EdgeType, String), ObjectId>,
}

impl EdgeIndex {
    pub fn new(node_id: ObjectId) -> Self {
        Self {
            node_id,
            outgoing: BTreeMap::new(),
        }
    }

    /// Add an outgoing edge.
    pub fn add(&mut self, edge_type: EdgeType, label: String, target: ObjectId) {
        self.outgoing.insert((edge_type, label), target);
    }

    /// Remove an outgoing edge. Returns the target if it existed.
    pub fn remove(&mut self, edge_type: EdgeType, label: &str) -> Option<ObjectId> {
        self.outgoing.remove(&(edge_type, label.to_string()))
    }

    /// Resolve a name under this node (lookup by type + label).
    pub fn resolve(&self, edge_type: EdgeType, label: &str) -> Option<ObjectId> {
        self.outgoing.get(&(edge_type, label.to_string())).copied()
    }

    /// All outgoing edges (for readdir).
    pub fn all_outgoing(&self) -> impl Iterator<Item = (&(EdgeType, String), &ObjectId)> {
        self.outgoing.iter()
    }

    /// Count of outgoing edges.
    pub fn count(&self) -> usize {
        self.outgoing.len()
    }
}
