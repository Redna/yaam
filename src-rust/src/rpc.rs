//! JSON-RPC 2.0 method dispatch.
//!
//! Routes incoming JSON-RPC requests to the appropriate handler,
//! wrapping each call in `catch_unwind` to prevent panics from killing the process.

use crate::embedding::EmbeddingModel;
use crate::graph::MemoryEngine;
use crate::query_dsl;
use crate::search::BM25FieldIndex;
use crate::ann_index::AnnIndex;
use crate::storage::EventStore;
use crate::types::*;
use crate::lsp_adapter::{LspAdapter, StdioLspClient};
use crate::language_adapter::get_adapter;
use std::panic;
use std::sync::{Arc, RwLock, Mutex};
use std::collections::HashMap;
use std::path::PathBuf;

/// Shared application state passed to all RPC handlers.
pub struct AppState {
    pub engine: Arc<RwLock<MemoryEngine>>,
    pub store: Arc<RwLock<EventStore>>,
    pub bm25: Arc<RwLock<BM25FieldIndex>>,
    /// ANN index for dense vector search (Spec #1).
    pub ann: Arc<RwLock<AnnIndex>>,
    pub embedder: Option<Arc<EmbeddingModel>>,
    /// Cache of embedding text hashes to skip ONNX inference for unchanged entities (Spec #3).
    pub embedding_cache: Arc<RwLock<EmbeddingCache>>,
    /// Lazily-started LSP clients, keyed by language_id (e.g. "typescript", "python").
    /// A client is only created the first time a file of that language is reconciled.
    pub lsp_clients: Arc<RwLock<HashMap<String, Arc<Mutex<StdioLspClient>>>>>,
    /// Project root directory, used as the LSP `rootUri` when starting servers.
    pub project_root: PathBuf,
    /// Channel sender for background LSP reference resolution (Spec #2).
    /// Pending references are sent here and processed by a background worker
    /// spawned in main.rs, so reconcile can return immediately without
    /// blocking on LSP round-trips.
    pub ref_queue: Option<tokio::sync::mpsc::UnboundedSender<crate::reconciler::PendingReference>>,
}

impl AppState {
    pub fn new(events_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::path::Path::new(events_path);
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        let store = EventStore::new(dir)?;
        let mut engine = MemoryEngine::default();
        
        store.replay(|event| {
            engine.apply_event(&event);
        })?;

        let mut bm25 = BM25FieldIndex::new();
        let mut ann = AnnIndex::new();
        let mut embedding_cache = EmbeddingCache::new();

        // Single pass over all nodes to build indices (BM25, ANN, Embedding Cache)
        for node in engine.all_nodes() {
            // 1. BM25
            let fields = build_bm25_fields(node);
            if !fields.is_empty() {
                bm25.add_document(&node.id, &fields);
            }

            // 2. ANN and Embedding Cache
            if let Some(ref embeddings) = node.embedding {
                if !embeddings.is_empty() {
                    ann.add(&node.id, embeddings);
                    
                    let text = build_embedding_text_from_node(node);
                    if !text.is_empty() {
                        let hash = EmbeddingCache::hash_text(&text);
                        embedding_cache.set_hash(&node.id, hash);
                    }
                }
            }
        }

        if ann.node_count() > 0 {
            eprintln!("[YAAM] ANN index primed with {} nodes ({} vectors)", ann.node_count(), ann.vector_count());
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

        if embedding_cache.len() > 0 {
            eprintln!("[YAAM] Embedding cache primed with {} entries", embedding_cache.len());
        }

        // LSP clients are lazily started on first reconcile of each language.
        // No LSP servers are spawned at daemon startup.

        Ok(Self {
            engine: Arc::new(RwLock::new(engine)),
            store: Arc::new(RwLock::new(store)),
            bm25: Arc::new(RwLock::new(bm25)),
            ann: Arc::new(RwLock::new(ann)),
            embedder,
            embedding_cache: Arc::new(RwLock::new(embedding_cache)),
            lsp_clients: Arc::new(RwLock::new(HashMap::new())),
            project_root: dir.to_path_buf(),
            ref_queue: None,
        })
    }
}

/// Extract the line number from a node's metadata JSON.
///
/// Code entities store `{"line": N}` in metadata. Sections store
/// `{"level": N, "start_line": N, "end_line": N}`. Returns `None`
/// if metadata is empty or doesn't contain a line field.
fn extract_line(node: &MemoryNode) -> Option<usize> {
    if node.metadata.is_empty() {
        return None;
    }
    let meta = serde_json::from_str::<serde_json::Value>(&node.metadata).ok()?;
    // Code entities: {"line": N}
    if let Some(line) = meta.get("line").and_then(|v| v.as_u64()) {
        return Some(line as usize);
    }
    // Sections: {"start_line": N}
    if let Some(line) = meta.get("start_line").and_then(|v| v.as_u64()) {
        return Some(line as usize);
    }
    None
}

/// Truncate content to approximately `max_words` words, appending "..." if truncated.
fn preview_content(content: &str, max_words: usize) -> String {
    let words: Vec<&str> = content.split_whitespace().collect();
    if words.len() <= max_words {
        return content.to_string();
    }
    let truncated = words[..max_words].join(" ");
    format!("{}...", truncated)
}

/// Apply the retrieval mode to a node's content, returning the (possibly truncated or empty) content string.
fn apply_retrieval_mode(content: &str, mode: RetrievalMode) -> String {
    match mode {
        RetrievalMode::Name => String::new(),
        RetrievalMode::Preview => preview_content(content, 100),
        RetrievalMode::Full => content.to_string(),
    }
}

/// Build the per-field text map for BM25 indexing from a node's relevant fields.
///
/// Returns a `HashMap` with keys `"name"`, `"content"`, and `"doc"` mapping
/// to the text to index for each field. Only non-empty fields are included.
/// The caller (BM25FieldIndex) skips missing fields.
///
/// # Field assignment by entity type
///
/// | Entity Type | name | content | doc |
/// |-------------|------|---------|-----|
/// | Function/Class | entity name | full source text | docComment from metadata |
/// | Section | entity name | heading + section body | — |
/// | File | entity name | — | docComment from metadata |
/// | Workspace | workspace name | description | — |
/// | Scratchpad | scratchpad label | scratchpad content | — |
fn build_bm25_fields(node: &MemoryNode) -> HashMap<String, String> {
    let mut fields = HashMap::new();

    // Name field — always present
    if !node.name.is_empty() {
        fields.insert("name".to_string(), node.name.clone());
    }

    match &node.label {
        NodeLabel::Entity { entity_type, .. } => {
            if entity_type == "Section" {
                // Index heading + full section body for markdown sections
                if !node.content.is_empty() {
                    fields.insert("content".to_string(), node.content.clone());
                }
            } else if entity_type == "Function" || entity_type == "Class" {
                // Index full source text for code entities
                if !node.content.is_empty() {
                    fields.insert("content".to_string(), node.content.clone());
                }
                // Also index docComment from metadata if present
                if !node.metadata.is_empty() {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
                        if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
                            fields.insert("doc".to_string(), doc.to_string());
                        }
                    }
                }
            } else {
                // File entities and other types: index docComment from metadata if present
                if !node.metadata.is_empty() {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
                        if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
                            fields.insert("doc".to_string(), doc.to_string());
                        }
                    }
                }
            }
        }
        NodeLabel::Workspace { description, .. } => {
            fields.insert("content".to_string(), description.clone());
        }
        NodeLabel::Scratchpad { .. } => {
            if !node.content.is_empty() {
                fields.insert("content".to_string(), node.content.clone());
            }
        }
    }

    fields
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
            let entity_type = props.get("entity_type").and_then(|v| v.as_str()).unwrap_or("");
            if entity_type == "Section" {
                // For markdown sections, embed heading + full section body.
                // Long text will be chunked by embed_chunked.
                let content = props.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if content.is_empty() {
                    name.to_string()
                } else {
                    format!("{} {}", name, content)
                }
            } else if entity_type == "Function" || entity_type == "Class" {
                // For code entities, embed name + full source text.
                // Long source text will be chunked by embed_chunked.
                let content = props.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if content.is_empty() {
                    name.to_string()
                } else {
                    format!("{} {}", name, content)
                }
            } else {
                // For File entities and other types, embed the name plus any docComment from metadata.
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
        }
        _ => name.to_string(),
    }
}

// ─── Embedding Cache (Spec #3) ──────────────────────────────────────────────

/// Cache of SHA-256 hashes of embedding text, keyed by entity ID.
///
/// Used to skip ONNX inference when an entity's embedding text hasn't
/// changed since the last embedding was computed. On a cache hit, the
/// existing embedding is copied from the graph instead of re-running the
/// ONNX model.
#[derive(Debug, Clone, Default)]
pub struct EmbeddingCache {
    /// Maps entity ID → SHA-256 hash of the embedding text.
    hashes: HashMap<String, [u8; 32]>,
}

impl EmbeddingCache {
    pub fn new() -> Self {
        Self { hashes: HashMap::new() }
    }

    /// Compute SHA-256 hash of the given text.
    fn hash_text(text: &str) -> [u8; 32] {
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher.finalize().into()
    }

