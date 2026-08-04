# YAAM v2 Design Specification

**Goal:** Move YAAM from 7/10 to 9/10 by addressing four high-impact improvements identified during codebase analysis.

**Status:** Specification — not yet implemented. This document defines the target design and validation criteria so implementation can be checked against it later.

---

## Table of Contents

1. [ANN Index for Dense Search](#1-ann-index-for-dense-search)
2. [Background LSP Resolution](#2-background-lsp-resolution)
3. [Embedding Skip for Unchanged Entities](#3-embedding-skip-for-unchanged-entities)
4. [WASM/Native Addon Evaluation](#4-wasmnative-addon-evaluation)

---

## 1. ANN Index for Dense Search

### Problem

In `src-rust/src/rpc.rs` → `handle_search()`, the dense semantic search computes cosine similarity against **every node with an embedding** via a linear scan:

```rust
for node in engine.all_nodes() {
    if let Some(ref embeddings) = node.embedding {
        let best_sim = embeddings.iter()
            .map(|emb| crate::embedding::cosine_similarity(&query_embedding, emb))
            .fold(0.0f32, f32::max);
        sem_scores.push((node.id.clone(), sim));
    }
}
```

This is O(n × d) per query where n = number of embedded nodes and d = embedding dimension (384 for gte-small). At ~636 nodes this is fine. At 10,000+ nodes (a medium codebase) it becomes a latency bottleneck — every search touches every vector.

The BM25 side is already properly indexed via an inverted index (`search.rs` → `BM25Index`). Only the dense side lacks acceleration.

### Design Specification

#### Index Choice: HNSW

Use a **Hierarchical Navigable Small World (HNSW)** graph index. Rationale:

- **Incremental updates**: HNSW supports O(log n) insert and delete without full rebuilds, unlike IVF which requires periodic retraining.
- **No training step**: IVF needs k-means clustering on the corpus; HNSW builds incrementally. This matters because embeddings arrive one at a time as files are reconciled.
- **Good recall at low latency**: HNSW achieves >95% recall at sub-millisecond query times for datasets under 100K vectors.
- **Rust ecosystem**: The `hnsw_rs` crate (or `hora` / `instant-distance`) provides production-quality HNSW with add/remove/search.

#### Multi-Chunk Embedding Handling

Each `MemoryNode` stores `embedding: Option<Vec<Vec<f32>>>` — a list of chunk vectors. The current search takes the **max** similarity across chunks. The ANN index must preserve this semantics.

**Approach:** Flatten chunk vectors into the index with a composite key.

```
ANN index key format: "{node_id}#{chunk_index}"
// e.g. "src/index.ts::reconcile#0", "src/index.ts::reconcile#1"
```

A secondary map `ann_key → (node_id, chunk_index)` resolves results back to nodes. After retrieving top-k ANN hits, group by `node_id`, take the max similarity per node, and proceed with RRF fusion as before.

#### Index Lifecycle

| Event | Action |
|-------|--------|
| `UpsertNode` with embedding | Remove all existing chunks for this `node_id` from the HNSW index (tracked via a `node_id → Vec<ann_key>` map). Insert new chunk vectors with composite keys. |
| `DeleteNode` | Remove all chunk vectors for this `node_id` from HNSW. |
| Daemon startup | Rebuild HNSW from all nodes in `MemoryEngine` that have embeddings. Single-threaded batch insert — acceptable since this runs once. |
| Search | Query HNSW with the query embedding. Retrieve `top_k × 5` candidates (same pool factor as BM25 side). Group by node_id, take max sim. Feed ranked list into RRF. |

#### Data Structures

```rust
pub struct AnnIndex {
    /// The HNSW graph index storing flattened chunk vectors.
    /// Key: composite "{node_id}#{chunk_idx}"
    /// Value: 384-dim f32 vector
    hnsw: HnswIndex,  // from hnsw_rs or equivalent

    /// Reverse map: composite key → (node_id, chunk_index)
    key_to_node: HashMap<String, (String, usize)>,

    /// Forward map: node_id → list of composite keys (for removal on upsert/delete)
    node_to_keys: HashMap<String, Vec<String>>,

    /// Embedding dimension (384 for gte-small)
    dim: usize,
}
```

#### HNSW Parameters

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `M` (max connections per node) | 16 | Standard for datasets < 100K. Balances memory and recall. |
| `ef_construction` | 200 | Higher = better index quality at insert time. Acceptable since inserts are not latency-critical. |
| `ef_search` | `max(50, top_k × 5)` | Dynamic: scales with requested result count to maintain recall. |

#### Module Placement

New file: `src-rust/src/ann_index.rs`

The `AnnIndex` struct is stored alongside `BM25FieldIndex` in `AppState`:

```rust
pub struct AppState {
    pub engine: Arc<RwLock<MemoryEngine>>,
    pub store: Arc<RwLock<EventStore>>,
    pub bm25: Arc<RwLock<BM25FieldIndex>>,
    pub ann: Arc<RwLock<AnnIndex>>,          // NEW
    pub embedder: Option<Arc<EmbeddingModel>>,
    pub lsp_clients: Arc<RwLock<HashMap<String, Arc<Mutex<StdioLspClient>>>>>,
    pub project_root: PathBuf,
}
```

#### Search Path Changes

In `handle_search()`, replace the linear scan with:

```rust
// 2. Dense Semantic Search via HNSW
if let Some(ref embedder) = state.embedder {
    if let Ok(query_embedding) = embedder.embed(&request.text) {
        let ann = state.ann.read().unwrap();
        let pool = top_k.saturating_mul(5).max(50);
        let hnsw_results = ann.search(&query_embedding, pool);

        // Group by node_id, take max similarity across chunks
        let mut sem_scores: HashMap<String, f32> = HashMap::new();
        for (ann_key, distance) in hnsw_results {
            if let Some((node_id, _chunk_idx)) = ann.key_to_node.get(&ann_key) {
                let sim = 1.0 - distance;  // HNSW returns L2 distance; convert to similarity
                // OR: if using cosine distance directly, sim = 1.0 - distance
                let current = sem_scores.get(node_id).copied().unwrap_or(f32::NEG_INFINITY);
                sem_scores.insert(node_id.clone(), current.max(sim));
            }
        }

        // Apply temporal decay for scratchpads
        let mut sem_ranked: Vec<(String, f32)> = sem_scores.into_iter().collect();
        // ... decay logic same as current ...

        // Sort for RRF
        sem_ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Equal));

        for (rank, (id, _)) in sem_ranked.iter().enumerate() {
            let rrf_score = 1.0 / (RRF_K + (rank + 1) as f32);
            *scores.entry(id.clone()).or_insert(0.0) += rrf_score;
        }
    }
}
```

**Note on distance metric:** gte-small embeddings are L2-normalized at embedding time (`embedding.rs` → `embed()`), so cosine similarity = dot product = `1 - 0.5 * L2_distance²`. The HNSW index should use **cosine distance** directly if the library supports it, or we normalize vectors before insertion and use L2 distance (which is equivalent for unit vectors).

### Validation Criteria

1. **Correctness — identical results for small N**: With fewer than 500 embedded nodes, search results must be **identical** to the current linear scan approach (same top-k ordering, same RRF scores). This validates that the HNSW doesn't drop relevant results at small scale.

2. **Correctness — multi-chunk max-sim preserved**: A node with 3 chunk embeddings where chunk #2 is closest to the query must produce the same similarity score as the current linear scan's `max` across chunks.

3. **Latency at scale**: With 5,000 embedded nodes (synthetic test: generate random 384-dim vectors), search latency must be < 5ms (vs. current ~50ms linear scan). Measured as p99.

4. **Incremental update correctness**: After upserting a node whose content changed (producing a different number of chunks), the old chunk vectors must be fully removed from HNSW and only new ones present. Verify by checking `node_to_keys` map size and querying for the old vectors.

5. **Startup rebuild**: After daemon restart, `AnnIndex` must contain exactly the same vectors as `MemoryNode.embedding` fields across all nodes. Verify: `ann.node_to_keys.len() == count of nodes with non-empty embedding`.

6. **Deletion correctness**: After `DeleteNode`, querying HNSW must not return any chunk keys for that node_id.

### Edge Cases

- **Empty embedding list**: Node with `embedding: Some(vec![])` (edge case from failed embedding) — skip insertion, handle gracefully in search.
- **Single-chunk vs multi-chunk**: Most nodes will have 1 chunk. The composite key scheme must not penalize this (no overhead beyond the key format).
- **Concurrent access**: HNSW reads (search) and writes (upsert/delete) happen from different `tokio::task::spawn_blocking` calls. The `Arc<RwLock<AnnIndex>>` must prevent data races. HNSW implementations are typically not thread-safe for concurrent read+write — the `RwLock` is sufficient since writes are serialized.
- **Model not loaded**: If `embedder` is `None` (ONNX model missing), `AnnIndex` stays empty. Search falls back to BM25-only (same as current behavior). This is already handled by the `if let Some(ref embedder)` guard.

---

## 2. Background LSP Resolution

### Problem

In `src-rust/src/rpc.rs` → `handle_reconcile()`, the reconcile handler synchronously calls LSP `textDocument/definition` for **every** call/import reference in the file being reconciled:

```rust
// 7. Resolve references via LSP
if let Some(lsp_client) = lsp {
    let _ = lsp_client.notify_open(&file_uri, content_str, adapter.language_id());
    for rf in references {
        if let Ok(locations) = lsp_client.get_definition(&file_uri, rf.line, rf.col) {
            // ... create LinkNodes events ...
        }
    }
}
```

Each `get_definition` call is a synchronous request-response over stdio with a 30-second timeout (`lsp_adapter.rs` → `LSP_REQUEST_TIMEOUT`). A file with 50 function calls means 50 sequential LSP round-trips. The LSP mutex (`Arc<Mutex<StdioLspClient>>`) serializes all requests for the same language, so concurrent reconciles of same-language files block each other.

This is the root cause of the ~2-minute agent blocking documented in the active `fix-sync-blocking` workspace.

### Design Specification

#### Architecture: Two-Phase Reconcile

Split `handle_reconcile` into two phases:

**Phase 1 (Synchronous — fast, returned to caller immediately):**
- Tree-sitter parsing (`parse_file`) — already fast, ~ms
- Delete old entities for this file
- Upsert file node
- Upsert new entity nodes (Functions, Classes, Sections) with `DECLARED_IN` edges
- Compute embeddings for new/changed entities
- Collect unresolved references into a background queue
- **Return `{ "status": "ok", "upserted_nodes": [...] }` immediately**

**Phase 2 (Background — async, no caller blocking):**
- For each queued reference, call LSP `textDocument/definition`
- Emit `LinkNodes` events (CALLS, IMPORTS) as results arrive
- Apply events to storage + memory graph + BM25 index
- No response to any caller — edges simply appear in the graph when ready

#### Reference Queue

```rust
pub struct PendingReference {
    pub source_file: String,
    pub source_file_uri: String,
    pub source_id: String,        // enclosing function ID or file ID
    pub ref_name: String,
    pub ref_type: String,          // "CALLS" or "IMPORTS"
    pub line: u32,
    pub col: u32,
    pub content: String,           // file content for LSP notify_open
    pub language_id: String,
}
```

Stored in `AppState` as a channel:

```rust
pub struct AppState {
    // ... existing fields ...
    pub ref_queue: tokio::sync::mpsc::UnboundedSender<PendingReference>,
    // The receiver is held by a background worker task spawned at daemon startup.
}
```

#### Background Worker

Spawned in `main.rs` after `AppState` construction:

```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PendingReference>();
state.ref_queue = tx;

// Spawn background LSP resolver worker
let state_clone = state.clone();
tokio::spawn(async move {
    while let Some(pref) = rx.recv().await {
        // Resolve via LSP in a spawn_blocking to avoid blocking the async runtime
        let state_for_task = state_clone.clone();
        tokio::task::spawn_blocking(move || {
            resolve_reference_sync(state_for_task, pref);
        }).await.ok();
    }
});
```

The worker processes references one at a time (per language) but different languages can proceed concurrently. The `lsp_clients` mutex already serializes same-language access.

**Optional enhancement (not required for v2):** Spawn N workers (one per language) to parallelize across languages. For v2, a single worker with the existing mutex is sufficient.

#### `resolve_reference_sync` Function

```rust
fn resolve_reference_sync(state: Arc<AppState>, pref: PendingReference) {
    // 1. Get or create LSP client for the file's language
    let lsp_arc = match get_or_create_lsp(state.as_ref(), Path::new(&pref.source_file)) {
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

    // 5. Create LinkNodes event(s)
    if let Some(loc) = locations.first() {
        let target_file = uri_to_path(&loc.uri);
        let target_id = format!("{}:{}", target_file, pref.ref_name);

        let event = Event {
            version: EVENT_VERSION,
            timestamp: get_timestamp(),
            event_type: EventType::LinkNodes,
            payload: EventPayload::LinkNodes(LinkNodesPayload {
                from_id: pref.source_id,
                to_id: target_id,
                relationship: pref.ref_type,
                properties: HashMap::new(),
            }),
        };

        // 6. Persist + apply
        let store = state.store.write().unwrap();
        store.append(&event);
        drop(store);

        let mut engine = state.engine.write().unwrap();
        engine.apply_event(&event);
    }
}
```

#### Modified `handle_reconcile` Flow

```rust
fn handle_reconcile(state, params) {
    // ... parse request ...

    // Phase 1: Synchronous
    let mut events = {
        let engine = state.engine.read().unwrap();
        crate::reconciler::reconcile_file(path, content, None, &engine)  // NOTE: lsp=None
    };

    // Compute embeddings (see spec #3 for skip optimization)
    // ... existing embedding code ...

    // Apply events (deletions + creations) to storage + memory + BM25
    // ... existing apply logic ...

    // Collect references for background resolution
    let references = extract_references_from_events(&events, &file_uri, content, language_id);
    for pref in references {
        let _ = state.ref_queue.send(pref);  // Non-blocking — never blocks
    }

    // Return immediately — edges will appear asynchronously
    Ok(serde_json::json!({
        "status": "ok",
        "upserted_nodes": generated_ids,
        "edges_pending": references.len(),  // NEW field for transparency
    }))
}
```

#### `reconcile_file` Changes

The `reconcile_file` function in `reconciler.rs` must accept `lsp: None` during Phase 1. It already handles this case — when `lsp` is `None`, it skips reference resolution entirely (step 7 is inside `if let Some(lsp_client) = lsp`). No changes needed to `reconcile_file` itself.

The references are still parsed by tree-sitter (returned in the `ParsedReference` list). We need to capture them before they're discarded. Two approaches:

**Approach A (preferred):** Modify `reconcile_file` to return references alongside events, even when LSP is not provided:

```rust
pub fn reconcile_file(
    file_path: &Path,
    content: Option<&str>,
    lsp: Option<&mut dyn LspAdapter>,
    engine: &MemoryEngine,
) -> (Vec<Event>, Vec<PendingReference>>)
//                                          ^^^^^^^^^^^^^^^^^^^^^^^
//                                          NEW return value
```

When `lsp` is `Some`, references are resolved inline (backward-compatible). When `lsp` is `None`, references are collected into the returned `Vec<PendingReference>` for the caller to queue.

**Approach B (alternative):** Parse the file a second time in `handle_reconcile` to extract references. Simpler but wasteful (double parse). Not recommended.

#### LSP Timeout Handling

The current 30-second per-request LSP timeout (`LSP_REQUEST_TIMEOUT`) is retained. If LSP times out on a background reference, the worker skips that reference and continues to the next. The LSP process is killed and will be restarted by `get_or_create_lsp` on the next reference. No caller is blocked.

#### TypeScript Reconciler Changes

The TypeScript reconciler (`src/reconciler.ts`) already calls `engine.reconcile()` via the RPC client. The new `edges_pending` field in the response is informational — no behavioral change required. The reconciler does not need to wait for edges to resolve.

### Validation Criteria

1. **No blocking — reconcile latency**: Reconcile a file with 50 function calls. Phase 1 must return in < 200ms (tree-sitter parse + entity upsert + embedding). Current behavior: 30s+ (50 × LSP round-trips).

2. **Edges eventually appear**: After Phase 1 returns, wait 5 seconds, then query the graph. All CALLS/IMPORTS edges that the LSP can resolve must be present. Verify by querying `get_forward_edges` on reconciled functions and checking edge count matches reference count (for references the LSP successfully resolves).

3. **No lost references**: The `edges_pending` count in the response must equal the number of `ParsedReference` entries produced by tree-sitter. Any LSP failures reduce the actual edge count but not the pending count — this is expected and acceptable (LSP may not resolve all references).

4. **Backward compatibility — existing callers**: The `engine.reconcile()` RPC method signature is unchanged. The response gains an optional `edges_pending` field. Existing callers that ignore unknown fields are unaffected.

5. **Concurrent reconcile safety**: Two files of the same language reconciled concurrently must not corrupt the LSP client. The `Arc<Mutex<StdioLspClient>>` serializes access. Verify: no panics, no corrupt LSP state, all references eventually resolved.

6. **Graph consistency**: Between Phase 1 completion and Phase 2 completion, the graph has entity nodes but may be missing some CALLS/IMPORTS edges. This is acceptable — search and query operations must handle gracefully (they already do — missing edges simply means fewer results). Verify: search during this window returns entity results without errors.

7. **Section re-linking preserved**: The existing Phase 2 section re-linking logic (`affected_sections` in `handle_reconcile`) must continue to work. It runs synchronously in Phase 1 since it depends on the graph state immediately after entity creation. Verify: markdown sections that reference code entities get their `REFERENCES` edges re-created.

### Edge Cases

- **LSP server not installed**: `get_or_create_lsp` returns `None`. References are silently dropped. No error. Same as current behavior.
- **LSP server crashes mid-resolution**: The `StdioLspClient` timeout kills the process. Next `get_or_create_lsp` call starts a fresh server. References being processed during the crash are lost — acceptable.
- **File deleted between Phase 1 and Phase 2**: LSP may fail to resolve definitions for a deleted file. The `PendingReference` includes the file content, so LSP `notify_open` provides the content even if the file is gone from disk. Edge creation targets may not exist in the graph — the `LinkNodes` event creates an edge to a non-existent node, which is handled gracefully by `MemoryEngine::link_nodes` (it creates the edge regardless; the target node may be created later or remain absent).
- **Daemon shutdown with pending references**: References in the channel are lost. Acceptable — they'll be resolved on the next reconcile of that file (triggered by mtime change detection or full sync).

---

## 3. Embedding Skip for Unchanged Entities

### Problem

In `src-rust/src/rpc.rs` → `handle_reconcile()`, after generating `UpsertNode` events for all declarations in a file, every entity gets re-embedded:

```rust
if let Some(ref embedder) = state.embedder {
    for event in events.iter_mut() {
        if let EventPayload::UpsertNode(ref mut payload) = event.payload {
            if payload.label == "Entity" {
                let text = build_embedding_text_from_props(&payload.label, &payload.properties);
                if !text.is_empty() {
                    match embedder.embed_chunked(&text, 400, 50) {
                        Ok(vectors) => {
                            payload.properties.insert("embedding".to_string(), json!(vectors));
                        }
                        Err(e) => { eprintln!("Failed to compute embedding: {}", e); }
                    }
                }
            }
        }
    }
}
```

The same pattern exists in `handle_upsert_node()`. ONNX inference for `gte-small` takes ~5-15ms per embed call on CPU. A file with 20 functions where only 1 changed still re-embeds all 20. That's ~200-300ms of wasted ONNX inference.

The TypeScript reconciler (`src/reconciler.ts`) already has a file-level content hash check that skips unchanged files entirely. But when a file *does* change, all its entities are re-embedded unconditionally.

### Design Specification

#### Per-Entity Content Hash

Store a hash of the **embedding text** (not the raw source) alongside each entity. Compare on reconcile — if the hash matches, reuse the existing embedding instead of running ONNX.

```rust
// In AppState or a separate EmbeddingCache:
pub struct EmbeddingCache {
    /// Maps entity ID → SHA-256 hash of the embedding text used to generate the current embedding
    hashes: HashMap<String, [u8; 32]>,
}
```

The hash is computed from the same text that `build_embedding_text_from_props` produces. This ensures that if the function name changes but the body doesn't (or vice versa), the hash changes and re-embedding occurs.

#### Modified Embedding Logic

```rust
fn maybe_embed(
    embedder: &EmbeddingModel,
    embedding_cache: &mut EmbeddingCache,
    payload: &mut UpsertNodePayload,
) {
    let text = build_embedding_text_from_props(&payload.label, &payload.properties);
    if text.is_empty() {
        return;
    }

    // Compute hash of the embedding text
    let hash = sha256(&text);

    // Check cache
    if let Some(existing_hash) = embedding_cache.hashes.get(&payload.id) {
        if *existing_hash == hash {
            // Reuse existing embedding from the graph
            // (The caller has access to the engine to retrieve the old embedding)
            return;  // Don't set embedding in properties — it's already in the graph
        }
    }

    // Hash mismatch or new entity — compute embedding
    match embedder.embed_chunked(&text, 400, 50) {
        Ok(vectors) => {
            payload.properties.insert("embedding".to_string(), json!(vectors));
            embedding_cache.hashes.insert(payload.id.clone(), hash);
        }
        Err(e) => {
            eprintln!("Failed to compute embedding for {}: {}", payload.id, e);
        }
    }
}
```

#### Reusing Existing Embeddings

When the hash matches, we skip embedding. But the `UpsertNode` event would not include an `embedding` property, which means `MemoryEngine::upsert_node` would set `embedding: None` (the property is absent). This would **lose** the existing embedding.

Two solutions:

**Solution A (preferred):** Before skipping, retrieve the existing embedding from the engine and copy it into the new payload:

```rust
if *existing_hash == hash {
    // Retrieve existing embedding from the graph
    let engine = state.engine.read().unwrap();
    if let Some(node) = engine.get_node(&payload.id) {
        if let Some(ref existing_embedding) = node.embedding {
            payload.properties.insert(
                "embedding".to_string(),
                serde_json::to_value(existing_embedding).unwrap_or(json!(null)),
            );
        }
    }
    return;
}
```

This ensures the `UpsertNode` event carries forward the existing embedding, maintaining consistency between the JSONL log and the in-memory graph.

**Solution B (alternative):** Modify `MemoryEngine::upsert_node` to preserve the existing embedding when the new payload doesn't include one. This changes the upsert semantics — riskier, affects all code paths. Not recommended.

#### Cache Lifecycle

| Event | Action |
|-------|--------|
| Daemon startup | Rebuild `EmbeddingCache` by computing hashes for all nodes with embeddings. Walk all nodes, compute `build_embedding_text_from_node(node)`, hash it, store in cache. |
| `UpsertNode` with embedding computed | Update `hashes[id] = new_hash`. |
| `UpsertNode` with embedding reused | No cache change needed — hash is already correct. |
| `DeleteNode` | Remove `hashes[id]`. |
| Node upserted without embedding property | If the node previously had an embedding, the cache hash is now stale. Remove it. (This shouldn't happen in normal operation — all code paths that set embeddings also set the hash.) |

#### Startup Rebuild

In `AppState::new()`, after loading events and building the BM25 index:

```rust
let mut embedding_cache = EmbeddingCache::new();
for node in engine.all_nodes() {
    if let Some(ref _embedding) = node.embedding {
        let text = build_embedding_text_from_node(node);
        if !text.is_empty() {
            embedding_cache.hashes.insert(node.id.clone(), sha256(&text));
        }
    }
}
```

This requires a `build_embedding_text_from_node` function that reconstructs the embedding text from a `MemoryNode`. This is the inverse of `build_embedding_text_from_props` — it reconstructs the text from the node's name, content, and metadata fields.

**Note:** The reconstruction won't be byte-identical to the original text (e.g., `format!("{} {}", name, content)` depends on exact spacing). To ensure consistency, the hash computation function must use the **same** formatting logic as `build_embedding_text_from_props`. The cleanest approach is to refactor `build_embedding_text_from_props` into a shared function that both the embedding and hash paths call.

#### `build_embedding_text_from_node` Implementation

```rust
fn build_embedding_text_from_node(node: &MemoryNode) -> String {
    match &node.label {
        NodeLabel::Workspace { description, .. } => {
            if node.name.is_empty() { description.clone() }
            else { format!("{} {}", node.name, description) }
        }
        NodeLabel::Scratchpad { .. } => {
            node.content.clone()
        }
        NodeLabel::Entity { entity_type, .. } => {
            match entity_type.as_str() {
                "Section" => {
                    if node.content.is_empty() { node.name.clone() }
                    else { format!("{} {}", node.name, node.content) }
                }
                "Function" | "Class" => {
                    if node.content.is_empty() { node.name.clone() }
                    else { format!("{} {}", node.name, node.content) }
                }
                _ => {
                    // File entities — reconstruct docComment from metadata
                    let doc = if !node.metadata.is_empty() {
                        serde_json::from_str::<serde_json::Value>(&node.metadata)
                            .ok()
                            .and_then(|m| m.get("docComment").and_then(|v| v.as_str()).map(String::from))
                            .unwrap_or_default()
                    } else { String::new() };
                    if doc.is_empty() { node.name.clone() }
                    else { format!("{} {}", node.name, doc) }
                }
            }
        }
    }
}
```

This mirrors the logic in `build_embedding_text_from_props` exactly, but reads from `MemoryNode` fields instead of raw `HashMap<String, serde_json::Value>` properties.

#### Where to Apply

1. `handle_reconcile()` — the main hot path. This is where most embedding computations happen.
2. `handle_upsert_node()` — for direct node upserts (workspaces, scratchpads). Lower frequency but same principle applies.

### Validation Criteria

1. **Skip correctness — identical embeddings**: When a file is reconciled and a function's content hasn't changed, the embedding stored in the new event must be **byte-identical** to the existing embedding in the graph. Verify by comparing `Vec<Vec<f32>>` for equality.

2. **Re-embed on content change**: When a function's body changes (even by one character), the hash must differ and a new embedding must be computed. Verify by checking that the `embedding` property in the event differs from the previous one.

3. **Re-embed on name change**: When a function's name changes (body unchanged), the embedding text changes (`"{new_name} {body}"` vs `"{old_name} {body}"`), so the hash changes and re-embedding occurs. Verify.

4. **Cache hit rate**: For a typical edit session where a developer changes 1 function in a 20-function file, the embedding skip rate must be ≥ 90% (19/20 skipped). Measure by counting ONNX calls vs total entities reconciled.

5. **Latency improvement**: Reconciling a 20-function file where 1 function changed must show ≥ 70% reduction in embedding-related latency vs. current behavior. Current: ~20 × 10ms = 200ms. Target: ~1 × 10ms = 10ms + hash computation overhead (~0.01ms per hash).

6. **Startup cache rebuild correctness**: After daemon restart, `EmbeddingCache.hashes` must contain entries for exactly the nodes that have non-empty embeddings. Verify: `cache.hashes.len() == count of nodes where embedding.is_some() && !embedding.is_empty()`.

7. **No embedding loss**: After a skip (hash match), the node in the graph must still have its embedding. Verify: `engine.get_node(id).embedding` is `Some` and non-empty after the skip path.

### Edge Cases

- **Entity deleted and recreated with same content**: During reconcile, old entities are deleted (removing hash from cache), then new ones created. The new entity won't find a cache hit (cache was cleared by deletion), so it gets re-embedded. This is correct behavior — the deletion clears the cache entry. To optimize, check the graph for the old embedding *before* deletion. However, this adds complexity for marginal gain. Accept re-embedding in this case for v2.

- **Embedding text is empty**: Node with no name and no content. `build_embedding_text_from_props` returns `""`. The `if !text.is_empty()` guard skips both embedding and hashing. Correct — no embedding, no hash, no cache entry.

- **ONNX model not loaded**: If `embedder` is `None`, the entire embedding block is skipped (existing behavior). The cache is not populated. Correct — no embeddings to cache.

- **Chunking produces different chunk count for same text**: This can't happen — `embed_chunked` is deterministic for the same input text and parameters (400, 50). Same text → same chunks → same vectors. The hash is on the text, not the chunks, so this is safe.

- **Hash collision**: SHA-256 with 16-byte truncation (as used in the TS reconciler) has negligible collision probability. For the embedding cache, use full 32-byte SHA-256 to be safe. Even with 100K entities, collision probability is ~10^-60.

---

## 4. WASM/Native Addon Evaluation

### Problem

YAAM's architecture requires a **separate long-running Rust daemon** (`src-rust/src/main.rs`) spawned by the Node.js client (`src/engine-client.ts`). This introduces:

1. **Process lifecycle complexity**: Port file handshake, idle timeout (10 min), stale daemon detection, reconnection logic. ~100 lines of `engine-client.ts` are dedicated to daemon management.
2. **Failure modes invisible to the agent**: The agent sees a 30-second RPC timeout. It can't introspect why — the daemon may have panicked, the LSP server may have deadlocked, or the ONNX model may have failed to load. The agent can only retry.
3. **Resource overhead**: A separate process has its own memory space, its own tokio runtime, its own LSP server processes. The Node.js process and the Rust daemon both hold the full graph in memory (Node.js via the reconciler's file tracking, Rust via `MemoryEngine`).
4. **Deployment friction**: The Rust binary must be compiled (`cargo build --release`) or pre-built. The agent's environment must have the binary or Rust toolchain. A WASM or native addon would ship as an npm package.

### Design Specification

This section is an **evaluation** rather than a final design. We evaluate three alternatives and recommend one.

#### Option A: NAPI-RS Native Addon (Recommended)

**What:** Compile the Rust engine as a Node.js native addon using [napi-rs](https://napi.rs). The engine runs in-process as a dynamically loaded `.node` file. No separate process, no socket communication, no port file.

**Architecture:**

```
┌─────────────────────────────────────────┐
│           Node.js Process               │
│                                         │
│  ┌──────────────┐  ┌─────────────────┐ │
│  │  MCP Server   │  │  YAAM Engine    │ │
│  │  (index.ts)   │──│  (native .node) │ │
│  │               │  │                  │ │
│  │  Reconciler   │  │  MemoryEngine   │ │
│  │  (TS)         │  │  BM25Index       │ │
│  │               │  │  AnnIndex        │ │
│  │  Engine Client│  │  EmbeddingModel  │ │
│  │  (→ removed)  │  │  LspAdapter       │ │
│  └──────────────┘  └─────────────────┘ │
└─────────────────────────────────────────┘
```

**Changes Required:**

1. **Replace `engine-client.ts`** with direct native calls:
   ```typescript
   import { MemoryEngine, BM25FieldIndex, ... } from './yaam-engine.node';

   const engine = new MemoryEngine();
   const bm25 = new BM25FieldIndex();
   // Direct method calls — no RPC, no socket, no serialization
   ```

2. **Rust side — `napi-rs` bindings:**
   ```rust
   #[napi]
   pub struct YaamEngine {
       engine: MemoryEngine,
       bm25: BM25FieldIndex,
       ann: AnnIndex,
       embedder: Option<EmbeddingModel>,
       // ...
   }

   #[napi]
   impl YaamEngine {
       #[napi(constructor)]
       pub fn new(events_path: String) -> Result<Self> { ... }

       pub fn reconcile(&mut self, file_path: String, content: Option<String>) -> Result<ReconcileResult> { ... }
       pub fn search(&self, request: SearchRequest) -> Result<SearchResponse> { ... }
       pub fn query(&self, dsl: serde_json::Value) -> Result<serde_json::Value> { ... }
       pub fn upsert_node(&mut self, payload: UpsertNodePayload) -> Result<()> { ... }
       // ...
   }
   ```

3. **LSP handling:** LSP servers are spawned as child processes from within the native addon. The `StdioLspClient` remains in Rust. Background resolution (Spec #2) runs on a Rust thread spawned by the addon.

4. **ONNX runtime:** The `ort` crate works in native addons. The model file is downloaded at `npm install` time via a postinstall script, or bundled in the package.

5. **`reconciler.ts` simplification:** The TypeScript reconciler no longer calls `engine.reconcile()` via RPC. It calls `engine.reconcile()` directly as a native method. The `YaamEngineClient` class and all socket/port/reconnection logic are deleted. ~200 lines of `engine-client.ts` are removed.

**Pros:**
- Zero IPC overhead — direct function calls
- No process lifecycle management
- No socket timeouts, reconnection, stale daemon detection
- Single memory space — no duplicate graph state
- Pre-built binaries via `napi-rs` CI (like `@swc/core`, `@biomejs/biome`)
- The agent can catch Rust panics as JS exceptions with stack traces

**Cons:**
- A Rust panic in the native addon crashes the Node.js process (no `catch_unwind` safety net like the daemon's `dispatch` wrapper). Mitigation: wrap all public methods in `catch_unwind` at the napi boundary.
- Native addons must be pre-built for each platform (Linux x64, macOS arm64, etc.). `napi-rs` handles this via its CLI and GitHub Actions templates.
- Debugging is harder — can't attach a Rust debugger to a separate process. Must use logging.
- The ONNX runtime and tree-sitter grammars increase the addon binary size (~20-50MB).

**Effort:** Medium. The Rust code is already well-structured. The main work is:
- Add `napi-rs` dependency and annotations
- Create wrapper types for napi-compatible serialization
- Set up CI for cross-platform builds
- Rewrite `engine-client.ts` as thin native bindings
- Delete daemon lifecycle code in `engine-client.ts` and `main.rs`

#### Option B: WASM Module

**What:** Compile the Rust engine to `wasm32-wasi` and load it in Node.js via `WebAssembly.instantiate()`.

**Architecture:**

```
┌─────────────────────────────────────────┐
│           Node.js Process               │
│                                         │
│  ┌──────────────┐  ┌─────────────────┐ │
│  │  MCP Server   │  │  YAAM WASM      │ │
│  │  (index.ts)   │──│  Module         │ │
│  │               │  │  (.wasm)        │ │
│  └──────────────┘  └─────────────────┘ │
│                                         │
│  WASI runtime (wasi.sh / @wasmer)       │
└─────────────────────────────────────────┘
```

**Pros:**
- Platform-independent single binary
- Sandboxed execution (can't crash the host process)
- Can be loaded dynamically without native compilation

**Cons:**
- **ONNX runtime**: `ort` does not support `wasm32-wasi` target well. The `onnxruntime-wasm` package is a separate build that targets the browser, not WASI. Running ONNX in WASM in Node.js is technically possible but practically fragile and slow.
- **File I/O**: WASI provides filesystem access but with restrictions. The event-sourcing model (append-only JSONL) requires reliable file I/O with locking (`fs2::FileExt::lock_exclusive`). WASI does not support file locking.
- **LSP servers**: Spawning child processes from WASM is not supported in WASI. LSP servers are stdio-based processes — WASM cannot spawn them.
- **Performance**: WASM is 10-30% slower than native for compute-intensive tasks (ONNX inference, tree-sitter parsing).
- **Tree-sitter**: tree-sitter grammars must be compiled into the WASM module (no dynamic loading). Each language grammar adds to the WASM binary size.

**Verdict:** **Not recommended.** The ONNX runtime and LSP server spawning are fundamental requirements that WASM/WASI cannot support well. The effort to work around these limitations exceeds the benefit.

#### Option C: Keep Daemon, Improve Lifecycle

**What:** Keep the separate Rust daemon but improve resilience and observability.

**Improvements:**
- Health check endpoint (ping/pong RPC)
- Structured logging (JSON lines to stderr, captured by Node.js)
- Graceful degradation: if daemon is unresponsive, fall back to BM25-only search
- Process supervision: auto-restart on crash with exponential backoff
- Shared memory for large datasets (mmap the events.jsonl for fast startup)

**Pros:**
- Minimal changes to existing architecture
- Isolation: Rust panics don't crash the agent
- Can run multiple agent sessions against one daemon (already supported)

**Cons:**
- All existing problems remain: IPC overhead, timeout handling, duplicate memory
- Adds complexity without removing the root cause

**Verdict:** **Not recommended for v2.** This is a stopgap, not an improvement. The native addon (Option A) is strictly better for the single-agent use case.

#### Recommendation

**Option A: NAPI-RS Native Addon** is the recommended path for v2.

**Phased approach:**

| Phase | Scope | Risk |
|-------|-------|------|
| Phase 1 | Specs #1-3 (ANN, background LSP, embedding skip) — keep daemon | Low — no architectural change |
| Phase 2 | Create `napi-rs` bindings alongside existing daemon code. Both paths coexist. | Medium — new code, no removal |
| Phase 3 | Switch `engine-client.ts` to native addon. Delete daemon lifecycle code. | Medium — behavioral change |
| Phase 4 | Remove `main.rs` daemon entry point. Keep as dead code for one release. | Low — cleanup |

**This spec covers Phase 1.** Phases 2-4 are documented here for direction but will be specified in detail when Phase 1 is validated.

### Validation Criteria (for Phase 1 — the implemented specs)

These criteria validate that the daemon architecture remains stable while Specs #1-3 are implemented:

1. **Daemon stability**: After implementing Specs #1-3, the daemon must still start, accept connections, and respond to all RPC methods without crashes. No new panics introduced.

2. **RPC compatibility**: All existing RPC methods (`reconcile`, `search`, `query`, `upsert_node`, `link_nodes`, `delete_node`, `delete_edges`) maintain their current request/response schemas. New fields (`edges_pending` in reconcile response) are additive only.

3. **No daemon lifecycle changes**: `engine-client.ts` daemon management code (port file, reconnection, timeout) is unchanged. No new daemon lifecycle states.

### Validation Criteria (for Phase 2-4 — to be validated when implemented)

4. **Native addon parity**: The native addon must produce identical search results, reconcile output, and graph state as the daemon for the same inputs. Verify by running a test suite against both backends and comparing outputs byte-for-byte.

5. **No IPC overhead**: Method call latency (e.g., `engine.search()`) must be < 0.1ms overhead (vs. current ~1-5ms for socket serialization + deserialization). Measured as the difference between the native call and a no-op.

6. **Panic isolation**: A simulated Rust panic in any public method must be caught and returned as a JavaScript `Error` — not crash the Node.js process. Verify by intentionally panicking in a test method.

7. **LSP from native addon**: LSP servers spawned from the native addon must start, resolve definitions, and shut down correctly. Verify by reconciling a TypeScript file and checking for CALLS edges.

8. **Cross-platform builds**: Pre-built `.node` binaries must be available for Linux x64, macOS x64, and macOS arm64. Verify by `npm install` on each platform without requiring a Rust toolchain.

### Edge Cases (Phase 1)

- **Daemon dies during Spec #1-3 development**: The existing `engine-client.ts` reconnection logic handles this. No change needed.
- **ONNX model missing after Spec #3 changes**: The embedding skip logic gracefully handles `embedder: None`. No embeddings are computed or cached. Search falls back to BM25-only.

---

## Implementation Order

The specs should be implemented in this order due to dependencies:

```
Spec #3 (Embedding Skip)     ← No dependencies. Standalone.
    ↓
Spec #2 (Background LSP)     ← Depends on reconcile flow changes that Spec #3 also touches.
    ↓
Spec #1 (ANN Index)          ← Depends on embedding storage format (unaffected by #2/#3).
    ↓
Spec #4 Phase 1 validation   ← Validate all three specs together.
```

Spec #3 is first because it's the simplest, has no new dependencies, and immediately improves reconcile latency. Spec #2 builds on the modified reconcile flow. Spec #1 is last because it's the most complex (new data structure) and independent of the reconcile flow.

---

## Appendix: Current Architecture Summary

```
┌─────────────────────────────────────────────────────────────┐
│                    Node.js Process                          │
│                                                             │
│  src/index.ts          MCP server, tool handlers            │
│  src/reconciler.ts     Debounced file sync, content hashing  │
│  src/workspace.ts      Workspace tracking, scratchpad mgmt   │
│  src/engine-client.ts  TCP socket → Rust daemon (JSON-RPC)  │
│  src/graph_explore.ts   Query DSL frontend                   │
│  src/visualizer.ts      Graph visualization                  │
│                                                             │
└────────────────────────┬────────────────────────────────────┘
                         │ TCP socket (JSON-RPC 2.0)
                         │
┌────────────────────────▼────────────────────────────────────┐
│                    Rust Daemon Process                       │
│                                                             │
│  src-rust/src/main.rs        TCP server, daemon lifecycle    │
│  src-rust/src/rpc.rs         RPC dispatch, search, reconcile  │
│  src-rust/src/graph.rs       In-memory graph (nodes + edges)  │
│  src-rust/src/search.rs      BM25 inverted index, tokenizer   │
│  src-rust/src/embedding.rs   ONNX gte-small, chunked embed     │
│  src-rust/src/reconciler.rs  Tree-sitter parse, LSP resolve   │
│  src-rust/src/storage.rs     Append-only JSONL persistence    │
│  src-rust/src/types.rs       Shared types, RPC schemas        │
│  src-rust/src/query_dsl.rs   JSON query DSL evaluation        │
│  src-rust/src/lsp_adapter.rs Stdio LSP client                 │
│  src-rust/src/language_adapter.rs  TS/Rust/Python grammars   │
│  src-rust/src/document_adapter.rs   Markdown parser          │
│                                                             │
│  Storage: .yaam/events.jsonl (append-only event log)         │
│  Model:   ~/.yaam/models/model.onnx (gte-small)              │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Key Data Flows

**Reconcile (current):**
```
File changed → TS Reconciler debounces 1s → engine.reconcile() RPC
  → Rust: tree-sitter parse → LSP resolve (blocking) → embed all entities
  → Append events to JSONL → Apply to memory graph → Update BM25
  → Return to TS
```

**Reconcile (after Spec #2 + #3):**
```
File changed → TS Reconciler debounces 1s → engine.reconcile() RPC
  → Rust: tree-sitter parse → embed changed entities only (Spec #3)
  → Append events to JSONL → Apply to memory graph → Update BM25
  → Queue references for background LSP (Spec #2)
  → Return immediately with edges_pending count
  → Background worker: LSP resolve → append LinkNodes events → apply to graph
```

**Search (current):**
```
yaam_search() RPC → Rust: BM25 search (inverted index) + linear scan embeddings
  → RRF fusion → filter → MMR → snippet/traverse enrichment
  → Return results
```

**Search (after Spec #1):**
```
yaam_search() RPC → Rust: BM25 search (inverted index) + HNSW ANN search
  → RRF fusion → filter → MMR → snippet/traverse enrichment
  → Return results
```