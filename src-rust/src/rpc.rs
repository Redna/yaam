//! JSON-RPC 2.0 method dispatch.
//!
//! Routes incoming JSON-RPC requests to the appropriate handler,
//! wrapping each call in `catch_unwind` to prevent panics from killing the process.

use crate::embedding::EmbeddingModel;
use crate::graph::MemoryEngine;
use crate::query_dsl;
use crate::search::BM25Index;
use crate::storage::EventStore;
use crate::types::*;
use crate::lsp_adapter::{LspAdapter, StdioLspClient};
use std::panic;
use std::sync::{Arc, RwLock, Mutex};
use std::collections::HashMap;

/// Shared application state passed to all RPC handlers.
pub struct AppState {
    pub engine: Arc<RwLock<MemoryEngine>>,
    pub store: Arc<RwLock<EventStore>>,
    pub bm25: Arc<RwLock<BM25Index>>,
    pub embedder: Option<Arc<EmbeddingModel>>,
    pub lsp: Option<Arc<Mutex<StdioLspClient>>>,
}

impl AppState {
    pub fn new(events_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::path::Path::new(events_path);
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        let store = EventStore::new(dir)?;
        let events = store.replay()?;

        let mut engine = MemoryEngine::default();
        engine.load_from_events(&events);

        let mut bm25 = BM25Index::new();
        // Index all existing nodes for BM25
        for node in engine.all_nodes() {
            let text = build_bm25_text(node);
            if !text.is_empty() {
                bm25.add_document(&node.id, &text);
            }
        }

        let model_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
            .join(".yaam").join("models");
        let embedder = match EmbeddingModel::new(&model_dir) {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                eprintln!("Warning: Failed to load ONNX model. Semantic search disabled. ({})", e);
                None
            }
        };

        let mut lsp_client = StdioLspClient::new("npx", &["typescript-language-server", "--stdio"]);
        let lsp = if let Ok(_) = lsp_client.start(dir) {
            Some(Arc::new(Mutex::new(lsp_client)))
        } else {
            None
        };

        Ok(Self {
            engine: Arc::new(RwLock::new(engine)),
            store: Arc::new(RwLock::new(store)),
            bm25: Arc::new(RwLock::new(bm25)),
            embedder,
            lsp,
        })
    }
}

/// Build the text to index for BM25 from a node's relevant fields.
fn build_bm25_text(node: &MemoryNode) -> String {
    let mut parts = Vec::new();

    // Always index the name
    if !node.name.is_empty() {
        parts.push(node.name.clone());
    }

    match &node.label {
        NodeLabel::Entity { .. } => {
            // Index docComment from metadata if present
            if !node.metadata.is_empty() {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
                    if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
                        parts.push(doc.to_string());
                    }
                }
            }
        }
        NodeLabel::Workspace { description, .. } => {
            parts.push(description.clone());
        }
        NodeLabel::Scratchpad { .. } => {
            parts.push(node.content.clone());
        }
    }

    parts.join(" ")
}

