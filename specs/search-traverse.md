# Specification: Combined Search + Graph Traversal

**Status:** Draft  
**Date:** 2025-07-16  
**Depends on:** Source text retrieval (specs/source-text-retrieval.md — full source text in `content` field)

---

## 1. Overview

YAAM's search (`yaam_search`) and graph exploration (`yaam_graph_explore`) are two separate tools with a gap between them:

- **`yaam_search`** finds entities by semantic + BM25 similarity. It returns entity IDs, names, scores, and content — but **no relationship information**. You cannot see who calls a function, what it imports, or what file it's declared in.
- **`yaam_graph_explore`** traverses relationships (CALLS, IMPORTS, DECLARED_IN, REFERENCES). But it requires **exact node IDs** — no fuzzy or semantic matching.

In practice, answering a question like *"how does chunking work for long documents?"* requires both:
1. Semantic search to find `embed_chunked` (the *what*)
2. Graph traversal to find who calls it (the *where*)

Currently this demands **two tool calls with a manual ID lookup in between**, or falling back to `grep`. This spec proposes extending `yaam_search` with an optional `traverse` block that resolves graph relationships for the top-N search results — in a single call, with strict token control.

Additionally, this spec addresses two related search-quality issues:
- **Snippet extraction** — return the highest-scoring passage within an entity's content, not just the first N characters.
- **Result diversity (MMR)** — penalize redundant results from the same file so the user gets a diverse starting point.

---

## 2. Goals

1. **Single-call find + traverse** — `yaam_search` optionally resolves graph edges for the top-N results, eliminating the search→graph_explore round-trip.
2. **Token-controlled** — traversed nodes return only identity fields (id, name, entity_type, relationship), never full content. Budget is bounded by `resolve_top_k`.
3. **Snippet extraction** — search results include the best-matching passage from each entity's content, not just a truncated prefix.
4. **Result diversity** — optional MMR (Maximal Marginal Relevance) re-ranking prevents 8/10 results coming from the same file.
5. **Backward compatible** — all new fields are optional. Existing `yaam_search` calls with no `traverse` block behave identically to today.

---

## 3. Scope

### In Scope
- `SearchRequest` gains optional `traverse`, `diversity_lambda`, and `snippet` fields
- `TraverseClause` on search reuses the existing graph engine's `get_forward_edges` / `get_reverse_edges`
- New `SearchTraverseResult` struct for compact edge representations
- Snippet extraction via BM25 term highlighting within entity content
- MMR re-ranking with configurable lambda
- TypeScript tool definition updated to expose new parameters

### Out of Scope (Deferred)
- Multi-hop traversal in search (max_depth > 1) — deferred; depth-1 only for v1
- Storing per-chunk text for snippet provenance — deferred (see source-text-retrieval.md §9.1)
- Full-text snippet for Sections (heading already serves as a good snippet)
- Snippet highlighting/markup in the model-facing output
- Caching of traversal results across search calls

---

## 4. Architecture

### 4.1 New Request Fields

**File:** `src-rust/src/types.rs`

Three new optional fields on `SearchRequest`:

```rust
pub struct SearchRequest {
    // ... existing fields (text, workspace, top_k, entity_types,
    //     include_paths, exclude_paths, retrieval) ...

    /// Optional: resolve graph relationships for the top-N search results.
    /// Default: None (no traversal, backward compatible).
    #[serde(default)]
    pub traverse: Option<SearchTraverseClause>,

    /// Optional: snippet extraction mode.
    /// Default: None (no snippet; content returned per `retrieval` mode as today).
    #[serde(default)]
    pub snippet: Option<SnippetMode>,

    /// Optional: MMR diversity lambda (0.0 = max diversity, 1.0 = max relevance).
    /// Default: None (pure relevance ranking, backward compatible).
    #[serde(default)]
    pub diversity_lambda: Option<f32>,
}
```

### 4.2 SearchTraverseClause

**File:** `src-rust/src/types.rs`

