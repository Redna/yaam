# Specification: Markdown Document Adapter

**Status:** Draft  
**Date:** 2025-07-05  
**Depends on:** Embedding format change (`Vec<f32>` → `Vec<Vec<f32>>`)

---

## 1. Overview

YAAM currently indexes only code files (TypeScript, Python, Rust) using tree-sitter + LSP. Markdown and other documentation files are upserted as bare `File` nodes with no entities, no edges, and no searchable content beyond the filename.

This feature adds a **markdown document adapter** that parses `.md` files, creates `Section` entities for each heading, and links them to existing code entities via `REFERENCES` edges — using inline code matching as the resolution mechanism (no LSP required).

It also includes a prerequisite change to the embedding format: `Option<Vec<f32>>` → `Option<Vec<Vec<f32>>>` to support chunked embeddings for long section bodies that exceed the gte-small model's 512-token limit.

---

## 2. Goals

1. **Markdown sections are first-class graph entities** — searchable, traversable, and semantically indexed.
2. **Docs link to code** — a Section that mentions `` `reconcile_file` `` gets a `REFERENCES` edge to that function node.
3. **Long sections are fully searchable** — chunked embeddings ensure semantic search covers the entire section, not just the first 400 words.
4. **No LSP dependency** — the graph itself serves as the resolver via name-matching.
5. **No breaking migration** — the graph will be wiped and re-reconciled; no backward compatibility with old event format needed.

---

## 3. Scope

### In Scope
- New entity type: `Section`
- New edge type: `REFERENCES` (Section → Function/Class/File)
- `tree-sitter-markdown` integration for parsing
- Embedding format change: `Vec<f32>` → `Vec<Vec<f32>>`
- Chunked embeddings with sliding window overlap
- Inline code (backtick) name-matching for REFERENCES edges
- File path reference matching (e.g., `src/reconciler.rs` in prose)
- TypeScript extension file discovery (`.md` added to `SUPPORTED_EXTENSIONS`)
- Text indexing updates for `build_bm25_text` and `build_embedding_text_from_props`

### Out of Scope (Deferred)
- Checkbox status tracking for spec-driven workflows (`- [ ]`, `- [x]`)
- Doc-to-doc links (markdown links between `.md` files)
- `CONTAINS` edges for nested section hierarchy (section → sub-section)
- `CodeBlock` entities for fenced code blocks within sections
- Config file indexing (`.json`, `.yaml`, `.toml`)
- Other markup formats (`.rst`, `.txt`, `.adoc`)
- Visualizer UI changes for Section nodes

---

## 4. Architecture

### 4.1 Embedding Format Change

**File:** `src-rust/src/types.rs`

```rust
// Before
pub struct MemoryNode {
    pub embedding: Option<Vec<f32>>,
}

// After
pub struct MemoryNode {
    pub embedding: Option<Vec<Vec<f32>>>,
}
```

**Rationale:** A single node may have multiple embedding vectors when its content is chunked. Code entities get `Some(vec![single_embedding])`. Markdown sections get `Some(vec![chunk1, chunk2, ...])`.

**File:** `src-rust/src/graph.rs` (`upsert_node`)

The embedding extraction from `properties["embedding"]` changes to parse an array of arrays of floats:

```rust
let embedding = props.get("embedding").and_then(|v| {
    v.as_array().map(|outer| {
        outer.iter().filter_map(|inner| {
            inner.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect::<Vec<f32>>()
            })
        }).collect::<Vec<Vec<f32>>>()
    })
});
```

### 4.2 Chunked Embeddings

**File:** `src-rust/src/embedding.rs`

New function `embed_chunked`:

```rust
pub fn embed_chunked(
    text: &str,
    embedder: &EmbeddingModel,
    max_tokens: usize,
    overlap_tokens: usize,
) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>>
```

**Algorithm:**
1. Tokenize the full text using the tokenizer to count tokens.
2. If token count ≤ `max_tokens`, return `vec![embedder.embed(text)?]`.
3. Otherwise, split the text into chunks:
   - Split on paragraph boundaries (`\n\n`) first.
   - Group paragraphs into chunks that stay under `max_tokens`.
   - Ensure `overlap_tokens` overlap between consecutive chunks (include the last ~50 tokens of the previous chunk at the start of the next).
   - If a single paragraph exceeds `max_tokens`, split it on sentence boundaries (`.` `!` `?`).
   - If a single sentence exceeds `max_tokens`, hard-split on token count.
4. Embed each chunk independently.
5. Return the list of embedding vectors.

**Parameters:**
| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `max_tokens` | 400 | Headroom under 512 for tokenizer special tokens |
| `overlap_tokens` | 50 | ~1-2 sentences of context preserved across boundaries |

**No changes to `embed()` itself** — it still takes text and returns a single `Vec<f32>`. The chunking function calls `embed()` multiple times.

### 4.3 New Entity Type: Section

**No struct or enum changes needed.** `NodeLabel::Entity` already stores `entity_type` as a `String`. Using `"Section"` is just a new string value.

**Section entity shape:**