/// Build the text to embed for semantic search from a node's properties.
/// Used during upsert and reconciliation to compute ONNX embeddings.
fn build_embedding_text_from_props(
    label: &str,
    props: &HashMap<String, serde_json::Value>,
) -> String {
    let name = props.get("name").and_then(|v| v.as_str()).unwrap_or("");
    match label {
        "Workspace" => {
            let desc = props.get("description").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                desc.to_string()
            } else {
                format!("{} {}", name, desc)
            }
        }
        "Scratchpad" => {
            props.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
        "Entity" => {
            // For code entities, embed the name plus any docComment from metadata.
            let metadata = props.get("metadata").and_then(|v| v.as_str()).unwrap_or("");
            let doc = if !metadata.is_empty() {
                serde_json::from_str::<serde_json::Value>(metadata)
                    .ok()
                    .and_then(|m| m.get("docComment").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if doc.is_empty() {
                name.to_string()
            } else {
                format!("{} {}", name, doc)
            }
        }
        _ => name.to_string(),
    }
}

/// Dispatch a single JSON-RPC request and return a response.
/// Panics in handlers are caught and returned as internal errors.
pub fn dispatch(state: Arc<AppState>, request: RpcRequest) -> RpcResponse {
    let id = request.id.clone();

    // Wrap the entire dispatch in catch_unwind
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let state_ref = state.as_ref();
        match request.method.as_str() {
            // ─── Mutation methods ─────────────────────────────────────────
            "upsert_node" => handle_upsert_node(state_ref, &request.params),
            "link_nodes" => handle_link_nodes(state_ref, &request.params),
            "delete_node" => handle_delete_node(state_ref, &request.params),
            "delete_edges" => handle_delete_edges(state_ref, &request.params),

            // ─── Query methods ────────────────────────────────────────────
            "query" => handle_query(state_ref, &request.params),
            "search" => handle_search(state_ref, &request.params),

            // ─── Reconciliation ───────────────────────────────────────────
            "reconcile" => handle_reconcile(state_ref, &request.params),

            // ─── Lifecycle methods ────────────────────────────────────────
            "initialize" => handle_initialize(state_ref, &request.params),
            "shutdown" => handle_shutdown(state_ref),

            _ => Err(RpcResponse::error(
                id.clone(),
                RPC_METHOD_NOT_FOUND,
                format!("Method '{}' not found", request.method),
            )),
        }
    }));

    match result {
        Ok(Ok(value)) => RpcResponse::success(id, value),
        Ok(Err(mut err_response)) => {
            err_response.id = id;
            err_response
        },
        Err(panic_info) => {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            RpcResponse::error(id, RPC_INTERNAL_ERROR, format!("Internal engine panic: {}", msg))
        }
    }
}

// ─── Mutation Handlers ──────────────────────────────────────────────────────

fn handle_upsert_node(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let payload: UpsertNodePayload = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid params: {}", e))
    })?;

    // Compute embedding for all node types (Workspace, Scratchpad, Entity)
    let mut payload = payload;
    if let Some(ref embedder) = state.embedder {
        let text_to_embed = build_embedding_text_from_props(&payload.label, &payload.properties);
        if !text_to_embed.is_empty() {
            match embedder.embed(&text_to_embed) {
                Ok(vec) => {
                    payload.properties.insert("embedding".to_string(), serde_json::json!(vec));
                }
                Err(e) => {
                    eprintln!("Failed to compute embedding: {}", e);
                }
            }
        }
    }

    // Append to storage
    {
        let store = state.store.write().unwrap();
        let event = crate::storage::upsert_node_event(
            payload.id.clone(),
            payload.label.clone(),
            payload.properties.clone(),
        );
        store.append(&event).map_err(|e| {
            RpcResponse::error(None, RPC_INTERNAL_ERROR, format!("Storage error: {}", e))
        })?;
    }

    // Update in-memory graph
    {
        let mut engine = state.engine.write().unwrap();
        engine.upsert_node(&payload);

        // Update BM25 index
        let node = engine.get_node(&payload.id).cloned();
        if let Some(ref node) = node {
            let text = build_bm25_text(node);
            let mut bm25 = state.bm25.write().unwrap();
            bm25.remove_document(&payload.id);
            if !text.is_empty() {
                bm25.add_document(&payload.id, &text);
            }
        }
    }

    Ok(serde_json::json!({"status": "ok", "id": payload.id}))
}

fn handle_link_nodes(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let payload: LinkNodesPayload = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid params: {}", e))
    })?;

    // Append to storage
    {
        let store = state.store.write().unwrap();
        let event = crate::storage::link_nodes_event(
            payload.from_id.clone(),
            payload.to_id.clone(),
            payload.relationship.clone(),
            payload.properties.clone(),
        );
        store.append(&event).map_err(|e| {
            RpcResponse::error(None, RPC_INTERNAL_ERROR, format!("Storage error: {}", e))
        })?;
    }

    // Update in-memory graph
    {
        let mut engine = state.engine.write().unwrap();
        engine.link_nodes(&payload);
    }

    Ok(serde_json::json!({"status": "ok"}))
}