A simplified traversal spec for search results. Unlike the graph DSL's `TraverseClause`, this supports **multiple relationships** and **field selection** because the search use-case is "show me the neighborhood," not "walk a specific path."

```rust
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
    /// Default: 3. Max: 10 (same as max top_k in practice).
    #[serde(default = "default_resolve_top_k")]
    pub resolve_top_k: usize,
}

fn default_traverse_direction() -> String {
    "both".to_string()
}

fn default_resolve_top_k() -> usize {
    3
}
```

**Design decisions:**

- **No `max_depth`** — depth-1 only for v1. Multi-hop traversal in search is a different access pattern (exploratory, not lookup). If needed later, add `max_depth` with default 1.
- **`resolve_top_k` default: 3** — the user almost always wants to start with the best match and expand. Resolving 3 is enough to see the call neighborhood without drowning in edges. The remaining `top_k - 3` results are returned as plain hits, preserving the "here are more candidates" behavior.
- **`relationship` is `Option<Vec<String>>`** — allows filtering to specific edge types (e.g., "just who calls this") or omitting to get all edges.

### 4.3 SnippetMode

**File:** `src-rust/src/types.rs`

```rust
/// Controls snippet extraction in search results.
///
/// When enabled, each result includes a `snippet` field containing the
/// passage from the entity's content that best matches the query —
/// determined by BM25 term overlap, not just position.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SnippetMode {
    /// Extract a snippet of approximately `max_snippet_tokens` tokens
    /// centered on the highest-scoring sentence/line.
    /// Default max_snippet_tokens: 64.
    Auto,
    /// Extract a snippet with a custom token budget.
    /// Specified via `snippet_tokens` on SearchRequest.
    Custom,
}
```

For v1, we implement only `Auto`. `Custom` is declared but returns the same as `Auto` — it's a forward-compatibility placeholder. The `snippet_tokens` field on `SearchRequest` is reserved but unused.

### 4.4 Traversal Result Representation

**File:** `src-rust/src/types.rs`

```rust
/// A single neighbor node discovered via traversal from a search result.
///
/// Compact representation — only identity fields, no content or embeddings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborNode {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub relationship: String,
    pub direction: String,  // "outbound" or "inbound"
}

/// Traversal results for a single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchTraversal {
    /// The search result's entity ID (for correlation).
    pub entity_id: String,
    /// Neighbor nodes found via traversal.
    pub neighbors: Vec<NeighborNode>,
}
```

**Token budget per neighbor:** ~4 fields × ~15 tokens each ≈ 60 tokens per neighbor. With `resolve_top_k=3` and ~10 neighbors each, total traversal output is ~1,800 tokens. Manageable.

### 4.5 Search Response Changes

**File:** `src-rust/src/rpc.rs`

The search response changes from a flat array of hits to a structured object:

**Before (current):**
```json
[
  { "id": "...", "name": "...", "score": 0.91, "content": "...", ... },
  { "id": "...", "name": "...", "score": 0.85, "content": "...", ... }
]
```

**After (when `traverse` or `snippet` is specified):**
```json
{
  "results": [
    {
      "id": "src-rust/src/embedding.rs::embed_chunked",
      "name": "embed_chunked",
      "score": 0.91,
      "content": "pub fn embed_chunked(...) { ... }",
      "snippet": "pub fn embed_chunked(&self, text: &str, max_tokens: usize, overlap_tokens: usize) -> Result<Vec<Vec<f32>>",
      "type": "Function",
      "path": "src-rust/src/embedding.rs",
      "category": "module",
      "line": 113,
      "traversal": {
        "entity_id": "src-rust/src/embedding.rs::embed_chunked",
        "neighbors": [
          { "id": "src-rust/src/rpc.rs::handle_search", "name": "handle_search", "entity_type": "Function", "relationship": "CALLS", "direction": "inbound" },
          { "id": "src-rust/src/embedding.rs::embed", "name": "embed", "entity_type": "Function", "relationship": "CALLS", "direction": "outbound" },
          { "id": "src-rust/src/embedding.rs::count_tokens", "name": "count_tokens", "entity_type": "Function", "relationship": "CALLS", "direction": "outbound" }
        ]
      }
    },
    // Results 4-10: no "traversal" key (only top resolve_top_k get traversal)
    { "id": "...", "name": "...", "score": 0.72, "content": "...", "type": "Section", ... }
  ]
}
```