| Field | Value | Example |
|-------|-------|---------|
| `id` | `{file_path}:{heading_text}` | `README.md:Reconciler Architecture` |
| `label` | `Entity` | — |
| `entity_type` | `"Section"` | — |
| `name` | Heading text (without `#` markers) | `Reconciler Architecture` |
| `content` | Full body text between this heading and the next heading at the same or higher level | `The reconciler works by...` |
| `status` | `"active"` | — |
| `last_modified` | Timestamp | — |
| `metadata` | JSON string with heading level and byte offsets | `{"level": 2, "start_line": 45, "end_line": 120}` |

**ID collision handling:** If two headings in the same file have identical text, append a numeric suffix: `README.md:Architecture`, `README.md:Architecture:2`.

**Content scope:** A Section's content includes all text until the next heading at the **same or higher** level. A `## Foo` section's content includes `### Bar` and its text. This means `### Bar` content is a subset of `## Foo` content. This is intentional — the parent section is the broader context, the child section is the specific focus.

### 4.4 Document Adapter Trait

**File:** `src-rust/src/document_adapter.rs` (new file)

```rust
use std::path::Path;
use crate::types::Event;

/// Parsed section from a markdown document.
pub struct ParsedSection {
    pub id: String,
    pub name: String,
    pub content: String,
    pub level: u8,
    pub start_line: usize,
    pub end_line: usize,
    pub inline_code_refs: Vec<String>,  // backtick-quoted identifiers
    pub file_path_refs: Vec<String>,    // path-like strings in the text
}

/// Trait for non-code document parsers.
pub trait DocumentAdapter: Send + Sync {
    /// Return the file extensions this adapter handles (without leading dot).
    fn extensions(&self) -> &[&str];

    /// Parse a document file and extract sections + references.
    fn parse_document(
        &self,
        file_path: &Path,
        content: &str,
    ) -> Vec<ParsedSection>;
}

/// Factory: returns the appropriate document adapter for a file.
pub fn get_document_adapter(file_path: &Path) -> Option<Box<dyn DocumentAdapter>> {
    let ext = file_path.extension().and_then(|e| e.to_str())?;
    // To be extended with more document types in the future.
    match ext {
        "md" => Some(Box::new(MarkdownAdapter)),
        _ => None,
    }
}
```

### 4.5 Markdown Parsing

**File:** `src-rust/src/document_adapter.rs`

The `MarkdownAdapter` implements `DocumentAdapter` using a **custom line-by-line parser** instead of tree-sitter-markdown. This avoids a tree-sitter version compatibility issue: `tree-sitter-md` 0.3.x requires tree-sitter ^0.23 (links conflict with our 0.22), and the old `tree-sitter-markdown` 0.7.x uses tree-sitter 0.19 (incompatible `Language` type). A custom parser is simpler and sufficient for markdown's regular structure.

**Parsing pipeline:**
1. Strip YAML frontmatter (`---\n...\n---`) from the start of the file.
2. Scan lines for ATX headings (`#` through `######`) and Setext headings (text followed by `===` or `---`).
3. For each heading:
   - Extract heading text (strip `#` prefixes and whitespace) → `name`
   - Determine heading level (count of `#` or `===`/`---` type) → `level`
   - Collect all text between this heading's line and the next heading at same-or-higher level → `content`
   - Find backtick-wrapped inline code (`` `identifier` ``) within the content → `inline_code_refs`
   - Scan content for path-like strings ending in known file extensions → `file_path_refs`
4. Generate unique IDs (with collision suffixing).
5. Return `Vec<ParsedSection>`.

**Heading detection rules:**
- ATX: 1-6 `#` characters followed by space or end of line
- Setext: non-empty line followed by a line of all `=` (level 1) or all `-` (level 2)
- Setext headings that look like thematic breaks (`---`) or frontmatter are excluded

**File path matching** uses a hardcoded extension list: `rs, ts, tsx, js, jsx, py, md, go, java, rb, c, cpp, h`. This is a known limitation (see review 9.8) — it should be made dynamic from the language registry in a future iteration.

### 4.6 REFERENCES Edge Creation

**Mechanism:** After parsing, the reconciler resolves references against the existing graph. No LSP is used — the graph itself is the resolver.

**File:** `src-rust/src/reconciler.rs`

New function `resolve_document_references`:

```rust
fn resolve_document_references(
    sections: &[ParsedSection],
    engine: &MemoryEngine,
    file_id: &str,
) -> Vec<Event>
```

**Resolution logic:**

1. **Inline code matching (primary, high-precision):**
   - For each `inline_code_ref` in a section (e.g., `reconcile_file`):
     - Search the graph for entities where `name == ref` and `entity_type` is `Function` or `Class`.
     - If exactly one match: create `REFERENCES` edge from Section to that entity.
     - If multiple matches: create edges to all matches (let search disambiguate).
     - If no match: skip silently.

2. **File path matching (secondary):**
   - For each `file_path_ref` in a section (e.g., `src/reconciler.rs`):
     - Search the graph for a `File` node with matching `id`.
     - If found: create `REFERENCES` edge from Section to that File node.

**Edge properties:**
```json
{
  "match_type": "inline_code" | "file_path",
  "matched_text": "reconcile_file"
}
```

**Edge direction:** `Section → Function/Class/File` (outbound REFERENCES from the doc to the code).

