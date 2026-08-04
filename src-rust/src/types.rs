//! Shared types for the YAAM memory engine.
//!
//! All event schemas, node/edge structures, and graph types used across modules.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── JSONL Event Schemas ────────────────────────────────────────────────────

/// Version of the event schema. Used for future migrations.
pub const EVENT_VERSION: u32 = 1;

/// Top-level event envelope stored in events.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub version: u32,
    pub timestamp: u64,
    pub event_type: EventType,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    UpsertNode,
    LinkNodes,
    DeleteNode,
    DeleteEdges,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventPayload {
    UpsertNode(UpsertNodePayload),
    LinkNodes(LinkNodesPayload),
    DeleteNode(DeleteNodePayload),
    DeleteEdges(DeleteEdgesPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertNodePayload {
    pub id: String,
    pub label: String,
    pub properties: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkNodesPayload {
    pub from_id: String,
    pub to_id: String,
    pub relationship: String,
    #[serde(default)]
    pub properties: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteNodePayload {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteEdgesPayload {
    pub from_id: String,
    #[serde(default = "default_direction")]
    pub direction: String,
}

fn default_direction() -> String {
    "outbound".to_string()
}

// ─── In-Memory Graph Types ──────────────────────────────────────────────────

/// Node label discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "label")]
pub enum NodeLabel {
    Entity {
        entity_type: String,
        status: String,
        last_modified: u64,
    },
    Workspace {
        description: String,
        status: String,
        #[serde(default)]
        closed_at: Option<u64>,
    },
    Scratchpad {
        created_at: u64,
    },
}

/// A node in the memory graph.
#[derive(Debug, Clone)]
pub struct MemoryNode {
    pub id: String,
    pub label: NodeLabel,
    pub name: String,
    pub content: String,
    pub metadata: String,
    pub embedding: Option<Vec<Vec<f32>>>,
}

/// A directed edge in the memory graph.
#[derive(Debug, Clone)]
pub struct Edge {
    pub from_id: String,
    pub to_id: String,
    pub relationship: String,
    pub properties: HashMap<String, serde_json::Value>,
}

// ─── JSON-RPC Types ─────────────────────────────────────────────────────────

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcResponse {
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<serde_json::Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message,
                data: None,
            }),
        }
    }
}

// ─── JSON-RPC Error Codes ───────────────────────────────────────────────────

pub const RPC_PARSE_ERROR: i32 = -32700;
pub const RPC_INVALID_REQUEST: i32 = -32600;
pub const RPC_METHOD_NOT_FOUND: i32 = -32601;
pub const RPC_INVALID_PARAMS: i32 = -32602;
pub const RPC_INTERNAL_ERROR: i32 = -32603;

// ─── Retrieval Mode ───────────────────────────────────────────────────────

/// Controls how much content is returned per node in search and query results.
///
/// - `Name`    — returns name + line number only (no content)
/// - `Preview` — returns name + line number + first ~100 words of content
/// - `Full`    — returns name + line number + full content (default)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RetrievalMode {
    Name,
    Preview,
    Full,
}

impl Default for RetrievalMode {
    fn default() -> Self {
        RetrievalMode::Full
    }
}

// ─── Query DSL Types ────────────────────────────────────────────────────────