**Backward compatibility:** When neither `traverse` nor `snippet` is specified, the response remains a **flat array** (same as today). When either is specified, the response is the structured object above. The TypeScript tool definition documents both shapes.

**Rationale for the shape change:** The flat array can't cleanly hold nested traversal data. The structured object is a natural superset. The TypeScript tool definition describes both possible response shapes so the model knows what to expect based on its request.

### 4.6 Traversal Resolution Logic

**File:** `src-rust/src/rpc.rs` — new function `resolve_traversals`

After the existing ranking + filtering pipeline produces `limited: Vec<(String, f32)>`, and before building result payloads:

```rust
/// Resolve graph relationships for the top-N search results.
///
/// For each of the top `resolve_top_k` results, query the graph engine
/// for forward (outbound) and/or reverse (inbound) edges, filter by the
/// requested relationship types, and return compact `NeighborNode` summaries.
///
/// Neighbors that don't exist in the graph (dangling edges) are skipped.
fn resolve_traversals(
    engine: &crate::graph::GraphEngine,
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
```

**Helper function:**
```rust
fn entity_type_string(label: &NodeLabel) -> String {
    match label {
        NodeLabel::Entity { entity_type, .. } => entity_type.clone(),
        NodeLabel::Workspace { .. } => "Workspace".to_string(),
        NodeLabel::Scratchpad { .. } => "Scratchpad".to_string(),
    }
}
```

### 4.7 Snippet Extraction

**File:** `src-rust/src/rpc.rs` — new function `extract_snippet`

The snippet is the **passage within the entity's content** that has the highest BM25 term overlap with the query. This is not a full BM25 search — it's a lightweight scoring of sentences/lines against the query tokens.

```rust
/// Extract the best-matching snippet from `content` for the given `query`.
///
/// Splits content into sentences (for prose) or lines (for code), scores each
/// segment by the number of query tokens it contains (case-insensitive), and
/// returns the segment with the highest score plus surrounding context.
///
/// If no segment contains any query tokens, returns the first ~64 tokens
/// as a fallback.
fn extract_snippet(content: &str, query: &str, max_tokens: usize) -> String {
    if content.is_empty() {
        return String::new();
    }

    let query_tokens: std::collections::HashSet<String> = crate::search::tokenize(query)
        .into_iter()
        .collect();

    if query_tokens.is_empty() {
        return preview_content(content, max_tokens * 4); // ~4 chars per token
    }

    // Split into segments: prefer sentence boundaries for prose, line boundaries for code.
    // Use whichever produces more segments (heuristic: code has more lines, prose has more sentences).
    let sentences = split_for_snippet(content);
    
    // Score each segment by query token overlap count.
    let mut best_idx = 0;
    let mut best_score = 0;
    for (i, segment) in sentences.iter().enumerate() {
        let segment_tokens: std::collections::HashSet<String> = crate::search::tokenize(segment)
            .into_iter()
            .collect();
        let overlap = query_tokens.intersection(&segment_tokens).count();
        if overlap > best_score {
            best_score = overlap;
            best_idx = i;
        }
    }

    // Build snippet: the best segment + adjacent context until we hit max_tokens.
    let mut snippet = String::new();
    let mut token_count = 0;
    
    // Start from best segment, expand outward
    let mut left = best_idx;
    let mut right = best_idx;
    let mut expanded = true;
    
    while expanded && token_count < max_tokens {
        expanded = false;
        
        // Try expanding right
        if right + 1 < sentences.len() {
            let addition_tokens = crate::search::tokenize(&sentences[right + 1]).len();
            if token_count + addition_tokens <= max_tokens {
                right += 1;
                token_count += addition_tokens;
                expanded = true;
            }
        }
        
        // Try expanding left
        if left > 0 {
            let addition_tokens = crate::search::tokenize(&sentences[left - 1]).len();
            if token_count + addition_tokens <= max_tokens {
                left -= 1;
                token_count += addition_tokens;
                expanded = true;
            }
        }
    }

    // Join segments
    for (i, segment) in sentences.iter().enumerate().skip(left).take(right - left + 1) {
        if !snippet.is_empty() {
            snippet.push(' ');
        }
        snippet.push_str(segment.trim());
    }

    // Fallback: if snippet is empty (shouldn't happen), return preview
    if snippet.is_empty() {
        return preview_content(content, max_tokens * 4);
    }

    snippet
}
```