**Deduplication:** If the same reference appears multiple times in a section (e.g., `` `reconcile_file` `` mentioned 3 times), only one `REFERENCES` edge is created per (source, target) pair.

### 4.7 Reconciler Changes

**File:** `src-rust/src/reconciler.rs`

`reconcile_file` becomes a dispatcher:

```rust
pub fn reconcile_file(
    file_path: &Path,
    content: Option<&str>,
    lsp: Option<&mut dyn LspAdapter>,
    engine: &MemoryEngine,
) -> Vec<Event> {
    // ... existing file node upsert + deletion logic ...

    // 1. Try code adapter (existing path)
    if let Some(adapter) = get_adapter(file_path) {
        // existing parse_file + LSP resolution logic
        return events;
    }

    // 2. Try document adapter (new path)
    if let Some(doc_adapter) = get_document_adapter(file_path) {
        let sections = doc_adapter.parse_document(file_path, content_str);
        // Upsert Section entities with DECLARED_IN edges
        // Resolve REFERENCES edges via graph name-matching
        return events;
    }

    // 3. Unsupported file type — file node only (existing behavior)
    events
}
```

**Section upsert logic:**
- Delete existing Section entities for this file (same pattern as code: find inbound `DECLARED_IN` edges to the file node, delete those source nodes).
- Upsert each `ParsedSection` as an Entity node with `entity_type: "Section"`.
- Create `DECLARED_IN` edge: Section → File.
- Create `REFERENCES` edges via `resolve_document_references`.

### 4.8 Embedding Computation Changes

**File:** `src-rust/src/rpc.rs` (`handle_reconcile`)

The embedding computation loop changes to use `embed_chunked` for Section entities and wraps single embeddings in `vec![]` for code entities:

```rust
if payload.label == "Entity" {
    let entity_type = payload.properties.get("entity_type")
        .and_then(|v| v.as_str()).unwrap_or("");
    
    let text = build_embedding_text_from_props(&payload.label, &payload.properties);
    if !text.is_empty() {
        if entity_type == "Section" {
            // Chunked embedding for long section content
            match crate::embedding::embed_chunked(&text, embedder, 400, 50) {
                Ok(vectors) => {
                    payload.properties.insert("embedding".to_string(),
                        serde_json::json!(vectors));
                }
                Err(e) => eprintln!("Failed to chunk-embed: {}", e),
            }
        } else {
            // Single embedding, wrapped in Vec for new format
            match embedder.embed(&text) {
                Ok(vec) => {
                    payload.properties.insert("embedding".to_string(),
                        serde_json::json!(vec![vec]));
                }
                Err(e) => eprintln!("Failed to compute embedding: {}", e),
            }
        }
    }
}
```

**File:** `src-rust/src/rpc.rs` (`handle_upsert_node` — the non-reconcile upsert path)

Same change: wrap single embeddings in `vec![]`, use `embed_chunked` for Section entities.

### 4.9 Text Indexing Changes

**File:** `src-rust/src/rpc.rs` (`build_bm25_text`)

```rust
fn build_bm25_text(node: &MemoryNode) -> String {
    let mut parts = Vec::new();
    if !node.name.is_empty() {
        parts.push(node.name.clone());
    }

    match &node.label {
        NodeLabel::Entity { entity_type, .. } => {
            if entity_type == "Section" {
                // Index heading + full section body
                parts.push(node.content.clone());
            } else {
                // Existing: index docComment from metadata
                if !node.metadata.is_empty() {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
                        if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
                            parts.push(doc.to_string());
                        }
                    }
                }
            }
        }
        // ... Workspace and Scratchpad unchanged ...
    }
    parts.join(" ")
}
```

**File:** `src-rust/src/rpc.rs` (`build_embedding_text_from_props`)

```rust
fn build_embedding_text_from_props(
    label: &str,
    props: &HashMap<String, serde_json::Value>,
) -> String {
    let name = props.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let entity_type = props.get("entity_type").and_then(|v| v.as_str()).unwrap_or("");

    match label {
        "Workspace" => { /* unchanged */ }
        "Scratchpad" => { /* unchanged */ }
        "Entity" => {
            if entity_type == "Section" {
                // Embed heading + section body (will be chunked by embed_chunked)
                let content = props.get("content").and_then(|v| v.as_str()).unwrap_or("");
                format!("{} {}", name, content)
            } else {
                // Existing: name + docComment from metadata
                /* unchanged */
            }
        }
        _ => name.to_string(),
    }
}
```

### 4.10 Search Changes

**File:** `src-rust/src/rpc.rs` (`handle_search`)

The semantic search loop changes to iterate over multiple embeddings per node and take the max similarity:

```rust
// Before:
if let Some(doc_embedding) = node.embedding.as_deref() {
    let sim = cosine_similarity(&query_embedding, doc_embedding);
    *scores.entry(node.id.clone()).or_insert(0.0) += sim;
}

// After:
if let Some(ref embeddings) = node.embedding {
    let best_sim = embeddings.iter()
        .map(|emb| cosine_similarity(&query_embedding, emb))
        .fold(0.0f32, f32::max);
    *scores.entry(node.id.clone()).or_insert(0.0) += best_sim;
}
```

