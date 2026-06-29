//! In-memory graph engine for the YAAM memory system.
//!
//! `MemoryEngine` holds all nodes and edges in RAM, providing fast lookups
//! by id, label, and entity type, plus bidirectional edge traversal via
//! separate forward and reverse adjacency lists.

use std::collections::HashMap;

use crate::types::{
    DeleteEdgesPayload, DeleteNodePayload, Edge, Event, EventPayload, EventType, LinkNodesPayload,
    MemoryNode, NodeLabel, UpsertNodePayload,
};

// ─── MemoryEngine ───────────────────────────────────────────────────────────

/// The core in-memory graph that stores all nodes and edges.
///
/// Edges are stored in **two** adjacency maps so that both forward (outbound)
/// and reverse (inbound) traversals are O(1) by node id.
pub struct MemoryEngine {
    /// All nodes keyed by their unique id.
    nodes: HashMap<String, MemoryNode>,
    /// Forward (outbound) edges: `from_id -> Vec<Edge>`.
    forward_edges: HashMap<String, Vec<Edge>>,
    /// Reverse (inbound) edges: `to_id -> Vec<Edge>`.
    reverse_edges: HashMap<String, Vec<Edge>>,
}

impl MemoryEngine {
    /// Create a new, empty graph.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
        }
    }

    // ── Node operations ─────────────────────────────────────────────────

    /// Insert or update a node.
    ///
    /// The `label` field in the payload is one of "Entity", "Workspace", or
    /// "Scratchpad".  Remaining fields are pulled from `properties`.
    pub fn upsert_node(&mut self, payload: &UpsertNodePayload) {
        let props = &payload.properties;

        let label = match payload.label.as_str() {
            "Entity" => NodeLabel::Entity {
                entity_type: extract_string(props, "type"),
                status: extract_string_or(props, "status", "active"),
                last_modified: extract_u64(props, "last_modified"),
            },
            "Workspace" => NodeLabel::Workspace {
                description: extract_string(props, "description"),
                status: extract_string_or(props, "status", "active"),
                closed_at: extract_optional_u64(props, "closed_at"),
            },
            "Scratchpad" => NodeLabel::Scratchpad {
                created_at: extract_u64(props, "created_at"),
            },
            other => {
                // Treat unknown labels as Entity with the raw label as entity_type.
                NodeLabel::Entity {
                    entity_type: other.to_string(),
                    status: extract_string_or(props, "status", "active"),
                    last_modified: extract_u64(props, "last_modified"),
                }
            }
        };

        let name = extract_string_or(props, "name", "");
        let content = extract_string_or(props, "content", "");
        let metadata = extract_string_or(props, "metadata", "");

        // Extract embedding vector from properties if present.
        // The RPC handler computes embeddings and stores them in properties["embedding"].
        // This also covers replay from JSONL events where embeddings are persisted.
        let embedding = props
            .get("embedding")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect::<Vec<f32>>()
            });

        let node = MemoryNode {
            id: payload.id.clone(),
            label,
            name,
            content,
            metadata,
            embedding,
        };

        self.nodes.insert(payload.id.clone(), node);
    }

    /// Delete a node and **all** of its edges (both outbound and inbound).
    pub fn delete_node(&mut self, id: &str) {
        self.nodes.remove(id);

        // Remove outbound edges from this node and clean up reverse side.
        if let Some(fwd) = self.forward_edges.remove(id) {
            for edge in &fwd {
                if let Some(rev) = self.reverse_edges.get_mut(&edge.to_id) {
                    rev.retain(|e| e.from_id != id);
                }
            }
        }

        // Remove inbound edges to this node and clean up forward side.
        if let Some(rev) = self.reverse_edges.remove(id) {
            for edge in &rev {
                if let Some(fwd) = self.forward_edges.get_mut(&edge.from_id) {
                    fwd.retain(|e| e.to_id != id);
                }
            }
        }
    }

    /// Get a node by its id.
    pub fn get_node(&self, id: &str) -> Option<&MemoryNode> {
        self.nodes.get(id)
    }

    /// Return all nodes that match a given label string ("Entity", "Workspace",
    /// "Scratchpad").
    pub fn get_nodes_by_label(&self, label: &str) -> Vec<&MemoryNode> {
        self.nodes
            .values()
            .filter(|n| node_label_str(&n.label) == label)
            .collect()
    }

    /// Return all Entity nodes whose `entity_type` matches `entity_type`.
    pub fn get_nodes_by_type(&self, entity_type: &str) -> Vec<&MemoryNode> {
        self.nodes
            .values()
            .filter(|n| match &n.label {
                NodeLabel::Entity {
                    entity_type: et, ..
                } => et == entity_type,
                _ => false,
            })
            .collect()
    }

    pub fn all_node_ids(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    pub fn all_nodes(&self) -> Vec<&MemoryNode> {
        self.nodes.values().collect()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    // ── Edge operations ─────────────────────────────────────────────────

    /// Add a directed edge between two nodes.
    ///
    /// The edge is stored in both `forward_edges` (keyed by `from_id`) and
    /// `reverse_edges` (keyed by `to_id`).
    pub fn link_nodes(&mut self, payload: &LinkNodesPayload) {
        let edge = Edge {
            from_id: payload.from_id.clone(),
            to_id: payload.to_id.clone(),
            relationship: payload.relationship.clone(),
            properties: payload.properties.clone(),
        };

        self.forward_edges
            .entry(payload.from_id.clone())
            .or_default()
            .push(edge.clone());

        self.reverse_edges
            .entry(payload.to_id.clone())
            .or_default()
            .push(edge);
    }

    /// Delete edges associated with `from_id`.
    ///
    /// * `"outbound"` — remove forward edges from `from_id`, clean up reverse.
    /// * `"inbound"` — remove reverse edges to `from_id`, clean up forward.
    /// * `"both"` — remove all edges touching `from_id`.
    pub fn delete_edges(&mut self, from_id: &str, direction: &str) {
        match direction {
            "outbound" => {
                if let Some(fwd) = self.forward_edges.remove(from_id) {
                    for edge in &fwd {
                        if let Some(rev) = self.reverse_edges.get_mut(&edge.to_id) {
                            rev.retain(|e| e.from_id != from_id);
                        }
                    }
                }
            }
            "inbound" => {
                if let Some(rev) = self.reverse_edges.remove(from_id) {
                    for edge in &rev {
                        if let Some(fwd) = self.forward_edges.get_mut(&edge.from_id) {
                            fwd.retain(|e| e.to_id != from_id);
                        }
                    }
                }
            }
            "both" | _ => {
                // Outbound
                if let Some(fwd) = self.forward_edges.remove(from_id) {
                    for edge in &fwd {
                        if let Some(rev) = self.reverse_edges.get_mut(&edge.to_id) {
                            rev.retain(|e| e.from_id != from_id);
                        }
                    }
                }
                // Inbound
                if let Some(rev) = self.reverse_edges.remove(from_id) {
                    for edge in &rev {
                        if let Some(fwd) = self.forward_edges.get_mut(&edge.from_id) {
                            fwd.retain(|e| e.to_id != from_id);
                        }
                    }
                }
            }
        }
    }

    /// Get the forward (outbound) edges for a node.
    pub fn get_forward_edges(&self, id: &str) -> &[Edge] {
        self.forward_edges
            .get(id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get the reverse (inbound) edges for a node.
    pub fn get_reverse_edges(&self, id: &str) -> &[Edge] {
        self.reverse_edges
            .get(id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ── Event replay ────────────────────────────────────────────────────

    /// Apply a single event to the graph.
    ///
    /// Delegates to the appropriate mutation method based on `event_type`.
    pub fn apply_event(&mut self, event: &Event) {
        match (&event.event_type, &event.payload) {
            (EventType::UpsertNode, EventPayload::UpsertNode(p)) => {
                self.upsert_node(p);
            }
            (EventType::LinkNodes, EventPayload::LinkNodes(p)) => {
                self.link_nodes(p);
            }
            (EventType::DeleteNode, EventPayload::DeleteNode(p)) => {
                self.delete_node(&p.id);
            }
            (EventType::DeleteEdges, EventPayload::DeleteEdges(p)) => {
                self.delete_edges(&p.from_id, &p.direction);
            }
            // Mismatched event_type / payload — ignore silently.
            _ => {}
        }
    }

    /// Replay a full slice of events to reconstitute the graph state.
    pub fn load_from_events(&mut self, events: &[Event]) {
        for event in events {
            self.apply_event(event);
        }
    }
}

impl Default for MemoryEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Return the string label discriminator for a `NodeLabel`.
fn node_label_str(label: &NodeLabel) -> &'static str {
    match label {
        NodeLabel::Entity { .. } => "Entity",
        NodeLabel::Workspace { .. } => "Workspace",
        NodeLabel::Scratchpad { .. } => "Scratchpad",
    }
}

/// Extract a `String` from properties, falling back to `""`.
fn extract_string(props: &HashMap<String, serde_json::Value>, key: &str) -> String {
    props
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract a `String` with a custom default.
fn extract_string_or(
    props: &HashMap<String, serde_json::Value>,
    key: &str,
    default: &str,
) -> String {
    props
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

/// Extract a `u64` from properties, falling back to `0`.
fn extract_u64(props: &HashMap<String, serde_json::Value>, key: &str) -> u64 {
    props
        .get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Extract an `Option<u64>` from properties.
fn extract_optional_u64(props: &HashMap<String, serde_json::Value>, key: &str) -> Option<u64> {
    props.get(key).and_then(|v| v.as_u64())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Helpers ─────────────────────────────────────────────────────────

    fn make_upsert(id: &str, label: &str, props: serde_json::Value) -> UpsertNodePayload {
        UpsertNodePayload {
            id: id.to_string(),
            label: label.to_string(),
            properties: props.as_object().unwrap().clone().into_iter().collect(),
        }
    }

    fn make_link(from: &str, to: &str, rel: &str) -> LinkNodesPayload {
        LinkNodesPayload {
            from_id: from.to_string(),
            to_id: to.to_string(),
            relationship: rel.to_string(),
            properties: HashMap::new(),
        }
    }

    fn make_event(event_type: EventType, payload: EventPayload) -> Event {
        Event {
            version: 1,
            timestamp: 1000,
            event_type,
            payload,
        }
    }

    // ── Node upsert / retrieve ──────────────────────────────────────────

    #[test]
    fn upsert_entity_node() {
        let mut engine = MemoryEngine::new();
        let payload = make_upsert(
            "node-1",
            "Entity",
            json!({
                "type": "Function",
                "status": "active",
                "last_modified": 1719680000,
                "name": "my_func",
                "metadata": "{\"lang\":\"rust\"}",
            }),
        );
        engine.upsert_node(&payload);

        let node = engine.get_node("node-1").expect("node should exist");
        assert_eq!(node.id, "node-1");
        assert_eq!(node.name, "my_func");
        assert_eq!(node.metadata, "{\"lang\":\"rust\"}");
        match &node.label {
            NodeLabel::Entity {
                entity_type,
                status,
                last_modified,
            } => {
                assert_eq!(entity_type, "Function");
                assert_eq!(status, "active");
                assert_eq!(*last_modified, 1719680000);
            }
            other => panic!("expected Entity, got {:?}", other),
        }
    }

    #[test]
    fn upsert_workspace_node() {
        let mut engine = MemoryEngine::new();
        let payload = make_upsert(
            "ws-1",
            "Workspace",
            json!({
                "description": "Project Alpha",
                "status": "active",
                "name": "alpha",
            }),
        );
        engine.upsert_node(&payload);

        let node = engine.get_node("ws-1").unwrap();
        assert_eq!(node.name, "alpha");
        match &node.label {
            NodeLabel::Workspace {
                description,
                status,
                closed_at,
            } => {
                assert_eq!(description, "Project Alpha");
                assert_eq!(status, "active");
                assert!(closed_at.is_none());
            }
            other => panic!("expected Workspace, got {:?}", other),
        }
    }

    #[test]
    fn upsert_scratchpad_node() {
        let mut engine = MemoryEngine::new();
        let payload = make_upsert(
            "sp-1",
            "Scratchpad",
            json!({
                "content": "scratch text here",
                "created_at": 1719680000,
                "name": "notes",
            }),
        );
        engine.upsert_node(&payload);

        let node = engine.get_node("sp-1").unwrap();
        assert_eq!(node.name, "notes");
        assert_eq!(node.content, "scratch text here");
        match &node.label {
            NodeLabel::Scratchpad { created_at } => {
                assert_eq!(*created_at, 1719680000);
            }
            other => panic!("expected Scratchpad, got {:?}", other),
        }
    }

    #[test]
    fn get_nodes_by_label_returns_correct_set() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("e1", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("e2", "Entity", json!({"type": "Class"})));
        engine.upsert_node(&make_upsert("ws1", "Workspace", json!({})));

        let entities = engine.get_nodes_by_label("Entity");
        assert_eq!(entities.len(), 2);

        let workspaces = engine.get_nodes_by_label("Workspace");
        assert_eq!(workspaces.len(), 1);
    }

    #[test]
    fn get_nodes_by_type_filters_entity_type() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("e1", "Entity", json!({"type": "Function"})));
        engine.upsert_node(&make_upsert("e2", "Entity", json!({"type": "Function"})));
        engine.upsert_node(&make_upsert("e3", "Entity", json!({"type": "Class"})));

        let funcs = engine.get_nodes_by_type("Function");
        assert_eq!(funcs.len(), 2);

        let classes = engine.get_nodes_by_type("Class");
        assert_eq!(classes.len(), 1);
    }

    #[test]
    fn duplicate_upsert_updates_existing_node() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert(
            "e1",
            "Entity",
            json!({"type": "File", "name": "old_name", "status": "active"}),
        ));
        assert_eq!(engine.get_node("e1").unwrap().name, "old_name");

        // Update the same id with new properties.
        engine.upsert_node(&make_upsert(
            "e1",
            "Entity",
            json!({"type": "File", "name": "new_name", "status": "inactive"}),
        ));

        let node = engine.get_node("e1").unwrap();
        assert_eq!(node.name, "new_name");
        match &node.label {
            NodeLabel::Entity { status, .. } => assert_eq!(status, "inactive"),
            _ => panic!("expected Entity"),
        }
        // Should still be only 1 node.
        assert_eq!(engine.get_nodes_by_label("Entity").len(), 1);
    }

    // ── Edge operations ─────────────────────────────────────────────────

    #[test]
    fn link_nodes_creates_forward_and_reverse() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("a", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("b", "Entity", json!({"type": "Function"})));

        engine.link_nodes(&make_link("a", "b", "CONTAINS"));

        let fwd = engine.get_forward_edges("a");
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].to_id, "b");
        assert_eq!(fwd[0].relationship, "CONTAINS");

        let rev = engine.get_reverse_edges("b");
        assert_eq!(rev.len(), 1);
        assert_eq!(rev[0].from_id, "a");
    }

    #[test]
    fn delete_node_removes_associated_edges() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("a", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("b", "Entity", json!({"type": "Function"})));
        engine.upsert_node(&make_upsert("c", "Entity", json!({"type": "Class"})));

        engine.link_nodes(&make_link("a", "b", "CONTAINS"));
        engine.link_nodes(&make_link("c", "a", "IMPORTS"));

        // Delete node "a" — should clean up both edges.
        engine.delete_node("a");

        assert!(engine.get_node("a").is_none());
        assert!(engine.get_forward_edges("a").is_empty());
        assert!(engine.get_reverse_edges("a").is_empty());

        // Edge from c->a should be gone from c's forward list.
        assert!(engine.get_forward_edges("c").is_empty());
        // Edge from a->b should be gone from b's reverse list.
        assert!(engine.get_reverse_edges("b").is_empty());
    }

    #[test]
    fn delete_edges_outbound() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("a", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("b", "Entity", json!({"type": "File"})));
        engine.link_nodes(&make_link("a", "b", "CALLS"));

        engine.delete_edges("a", "outbound");
        assert!(engine.get_forward_edges("a").is_empty());
        assert!(engine.get_reverse_edges("b").is_empty());
    }

    #[test]
    fn delete_edges_inbound() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("a", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("b", "Entity", json!({"type": "File"})));
        engine.link_nodes(&make_link("a", "b", "CALLS"));

        // Delete inbound edges to "b" (which is the edge from a->b).
        engine.delete_edges("b", "inbound");
        assert!(engine.get_reverse_edges("b").is_empty());
        assert!(engine.get_forward_edges("a").is_empty());
    }

    #[test]
    fn delete_edges_both() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("a", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("b", "Entity", json!({"type": "File"})));
        engine.upsert_node(&make_upsert("c", "Entity", json!({"type": "File"})));
        engine.link_nodes(&make_link("a", "b", "CALLS"));
        engine.link_nodes(&make_link("c", "a", "IMPORTS"));

        engine.delete_edges("a", "both");
        assert!(engine.get_forward_edges("a").is_empty());
        assert!(engine.get_reverse_edges("a").is_empty());
        assert!(engine.get_reverse_edges("b").is_empty());
        assert!(engine.get_forward_edges("c").is_empty());
    }

    #[test]
    fn get_edges_returns_empty_for_unknown_node() {
        let engine = MemoryEngine::new();
        assert!(engine.get_forward_edges("nonexistent").is_empty());
        assert!(engine.get_reverse_edges("nonexistent").is_empty());
    }

    // ── Event replay ────────────────────────────────────────────────────

    #[test]
    fn apply_event_upsert() {
        let mut engine = MemoryEngine::new();
        let event = make_event(
            EventType::UpsertNode,
            EventPayload::UpsertNode(UpsertNodePayload {
                id: "ev-1".to_string(),
                label: "Entity".to_string(),
                properties: json!({"type": "Module", "name": "core"})
                    .as_object()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .collect(),
            }),
        );
        engine.apply_event(&event);
        assert!(engine.get_node("ev-1").is_some());
    }

    #[test]
    fn apply_event_link() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("x", "Entity", json!({"type": "A"})));
        engine.upsert_node(&make_upsert("y", "Entity", json!({"type": "B"})));

        let event = make_event(
            EventType::LinkNodes,
            EventPayload::LinkNodes(LinkNodesPayload {
                from_id: "x".to_string(),
                to_id: "y".to_string(),
                relationship: "DEPENDS_ON".to_string(),
                properties: HashMap::new(),
            }),
        );
        engine.apply_event(&event);
        assert_eq!(engine.get_forward_edges("x").len(), 1);
    }

    #[test]
    fn apply_event_delete_node() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("d", "Entity", json!({"type": "X"})));
        assert!(engine.get_node("d").is_some());

        let event = make_event(
            EventType::DeleteNode,
            EventPayload::DeleteNode(DeleteNodePayload {
                id: "d".to_string(),
            }),
        );
        engine.apply_event(&event);
        assert!(engine.get_node("d").is_none());
    }

    #[test]
    fn apply_event_delete_edges() {
        let mut engine = MemoryEngine::new();
        engine.upsert_node(&make_upsert("p", "Entity", json!({"type": "A"})));
        engine.upsert_node(&make_upsert("q", "Entity", json!({"type": "B"})));
        engine.link_nodes(&make_link("p", "q", "REL"));

        let event = make_event(
            EventType::DeleteEdges,
            EventPayload::DeleteEdges(DeleteEdgesPayload {
                from_id: "p".to_string(),
                direction: "outbound".to_string(),
            }),
        );
        engine.apply_event(&event);
        assert!(engine.get_forward_edges("p").is_empty());
    }

    #[test]
    fn load_from_events_builds_correct_topology() {
        let events = vec![
            make_event(
                EventType::UpsertNode,
                EventPayload::UpsertNode(make_upsert(
                    "file1",
                    "Entity",
                    json!({"type": "File", "name": "main.rs"}),
                )),
            ),
            make_event(
                EventType::UpsertNode,
                EventPayload::UpsertNode(make_upsert(
                    "fn1",
                    "Entity",
                    json!({"type": "Function", "name": "main"}),
                )),
            ),
            make_event(
                EventType::UpsertNode,
                EventPayload::UpsertNode(make_upsert(
                    "fn2",
                    "Entity",
                    json!({"type": "Function", "name": "helper"}),
                )),
            ),
            make_event(
                EventType::LinkNodes,
                EventPayload::LinkNodes(make_link("file1", "fn1", "CONTAINS")),
            ),
            make_event(
                EventType::LinkNodes,
                EventPayload::LinkNodes(make_link("file1", "fn2", "CONTAINS")),
            ),
            make_event(
                EventType::LinkNodes,
                EventPayload::LinkNodes(make_link("fn1", "fn2", "CALLS")),
            ),
        ];

        let mut engine = MemoryEngine::new();
        engine.load_from_events(&events);

        // 3 nodes total
        assert_eq!(engine.get_nodes_by_label("Entity").len(), 3);

        // file1 has 2 outbound CONTAINS edges
        let file_fwd = engine.get_forward_edges("file1");
        assert_eq!(file_fwd.len(), 2);
        assert!(file_fwd.iter().all(|e| e.relationship == "CONTAINS"));

        // fn1 has 1 inbound CONTAINS + 0 outbound CALLS -> 1 outbound
        assert_eq!(engine.get_forward_edges("fn1").len(), 1);
        assert_eq!(engine.get_forward_edges("fn1")[0].relationship, "CALLS");

        // fn2 has 2 inbound edges: one CONTAINS from file1, one CALLS from fn1
        let fn2_rev = engine.get_reverse_edges("fn2");
        assert_eq!(fn2_rev.len(), 2);

        // fn1 has 1 inbound edge (CONTAINS from file1)
        assert_eq!(engine.get_reverse_edges("fn1").len(), 1);
    }

    #[test]
    fn load_from_events_with_delete_in_sequence() {
        let events = vec![
            make_event(
                EventType::UpsertNode,
                EventPayload::UpsertNode(make_upsert("a", "Entity", json!({"type": "X"}))),
            ),
            make_event(
                EventType::UpsertNode,
                EventPayload::UpsertNode(make_upsert("b", "Entity", json!({"type": "Y"}))),
            ),
            make_event(
                EventType::LinkNodes,
                EventPayload::LinkNodes(make_link("a", "b", "REL")),
            ),
            // Now delete node "a" — should also remove the edge.
            make_event(
                EventType::DeleteNode,
                EventPayload::DeleteNode(DeleteNodePayload {
                    id: "a".to_string(),
                }),
            ),
        ];

        let mut engine = MemoryEngine::new();
        engine.load_from_events(&events);

        assert!(engine.get_node("a").is_none());
        assert!(engine.get_node("b").is_some());
        assert!(engine.get_forward_edges("a").is_empty());
        assert!(engine.get_reverse_edges("b").is_empty());
    }
}