**Helper:**
```rust
/// Split content into segments for snippet extraction.
///
/// For prose (markdown sections), splits on sentence boundaries (`. `, `! `, `? `).
/// For code (source text), splits on newlines.
/// Chooses sentence splitting if the content has paragraph breaks (`\n\n`),
/// otherwise falls back to line splitting.
fn split_for_snippet(content: &str) -> Vec<String> {
    if content.contains("\n\n") {
        // Prose: split into sentences
        let mut segments = Vec::new();
        for paragraph in content.split("\n\n") {
            for sentence in split_sentences_for_snippet(paragraph) {
                if !sentence.trim().is_empty() {
                    segments.push(sentence.trim().to_string());
                }
            }
        }
        segments
    } else {
        // Code: split into lines
        content.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// Simple sentence splitter for snippet extraction.
/// Splits on `. `, `! `, `? ` boundaries.
fn split_sentences_for_snippet(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        current.push(c);
        if c == '.' || c == '!' || c == '?' {
            sentences.push(current.clone());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current);
    }
    sentences
}
```

**Snippet token budget:** Default 64 tokens (~256 chars). This is enough to show a function signature + a key line, or 2-3 sentences of prose. The snippet is returned alongside the full `content` (per the existing `retrieval` mode), not as a replacement.

### 4.8 MMR Re-Ranking

**File:** `src-rust/src/rpc.rs` — new function `apply_mmr`

Maximal Marginal Relevance re-ranks results to balance relevance with diversity. After the initial ranking, each candidate is scored as:

```
MMR(c) = λ × relevance(c) - (1 - λ) × max(similarity(c, selected))
```

Where `relevance(c)` is the normalized search score and `similarity(c, selected)` is the **path-based similarity** between the candidate and already-selected results.

**Path similarity heuristic:** Two entities are "similar" if they share the same file path prefix. This is simpler than embedding-based similarity (which would require extra cosine computations) and directly addresses the "8/10 results from the same file" problem.

```rust
/// Apply Maximal Marginal Relevance re-ranking to search results.
///
/// Balances relevance score with diversity by penalizing results
/// from the same file as already-selected results.
///
/// `lambda`: 1.0 = pure relevance (no diversity), 0.0 = pure diversity.
/// Default recommended: 0.7.
fn apply_mmr(
    ranked: &mut Vec<(String, f32)>,
    lambda: f32,
) {
    if ranked.len() <= 1 || lambda >= 1.0 {
        return;  // No re-ranking needed
    }

    let mut selected: Vec<(String, f32)> = Vec::new();
    let mut remaining: Vec<(String, f32)> = ranked.drain(..).collect();

    // Normalize scores to [0, 1]
    let max_score = remaining.iter().map(|(_, s)| *s).fold(0.0f32, f32::max).max(1e-9);
    for (_, s) in &mut remaining {
        *s /= max_score;
    }

    while !remaining.is_empty() {
        // Find the candidate with the best MMR score
        let mut best_idx = 0;
        let mut best_mmr = f32::NEG_INFINITY;

        for (i, (id, rel)) in remaining.iter().enumerate() {
            // Compute max path-similarity to already-selected results
            let candidate_path = extract_path(id);
            let max_sim = selected.iter()
                .map(|(sel_id, _)| {
                    let sel_path = extract_path(sel_id);
                    path_similarity(&candidate_path, &sel_path)
                })
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

/// Extract the file path from an entity ID.
/// Entity IDs are formatted as "file_path::name" or "file_path:name".
/// For non-entity IDs (workspaces, scratchpads), returns the full ID.
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
///
/// Returns 1.0 if identical, 0.5 if same directory, 0.0 otherwise.
/// This is a coarse heuristic — we don't need fine-grained similarity
/// for diversity re-ranking.
fn path_similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    // Same directory (last path component differs)
    let a_dir = a.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let b_dir = b.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    if a_dir == b_dir && !a_dir.is_empty() {
        return 0.5;
    }
    0.0
}
```