fn handle_delete_node(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let payload: DeleteNodePayload = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid params: {}", e))
    })?;

    // Append to storage
    {
        let store = state.store.write().unwrap();
        let event = crate::storage::delete_node_event(payload.id.clone());
        store.append(&event).map_err(|e| {
            RpcResponse::error(None, RPC_INTERNAL_ERROR, format!("Storage error: {}", e))
        })?;
    }

    // Update in-memory graph
    {
        let mut engine = state.engine.write().unwrap();
        engine.delete_node(&payload.id);
    }

    // Update BM25 index
    {
        let mut bm25 = state.bm25.write().unwrap();
        bm25.remove_document(&payload.id);
    }

    Ok(serde_json::json!({"status": "ok"}))
}

fn handle_delete_edges(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let payload: DeleteEdgesPayload = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid params: {}", e))
    })?;

    // Append to storage
    {
        let store = state.store.write().unwrap();
        let event = crate::storage::delete_edges_event(payload.from_id.clone(), payload.direction.clone());
        store.append(&event).map_err(|e| {
            RpcResponse::error(None, RPC_INTERNAL_ERROR, format!("Storage error: {}", e))
        })?;
    }

    // Update in-memory graph
    {
        let mut engine = state.engine.write().unwrap();
        engine.delete_edges(&payload.from_id, &payload.direction);
    }

    Ok(serde_json::json!({"status": "ok"}))
}

// ─── Query Handlers ─────────────────────────────────────────────────────────

fn handle_query(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let query: DslQuery = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid query DSL: {}", e))
    })?;

    let engine = state.engine.read().unwrap();
    let result = query_dsl::evaluate_query(&engine, &query);
    Ok(result)
}

fn handle_search(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let request: SearchRequest = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid search request: {}", e))
    })?;

    let top_k = request.top_k.unwrap_or(10);
    let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    // 1. BM25 keyword search
    {
        let bm25 = state.bm25.read().unwrap();
        let bm25_results = bm25.search(&request.text, top_k * 2);
        for (id, score) in bm25_results {
            // Normalize BM25 score roughly (just an approximation for hybrid merge)
            scores.insert(id, score * 0.1);
        }
    }

    let engine = state.engine.read().unwrap();

    // 2. Dense Semantic Search
    if let Some(ref embedder) = state.embedder {
        if let Ok(query_embedding) = embedder.embed(&request.text) {
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            for node in engine.all_nodes() {
                if let Some(doc_embedding) = node.embedding.as_deref() {
                    let mut sim = crate::embedding::cosine_similarity(&query_embedding, doc_embedding);
                    
                    // Apply temporal decay for scratchpads
                    if let NodeLabel::Scratchpad { created_at } = node.label {
                        let decay = crate::embedding::decay_weight(created_at, current_time);
                        sim *= decay;
                    }
                    
                    // Combine with BM25 score if it exists
                    *scores.entry(node.id.clone()).or_insert(0.0) += sim;
                }
            }
        }
    }

    let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // If workspace is specified, filter results
    let filtered: Vec<(String, f32)> = if let Some(ref ws_name) = request.workspace {
        let ws_entities: std::collections::HashSet<String> = engine
            .get_forward_edges(ws_name)
            .iter()
            .filter(|e| e.relationship == "MAPPED_TO" || e.relationship == "HAS_SCRATCHPAD")
            .map(|e| e.to_id.clone())
            .collect();

        ranked
            .into_iter()
            .filter(|(id, _)| ws_entities.contains(id))
            .collect()
    } else {
        ranked
    };

    let limited: Vec<(String, f32)> = filtered.into_iter().take(top_k).collect();

    // Build result payloads
    let results: Vec<serde_json::Value> = limited
        .iter()
        .filter_map(|(id, score)| {
            engine.get_node(id).map(|node| {
                serde_json::json!({
                    "id": node.id,
                    "name": node.name,
                    "score": score,
                    "content": node.content,
                    "metadata": node.metadata,
                    "type": match &node.label {
                        NodeLabel::Entity { entity_type, .. } => entity_type.clone(),
                        NodeLabel::Workspace { .. } => "Workspace".to_string(),
                        NodeLabel::Scratchpad { .. } => "Scratchpad".to_string(),
                    },
                })
            })
        })
        .collect();

    Ok(serde_json::json!(results))
}