**Deduplication is inherent:** `scores` is a `HashMap<String, f32>` keyed by `node.id`. Each node contributes one entry regardless of how many chunk embeddings it has. The best-matching chunk determines the node's semantic score.

**Scratchpad temporal decay:** The existing decay logic for `NodeLabel::Scratchpad` is unchanged. Section entities are `NodeLabel::Entity` and do not get temporal decay.

### 4.11 TypeScript Extension Changes

**File:** `src/reconciler.ts` (`scheduleFull`)

```typescript
// Before:
const SUPPORTED_EXTENSIONS = ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs'];

// After:
const SUPPORTED_EXTENSIONS = ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs', '.md'];
```

`SKIP_DIRS` already excludes `node_modules`, `dist`, `.git`, `target`, `.chunks`, `.yaam` — this prevents indexing dependency README files.

**Incremental reconciliation** (`scheduleIncremental`) already works for any file type — it watches `write`/`edit`/`bash` tool calls and adds the path to the sync queue. No changes needed.

### 4.12 Dependency Addition

No external dependency needed. Markdown parsing uses a custom line-by-line parser in `document_adapter.rs`.

> **Note:** The original spec planned to use `tree-sitter-markdown`, but no compatible version exists for tree-sitter 0.22. `tree-sitter-md` 0.3.x requires tree-sitter ^0.23 (links conflict), and `tree-sitter-markdown` 0.7.x uses tree-sitter 0.19 (incompatible `Language` type). A custom parser is simpler and sufficient for markdown's regular heading structure.

### 4.13 Reverse Re-linking on Code Entity Changes

**Problem:** When a code file is re-reconciled, its old Function/Class entities are deleted and new ones created. `delete_node` removes all edges including inbound `REFERENCES` edges from Section nodes. The Section still exists and its content still mentions the code entity, but the `REFERENCES` edge is gone. It is only recreated when the markdown file itself is next re-reconciled — which may never happen if only the code file changed.

**Solution:** A targeted re-linking pass within `handle_reconcile`. Before deletion events are applied, snapshot the inbound `REFERENCES` edges. After all events (deletions + creations) are applied, re-resolve those references against the new graph state and recreate edges.

**File:** `src-rust/src/rpc.rs` (`handle_reconcile`)

```rust
// ── Phase 1: Snapshot affected Sections BEFORE applying events ──
let mut affected_sections: Vec<(String, String)> = Vec::new(); // (section_id, matched_text)

for event in &events {
    if let EventPayload::DeleteNode(payload) = &event.payload {
        let engine = state.engine.read().unwrap();
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

// ── Phase 2: Apply all events (deletions + creations) ──
// (existing code: append to JSONL, apply to engine, update BM25)

// ── Phase 3: Re-link affected Sections to new entities ──
if !affected_sections.is_empty() {
    let engine = state.engine.read().unwrap();
    let mut relink_events = Vec::new();
    let timestamp = get_timestamp();

    for (section_id, matched_text) in &affected_sections {
        // Skip if the Section itself was deleted (markdown file changed too)
        if engine.get_node(section_id).is_none() {
            continue;
        }

        // Find entities with this name in the current graph
        let candidates: Vec<String> = engine
            .all_nodes()
            .iter()
            .filter(|n| n.name == *matched_text)
            .filter(|n| matches!(&n.label, NodeLabel::Entity { entity_type, .. }
                if entity_type == "Function" || entity_type == "Class"))
            .map(|n| n.id.clone())
            .collect();

        for target_id in candidates {
            relink_events.push(Event {
                version: EVENT_VERSION,
                timestamp,
                event_type: EventType::LinkNodes,
                payload: EventPayload::LinkNodes(LinkNodesPayload {
                    from_id: section_id.clone(),
                    to_id: target_id,
                    relationship: "REFERENCES".to_string(),
                    properties: {
                        let mut props = HashMap::new();
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
                        props
                    },
                }),
            });
        }
    }

    // Apply relink events
    let store = state.store.write().unwrap();
    let mut engine = state.engine.write().unwrap();
    for event in &relink_events {
        let _ = store.append(event);
        engine.apply_event(event);
    }
}
```

**Edge properties for re-linked edges:**
```json
{
  "match_type": "inline_code",
  "matched_text": "reconcile_file",
  "relinked": true
}
```

The `relinked: true` property distinguishes edges created by the reverse re-linking pass from edges created during initial markdown reconciliation. This is useful for debugging and potential future analytics.

**Behavior by scenario:**

| Scenario | Result |
|----------|--------|
| Function modified, same name | `REFERENCES` edge recreated ✅ |
| Function renamed | Edge stays gone (old `matched_text` no longer matches) ✅ |
| Function deleted entirely | Edge stays gone ✅ |
| Section also changed in same reconcile batch | Section deleted, skip re-linking ✅ |
| New function added that Section should reference | Not caught (Section content unchanged, no new backtick to discover) — caught on next markdown re-reconcile ✅ |

**Performance:** For a typical code file change, 0-5 Sections are affected. Each requires one `all_nodes()` scan filtered by name (currently ~220 nodes). Cost is negligible.

**Deduplication:** If the same (Section, target) pair already has a `REFERENCES` edge (e.g., from a prior re-link), the `LinkNodes` event creates a duplicate edge. The engine should check for existing edges before creating new ones, or the re-link pass should filter out pairs that already have an edge. Add a check:

```rust
// Skip if edge already exists
let existing = engine.get_forward_edges(section_id);
if existing.iter().any(|e| e.to_id == target_id && e.relationship == "REFERENCES") {
    continue;
}
```

---

## 5. File-by-File Change Inventory

| File | Change Type | Description |
|------|------------|-------------|
| `src-rust/Cargo.toml` | No change | No new dependency — custom parser used instead of tree-sitter-markdown |
| `src-rust/src/types.rs` | Modify | `MemoryNode.embedding`: `Option<Vec<f32>>` → `Option<Vec<Vec<f32>>>` |
| `src-rust/src/graph.rs` | Modify | `upsert_node`: parse embedding as array-of-arrays |
| `src-rust/src/embedding.rs` | Add function | `embed_chunked()`: splits text, embeds chunks, returns `Vec<Vec<f32>>` |
| `src-rust/src/document_adapter.rs` | **New file** | `DocumentAdapter` trait, `MarkdownAdapter` impl, `get_document_adapter()` factory |
| `src-rust/src/reconciler.rs` | Modify | Add document adapter dispatch path in `reconcile_file`; add `resolve_document_references()` |
| `src-rust/src/rpc.rs` | Modify | `build_bm25_text`: Section branch; `build_embedding_text_from_props`: Section branch; `handle_search`: iterate chunk embeddings; `handle_reconcile` + `handle_upsert_node`: wrap embeddings in `vec![]`, use `embed_chunked` for Sections; **add reverse re-linking pass in `handle_reconcile`** |
| `src-rust/src/main.rs` | Modify | Add `mod document_adapter;` |
| `src/reconciler.ts` | Modify | Add `.md` to `SUPPORTED_EXTENSIONS` |

---

## 6. Testing Plan

### 6.1 Embedding Format
- **Unit test:** `MemoryNode` with `embedding: Some(vec![vec![0.1, 0.2]])` serializes/deserializes correctly.
- **Unit test:** `embed_chunked` with short text returns single-element vector.
- **Unit test:** `embed_chunked` with long text (>400 tokens) returns multiple vectors with overlap.
- **Integration test:** Wipe graph, re-reconcile code files, verify search results unchanged (single-embedding wrapped in `vec![]` produces same cosine similarity).

### 6.2 Markdown Parsing
- **Unit test:** Parse a markdown file with 3 headings → 3 `ParsedSection` structs with correct names, levels, and content boundaries.
- **Unit test:** Heading with no following content → empty `content` string.
- **Unit test:** Duplicate heading names in same file → ID suffixing (`:2`, `:3`).
- **Unit test:** Inline code spans (`` `reconcile_file` ``) extracted as `inline_code_refs`.
- **Unit test:** File path references (`src/reconciler.rs`) extracted as `file_path_refs`.
- **Unit test:** Nested headings — `## Parent` content includes `### Child` content.

### 6.3 REFERENCES Edges
- **Unit test:** Section with `` `reconcile_file` `` inline code → `REFERENCES` edge to `reconcile_file` entity if it exists in graph.
- **Unit test:** No match for inline code → no edge created, no error.
- **Unit test:** Multiple matches for same name → edges to all matching entities.
- **Unit test:** File path reference → `REFERENCES` edge to File node.
- **Unit test:** Same reference mentioned 3 times → only one edge created.

### 6.4 Search
- **Integration test:** Search for "reconciler architecture" → Section entity with that heading appears in results.
- **Integration test:** Search for concept mentioned in paragraph 10 of a long section → Section appears (chunked embedding catches it).
- **Integration test:** Search with `entity_types: ["Section"]` → only Section entities returned.
- **Integration test:** Code entity search results unchanged after embedding format change.

### 6.5 Reconciliation
- **Integration test:** Reconcile a `.md` file → Section entities created with `DECLARED_IN` edges to File node.
- **Integration test:** Modify a `.md` file (change a heading) → old Section deleted, new Section upserted.
- **Integration test:** Delete a `.md` file → all Section entities and their edges removed.
- **Integration test:** `scheduleFull` discovers `.md` files and reconciles them.

### 6.6 Reverse Re-linking
- **Integration test:** Reconcile markdown file referencing `reconcile_file` → modify and re-reconcile `reconciler.rs` → `REFERENCES` edge from Section to new `reconcile_file` entity recreated.
- **Integration test:** Rename a function → old `REFERENCES` edge not recreated (matched_text no longer matches).
- **Integration test:** Delete a function → `REFERENCES` edge not recreated.
- **Integration test:** Re-link pass does not create duplicate edges if edge already exists.
- **Integration test:** Re-link pass skips Sections that were themselves deleted in the same batch.
- **Integration test:** Re-linked edges carry `relinked: true` property.

---

## 7. Implementation Sequencing

### Phase 1: Embedding Format Change
1. Change `MemoryNode.embedding` to `Option<Vec<Vec<f32>>>`.
2. Update `graph.rs` `upsert_node` to parse array-of-arrays.
3. Update `rpc.rs` to wrap single embeddings in `vec![]`.
4. Update `handle_search` to iterate inner vectors.
5. Wipe graph, re-reconcile, verify search works.
6. Add `embed_chunked` to `embedding.rs`.