**Integration into `handle_search`:**

```rust
// After ranking and filtering, before building results:
let mut limited: Vec<(String, f32)> = filtered.into_iter().take(top_k).collect();

// Apply MMR if requested
if let Some(lambda) = request.diversity_lambda {
    if (0.0..1.0).contains(&lambda) {
        apply_mmr(&mut limited, lambda);
    }
}

// Apply traversal resolution if requested
let traversals: HashMap<String, SearchTraversal> = if let Some(ref trav) = request.traverse {
    resolve_traversals(&engine, &limited, trav)
} else {
    HashMap::new()
};

// Build result payloads (see §4.5 for response shape logic)
```

### 4.9 Response Shape Decision Logic

**File:** `src-rust/src/rpc.rs` — `handle_search` return

```rust
let has_traverse = request.traverse.is_some();
let has_snippet = request.snippet.is_some();

if !has_traverse && !has_snippet {
    // Backward-compatible: return flat array (same as today)
    Ok(serde_json::json!(results_flat))
} else {
    // New structured response
    let results_structured: Vec<serde_json::Value> = limited
        .iter()
        .filter_map(|(id, score)| {
            engine.get_node(id).map(|node| {
                let mut hit = build_search_hit(node, id, score, &retrieval);
                
                if has_snippet {
                    hit["snippet"] = serde_json::json!(
                        extract_snippet(&node.content, &request.text, 64)
                    );
                }
                
                if let Some(trav) = traversals.get(id) {
                    hit["traversal"] = serde_json::json!(trav);
                }
                
                hit
            })
        })
        .collect();
    
    Ok(serde_json::json!({ "results": results_structured }))
}
```

---

## 5. TypeScript Tool Definition Changes

### 5.1 `yaam_search` Tool Schema

The tool description and schema must be updated to expose the new parameters:

```typescript
// Updated yaam_search parameters
{
  text: string,           // required (unchanged)
  top_k: number,          // optional (unchanged)
  workspace: string,      // optional (unchanged)
  entity_types: string[], // optional (unchanged)
  include_paths: string[],// optional (unchanged)
  exclude_paths: string[],// optional (unchanged)

  // NEW: optional traversal
  traverse: {
    relationship?: string[],  // e.g. ["CALLS", "IMPORTS"]
    direction?: "outbound" | "inbound" | "both",  // default: "both"
    resolve_top_k?: number,    // default: 3
  },

  // NEW: optional snippet extraction
  snippet: "auto",  // returns best-matching passage from content

  // NEW: optional diversity
  diversity_lambda: number,  // 0.0–1.0, default: 1.0 (pure relevance)
}
```

**Tool description update:**

> When `traverse` is specified, the top `resolve_top_k` results include a `traversal` object with neighbor nodes (inbound/outbound edges). Neighbors contain only `id`, `name`, `entity_type`, `relationship`, and `direction` — no content.
>
> When `snippet` is specified, each result includes a `snippet` field with the best-matching passage from the entity's content.
>
> When `diversity_lambda` < 1.0, results are re-ranked with MMR to reduce redundancy from the same file.
>
> When `traverse` or `snippet` is specified, the response is `{ "results": [...] }` instead of a flat array.