    /// Check if the embedding text for `id` has changed since the last embedding.
    /// Returns `Some(hash)` if the text is new or changed (caller should embed).
    /// Returns `None` if the hash matches the cached value (caller should skip).
    pub fn check_and_update(&mut self, id: &str, text: &str) -> CacheResult {
        let hash = Self::hash_text(text);
        if let Some(existing) = self.hashes.get(id) {
            if *existing == hash {
                return CacheResult::Unchanged;
            }
        }
        CacheResult::Changed(hash)
    }

    /// Record the hash for an entity after embedding.
    pub fn set_hash(&mut self, id: &str, hash: [u8; 32]) {
        self.hashes.insert(id.to_string(), hash);
    }

    /// Remove an entity from the cache (on deletion).
    pub fn remove(&mut self, id: &str) {
        self.hashes.remove(id);
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }
}

/// Result of an embedding cache check.
pub enum CacheResult {
    /// The text hash matches the cached value — skip embedding, reuse existing.
    Unchanged,
    /// The text is new or changed — embed and store the new hash.
    Changed([u8; 32]),
}

/// Build the embedding text from a `MemoryNode`'s fields.
///
/// This is the inverse of `build_embedding_text_from_props` — it reconstructs
/// the text from the node's name, content, and metadata fields using the same
/// formatting logic. Used at startup to prime the `EmbeddingCache`.
fn build_embedding_text_from_node(node: &MemoryNode) -> String {
    match &node.label {
        NodeLabel::Workspace { description, .. } => {
            if node.name.is_empty() {
                description.clone()
            } else {
                format!("{} {}", node.name, description)
            }
        }
        NodeLabel::Scratchpad { .. } => {
            node.content.clone()
        }
        NodeLabel::Entity { entity_type, .. } => {
            match entity_type.as_str() {
                "Section" => {
                    if node.content.is_empty() {
                        node.name.clone()
                    } else {
                        format!("{} {}", node.name, node.content)
                    }
                }
                "Function" | "Class" => {
                    if node.content.is_empty() {
                        node.name.clone()
                    } else {
                        format!("{} {}", node.name, node.content)
                    }
                }
                _ => {
                    // File entities and other types: name + docComment from metadata
                    let doc = if !node.metadata.is_empty() {
                        serde_json::from_str::<serde_json::Value>(&node.metadata)
                            .ok()
                            .and_then(|m| m.get("docComment").and_then(|v| v.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    if doc.is_empty() {
                        node.name.clone()
                    } else {
                        format!("{} {}", node.name, doc)
                    }
                }
            }
        }
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
            "compact" => handle_compact(state_ref),
            "query" => handle_query(state_ref, &request.params),
            "search" => handle_search(state_ref, &request.params),

            // ─── Reconciliation ───────────────────────────────────────────
            "reconcile" => handle_reconcile(state_ref, &request.params),

            // ─── Language Registry ──────────────────────────────────────────
            "languages.list" => handle_list_languages(state_ref),

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

    // Check system memory before doing heavy ONNX embeddings
    let mut has_mem = true;
    #[cfg(target_os = "linux")]
    {
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            for line in meminfo.lines() {
                if line.starts_with("MemAvailable:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(kb) = parts[1].parse::<u64>() {
                            if kb / 1024 < 300 { // Less than 300MB available
                                has_mem = false;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    // Compute embedding for all node types (Workspace, Scratchpad, Entity)
    // Uses embedding cache to skip ONNX inference when text hasn't changed (Spec #3).
    let mut payload = payload;
    if has_mem {
        if let Some(ref embedder) = state.embedder {
            let text_to_embed = build_embedding_text_from_props(&payload.label, &payload.properties);
        if !text_to_embed.is_empty() {
            let mut cache = state.embedding_cache.write().unwrap();
            match cache.check_and_update(&payload.id, &text_to_embed) {
                CacheResult::Unchanged => {
                    // Hash matches — reuse existing embedding from the graph
                    let engine = state.engine.read().unwrap();
                    if let Some(node) = engine.get_node(&payload.id) {
                        if let Some(ref existing_embedding) = node.embedding {
                            payload.properties.insert(
                                "embedding".to_string(),
                                serde_json::to_value(existing_embedding).unwrap_or(serde_json::json!(null)),
                            );
                        }
                    }
                }
                CacheResult::Changed(hash) => {
                    // Text is new or changed — compute embedding
                    match embedder.embed_chunked(&text_to_embed, 400, 50) {
                        Ok(vectors) => {
                            payload.properties.insert("embedding".to_string(), serde_json::json!(vectors));
                            cache.set_hash(&payload.id, hash);
                        }
                        Err(e) => {
                            eprintln!("Failed to compute embedding: {}", e);
                        }
                    }
                }
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
            let fields = build_bm25_fields(node);
            let mut bm25 = state.bm25.write().unwrap();
            if !fields.is_empty() {
                bm25.add_document(&payload.id, &fields);
            }

            // Update ANN index (Spec #1)
            if let Some(ref embeddings) = node.embedding {
                if !embeddings.is_empty() {
                    let mut ann = state.ann.write().unwrap();
                    ann.add(&payload.id, embeddings);
                }
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

    // Update ANN index (Spec #1)
    {
        let mut ann = state.ann.write().unwrap();
        ann.remove(&payload.id);
    }

    // Update embedding cache (Spec #3)
    {
        let mut cache = state.embedding_cache.write().unwrap();
        cache.remove(&payload.id);
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

fn handle_compact(state: &AppState) -> Result<serde_json::Value, RpcResponse> {
    let (archive_events, new_state_events) = {
        let mut engine = state.engine.write().unwrap();
        // Prune workspaces closed > 30 days ago (30 * 24 * 60 * 60 = 2592000 seconds)
        let archive_events = engine.prune_old_workspaces(2592000);
        let new_state_events = engine.synthesize_current_state();
        (archive_events, new_state_events)
    };

    let store = state.store.write().unwrap();
    if let Err(e) = store.append_to_archive(&archive_events) {
        return Err(RpcResponse::error(None, RPC_INTERNAL_ERROR, format!("Failed to archive events: {}", e)));
    }
    
    if let Err(e) = store.rewrite(&new_state_events) {
        return Err(RpcResponse::error(None, RPC_INTERNAL_ERROR, format!("Failed to rewrite events log: {}", e)));
    }

    // Rebuild BM25 and Cache to reflect dropped nodes
    // Actually prune_old_workspaces already deleted from `engine`, but we should
    // ideally also remove them from bm25 and cache. Given it's a cold operation,
    // we could just leave them (cache will naturally miss on deleted ids).
    
    Ok(serde_json::json!({
        "status": "ok",
        "archived_events_count": archive_events.len(),
        "compacted_events_count": new_state_events.len()
    }))
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

/// Derive a category label from a file path.
///
/// Heuristic classification:
/// - `library` — paths containing known dependency directories
///   (node_modules, .venv, site-packages, dist, build, target, etc.)
/// - `module` — everything else (the project's own source code)
fn derive_category(path: &str) -> String {
    const LIBRARY_MARKERS: &[&str] = &[
        "node_modules/",
        ".venv/",
        "site-packages/",
        "/dist/",
        "/build/",
        "/target/",
        ".eggs/",
        ".npm/",
        ".cache/",
    ];
    if LIBRARY_MARKERS.iter().any(|marker| path.contains(marker)) {
        "library".to_string()
    } else {
        "module".to_string()
    }
}

/// Extract the entity type string from a NodeLabel.
fn entity_type_string(label: &NodeLabel) -> String {
    match label {
        NodeLabel::Entity { entity_type, .. } => entity_type.clone(),
        NodeLabel::Workspace { .. } => "Workspace".to_string(),
        NodeLabel::Scratchpad { .. } => "Scratchpad".to_string(),
    }
}

/// Resolve graph relationships for the top-N search results.
///
/// For each of the top `resolve_top_k` results, query the graph engine
/// for forward (outbound) and/or reverse (inbound) edges, filter by the
/// requested relationship types, and return compact `NeighborNode` summaries.
fn resolve_traversals(
    engine: &MemoryEngine,
    ranked: &[(String, f32)],
    traverse: &SearchTraverseClause,
) -> HashMap<String, SearchTraversal> {
    let resolve_count = traverse.resolve_top_k.min(ranked.len());
    let relationships: Option<&[String]> = traverse.relationship.as_deref();
    let mut result = HashMap::new();

    for (id, _) in ranked.iter().take(resolve_count) {
        let mut neighbors = Vec::new();

        // Outbound edges (this entity → others)
        if traverse.direction == "outbound" || traverse.direction == "both" {
            for edge in engine.get_forward_edges(id) {
                if let Some(rels) = relationships {
                    if !rels.iter().any(|r| r == &edge.relationship) {
                        continue;
                    }
                }
                if let Some(target) = engine.get_node(&edge.to_id) {
                    neighbors.push(NeighborNode {
                        id: target.id.clone(),
                        name: target.name.clone(),
                        entity_type: entity_type_string(&target.label),
                        relationship: edge.relationship.clone(),
                        direction: "outbound".to_string(),
                    });
                }
            }
        }

        // Inbound edges (others → this entity)
        if traverse.direction == "inbound" || traverse.direction == "both" {
            for edge in engine.get_reverse_edges(id) {
                if let Some(rels) = relationships {
                    if !rels.iter().any(|r| r == &edge.relationship) {
                        continue;
                    }
                }
                if let Some(source) = engine.get_node(&edge.from_id) {
                    neighbors.push(NeighborNode {
                        id: source.id.clone(),
                        name: source.name.clone(),
                        entity_type: entity_type_string(&source.label),
                        relationship: edge.relationship.clone(),
                        direction: "inbound".to_string(),
                    });
                }
            }
        }

        result.insert(id.clone(), SearchTraversal {
            entity_id: id.clone(),
            neighbors,
        });
    }

    result
}

/// Split content into segments for snippet extraction.
///
/// For prose (markdown sections), splits on sentence boundaries.
/// For code (source text), splits on newlines.
fn split_for_snippet(content: &str) -> Vec<String> {
    if content.contains("\n\n") {
        // Prose: split into sentences
        let mut segments = Vec::new();
        for paragraph in content.split("\n\n") {
            let mut current = String::new();
            for c in paragraph.chars() {
                current.push(c);
                if c == '.' || c == '!' || c == '?' {
                    let trimmed = current.trim();
                    if !trimmed.is_empty() {
                        segments.push(trimmed.to_string());
                    }
                    current.clear();
                }
            }
            if !current.trim().is_empty() {
                segments.push(current.trim().to_string());
            }
        }
        segments
    } else {
        // Code: split into lines
        content
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// Extract the best-matching snippet from `content` for the given `query`.
///
/// Scores each segment by query token overlap, then expands outward from
/// the best segment to approximately `max_tokens` tokens.
fn extract_snippet(content: &str, query: &str, max_tokens: usize) -> String {
    if content.is_empty() {
        return String::new();
    }

    let query_tokens: std::collections::HashSet<String> = crate::search::tokenize(query)
        .into_iter()
        .collect();

    if query_tokens.is_empty() {
        return preview_content(content, max_tokens * 4);
    }

    let segments = split_for_snippet(content);
    if segments.is_empty() {
        return preview_content(content, max_tokens * 4);
    }

    // Score each segment by query token overlap count.
    let mut best_idx = 0;
    let mut best_score = 0;
    for (i, segment) in segments.iter().enumerate() {
        let segment_tokens: std::collections::HashSet<String> =
            crate::search::tokenize(segment).into_iter().collect();
        let overlap = query_tokens.intersection(&segment_tokens).count();
        if overlap > best_score {
            best_score = overlap;
            best_idx = i;
        }
    }

    // Build snippet: expand outward from best segment until max_tokens.
    let mut snippet = String::new();
    let mut token_count = 0;
    let mut left = best_idx;
    let mut right = best_idx;
    let mut expanded = true;

    while expanded && token_count < max_tokens {
        expanded = false;

        // Try expanding right
        if right + 1 < segments.len() {
            let addition_tokens = crate::search::tokenize(&segments[right + 1]).len();
            if token_count + addition_tokens <= max_tokens {
                right += 1;
                token_count += addition_tokens;
                expanded = true;
            }
        }

        // Try expanding left
        if left > 0 {
            let addition_tokens = crate::search::tokenize(&segments[left - 1]).len();
            if token_count + addition_tokens <= max_tokens {
                left -= 1;
                token_count += addition_tokens;
                expanded = true;
            }
        }
    }

    for segment in segments.iter().skip(left).take(right - left + 1) {
        if !snippet.is_empty() {
            snippet.push(' ');
        }
        snippet.push_str(segment.trim());
    }

    if snippet.is_empty() {
        return preview_content(content, max_tokens * 4);
    }

    snippet
}

/// Extract the file path from an entity ID.
/// Entity IDs are formatted as "file_path::name" or "file_path:name".
fn extract_path(id: &str) -> &str {
    if let Some(pos) = id.rfind("::") {
        &id[..pos]
    } else if let Some(pos) = id.rfind(':') {
        &id[..pos]
    } else {
        id
    }
}

/// Compute path similarity between two entity paths.
/// 1.0 if identical, 0.5 if same directory, 0.0 otherwise.
fn path_similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let a_dir = a.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let b_dir = b.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    if a_dir == b_dir && !a_dir.is_empty() {
        return 0.5;
    }
    0.0
}

/// Apply Maximal Marginal Relevance re-ranking to search results.
///
/// Balances relevance score with diversity by penalizing results
/// from the same file as already-selected results.
fn apply_mmr(ranked: &mut Vec<(String, f32)>, lambda: f32) {
    if ranked.len() <= 1 || lambda >= 1.0 {
        return;
    }

    // Normalize scores to [0, 1]
    let max_score = ranked.iter().map(|(_, s)| *s).fold(0.0f32, f32::max).max(1e-9);
    for (_, s) in ranked.iter_mut() {
        *s /= max_score;
    }

    let mut selected: Vec<(String, f32)> = Vec::new();
    let mut remaining: Vec<(String, f32)> = ranked.drain(..).collect();

    while !remaining.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f32::NEG_INFINITY;

        for (i, (id, rel)) in remaining.iter().enumerate() {
            let candidate_path = extract_path(id);
            let max_sim = selected
                .iter()
                .map(|(sel_id, _)| path_similarity(candidate_path, extract_path(sel_id)))
                .fold(0.0f32, f32::max);

            let mmr = lambda * rel - (1.0 - lambda) * max_sim;
            if mmr > best_mmr {
                best_mmr = mmr;
                best_idx = i;
            }
        }

        selected.push(remaining.remove(best_idx));
    }

    *ranked = selected;
}

fn handle_search(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    let request: SearchRequest = serde_json::from_value(params.clone()).map_err(|e| {
        RpcResponse::error(None, RPC_INVALID_PARAMS, format!("Invalid search request: {}", e))
    })?;

    let top_k = request.top_k.unwrap_or(10);

    // ── Reciprocal Rank Fusion (RRF) ──────────────────────────────────────
    //
    // Instead of naively adding BM25 * 0.1 + cosine_sim (which is fragile
    // because BM25 scores are unbounded), we use RRF: each retrieval system
    // produces a ranked list, and we fuse them by summing reciprocal ranks:
    //
    //   rrf_score(d) = Σ  1 / (k + rank_i(d))
    //
    // where k = 60 (the standard constant from the original RRF paper) and
    // rank_i(d) is the 1-based rank of document d in system i's result list.
    //
    // RRF is robust because it only depends on rank position, not on the
    // magnitude or distribution of raw scores — no normalization needed.
    const RRF_K: f32 = 60.0;
    let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    // 1. BM25 keyword search (field-level: name, content, doc)
    {
        let bm25 = state.bm25.read().unwrap();
        // Fetch a larger candidate pool for RRF — rank position matters,
        // not score magnitude, so more candidates improve fusion quality.
        let bm25_results = bm25.search(&request.text, top_k.saturating_mul(5).max(50));
        for (rank, (id, _)) in bm25_results.iter().enumerate() {
            let rrf_score = 1.0 / (RRF_K + (rank + 1) as f32);
            *scores.entry(id.clone()).or_insert(0.0) += rrf_score;
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

            // Dense semantic search via ANN index (Spec #1)
            // Replaces the O(n) linear scan over all graph nodes with an indexed lookup.
            // The ANN index stores flattened chunk vectors with composite keys.
            // We retrieve top candidates, group by node_id (taking max sim across chunks),
            // and apply temporal decay for scratchpads.
            let pool = top_k.saturating_mul(5).max(50);
            let ann = state.ann.read().unwrap();
            let ann_results = ann.search(&query_embedding, pool);

            // Group by node_id, take max similarity across chunks
            let mut sem_scores: HashMap<String, f32> = HashMap::new();
            for (ann_key, sim) in &ann_results {
                if let Some((node_id, _chunk_idx)) = ann.resolve_key(ann_key) {
                    let current = sem_scores.get(node_id).copied().unwrap_or(f32::NEG_INFINITY);
                    sem_scores.insert(node_id.clone(), current.max(*sim));
                }
            }

            // Apply temporal decay for scratchpads and build ranked list
            let mut sem_ranked: Vec<(String, f32)> = sem_scores
                .into_iter()
                .map(|(id, mut sim)| {
                    if let Some(node) = engine.get_node(&id) {
                        if let NodeLabel::Scratchpad { created_at } = node.label {
                            let decay = crate::embedding::decay_weight(created_at, current_time);
                            sim *= decay;
                        }
                    }
                    (id, sim)
                })
                .collect();

            // Sort by similarity descending to produce ranked list for RRF
            sem_ranked.sort_by(|a, b| {
                b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            });

            // Contribute RRF scores from semantic ranking
            for (rank, (id, _)) in sem_ranked.iter().enumerate() {
                let rrf_score = 1.0 / (RRF_K + (rank + 1) as f32);
                *scores.entry(id.clone()).or_insert(0.0) += rrf_score;
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

    // Apply path and entity-type filters.
    let entity_types = request.entity_types.as_deref();
    let include_paths = request.include_paths.as_deref();
    let exclude_paths = request.exclude_paths.as_deref();

    let filtered: Vec<(String, f32)> = filtered
        .into_iter()
        .filter(|(id, _)| {
            // entity_types filter: check the node's entity_type against the allowed list.
            if let Some(allowed_types) = entity_types {
                if let Some(node) = engine.get_node(id) {
                    let node_type = match &node.label {
                        NodeLabel::Entity { entity_type, .. } => entity_type.as_str(),
                        NodeLabel::Workspace { .. } => "Workspace",
                        NodeLabel::Scratchpad { .. } => "Scratchpad",
                    };
                    if !allowed_types.iter().any(|t| t == node_type) {
                        return false;
                    }
                }
            }

            // include_paths filter: ID must start with at least one prefix.
            if let Some(prefixes) = include_paths {
                if !prefixes.iter().any(|p| id.starts_with(p)) {
                    return false;
                }
            }

            // exclude_paths filter: ID must not start with any prefix.
            if let Some(prefixes) = exclude_paths {
                if prefixes.iter().any(|p| id.starts_with(p)) {
                    return false;
                }
            }

            true
        })
        .collect();

    let mut limited: Vec<(String, f32)> = filtered.into_iter().take(top_k).collect();

    // Apply MMR re-ranking if requested
    if let Some(lambda) = request.diversity_lambda {
        if (0.0..1.0).contains(&lambda) {
            apply_mmr(&mut limited, lambda);
        }
    }

    // Resolve retrieval mode (default: Full)
    let retrieval = request.retrieval.unwrap_or_default();

    // Determine whether to use the structured response shape
    let has_traverse = request.traverse.is_some();
    let has_snippet = request.snippet.is_some();
    let structured_response = has_traverse || has_snippet;

    // Resolve graph traversals for top-N results if requested
    let traversals: HashMap<String, SearchTraversal> = if let Some(ref trav) = request.traverse {
        resolve_traversals(&engine, &limited, trav)
    } else {
        HashMap::new()
    };

    // Build result payloads
    let results: Vec<serde_json::Value> = limited
        .iter()
        .filter_map(|(id, score)| {
            engine.get_node(id).map(|node| {
                let (entity_type_str, file_path) = match &node.label {
                    NodeLabel::Entity { entity_type, .. } => {
                        let path = if id.contains(':') {
                            id.splitn(2, ':').next().unwrap_or("")
                        } else {
                            id.as_str()
                        };
                        (entity_type.clone(), Some(path.to_string()))
                    }
                    NodeLabel::Workspace { .. } => ("Workspace".to_string(), None),
                    NodeLabel::Scratchpad { .. } => ("Scratchpad".to_string(), None),
                };

                let category = file_path
                    .as_deref()
                    .map(derive_category)
                    .unwrap_or("workspace".to_string());

                let line = extract_line(node);

                let mut hit = serde_json::json!({
                    "id": node.id,
                    "name": node.name,
                    "score": score,
                    "line": line,
                    "content": apply_retrieval_mode(&node.content, retrieval),
                    "type": entity_type_str,
                    "path": file_path,
                    "category": category,
                });

                // Add snippet if requested
                if has_snippet {
                    hit["snippet"] = serde_json::json!(
                        extract_snippet(&node.content, &request.text, 64)
                    );
                }

                // Add traversal data if available for this result
                if let Some(trav) = traversals.get(id) {
                    hit["traversal"] = serde_json::json!(trav);
                }

                hit
            })
        })
        .collect();

    if structured_response {
        Ok(serde_json::json!({ "results": results }))
    } else {
        Ok(serde_json::json!(results))
    }
}

// ─── Reconciliation Handlers ────────────────────────────────────────────────

/// Lazily obtain an LSP client for the language of the given file path.
///
/// If a client for the file's language is already running, returns the existing
/// `Arc`.  If not, starts a new LSP server (using the adapter's `lsp_command`),
/// stores it in the shared `lsp_clients` map, and returns it.  If the language
/// is not supported or the LSP server fails to start, returns `None`.
fn get_or_create_lsp(
    state: &AppState,
    file_path: &std::path::Path,
) -> Option<Arc<Mutex<StdioLspClient>>> {
    let adapter = get_adapter(file_path)?;
    let lsp_cmd = adapter.lsp_command()?;
    let lang_id = adapter.language_id().to_string();

    // Fast path: a client for this language is already running.
    {
        let clients = state.lsp_clients.read().unwrap();
        if let Some(c) = clients.get(&lang_id) {
            return Some(c.clone());
        }
    }

    // Slow path: start a new LSP server for this language.
    let args: Vec<&str> = lsp_cmd.args.iter().map(|s| s.as_str()).collect();
    let mut client = StdioLspClient::new(&lsp_cmd.command, &args);
    match client.start(&state.project_root) {
        Ok(_) => {
            eprintln!(
                "Started LSP server for '{}' ({} {})",
                lang_id,
                lsp_cmd.command,
                lsp_cmd.args.join(" ")
            );
        }
        Err(e) => {
            eprintln!(
                "Warning: Failed to start LSP server for '{}' ({} {}). \
                 Cross-file resolution disabled for this language. ({})",
                lang_id, lsp_cmd.command, lsp_cmd.args.join(" "), e
            );
            return None;
        }
    }

    let arc = Arc::new(Mutex::new(client));
    let mut clients = state.lsp_clients.write().unwrap();
    // Double-check: another thread may have started the same language concurrently.
    if let Some(existing) = clients.get(&lang_id) {
        // Another thread won the race; stop our newly-started client to avoid
        // a zombie process and return the existing one.
        let _ = arc.lock().unwrap().stop();
        return Some(existing.clone());
    }
    clients.insert(lang_id, arc.clone());
    Some(arc)
}

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

    // Phase 1 (Spec #2): Parse with tree-sitter, upsert entities, collect references.
    // LSP is NOT passed here — references are collected for background resolution.
    let (mut events, pending_refs) = {
        let engine = state.engine.read().unwrap();
        crate::reconciler::reconcile_file(path, request.content.as_deref(), None, &engine)
    };

    // Compute embeddings for Entity UpsertNode events before persistence.
    // Uses embedding cache to skip ONNX inference for unchanged entities (Spec #3).
    // Batch-embeds all cache misses in a single ONNX forward pass for efficiency.
    if let Some(ref embedder) = state.embedder {
        // Phase A: Check cache for all entities, collect texts that need embedding
        // (cache misses). Reuse existing embeddings for cache hits.
        let mut texts_to_embed: Vec<(usize, String, [u8; 32])> = Vec::new(); // (event_index, text, hash)
        let mut event_indices: Vec<usize> = Vec::new();

        {
            let mut cache = state.embedding_cache.write().unwrap();
            let engine = state.engine.read().unwrap();

            for (idx, event) in events.iter().enumerate() {
                if let EventPayload::UpsertNode(ref payload) = event.payload {
                    if payload.label == "Entity" {
                        let text = build_embedding_text_from_props(&payload.label, &payload.properties);
                        if text.is_empty() { continue; }

                        match cache.check_and_update(&payload.id, &text) {
                            CacheResult::Unchanged => {
                                // Hash matches — reuse existing embedding from the graph
                                if let Some(node) = engine.get_node(&payload.id) {
                                    if let Some(ref existing_embedding) = node.embedding {
                                        // We'll set this in Phase B (need mutable access)
                                        texts_to_embed.push((idx, String::new(), [0u8; 32])); // marker: reuse
                                    }
                                }
                            }
                            CacheResult::Changed(hash) => {
                                // Text is new or changed — needs embedding
                                texts_to_embed.push((idx, text, hash));
                                event_indices.push(idx);
                            }
                        }
                    }
                }
            }
        }

        // Phase B: Batch-embed all cache misses in one ONNX forward pass
        // Separate the texts that actually need embedding (non-empty text)
        let embed_texts: Vec<&str> = texts_to_embed
            .iter()
            .filter(|(_, text, _)| !text.is_empty())
            .map(|(_, text, _)| text.as_str())
            .collect();

        let batch_results = if !embed_texts.is_empty() {
            embedder.embed_batch(&embed_texts)
        } else {
            Ok(Vec::new())
        };

        // Phase C: Assign results back to events
        let mut batch_idx = 0;
        let mut cache = state.embedding_cache.write().unwrap();

        match batch_results {
            Ok(embeddings) => {
                for (event_idx, text, hash) in &texts_to_embed {
                    if text.is_empty() {
                        // Cache hit — reuse existing embedding from graph
                        if let EventPayload::UpsertNode(ref payload) = events[*event_idx].payload {
                            let engine = state.engine.read().unwrap();
                            if let Some(node) = engine.get_node(&payload.id) {
                                if let Some(ref existing) = node.embedding {
                                    if let EventPayload::UpsertNode(ref mut payload) = events[*event_idx].payload {
                                        payload.properties.insert(
                                            "embedding".to_string(),
                                            serde_json::to_value(existing).unwrap_or(serde_json::json!(null)),
                                        );
                                    }
                                }
                            }
                        }
                    } else {
                        // Cache miss — use batched result
                        if batch_idx < embeddings.len() {
                            if let EventPayload::UpsertNode(ref mut payload) = events[*event_idx].payload {
                                // embed_batch returns single vectors; for long text that needs chunking,
                                // we need to use embed_chunked instead. Check if text needs chunking.
                                let token_count = embedder.count_tokens(text);
                                if token_count > 400 {
                                    // Long text — fall back to embed_chunked for this entity
                                    match embedder.embed_chunked(text, 400, 50) {
                                        Ok(vectors) => {
                                            payload.properties.insert("embedding".to_string(), serde_json::json!(vectors));
                                            cache.set_hash(&payload.id, *hash);
                                        }
                                        Err(e) => {
                                            eprintln!("Failed to compute chunked embedding for {}: {}", payload.id, e);
                                        }
                                    }
                                } else {
                                    // Short text — use batched result directly
                                    payload.properties.insert(
                                        "embedding".to_string(),
                                        serde_json::json!(vec![embeddings[batch_idx].clone()]),
                                    );
                                    cache.set_hash(&payload.id, *hash);
                                }
                            }
                        }
                        batch_idx += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("Batch embedding failed, falling back to per-entity: {}", e);
                // Fallback: embed each text individually
                for (event_idx, text, hash) in &texts_to_embed {
                    if !text.is_empty() {
                        if let EventPayload::UpsertNode(ref mut payload) = events[*event_idx].payload {
                            match embedder.embed_chunked(text, 400, 50) {
                                Ok(vectors) => {
                                    payload.properties.insert("embedding".to_string(), serde_json::json!(vectors));
                                    cache.set_hash(&payload.id, *hash);
                                }
                                Err(e2) => {
                                    eprintln!("Failed to compute embedding for {}: {}", payload.id, e2);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut generated_ids = Vec::new();

    // ── Phase 1: Snapshot affected Sections BEFORE applying events ──
    // When code entities are deleted (file re-reconciled), their inbound REFERENCES
    // edges from Section nodes are removed. We snapshot these so we can re-link
    // the Sections to the newly created entities after all events are applied.
    let mut affected_sections: Vec<(String, String)> = Vec::new(); // (section_id, matched_text)
    {
        let engine = state.engine.read().unwrap();
        for event in &events {
            if let EventPayload::DeleteNode(ref payload) = event.payload {
                let inbound = engine.get_reverse_edges(&payload.id);
                for edge in inbound {
                    if edge.relationship == "REFERENCES" {
                        if let Some(matched_text) = edge.properties
                            .get("matched_text")
                            .and_then(|v| v.as_str())
                        {
                            affected_sections.push((
                                edge.from_id.clone(),
                                matched_text.to_string(),
                            ));
                        }
                    }
                }
            }
        }
    }

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
                        let fields = crate::rpc::build_bm25_fields(node);
                        let mut bm25 = state.bm25.write().unwrap();
                        if !fields.is_empty() {
                            bm25.add_document(&payload.id, &fields);
                        }
                    }

                    // Update ANN index (Spec #1)
                    if let Some(node) = engine.get_node(&payload.id) {
                        if let Some(ref embeddings) = node.embedding {
                            if !embeddings.is_empty() {
                                let mut ann = state.ann.write().unwrap();
                                ann.add(&payload.id, embeddings);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ── Phase 2: Re-link affected Sections to new entities ──
    // After all events (deletions + creations) are applied, re-resolve the
    // snapshotted references against the new graph state. This recreates
    // REFERENCES edges from Sections to code entities that were recreated.
    if !affected_sections.is_empty() {
        let engine = state.engine.read().unwrap();
        let mut relink_events = Vec::new();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Build name → node_ids index for current Function and Class entities
        let mut name_index: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for node in engine.all_nodes() {
            if let NodeLabel::Entity { entity_type, .. } = &node.label {
                if entity_type == "Function" || entity_type == "Class" {
                    name_index
                        .entry(node.name.clone())
                        .or_default()
                        .push(node.id.clone());
                }
            }
        }

        for (section_id, matched_text) in &affected_sections {
            // Skip if the Section itself was deleted (e.g., markdown file changed too)
            if engine.get_node(section_id).is_none() {
                continue;
            }

            // Find entities with this name in the current graph
            if let Some(candidates) = name_index.get(matched_text) {
                for target_id in candidates {
                    // Skip if edge already exists (avoid duplicates)
                    let existing = engine.get_forward_edges(section_id);
                    if existing.iter().any(|e| e.to_id == *target_id && e.relationship == "REFERENCES") {
                        continue;
                    }

                    let mut props = std::collections::HashMap::new();
                    props.insert(
                        "match_type".to_string(),
                        serde_json::json!("inline_code"),
                    );
                    props.insert(
                        "matched_text".to_string(),
                        serde_json::json!(matched_text),
                    );
                    props.insert(
                        "relinked".to_string(),
                        serde_json::json!(true),
                    );

                    relink_events.push(Event {
                        version: EVENT_VERSION,
                        timestamp,
                        event_type: EventType::LinkNodes,
                        payload: EventPayload::LinkNodes(LinkNodesPayload {
                            from_id: section_id.clone(),
                            to_id: target_id.clone(),
                            relationship: "REFERENCES".to_string(),
                            properties: props,
                        }),
                    });
                }
            }
        }
        drop(engine);

        // Apply relink events
        if !relink_events.is_empty() {
            let store = state.store.write().unwrap();
            let mut engine = state.engine.write().unwrap();
            for event in &relink_events {
                if let Err(e) = store.append(event) {
                    eprintln!("Failed to append relink event: {}", e);
                    continue;
                }
                engine.apply_event(event);
            }
        }
    }

    // ── Phase 2 (Spec #2): Queue pending references for background LSP resolution ──
    // References are sent to a background worker via tokio channel.
    // The worker resolves them via LSP and applies LinkNodes events asynchronously.
    // This ensures reconcile returns immediately without blocking on LSP round-trips.
    let edges_pending = pending_refs.len();
    if edges_pending > 0 {
        if let Some(ref tx) = state.ref_queue {
            for pref in pending_refs {
                let _ = tx.send(pref);  // Non-blocking — never blocks the caller
            }
        }
    }

    Ok(serde_json::json!({
        "status": "ok",
        "upserted_nodes": generated_ids,
        "edges_pending": edges_pending
    }))
}

// ─── Background LSP Resolution (Spec #2) ────────────────────────────────────

/// Resolve a single pending reference via LSP and apply the resulting edge.
///
/// Called by the background worker in main.rs via `spawn_blocking`.
/// This function is synchronous and blocking — it locks the LSP client,
/// calls `get_definition`, and applies the resulting `LinkNodes` event
/// to storage and the memory graph.
pub fn resolve_reference_sync(state: &AppState, pref: crate::reconciler::PendingReference) {
    use crate::reconciler::PendingReference;
    use crate::lsp_adapter::LspAdapter;

    let path = std::path::Path::new(&pref.source_file);

    // 1. Get or create LSP client for the file's language
    let lsp_arc = match get_or_create_lsp(state, path) {
        Some(c) => c,
        None => return,  // LSP not available for this language
    };

    // 2. Lock the LSP client
    let mut lsp = lsp_arc.lock().unwrap();

    // 3. Notify open (idempotent — LSP servers handle duplicate notifications)
    let _ = lsp.notify_open(&pref.source_file_uri, &pref.content, &pref.language_id);

    // 4. Resolve definition
    let locations = match lsp.get_definition(&pref.source_file_uri, pref.line, pref.col) {
        Ok(locs) => locs,
        Err(_) => return,  // LSP failed — skip this reference
    };
    drop(lsp);  // Release LSP lock as early as possible

    // 5. Create LinkNodes event(s)
    if let Some(loc) = locations.first() {
        let absolute_path = if let Some(stripped) = loc.uri.strip_prefix("file://") {
            stripped.to_string()
        } else {
            loc.uri.clone()
        };
        let target_file_path = std::path::Path::new(&absolute_path)
            .strip_prefix(std::env::current_dir().unwrap_or_default())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(absolute_path);
        let target_id = format!("{}:{}", target_file_path, pref.ref_name);

        let event = Event {
            version: EVENT_VERSION,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            event_type: EventType::LinkNodes,
            payload: EventPayload::LinkNodes(LinkNodesPayload {
                from_id: pref.source_id,
                to_id: target_id,
                relationship: pref.ref_type,
                properties: HashMap::new(),
            }),
        };

        // 6. Persist + apply
        {
            let store = state.store.write().unwrap();
            if let Err(e) = store.append(&event) {
                eprintln!("Failed to append background LSP event: {}", e);
                return;
            }
        }
        let mut engine = state.engine.write().unwrap();
        engine.apply_event(&event);
    }
}

// ─── Lifecycle Handlers ────────────────────────────────────────────────────

fn handle_initialize(
    _state: &AppState,
    _params: &serde_json::Value,
) -> Result<serde_json::Value, RpcResponse> {
    // The engine is already initialized at AppState construction.
    // This method can be extended for re-initialization or config changes.
    Ok(serde_json::json!({"status": "ok", "message": "Engine initialized"}))
}

fn handle_shutdown(state: &AppState) -> Result<serde_json::Value, RpcResponse> {
    // Stop all running LSP servers.
    let clients = state.lsp_clients.read().unwrap();
    for (lang_id, client) in clients.iter() {
        let _ = client.lock().unwrap().stop();
        eprintln!("Stopped LSP server for '{}'", lang_id);
    }
    // Signal the main loop to exit
    Ok(serde_json::json!({"status": "shutdown"}))
}

// ─── Language Registry Handlers ─────────────────────────────────────────────

/// Returns all registered languages with their extensions, LSP command,
/// and whether the LSP server is currently running.
fn handle_list_languages(state: &AppState) -> Result<serde_json::Value, RpcResponse> {
    let languages = crate::language_adapter::list_languages();
    let lsp_clients = state.lsp_clients.read().unwrap();

    let result: Vec<serde_json::Value> = languages
        .iter()
        .map(|lang| {
            let lsp_running = lsp_clients.contains_key(&lang.language_id);
            let lsp_cmd_str = lang.lsp_command.as_ref().map(|c| {
                if c.args.is_empty() {
                    c.command.clone()
                } else {
                    format!("{} {}", c.command, c.args.join(" "))
                }
            });
            serde_json::json!({
                "name": lang.name,
                "extensions": lang.extensions,
                "language_id": lang.language_id,
                "lsp_command": lsp_cmd_str,
                "lsp_running": lsp_running,
            })
        })
        .collect();

    Ok(serde_json::json!({"languages": result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryNode, NodeLabel};

    // ── 7.2 BM25 Indexing ──────────────────────────────────────────────

    fn make_function_node(name: &str, content: &str) -> MemoryNode {
        MemoryNode {
            id: format!("test.rs:{}", name),
            label: NodeLabel::Entity {
                entity_type: "Function".to_string(),
                status: "active".to_string(),
                last_modified: 0,
            },
            name: name.to_string(),
            content: content.to_string(),
            metadata: String::new(),
            embedding: None,
        }
    }

    fn make_class_node(name: &str, content: &str) -> MemoryNode {
        MemoryNode {
            id: format!("test.rs:{}", name),
            label: NodeLabel::Entity {
                entity_type: "Class".to_string(),
                status: "active".to_string(),
                last_modified: 0,
            },
            name: name.to_string(),
            content: content.to_string(),
            metadata: String::new(),
            embedding: None,
        }
    }

    fn make_file_node(name: &str) -> MemoryNode {
        MemoryNode {
            id: name.to_string(),
            label: NodeLabel::Entity {
                entity_type: "File".to_string(),
                status: "active".to_string(),
                last_modified: 0,
            },
            name: name.to_string(),
            content: String::new(),
            metadata: String::new(),
            embedding: None,
        }
    }

    #[test]
    fn test_bm25_fields_function_with_content() {
        let node = make_function_node(
            "embed_chunked",
            "pub fn embed_chunked(&self, text: &str, max_tokens: usize) -> Result<Vec<Vec<f32>>",
        );
        let fields = build_bm25_fields(&node);
        // Name field should contain the function name
        assert_eq!(fields.get("name"), Some(&"embed_chunked".to_string()));
        // Content field should contain the source text
        let content = fields.get("content").expect("content field should exist");
        assert!(content.contains("embed_chunked"));
        assert!(content.contains("max_tokens"));
        assert!(content.contains("text"));
    }

    #[test]
    fn test_bm25_fields_function_empty_content() {
        let node = make_function_node("simple", "");
        let fields = build_bm25_fields(&node);
        // Should have only the name field
        assert_eq!(fields.get("name"), Some(&"simple".to_string()));
        assert!(!fields.contains_key("content"));
    }

    #[test]
    fn test_bm25_fields_class_with_content() {
        let node = make_class_node(
            "EmbeddingModel",
            "pub struct EmbeddingModel { session: Mutex<Session> }",
        );
        let fields = build_bm25_fields(&node);
        assert_eq!(fields.get("name"), Some(&"EmbeddingModel".to_string()));
        let content = fields.get("content").expect("content field should exist");
        assert!(content.contains("EmbeddingModel"));
        assert!(content.contains("session"));
        assert!(content.contains("Mutex"));
    }

    #[test]
    fn test_bm25_fields_file_entity_no_content() {
        let node = make_file_node("test.rs");
        let fields = build_bm25_fields(&node);
        // File entities don't have content; should have only the name field
        assert_eq!(fields.get("name"), Some(&"test.rs".to_string()));
        assert!(!fields.contains_key("content"));
    }

    // ── 7.3 Embedding Text ─────────────────────────────────────────────

    fn make_props(name: &str, entity_type: &str, content: &str) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String(name.to_string()));
        props.insert("entity_type".to_string(), serde_json::Value::String(entity_type.to_string()));
        props.insert("content".to_string(), serde_json::Value::String(content.to_string()));
        props
    }

    #[test]
    fn test_embedding_text_function_with_content() {
        let props = make_props("embed_chunked", "Function", "pub fn embed_chunked(&self, text: &str)");
        let text = build_embedding_text_from_props("Entity", &props);
        assert!(text.starts_with("embed_chunked"));
        assert!(text.contains("pub fn embed_chunked"));
        assert!(text.contains("text"));
    }

    #[test]
    fn test_embedding_text_function_empty_content() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String("simple".to_string()));
        props.insert("entity_type".to_string(), serde_json::Value::String("Function".to_string()));
        props.insert("content".to_string(), serde_json::Value::String(String::new()));
        let text = build_embedding_text_from_props("Entity", &props);
        // Should fall back to just the name
        assert_eq!(text, "simple");
    }

    #[test]
    fn test_embedding_text_class_with_content() {
        let props = make_props("MyClass", "Class", "class MyClass { method() { } }");
        let text = build_embedding_text_from_props("Entity", &props);
        assert!(text.starts_with("MyClass"));
        assert!(text.contains("method"));
    }

    #[test]
    fn test_embedding_text_section_with_content() {
        let props = make_props("Architecture", "Section", "The system uses a reconciler pattern.");
        let text = build_embedding_text_from_props("Entity", &props);
        assert!(text.starts_with("Architecture"));
        assert!(text.contains("reconciler"));
    }

    #[test]
    fn test_embedding_text_file_entity_no_content() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String("test.rs".to_string()));
        props.insert("entity_type".to_string(), serde_json::Value::String("File".to_string()));
        // No content property
        let text = build_embedding_text_from_props("Entity", &props);
        // File entities with no content and no metadata should return just name
        assert_eq!(text, "test.rs");
    }

    #[test]
    fn test_embedding_text_file_entity_with_doccomment() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String("test.rs".to_string()));
        props.insert("entity_type".to_string(), serde_json::Value::String("File".to_string()));
        let metadata = serde_json::json!({"docComment": "This file handles authentication."}).to_string();
        props.insert("metadata".to_string(), serde_json::Value::String(metadata));
        let text = build_embedding_text_from_props("Entity", &props);
        assert!(text.contains("test.rs"));
        assert!(text.contains("authentication"));
    }

    #[test]
    fn test_embedding_text_workspace() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String("auth-fix".to_string()));
        props.insert("description".to_string(), serde_json::Value::String("Fix the authentication flow".to_string()));
        let text = build_embedding_text_from_props("Workspace", &props);
        assert!(text.contains("auth-fix"));
        assert!(text.contains("authentication"));
    }

    #[test]
    fn test_embedding_text_scratchpad() {
        let mut props = HashMap::new();
        props.insert("content".to_string(), serde_json::Value::String("Decided to use JWT tokens".to_string()));
        let text = build_embedding_text_from_props("Scratchpad", &props);
        assert_eq!(text, "Decided to use JWT tokens");
    }

    // -- Retrieval Mode Tests ---------------------------------------------

    #[test]
    fn test_preview_content_short_text() {
        let result = preview_content("hello world", 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_preview_content_truncates() {
        let long_text = "word ".repeat(150);
        let result = preview_content(long_text.trim(), 100);
        assert!(result.ends_with("..."));
        let word_count = result.split_whitespace().count();
        assert!(word_count <= 101);
    }

    #[test]
    fn test_preview_content_exact_boundary() {
        let text = "a ".repeat(100).trim().to_string();
        let result = preview_content(&text, 100);
        assert!(!result.ends_with("..."));
        assert_eq!(result, text);
    }

    #[test]
    fn test_apply_retrieval_mode_name() {
        let result = apply_retrieval_mode("some code content here", RetrievalMode::Name);
        assert_eq!(result, "");
    }

    #[test]
    fn test_apply_retrieval_mode_preview() {
        let long_text = "word ".repeat(150);
        let result = apply_retrieval_mode(long_text.trim(), RetrievalMode::Preview);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_apply_retrieval_mode_full() {
        let content = "full content here";
        let result = apply_retrieval_mode(content, RetrievalMode::Full);
        assert_eq!(result, content);
    }

    #[test]
    fn test_extract_line_code_entity() {
        let node = make_function_node("test", "content");
        let mut node = node;
        node.metadata = serde_json::json!({"line": 42}).to_string();
        let line = extract_line(&node);
        assert_eq!(line, Some(42));
    }

    #[test]
    fn test_extract_line_section() {
        let node = MemoryNode {
            id: "test.md:Header".to_string(),
            label: NodeLabel::Entity {
                entity_type: "Section".to_string(),
                status: "active".to_string(),
                last_modified: 0,
            },
            name: "Header".to_string(),
            content: "body text".to_string(),
            metadata: serde_json::json!({"level": 2, "start_line": 10, "end_line": 20}).to_string(),
            embedding: None,
        };
        let line = extract_line(&node);
        assert_eq!(line, Some(10));
    }

    #[test]
    fn test_extract_line_empty_metadata() {
        let node = make_function_node("test", "content");
        let line = extract_line(&node);
        assert_eq!(line, None);
    }

    // ── Traversal Resolution Tests ───────────────────────────────────────

    #[test]
    fn test_entity_type_string_entity() {
        let label = NodeLabel::Entity {
            entity_type: "Function".to_string(),
            status: "active".to_string(),
            last_modified: 0,
        };
        assert_eq!(entity_type_string(&label), "Function");
    }

    #[test]
    fn test_entity_type_string_workspace() {
        let label = NodeLabel::Workspace {
            description: "test".to_string(),
            status: "active".to_string(),
            closed_at: None,
        };
        assert_eq!(entity_type_string(&label), "Workspace");
    }

    #[test]
    fn test_entity_type_string_scratchpad() {
        let label = NodeLabel::Scratchpad { created_at: 0 };
        assert_eq!(entity_type_string(&label), "Scratchpad");
    }

    // ── Snippet Extraction Tests ────────────────────────────────────────

    #[test]
    fn test_extract_snippet_empty_content() {
        let result = extract_snippet("", "query", 64);
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_snippet_empty_query() {
        let content = "some content here without any query match";
        let result = extract_snippet(content, "", 64);
        // Empty query falls back to preview
        assert!(!result.is_empty());
    }

    #[test]
    fn test_extract_snippet_code_matches_query() {
        let content = "pub fn embed_chunked(&self, text: &str, max_tokens: usize, overlap_tokens: usize) -> Result<Vec<Vec<f32>>> {\n    let total_tokens = self.count_tokens(text);\n    if total_tokens <= max_tokens {\n        return Ok(vec![self.embed(text)?]);\n    }\n}";
        let result = extract_snippet(content, "max_tokens overlap_tokens", 64);
        // Should find the line containing max_tokens and overlap_tokens
        assert!(result.contains("max_tokens"));
        assert!(result.contains("overlap_tokens"));
    }

    #[test]
    fn test_extract_snippet_prose_matches_query() {
        let content = "This is an introduction paragraph.\n\nThe function handles chunking with max_tokens. It splits on paragraph boundaries.\n\nThe overlap parameter controls context sharing.";
        let result = extract_snippet(content, "chunking max_tokens", 64);
        assert!(result.contains("chunking") || result.contains("max_tokens"));
    }

    #[test]
    fn test_extract_snippet_no_match_returns_preview() {
        let content = "completely unrelated text about nothing";
        let result = extract_snippet(content, "nonexistent_query_terms", 64);
        // No query tokens match any segment, best_score stays 0, best_idx stays 0
        // Falls back to returning content from the first segment
        assert!(!result.is_empty());
    }

    #[test]
    fn test_split_for_snippet_code() {
        let content = "line1\nline2\nline3";
        let segments = split_for_snippet(content);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0], "line1");
    }

    #[test]
    fn test_split_for_snippet_prose() {
        let content = "First sentence. Second sentence.\n\nThird paragraph here.";
        let segments = split_for_snippet(content);
        // Splits on sentence boundaries within paragraphs
        assert!(segments.len() >= 3);
        assert!(segments[0].contains("First sentence"));
    }

    // ── MMR Tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_mmr_no_change_with_lambda_one() {
        let mut ranked = vec![
            ("a.rs::foo".to_string(), 0.9),
            ("a.rs::bar".to_string(), 0.8),
            ("b.rs::baz".to_string(), 0.7),
        ];
        let original = ranked.clone();
        apply_mmr(&mut ranked, 1.0);
        assert_eq!(ranked, original);
    }

    #[test]
    fn test_mmr_single_result_noop() {
        let mut ranked = vec![("a.rs::foo".to_string(), 0.9)];
        apply_mmr(&mut ranked, 0.5);
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn test_mmr_diversifies_same_file() {
        let mut ranked = vec![
            ("a.rs::foo".to_string(), 0.9),
            ("a.rs::bar".to_string(), 0.85),
            ("a.rs::baz".to_string(), 0.8),
            ("b.rs::qux".to_string(), 0.75),
        ];
        apply_mmr(&mut ranked, 0.5);
        // First result is always pure relevance
        assert_eq!(ranked[0].0, "a.rs::foo");
        // With diversity, b.rs::qux should be promoted (different file)
        let top3: Vec<&str> = ranked.iter().take(3).map(|(id, _)| id.as_str()).collect();
        assert!(top3.contains(&"b.rs::qux"), "b.rs::qux should be in top 3 with diversity");
    }

    #[test]
    fn test_mmr_pure_diversity_maximally_spreads() {
        let mut ranked = vec![
            ("a.rs::foo".to_string(), 0.9),
            ("a.rs::bar".to_string(), 0.8),
            ("b.rs::baz".to_string(), 0.7),
        ];
        apply_mmr(&mut ranked, 0.0);
        // First is always pure relevance
        assert_eq!(ranked[0].0, "a.rs::foo");
        // Second should be from a different file (b.rs::baz)
        assert_eq!(ranked[1].0, "b.rs::baz");
    }

    // ── Path Similarity Tests ─────────────────────────────────────────────

    #[test]
    fn test_path_similarity_identical() {
        assert_eq!(path_similarity("src/foo.rs", "src/foo.rs"), 1.0);
    }

    #[test]
    fn test_path_similarity_same_dir() {
        assert_eq!(path_similarity("src/foo.rs", "src/bar.rs"), 0.5);
    }

    #[test]
    fn test_path_similarity_different_dir() {
        assert_eq!(path_similarity("src/foo.rs", "lib/bar.rs"), 0.0);
    }

    #[test]
    fn test_path_similarity_no_dir() {
        assert_eq!(path_similarity("foo.rs", "bar.rs"), 0.0);
    }

    #[test]
    fn test_extract_path_double_colon() {
        assert_eq!(extract_path("src-rust/src/rpc.rs::handle_search"), "src-rust/src/rpc.rs");
    }

    #[test]
    fn test_extract_path_single_colon() {
        assert_eq!(extract_path("README.md:Architecture"), "README.md");
    }

    #[test]
    fn test_extract_path_no_colon() {
        assert_eq!(extract_path("some-workspace"), "some-workspace");
    }

    // ── 7.4 RRF Fusion Tests ───────────────────────────────────────────

    #[test]
    fn test_rrf_fusion_basic() {
        // Simulate RRF fusion: two ranked lists with overlapping results.
        let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
        const RRF_K: f32 = 60.0;

        let bm25_results = vec![
            ("doc_a".to_string(), 10.0),
            ("doc_b".to_string(), 5.0),
            ("doc_c".to_string(), 1.0),
        ];
        for (rank, (id, _)) in bm25_results.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + (rank + 1) as f32);
        }

        let sem_results = vec![
            ("doc_b".to_string(), 0.9),
            ("doc_a".to_string(), 0.8),
            ("doc_d".to_string(), 0.7),
        ];
        for (rank, (id, _)) in sem_results.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + (rank + 1) as f32);
        }

        // doc_a: rank 1 in BM25 + rank 2 in semantic = 1/61 + 1/62
        // doc_b: rank 2 in BM25 + rank 1 in semantic = 1/62 + 1/61
        // Should be equal (both at rank 1 and 2 across lists)
        let score_a = scores.get("doc_a").copied().unwrap_or(0.0);
        let score_b = scores.get("doc_b").copied().unwrap_or(0.0);
        assert!((score_a - score_b).abs() < 1e-10);

        let score_c = scores.get("doc_c").copied().unwrap_or(0.0);
        assert!(score_a > score_c, "doc_a should outrank doc_c");

        // doc_d (only in semantic at rank 3) gets same RRF as doc_c (only in BM25 at rank 3)
        let score_d = scores.get("doc_d").copied().unwrap_or(0.0);
        assert!((score_c - score_d).abs() < 1e-10, "doc_c and doc_d should have equal RRF scores");
    }

    #[test]
    fn test_rrf_score_range() {
        let rrf_max = 1.0 / 61.0;
        let combined_max = 2.0 * rrf_max;
        assert!(combined_max < 0.04, "RRF scores should be small and bounded");
        assert!(combined_max > 0.0, "RRF scores should be positive");
    }

    // ── 7.5 Field-Level BM25 Integration Tests ─────────────────────────

    #[test]
    fn test_field_level_bm25_name_boost() {
        use crate::search::BM25FieldIndex;
        let mut index = BM25FieldIndex::new();

        let mut f1 = HashMap::new();
        f1.insert("name".to_string(), "search".to_string());
        f1.insert("content".to_string(), "performs a linear scan across all nodes".to_string());
        index.add_document("fn1", &f1);

        let mut f2 = HashMap::new();
        f2.insert("name".to_string(), "linearScan".to_string());
        f2.insert("content".to_string(), "search search search search search".to_string());
        index.add_document("fn2", &f2);

        let results = index.search("search", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, "fn1", "name match should outrank repeated content match");
    }

    #[test]
    fn test_field_level_bm25_doc_comment_boost() {
        use crate::search::BM25FieldIndex;
        let mut index = BM25FieldIndex::new();

        // Add filler docs so corpus statistics are realistic (avoids
        // degenerate IDF when a field index has only 1 document).
        for i in 0..5 {
            let mut f = HashMap::new();
            f.insert("name".to_string(), format!("func{}", i));
            f.insert("content".to_string(), format!("does thing number {}", i));
            f.insert("doc".to_string(), "Unrelated documentation".to_string());
            index.add_document(&format!("filler{}", i), &f);
        }

        // fn1: has "search" in doc field (1 of 6 docs in doc index)
        let mut f1 = HashMap::new();
        f1.insert("name".to_string(), "doScan".to_string());
        f1.insert("content".to_string(), "let x = 1; let y = 2;".to_string());
        f1.insert("doc".to_string(), "Searches the index for matching terms".to_string());
        index.add_document("fn1", &f1);

        // fn2: has "search" only in content (1 of 7 docs in content index)
        let mut f2 = HashMap::new();
        f2.insert("name".to_string(), "otherFunc".to_string());
        f2.insert("content".to_string(), "search and replace".to_string());
        index.add_document("fn2", &f2);

        let results = index.search("search", 10);
        assert!(!results.is_empty());
        // With realistic corpus stats, doc match (weight 2.0) should beat
        // single content match (weight 1.0)
        assert_eq!(results[0].0, "fn1", "docComment match should outrank single content match");
    }

    // ── Embedding Cache Tests (Spec #3) ───────────────────────────────

    #[test]
    fn test_embedding_cache_new_is_empty() {
        let cache = EmbeddingCache::new();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_embedding_cache_first_check_is_changed() {
        let mut cache = EmbeddingCache::new();
        let result = cache.check_and_update("entity1", "some text");
        assert!(matches!(result, CacheResult::Changed(_)));
    }

    #[test]
    fn test_embedding_cache_same_text_is_unchanged() {
        let mut cache = EmbeddingCache::new();
        let result = cache.check_and_update("entity1", "some text");
        if let CacheResult::Changed(hash) = result {
            cache.set_hash("entity1", hash);
        }
        // Check again with the same text — should be Unchanged
        let result2 = cache.check_and_update("entity1", "some text");
        assert!(matches!(result2, CacheResult::Unchanged));
    }

    #[test]
    fn test_embedding_cache_different_text_is_changed() {
        let mut cache = EmbeddingCache::new();
        let result = cache.check_and_update("entity1", "some text");
        if let CacheResult::Changed(hash) = result {
            cache.set_hash("entity1", hash);
        }
        // Different text — should be Changed
        let result2 = cache.check_and_update("entity1", "different text");
        assert!(matches!(result2, CacheResult::Changed(_)));
    }

    #[test]
    fn test_embedding_cache_name_change_detected() {
        // Even if body is the same, changing the name changes the embedding text
        // (since embedding text = "{name} {content}")
        let mut cache = EmbeddingCache::new();
        let text1 = "old_name function body here";
        let result = cache.check_and_update("file:old_name", text1);
        if let CacheResult::Changed(hash) = result {
            cache.set_hash("file:old_name", hash);
        }
        // Same body, different name in the text
        let text2 = "new_name function body here";
        let result2 = cache.check_and_update("file:new_name", text2);
        assert!(matches!(result2, CacheResult::Changed(_)));
    }

    #[test]
    fn test_embedding_cache_remove() {
        let mut cache = EmbeddingCache::new();
        let result = cache.check_and_update("entity1", "text");
        if let CacheResult::Changed(hash) = result {
            cache.set_hash("entity1", hash);
        }
        assert_eq!(cache.len(), 1);
        cache.remove("entity1");
        assert_eq!(cache.len(), 0);
        // After removal, the same text should be Changed again
        let result2 = cache.check_and_update("entity1", "text");
        assert!(matches!(result2, CacheResult::Changed(_)));
    }

    #[test]
    fn test_embedding_cache_independent_entities() {
        let mut cache = EmbeddingCache::new();
        let r1 = cache.check_and_update("e1", "text a");
        if let CacheResult::Changed(h) = r1 { cache.set_hash("e1", h); }
        let r2 = cache.check_and_update("e2", "text b");
        if let CacheResult::Changed(h) = r2 { cache.set_hash("e2", h); }
        assert_eq!(cache.len(), 2);
        // Both should be Unchanged on re-check
        assert!(matches!(cache.check_and_update("e1", "text a"), CacheResult::Unchanged));
        assert!(matches!(cache.check_and_update("e2", "text b"), CacheResult::Unchanged));
    }

    #[test]
    fn test_embedding_cache_hash_deterministic() {
        // Same text always produces the same hash
        let h1 = EmbeddingCache::hash_text("hello world");
        let h2 = EmbeddingCache::hash_text("hello world");
        assert_eq!(h1, h2);
        // Different text produces different hash
        let h3 = EmbeddingCache::hash_text("hello earth");
        assert_ne!(h1, h3);
    }

    // ── build_embedding_text_from_node Tests (Spec #3) ─────────────────

    #[test]
    fn test_build_embedding_text_from_node_function() {
        let node = make_function_node("embed_chunked", "pub fn embed_chunked(&self, text: &str)");
        let text = build_embedding_text_from_node(&node);
        assert!(text.starts_with("embed_chunked"));
        assert!(text.contains("pub fn embed_chunked"));
    }

    #[test]
    fn test_build_embedding_text_from_node_function_empty_content() {
        let node = make_function_node("simple", "");
        let text = build_embedding_text_from_node(&node);
        assert_eq!(text, "simple");
    }

    #[test]
    fn test_build_embedding_text_from_node_class() {
        let node = make_class_node("MyClass", "class MyClass { method() {} }");
        let text = build_embedding_text_from_node(&node);
        assert!(text.starts_with("MyClass"));
        assert!(text.contains("method"));
    }

    #[test]
    fn test_build_embedding_text_from_node_file_entity() {
        let node = make_file_node("test.rs");
        let text = build_embedding_text_from_node(&node);
        assert_eq!(text, "test.rs");
    }

    #[test]
    fn test_build_embedding_text_from_node_file_with_doccomment() {
        let mut node = make_file_node("test.rs");
        node.metadata = serde_json::json!({"docComment": "Handles authentication."}).to_string();
        let text = build_embedding_text_from_node(&node);
        assert!(text.contains("test.rs"));
        assert!(text.contains("authentication"));
    }

    #[test]
    fn test_build_embedding_text_from_node_section() {
        let node = MemoryNode {
            id: "doc.md:Architecture".to_string(),
            label: NodeLabel::Entity {
                entity_type: "Section".to_string(),
                status: "active".to_string(),
                last_modified: 0,
            },
            name: "Architecture".to_string(),
            content: "The system uses a reconciler pattern.".to_string(),
            metadata: String::new(),
            embedding: None,
        };
        let text = build_embedding_text_from_node(&node);
        assert!(text.starts_with("Architecture"));
        assert!(text.contains("reconciler"));
    }

    #[test]
    fn test_build_embedding_text_from_node_workspace() {
        let node = MemoryNode {
            id: "ws-1".to_string(),
            label: NodeLabel::Workspace {
                description: "Fix auth flow".to_string(),
                status: "active".to_string(),
                closed_at: None,
            },
            name: "auth-fix".to_string(),
            content: String::new(),
            metadata: String::new(),
            embedding: None,
        };
        let text = build_embedding_text_from_node(&node);
        assert!(text.contains("auth-fix"));
        assert!(text.contains("Fix auth flow"));
    }

    #[test]
    fn test_build_embedding_text_from_node_scratchpad() {
        let node = MemoryNode {
            id: "sp-1".to_string(),
            label: NodeLabel::Scratchpad { created_at: 0 },
            name: "notes".to_string(),
            content: "Decided to use JWT".to_string(),
            metadata: String::new(),
            embedding: None,
        };
        let text = build_embedding_text_from_node(&node);
        assert_eq!(text, "Decided to use JWT");
    }

    #[test]
    fn test_build_embedding_text_from_node_matches_props_for_function() {
        // Verify that build_embedding_text_from_node and build_embedding_text_from_props
        // produce identical output for the same entity — this is critical for cache correctness.
        let name = "embed_chunked";
        let content = "pub fn embed_chunked(&self, text: &str, max_tokens: usize) -> Vec<Vec<f32>>";

        let node = make_function_node(name, content);
        let text_from_node = build_embedding_text_from_node(&node);

        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String(name.to_string()));
        props.insert("entity_type".to_string(), serde_json::Value::String("Function".to_string()));
        props.insert("content".to_string(), serde_json::Value::String(content.to_string()));
        let text_from_props = build_embedding_text_from_props("Entity", &props);

        assert_eq!(text_from_node, text_from_props, "build_embedding_text_from_node must match build_embedding_text_from_props");
    }

    #[test]
    fn test_build_embedding_text_from_node_matches_props_for_file_with_doccomment() {
        let doc_comment = "This file handles authentication.";
        let metadata = serde_json::json!({"docComment": doc_comment}).to_string();

        let node = MemoryNode {
            id: "test.rs".to_string(),
            label: NodeLabel::Entity {
                entity_type: "File".to_string(),
                status: "active".to_string(),
                last_modified: 0,
            },
            name: "test.rs".to_string(),
            content: String::new(),
            metadata: metadata.clone(),
            embedding: None,
        };
        let text_from_node = build_embedding_text_from_node(&node);

        let mut props = HashMap::new();
        props.insert("name".to_string(), serde_json::Value::String("test.rs".to_string()));
        props.insert("entity_type".to_string(), serde_json::Value::String("File".to_string()));
        props.insert("metadata".to_string(), serde_json::Value::String(metadata));
        let text_from_props = build_embedding_text_from_props("Entity", &props);

        assert_eq!(text_from_node, text_from_props, "File entity embedding text must match");
    }
}