// ─── Reconciliation Handlers ────────────────────────────────────────────────

fn handle_reconcile(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    #[derive(serde::Deserialize)]
    struct ReconcileRequest {
        file_path: String,
        content: Option<String>,
    }

    let request: ReconcileRequest = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid reconcile request: {}", e))
    })?;

    let path = std::path::Path::new(&request.file_path);
    
    let mut lsp_guard = state.lsp.as_ref().map(|l| l.lock().unwrap());
    let lsp_dyn: Option<&mut dyn crate::lsp_adapter::LspAdapter> = match lsp_guard {
        Some(ref mut client) => Some(&mut **client),
        None => None,
    };

    let mut events = {
        let engine = state.engine.read().unwrap();
        crate::reconciler::reconcile_file(path, request.content.as_deref(), lsp_dyn, &engine)
    };

    // Compute embeddings for Entity UpsertNode events before persistence.
    // This ensures embeddings are written to JSONL and loaded into MemoryNode.embedding.
    if let Some(ref embedder) = state.embedder {
        for event in events.iter_mut() {
            if let EventPayload::UpsertNode(ref mut payload) = event.payload {
                if payload.label == "Entity" {
                    let text = build_embedding_text_from_props(&payload.label, &payload.properties);
                    if !text.is_empty() {
                        match embedder.embed(&text) {
                            Ok(vec) => {
                                payload.properties.insert("embedding".to_string(), serde_json::json!(vec));
                            }
                            Err(e) => {
                                eprintln!("Failed to compute embedding for {}: {}", payload.id, e);
                            }
                        }
                    }
                }
            }
        }
    }

    let mut generated_ids = Vec::new();

    // Apply the generated events to storage and memory
    {
        let store = state.store.write().unwrap();
        let mut engine = state.engine.write().unwrap();
        
        for event in events {
            // Append to JSONL
            if let Err(e) = store.append(&event) {
                eprintln!("Failed to append reconciled event: {}", e);
                continue;
            }

            // Apply to memory
            engine.apply_event(&event);

            // Track generated IDs for response
            match &event.payload {
                EventPayload::UpsertNode(payload) => {
                    generated_ids.push(payload.id.clone());
                    
                    // Update BM25 index
                    if let Some(node) = engine.get_node(&payload.id) {
                        let text = crate::rpc::build_bm25_text(node);
                        let mut bm25 = state.bm25.write().unwrap();
                        bm25.remove_document(&payload.id);
                        if !text.is_empty() {
                            bm25.add_document(&payload.id, &text);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(serde_json::json!({
        "status": "ok",
        "upserted_nodes": generated_ids
    }))
}

// ─── Lifecycle Handlers ─────────────────────────────────────────────────────

fn handle_initialize(
    _state: &AppState,
    _params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    // The engine is already initialized at AppState construction.
    // This method can be extended for re-initialization or config changes.
    Ok(serde_json::json!({"status": "ok", "message": "Engine initialized"}))
}

fn handle_shutdown(state: &AppState) -> Result<serde_json::Value, RpcResponse> {
    if let Some(lsp) = &state.lsp {
        let _ = lsp.lock().unwrap().stop();
    }
    // Signal the main loop to exit
    Ok(serde_json::json!({"status": "shutdown"}))
}