---

## 6. File-by-File Change Inventory

| File | Change Type | Description |
|------|------------|-------------|
| `src-rust/src/types.rs` | Add structs | `SearchTraverseClause`, `NeighborNode`, `SearchTraversal`, `SnippetMode` |
| `src-rust/src/types.rs` | Modify struct | Add `traverse`, `snippet`, `diversity_lambda` fields to `SearchRequest` |
| `src-rust/src/rpc.rs` | Add functions | `resolve_traversals`, `extract_snippet`, `apply_mmr`, helpers |
| `src-rust/src/rpc.rs` | Modify function | `handle_search`: integrate MMR, traversal, snippet into pipeline |
| `src-rust/src/rpc.rs` | Modify function | `handle_search`: response shape decision logic |
| `src-rust/src/search.rs` | No changes | `tokenize` already public, reused for snippet scoring |
| `index.ts` | Modify tool def | Update `yaam_search` tool schema to expose new parameters |

---

## 7. Data Flow

### Before (current)

```
User asks: "how does chunking work for long documents?"

Step 1: yaam_search("embedding token limit chunking")
  → [{ id: "embedding.rs::embed_chunked", name: "embed_chunked", score: 0.91, content: "pub fn embed_chunked..." }]
  → No relationships. Model doesn't know who calls it.

Step 2: bash: grep -rn "embed_chunked" src-rust/src/
  → finds rpc.rs:299, rpc.rs:733

Step 3: read rpc.rs offset=290 limit=30
  → sees embedder.embed_chunked(&text, 400, 50)

Total: 3 tool calls, ~5000 tokens of context
```

### After (with this spec)

```
User asks: "how does chunking work for long documents?"

Step 1: yaam_search("embedding token limit chunking", traverse: { direction: "inbound" }, snippet: "auto")
  → {
      results: [
        {
          id: "embedding.rs::embed_chunked",
          name: "embed_chunked",
          score: 0.91,
          snippet: "pub fn embed_chunked(&self, text: &str, max_tokens: usize, overlap_tokens: usize) -> Result<Vec<Vec<f32>>",
          content: "pub fn embed_chunked(...) { ... full body ... }",
          traversal: {
            neighbors: [
              { id: "rpc.rs::handle_search", name: "handle_search", entity_type: "Function", relationship: "CALLS", direction: "inbound" },
              { id: "rpc.rs::handle_upsert_node", name: "handle_upsert_node", entity_type: "Function", relationship: "CALLS", direction: "inbound" }
            ]
          }
        },
        // ... more results
      ]
    }

Model now knows: embed_chunked is called by handle_search and handle_upsert_node,
and sees the function signature in the snippet. Can read rpc.rs::handle_search
directly if it needs the call-site parameters.

Total: 1 tool call, ~1200 tokens of context
```

---

## 8. Testing Plan

### 8.1 Traversal Resolution

- **Unit test:** Search with `traverse: { direction: "inbound" }` for "embed_chunked" → result includes inbound CALLS edges to `handle_search` and `handle_upsert_node`.
- **Unit test:** Search with `traverse: { relationship: ["CALLS"], direction: "outbound" }` → result includes only outbound CALLS edges, no IMPORTS or DECLARED_IN.
- **Unit test:** Search with `traverse: { resolve_top_k: 1 }` → only the top result has `traversal`; results 2-N have no `traversal` key.
- **Unit test:** Search without `traverse` → no `traversal` key on any result, response is flat array (backward compatible).
- **Unit test:** Entity with no edges → `traversal.neighbors` is empty array, not null.

### 8.2 Snippet Extraction

- **Unit test:** Search for "max_tokens overlap" → snippet from `embed_chunked` contains the parameter list with `max_tokens` and `overlap_tokens`.
- **Unit test:** Search for a term only in the middle of a long function → snippet is centered on the matching line, not the beginning.
- **Unit test:** Entity with empty content → snippet is empty string.
- **Unit test:** Search with `snippet: "auto"` and `retrieval: "name"` → both `snippet` and empty `content` are returned.