### Phase 2: Markdown Adapter
7. Add `tree-sitter-markdown` dependency.
8. Create `document_adapter.rs` with trait + `MarkdownAdapter`.
9. Add `mod document_adapter` to `main.rs`.
10. Update `reconciler.rs` with document adapter dispatch + `resolve_document_references`.
11. Update `build_bm25_text` and `build_embedding_text_from_props` for Section type.
12. Update `handle_reconcile` to use `embed_chunked` for Section entities.
13. **Add reverse re-linking pass to `handle_reconcile` (Section 4.13).**
14. Add `.md` to `SUPPORTED_EXTENSIONS` in `src/reconciler.ts`.
15. Wipe graph, re-reconcile, verify markdown sections appear in search and traversal.
16. Verify re-linking: modify a code file, confirm REFERENCES edges from Sections are recreated.

---

## 8. Graph Schema Additions

### New Entity Type

| Entity Type | Fields | Description |
|-------------|--------|-------------|
| `Section` | `name` (heading text), `content` (body text), `metadata` (level, line range) | A markdown section headed by a `#`-heading |

### New Edge Type

| Relationship | From → To | Properties | Description |
|-------------|-----------|------------|-------------|
| `REFERENCES` | Section → Function/Class/File | `match_type`: `"inline_code"` \| `"file_path"`, `matched_text`: string | A documentation section references a code entity |

### Updated Schema Table (for README)

| Node Table | Key | Fields |
|------------|-----|--------|
| `Entity` | `id` | `type` (File \| Function \| Class \| **Section**), `status`, `last_modified`, `metadata` |

| Rel Table | From → To | Properties |
|-----------|-----------|------------|
| `LINKED_TO` | Entity → Entity | `relationship_type` (CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM, **REFERENCES**) |

---

## 9. Review — Gaps and Edge Cases

The following issues were identified during self-review of this spec. Items marked **[MUST FIX]** must be resolved before implementation; items marked **[SHOULD ADDRESS]** are recommended but not blocking.

### 9.1 [MUST FIX] Tokenizer access for `embed_chunked`

**Problem:** `embed_chunked` needs to count tokens to decide chunk boundaries. The `tokenizer` field on `EmbeddingModel` is private. The function as specified takes `&EmbeddingModel` but cannot access the tokenizer.

**Fix:** Make `embed_chunked` a method on `EmbeddingModel` (`impl EmbeddingModel { pub fn embed_chunked(&self, ...) }`), or add a `pub fn count_tokens(&self, text: &str) -> usize` method that exposes tokenization without exposing the tokenizer itself.

### 9.2 [MUST FIX] Name lookup for REFERENCES edge resolution

**Problem:** `MemoryEngine` has no `get_nodes_by_name()` method. The spec says "search the graph for entities where `name == ref`" but doesn't specify how. Iterating `all_nodes()` for each inline code reference would be O(n) per reference.

**Fix:** Use `get_nodes_by_type("Function")` and `get_nodes_by_type("Class")` to get the candidate set (currently ~220 nodes), build a `HashMap<String, Vec<String>>` mapping name → node IDs, then look up each `inline_code_ref` against this map. This is O(n) once per file reconciliation, not per reference. Add a helper method `get_name_index(&self) -> HashMap<String, Vec<String>>` to `MemoryEngine` or build the map locally in `resolve_document_references`.

### 9.3 [MUST FIX] Reconciliation ordering during `scheduleFull`

**Problem:** `scheduleFull` adds files to the sync queue in filesystem walk order (arbitrary). If a markdown file is reconciled before the code file it references, the code entity won't exist yet and no `REFERENCES` edge will be created.

**Fix:** In `scheduleFull` (or `runSync`), process code files first (`.ts`, `.py`, `.rs`, etc.), then markdown files (`.md`) in a second pass. Alternatively, sort `filesToSync` by extension priority before processing. This ensures code entities exist when markdown REFERENCES are resolved.

### 9.4 [SHOULD ADDRESS] Content before the first heading

**Problem:** Markdown files often have content before the first `#` heading (introduction text, description). The spec doesn't define what happens to this content — it's not captured by any Section entity.

**Recommendation:** Ignore it for v1. The File node already exists with the filename as its name. Content before the first heading is typically a one-liner description that's less valuable than headed sections. If needed later, a synthetic "Introduction" section could be created.

### 9.5 [SHOULD ADDRESS] Files with no headings

**Problem:** A markdown file with no headings at all (e.g., a flat `CHANGELOG.md`) would produce zero Section entities. The file would be a bare File node — same as today.

**Recommendation:** Accept this for v1. Headed content is the valuable case. A future enhancement could create a single Section with the filename as the name and the full content as the body.

### 9.6 [SHOULD ADDRESS] YAML frontmatter exclusion

**Problem:** Many markdown files start with YAML frontmatter delimited by `---`. This content would be included in the first section's `content` if the first heading comes after it, or would be uncaptured content before the first heading.

**Recommendation:** Strip frontmatter before parsing. Detect `^---\n...\n---\n` at the start of the file and skip it. Add this to the `MarkdownAdapter::parse_document` pipeline as step 0.