/// A read-only query expressed via the JSON DSL.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DslQuery {
    /// Match criteria: filter nodes by label, type, id, or status.
    #[serde(default, rename = "match")]
    pub match_clause: Option<MatchClause>,

    /// Traverse from matched nodes along a relationship.
    #[serde(default)]
    pub traverse: Option<TraverseClause>,

    /// Filter by edge relationship to another node.
    #[serde(default, rename = "where")]
    pub where_clause: Option<WhereClause>,

    /// Aggregate results (group_by, count).
    #[serde(default)]
    pub aggregate: Option<AggregateClause>,

    /// Which fields to return.
    #[serde(default, rename = "return")]
    pub return_fields: Option<Vec<String>>,

    /// How much content to return per result.
    #[serde(default)]
    pub retrieval: Option<RetrievalMode>,

    /// Limit number of results.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MatchClause {
    /// Match by node label: "Entity", "Workspace", "Scratchpad".
    #[serde(default)]
    pub label: Option<String>,
    /// Match by entity type: "File", "Class", "Function".
    #[serde(default, rename = "entity_type")]
    pub entity_type: Option<String>,
    /// Match by exact node ID.
    #[serde(default)]
    pub id: Option<String>,
    /// Match by status: "active", "inactive", "deleted".
    #[serde(default)]
    pub status: Option<String>,
    /// Match by name substring (case-insensitive).
    #[serde(default)]
    pub name_contains: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseClause {
    /// Edge relationship type to follow: "CALLS", "DECLARED_IN", etc.
    pub relationship: String,
    /// Direction: "outbound" (default), "inbound", or "both".
    #[serde(default = "default_direction")]
    pub direction: String,
    /// Maximum traversal depth (default: 1, max: 5).
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

fn default_max_depth() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhereClause {
    /// Filter nodes that have an edge to a specific target node.
    #[serde(default)]
    pub edge_to: Option<EdgeFilter>,
    /// Filter nodes that have an inbound edge from a specific source node.
    #[serde(default)]
    pub edge_from: Option<EdgeFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeFilter {
    /// Target/source node ID.
    pub id: String,
    /// Edge relationship type.
    pub relationship: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateClause {
    /// Field to group by (e.g., "type", "label", "status").
    pub group_by: String,
    /// Whether to count occurrences per group.
    #[serde(default)]
    pub count: bool,
}

/// Hybrid search request (BM25 + semantic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The natural language or keyword query text.
    pub text: String,
    /// Optional: scope to a specific workspace.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Maximum results to return (default: 10).
    #[serde(default = "default_top_k")]
    pub top_k: Option<usize>,
    /// Optional: filter results by entity type (e.g. ["Function", "Class"]).
    #[serde(default)]
    pub entity_types: Option<Vec<String>>,
    /// Optional: include only results whose ID starts with one of these path
    /// prefixes (e.g. ["src/", "lib/"]).
    #[serde(default)]
    pub include_paths: Option<Vec<String>>,
    /// Optional: exclude results whose ID starts with one of these path
    /// prefixes (e.g. ["node_modules/", ".venv/"]).
    #[serde(default)]
    pub exclude_paths: Option<Vec<String>>,

    /// How much content to return per result. Default: Full.
    #[serde(default)]
    pub retrieval: Option<RetrievalMode>,

    /// Optional: resolve graph relationships for the top-N search results.
    /// When specified, the response is `{ "results": [...] }` instead of a
    /// flat array, and each of the top `resolve_top_k` results includes a
    /// `traversal` object with neighbor nodes.
    #[serde(default)]
    pub traverse: Option<SearchTraverseClause>,

    /// Optional: extract a snippet (best-matching passage) from each result's
    /// content. When specified, the response is `{ "results": [...] }`.
    #[serde(default)]
    pub snippet: Option<SnippetMode>,

    /// Optional: MMR diversity lambda (0.0 = max diversity, 1.0 = max
    /// relevance). When omitted, pure relevance ranking is used (backward
    /// compatible).
    #[serde(default)]
    pub diversity_lambda: Option<f32>,
}

fn default_top_k() -> Option<usize> {
    Some(10)
}

/// Traversal spec for search results.
///
/// Applied to the top `resolve_top_k` search hits. Each hit's edges
/// (filtered by relationship and direction) are returned as compact
/// neighbor summaries — no content, no embeddings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchTraverseClause {
    /// Relationship types to follow. If omitted, all relationships are included.
    /// Examples: ["CALLS"], ["CALLS", "IMPORTS"], ["REFERENCES"]
    #[serde(default)]
    pub relationship: Option<Vec<String>>,

    /// Direction: "outbound" (default), "inbound", or "both".
    #[serde(default = "default_traverse_direction")]
    pub direction: String,

    /// How many of the top search results to resolve edges for.
    /// Remaining results are returned as plain search hits (no edges).
    /// Default: 3.
    #[serde(default = "default_resolve_top_k")]
    pub resolve_top_k: usize,
}

fn default_traverse_direction() -> String {
    "both".to_string()
}

fn default_resolve_top_k() -> usize {
    3
}

/// Controls snippet extraction in search results.
///
/// When enabled, each result includes a `snippet` field containing the
/// passage from the entity's content that best matches the query.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SnippetMode {
    /// Extract a snippet of approximately 64 tokens centered on the
    /// highest-scoring sentence/line.
    Auto,
}

/// A single neighbor node discovered via traversal from a search result.
///
/// Compact representation — only identity fields, no content or embeddings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborNode {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub relationship: String,
    pub direction: String,
}

/// Traversal results for a single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchTraversal {
    /// The search result's entity ID (for correlation).
    pub entity_id: String,
    /// Neighbor nodes found via traversal.
    pub neighbors: Vec<NeighborNode>,
}