### 8.3 MMR Diversity

- **Unit test:** 10 results all from `embedding.rs`, `diversity_lambda: 0.5` → top results include entities from different files.
- **Unit test:** `diversity_lambda: 1.0` → ranking unchanged (pure relevance).
- **Unit test:** `diversity_lambda: 0.0` → results from same file are maximally spread out.
- **Unit test:** Single result → MMR is a no-op.

### 8.4 Response Shape

- **Integration test:** Search with no `traverse` or `snippet` → response is JSON array (backward compatible).
- **Integration test:** Search with `traverse` → response is `{ "results": [...] }`.
- **Integration test:** Search with `snippet` → response is `{ "results": [...] }`.

### 8.5 Token Budget

- **Integration test:** Search with `traverse: { resolve_top_k: 3 }` on a graph with ~20 nodes → total response size < 5000 tokens. Verify by measuring JSON string length.
- **Integration test:** Search with `traverse: { resolve_top_k: 10 }` → response size grows linearly but stays under 15000 tokens.

---

## 9. Review — Gaps and Edge Cases

### 9.1 [SHOULD ADDRESS] Response shape breakage

**Problem:** Changing from a flat array to `{ "results": [...] }` when `traverse` or `snippet` is specified means the response shape depends on the request. This could confuse the consuming model.

**Mitigation:** The TypeScript tool description explicitly documents both shapes:
> "When `traverse` or `snippet` is specified, the response is `{ 'results': [...] }` instead of a flat array."

The model reads the tool description and knows what to expect based on what it requested. In practice, models handle conditional response shapes well when documented.

**Alternative considered:** Always return `{ "results": [...] }`. Rejected — this would break backward compatibility for all existing search calls, including the TypeScript extension's own internal usage.

### 9.2 [SHOULD ADDRESS] Traversal for Workspace and Scratchpad nodes

**Problem:** `SearchTraverseClause` applies to all result types, including Workspaces and Scratchpads. Workspaces have `MAPPED_TO` and `HAS_SCRATCHPAD` edges. Scratchpads typically have no edges.

**Assessment:** This is fine. Traversal just returns whatever edges exist. A Scratchpad with no edges gets `neighbors: []`. A Workspace with `MAPPED_TO` edges gets those neighbors. No special handling needed — the user gets what they ask for.

### 9.3 [VERIFIED OK] Graph engine is already read-locked during search

**Problem:** `resolve_traversals` calls `engine.get_forward_edges` and `engine.get_reverse_edges` during search. Is the engine lock held?

**Result:** Yes — `handle_search` already acquires `let engine = state.engine.read().unwrap();` at the top (line 486). The traversal resolution runs within this lock scope. No additional locking needed. ✅

### 9.4 [SHOULD ADDRESS] Snippet quality for code

**Problem:** The snippet extractor splits code on newlines and scores each line by query token overlap. A line like `let max_tokens = 400;` would score high for "max_tokens" but miss the surrounding context (`fn embed_chunked(..., max_tokens: usize, ...)`).

**Mitigation:** The snippet expands outward from the best-matching line to include adjacent lines up to the token budget. So the snippet would include the function signature line + the matching line. The expansion logic (§4.7) handles this.

**Future improvement:** For code, prefer the function signature (first non-blank line containing `fn`/`def`/`function`/`class`) as the anchor, then expand to query-matching lines. This would give better snippets for code. Deferred to v2.

### 9.5 [SHOULD ADDRESS] MMR path similarity is coarse

**Problem:** `path_similarity` returns 1.0 for same file, 0.5 for same directory, 0.0 otherwise. This is very coarse — two unrelated functions in a large file like `rpc.rs` would be penalized even if they handle completely different concerns.