### 9.7 [SHOULD ADDRESS] Unused `inline_link` capture in tree-sitter query

**Problem:** The tree-sitter query includes `(inline_link) @link` but doc-to-doc links are explicitly out of scope. This capture is unused.

**Recommendation:** Remove `(inline_link) @link` from the query. It can be added back when doc-to-doc links are implemented.

### 9.8 [SHOULD ADDRESS] File path regex uses hardcoded extensions

**Problem:** The regex `[a-zA-Z0-9_\-/]+\.(rs|ts|tsx|js|jsx|py|md)` hardcodes supported extensions. When new languages are added via the adapter system, the regex won't pick up their file paths in markdown.

**Recommendation:** For v1, accept the hardcoded list. In the future, build the regex dynamically from `list_languages()` extensions + `.md`. Document this as a known limitation.

### 9.9 [RESOLVED] Dangling REFERENCES when code entities are deleted

**Problem:** When a code file is re-reconciled, its old Function/Class entities are deleted and new ones created. `delete_node` removes all edges (including inbound REFERENCES from Sections). The Section still exists but its REFERENCES edge is gone.

**Resolution:** Section 4.13 adds a reverse re-linking pass to `handle_reconcile`. Before deletion events are applied, inbound REFERENCES edges are snapshotted (Section ID + matched_text). After all events are applied, the pass re-resolves those references against the new graph state and recreates edges. This runs within the same reconcile call — no cross-process coordination needed.

**Remaining limitation:** New functions added to code that a Section *should* reference (but didn't before) are not caught until the markdown file is re-reconciled. This is correct behavior — the Section content didn't change, so there's no new backtick reference to discover.

### 9.10 [SHOULD ADDRESS] Performance impact of chunked embeddings

**Problem:** A project with 10 markdown files, 5 sections each, averaging 1500 tokens per section, would produce ~50 sections × ~4 chunks = 200 ONNX inference calls during full reconcile. Current code entities produce ~220 single calls. This roughly doubles embedding compute time.

**Recommendation:** Acceptable for v1. ONNX inference on CPU for gte-small is ~5-10ms per call, so 200 extra calls ≈ 1-2 seconds. Note this in the spec. If it becomes a problem, batch multiple chunks in a single ONNX forward pass (the model supports batched input).

### 9.11 [SHOULD ADDRESS] `tree-sitter-markdown` crate version

**Problem:** The spec specifies `tree-sitter-markdown = "0.3"` but this hasn't been verified against crates.io.

**Recommendation:** Verify the latest version and API before implementation. The crate may have a different API than expected (e.g., separate `tree-sitter-md` crate name, or different node types).

### 9.12 [NICE TO HAVE] `languages.list` RPC and config

**Problem:** The `languages.list` RPC method returns registered code languages. Markdown is not a "language" with an LSP, so it doesn't fit cleanly. `.yaam/config.json` has per-language enable/disable flags but no markdown entry.

**Recommendation:** Defer for v1. Markdown doesn't need an LSP entry. If config is needed later, add a `"markdown": { "enabled": true }` section to config.json. The `SUPPORTED_EXTENSIONS` list in `reconciler.ts` is the effective toggle for now.

### 9.13 [VERIFIED OK] Search result path extraction for Section IDs

**Concern:** Section IDs are `README.md:Reconciler Architecture`. The search result code uses `id.splitn(2, ':').next()` to extract the file path.

**Result:** `splitn(2, ':')` on `README.md:Reconciler Architecture` yields `["README.md", "Reconciler Architecture"]`. Path = `README.md`. ✅ Works correctly.

### 9.14 [VERIFIED OK] `derive_category` for Section entities

**Concern:** Section IDs contain a markdown file path. Does `derive_category` classify them correctly?

**Result:** `derive_category("README.md")` checks for library markers (`node_modules/`, `target/`, etc.). A project-level `README.md` has none → `"module"`. ✅ Works correctly.

### 9.15 [VERIFIED OK] `handle_upsert_node` embedding path

**Concern:** `handle_upsert_node` (the direct upsert RPC, not reconcile) also computes embeddings at line 212-222. Does the spec cover this?

**Result:** Section 4.8 explicitly mentions: "File: src-rust/src/rpc.rs (handle_upsert_node — the non-reconcile upsert path). Same change: wrap single embeddings in vec![], use embed_chunked for Section entities." ✅ Covered.

### 9.16 [VERIFIED OK] BM25 index initialization on startup

**Concern:** `AppState::new` builds the BM25 index from existing nodes on startup (line 51-57). Will Section nodes be indexed correctly after the `build_bm25_text` change?

**Result:** The startup loop calls `build_bm25_text(node)` for every node. After the spec's change to add a Section branch, Section nodes will be indexed with heading + body. ✅ Covered.

---

## 10. Summary of Review Findings

| # | Issue | Severity | Action |
|---|-------|----------|--------|
| 9.1 | Tokenizer access for `embed_chunked` | MUST FIX | Make it a method on `EmbeddingModel` or add `count_tokens` |
| 9.2 | Name lookup for REFERENCES | MUST FIX | Build name→ID map from `get_nodes_by_type` |
| 9.3 | Reconciliation ordering | MUST FIX | Process code files before markdown in `scheduleFull` |
| 9.4 | Content before first heading | SHOULD ADDRESS | Ignore for v1 |
| 9.5 | Files with no headings | SHOULD ADDRESS | Accept — bare File node |
| 9.6 | YAML frontmatter | SHOULD ADDRESS | Strip before parsing |
| 9.7 | Unused `inline_link` capture | SHOULD ADDRESS | Remove from query |
| 9.8 | Hardcoded file path regex | SHOULD ADDRESS | Accept for v1, document limitation |
| 9.9 | Dangling REFERENCES on code deletion | RESOLVED | Section 4.13: reverse re-linking pass in `handle_reconcile` |
| 9.10 | Chunking performance | SHOULD ADDRESS | ~1-2s extra, acceptable |
| 9.11 | `tree-sitter-markdown` version | SHOULD ADDRESS | Verify before implementation |
| 9.12 | `languages.list` and config | NICE TO HAVE | Defer |
| 9.13 | Search path extraction | VERIFIED OK | — |
| 9.14 | `derive_category` | VERIFIED OK | — |
| 9.15 | `handle_upsert_node` path | VERIFIED OK | — |
| 9.16 | BM25 startup indexing | VERIFIED OK | — |

**Conclusion:** 3 must-fix items (tokenizer access, name lookup, reconciliation ordering) need to be incorporated into the spec before implementation. The remaining items are acceptable for v1 with documented limitations.

---

## 11. Research Comparison

Comparison of this spec against external research on markdown-to-code traceability approaches.

### 11.1 Matching Markdown to Code

| Research Approach | Our Approach | Verdict |
|------------------|-------------|--------|
| **YAML Frontmatter** — explicit `is_satisfied_by`/`depends_on` fields parsed into knowledge graph | Inline-code backtick matching + file path matching | **Complementary.** Frontmatter is higher-precision but requires manual authoring. Inline-code is zero-friction. Consider supporting frontmatter as an optional enhanced signal in a future iteration. |
| **Inline Tagging** — `<!-- @REQ-001@ -->` tags in docs + code comments | Backtick inline code (`` `reconcile_file` ``) | **Our approach is superior for v1.** Backticks are already used by developers. No double-maintenance of tags in both docs and code. |
| **Code-Native Traceability** — generate compiler-enforced Traceable types from specs | Post-hoc graph name-matching | **Different architecture.** Compiler-enforced traceability is the gold standard but requires code generation + workflow changes. Noted as long-term direction, not v1. |

### 11.2 Detecting Changes and Synchronization

| Research Approach | Our Approach | Verdict |
|------------------|-------------|--------|
| **Tree-sitter Parsing** — AST queries for `atx_heading`, `fenced_code_block` | `tree-sitter-markdown` with heading captures | **Aligned.** ✅ |
| **AST Diffing (GumTree)** — compare ASTs to detect moves vs. changes, migrate traceability links | Delete-and-recreate all Sections on re-reconcile | **Acceptable for v1.** Our approach loses Section identity across renames (REFERENCES edges recreated by name-matching). GumTree would preserve identity but adds significant complexity. Noted as future enhancement. |
| **Block-Level Semantic Hashing** — cryptographic hash of AST nodes with literal abstraction | Not applicable | **Overkill for v1.** Only relevant for tracking Sections across massive refactors where both heading AND content change. |

### 11.3 Language Servers

| Research Finding | Our Approach | Verdict |
|-----------------|-------------|--------|
| **Marksman LSP** — markdown LSP with wiki-style `[[link]]` resolution, diagnostics, Go-to-Definition | Graph name-matching (no markdown LSP) | **Marksman doesn't solve our problem.** Its Go-to-Definition is for wiki-links between markdown files, not for resolving code references in markdown. Our graph name-matching is still needed for doc-to-code linking. Marksman could be useful for future doc-to-doc linking (deferred). |
| **Cross-Language LSP Forwarding** — markdown LSP forwards code queries to code LSP (e.g., Rust LSP) | Graph name-matching | **Their approach is more precise but too complex for v1.** LSP forwarding gives scope-aware resolution but requires virtual document synthesis, language detection, and LSP servers that accept prose-context queries. Graph name-matching works for the 90% case. Noted as future precision enhancement. |
| **MCP (Model Context Protocol)** — expose graph to agents for targeted AST node fetching | JSON-RPC tools (`yaam_graph_explore`, `yaam_search`) | **We already do this.** ✅ Our tools serve the same purpose — agents can query the graph, traverse REFERENCES edges, and search for concepts. MCP is a protocol standard; our JSON-RPC is functionally equivalent. |

### 11.4 Key Takeaways

1. **Our core approach is validated.** Tree-sitter + inline-code name-matching is the right v1 tradeoff.
2. **Marksman LSP corrects our assumption** — a markdown LSP exists, but it solves doc-to-doc linking, not doc-to-code. Our graph name-matching remains necessary.
3. **Reverse re-linking (Section 4.13) addresses the main synchronization gap** that the research's AST diffing would solve — but at much lower complexity.
4. **Code-native traceability is the most robust long-term direction** but is a fundamentally different architecture, not an incremental enhancement.
5. **Frontmatter explicit linking is worth considering** as an optional high-confidence signal in a future iteration, layered on top of our heuristic matching.