**Assessment:** This is the right tradeoff for v1. The goal of MMR in search is to give the user a *diverse starting point*, not to find the optimal clustering. Penalizing same-file results ensures the user sees results from at least 2-3 different files in the top 5. Fine-grained diversity (based on embedding similarity) would require computing pairwise cosine similarities among all candidates — O(k²) cosine computations on 384-dim vectors. Not expensive (k=10, so 100 dot products), but adds complexity. Deferred to v2.

### 9.6 [SHOULD ADDRESS] MMR degrades top result quality

**Problem:** MMR can push the most relevant result down if it's from the same file as an already-selected result. The user expects the #1 result to be the best match.

**Mitigation:** MMR always selects the top relevance result first (before the diversity loop begins), since the first selection has no "already selected" set to be penalized against. After that, subsequent selections balance relevance and diversity. The #1 result is always the pure-relevance winner. Only positions 2-N are affected.

**Implementation note:** The `apply_mmr` function naturally does this — the first iteration has `selected` empty, so `max_sim = 0.0` and `MMR = lambda * rel`, which is pure relevance. The first pick is always the highest-relevance result. ✅

### 9.7 [VERIFIED OK] `tokenize` is public

**Problem:** `extract_snippet` and the snippet scorer call `crate::search::tokenize`. Is it accessible?

**Result:** Yes — `tokenize` is `pub fn tokenize(text: &str) -> Vec<String>` in `search.rs` (line 46). ✅

### 9.8 [SHOULD ADDRESS] Snippet vs. retrieval interaction

**Problem:** When `retrieval: "name"` (content suppressed) and `snippet: "auto"` are both specified, the result has empty `content` but a populated `snippet`. Is this useful?

**Assessment:** Yes — this is actually a powerful combination. The user gets just the name + line (minimal context) plus a targeted snippet showing why this result matched, without paying the token cost of full content. This is the "search results page" pattern: title + snippet, no full text.

### 9.9 [DEFERRED] Multi-hop traversal in search

**Problem:** Some questions require multi-hop traversal ("what functions call things that embed_chunked calls?"). The current spec only supports depth-1.

**Rationale for deferral:** Multi-hop traversal in search is exploratory, not lookup. The user would be better served by `yaam_graph_explore` with a proper `max_depth` for that use case. Depth-1 in search covers the 90% case: "who calls this" and "what does this call."

### 9.10 [DEFERRED] Embedding-based MMR

**Problem:** Path-based similarity is coarse. Embedding-based similarity would be more accurate.

**Rationale for deferral:** Path-based MMR already solves the "8/10 from same file" problem. Embedding-based MMR adds O(k²) cosine computations and complexity for marginal gain. Revisit if path-based diversity proves insufficient in practice.

---

## 10. Summary of Review Findings

| # | Issue | Severity | Action |
|---|-------|----------|--------|
| 9.1 | Response shape breakage | SHOULD ADDRESS | Document both shapes in tool description; conditional shape based on request |
| 9.2 | Traversal for Workspace/Scratchpad | SHOULD ADDRESS | No special handling — return whatever edges exist |
| 9.3 | Graph engine lock during traversal | VERIFIED OK | Already held by `handle_search` |
| 9.4 | Snippet quality for code | SHOULD ADDRESS | Line-based expansion handles most cases; signature-anchored snippets deferred to v2 |
| 9.5 | MMR path similarity is coarse | SHOULD ADDRESS | Right tradeoff for v1; embedding-based MMR deferred |
| 9.6 | MMR degrades top result | VERIFIED OK | First selection is always pure relevance |
| 9.7 | `tokenize` accessibility | VERIFIED OK | Already public |
| 9.8 | Snippet + retrieval interaction | SHOULD ADDRESS | Powerful combination, no issue |
| 9.9 | Multi-hop traversal in search | DEFERRED | Depth-1 covers 90% case; multi-hop is exploratory (use graph_explore) |
| 9.10 | Embedding-based MMR | DEFERRED | Path-based MMR solves the immediate problem |

**Conclusion:** No must-fix items. All should-address items are acceptable for v1 with documented limitations. The change is backward compatible — existing search calls with no new parameters behave identically to today.