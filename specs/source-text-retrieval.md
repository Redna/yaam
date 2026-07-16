# Specification: Source Text Retrieval for Code Entities

**Status:** Draft  
**Date:** 2025-07-05  
**Depends on:** Chunked embeddings infrastructure (`embed_chunked`, `Vec<Vec<f32>>` format — already implemented)

---

## 1. Overview

YAAM currently stores only the **name** and **line number** for code entities (Functions, Classes). The `content` field — which is already returned by both `yaam_search` and `yaam_graph_explore` — is empty for all code entities. This forces the model into a two-step workflow:

```
1. yaam_search("chunking embeddings")  → finds "embed_chunked" by name only
2. bash: grep -n "embed_chunked" ...   → finds the line number
3. read: embedding.rs offset=78 limit=80 → gets the actual code
```

This spec proposes extracting the **full source text** of each declaration during tree-sitter parsing and storing it in the `content` field. This eliminates the `grep` + `read` round-trip and, as a side effect, dramatically improves search recall — the function body text will be indexed by BM25 and embedded by the model, so queries about behavior ("chunking", "token limit", "splitting") will match against the actual code.

### Current State (Critical Finding)

The `build_bm25_text` function has a branch for code entities that reads `metadata.docComment`:

```rust
if !node.metadata.is_empty() {
    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
        if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
            parts.push(doc.to_string());
        }
    }
}
```

**This is dead code.** The `metadata` field is never populated for code entities during reconciliation — it is only set for markdown `Section` entities. As a result:

- Code entity BM25 text = `name` only (e.g., `"embed_chunked"`)
- Code entity embedding text = `name` only

A search for "chunking embeddings token limit" could not find `embed_chunked` because the only indexed text was the two tokens `"embed"` and `"chunked"`. The function body — which contains `max_tokens`, `overlap_tokens`, `paragraphs`, `split_sentences`, `overlap_text`, and extensive logic about chunking — was invisible to both BM25 and semantic search.

---

## 2. Goals

1. **Search results include implementation** — `yaam_search` and `yaam_graph_explore` return the full source text of functions and classes in the `content` field, eliminating the need for follow-up `grep` + `read`.
2. **Search recall improves** — BM25 and embeddings see the actual code, not just the function name. Queries about behavior match against implementation details.
3. **No new storage fields** — Reuses the existing `content` field on `MemoryNode`, which is already returned by all query paths.
4. **No new dependencies** — Tree-sitter already provides the AST node with byte ranges; extracting text is `node.utf8_text(source_code)`.
5. **Chunked embeddings for long functions** — Functions exceeding 400 tokens get multiple embedding vectors via the existing `embed_chunked` infrastructure.

---

## 3. Scope

### In Scope
- Extract full declaration source text during `parse_file`
- Store source text in the `content` property of code entity upsert events
- Update `build_bm25_text` to index source text for code entities
- Update `build_embedding_text_from_props` to embed source text for code entities
- Use `embed_chunked` for all entity types (not just Sections)
- Add a new `LanguageAdapter` method for extracting the declaration node from a name capture

### Out of Scope (Deferred)
- Extracting doc comments separately from source text (the source text includes them)
- Storing chunk text alongside chunk embeddings (deferred — see Section 9.1)
- Syntax highlighting or code formatting in search results
- Truncating very long functions in search results (full text returned)
- Storing source text for `File` entity type (only Function and Class)

---

## 4. Architecture

### 4.1 Tree-Sitter Capture → Declaration Node

**Problem:** The tree-sitter query captures the **name** node (e.g., the `identifier` inside a `function_item`), not the full declaration. We need to walk up to the parent declaration node to get the full source text.

**Current flow in `parse_file`:**

```rust
for capture in m.captures {
    let capture_name = query.capture_names()[capture.index as usize];
    let node = capture.node;  // ← this is the NAME node (identifier)
    let name = node.utf8_text(source_code).unwrap_or("").to_string();
    
    if capture_name.starts_with("function") || capture_name.starts_with("class") {
        declarations.push(ParsedDeclaration {
            id,
            entity_type,
            name,
            line: node.start_position().row + 1,
            // ← no source text extracted
        });
    }
}
```

**New flow:** Walk up from the name capture to the enclosing declaration node, then extract its full text.

The declaration node kinds vary by language:

| Language | Capture Name | Declaration Node Kind | `node.parent()` |
|----------|-------------|---------------------|-----------------|
| TypeScript | `function.name` | `function_declaration` | Direct parent |
| TypeScript | `method.name` | `method_definition` | Direct parent |
| TypeScript | `class.name` | `class_declaration` | Direct parent |
| TypeScript | `variable.name` | `variable_declarator` | Direct parent (contains arrow function) |
| Python | `function.name` | `function_definition` | Direct parent |
| Python | `class.name` | `class_definition` | Direct parent |
| Rust | `function.name` | `function_item` | Direct parent |
| Rust | `class.name` (struct) | `struct_item` | Direct parent |
| Rust | `class.name` (enum) | `enum_item` | Direct parent |
| Rust | `class.name` (trait) | `trait_item` | Direct parent |

In all cases, the name capture's **direct parent** is the declaration node. This is because the tree-sitter queries capture the `name` field of the declaration node, which is always a direct child.

**New `LanguageAdapter` method:**

```rust
/// Given a name capture node, return the enclosing declaration node
/// whose full source text should be stored as the entity's content.
///
/// For most languages, this is simply `node.parent()`. Override if
/// the declaration node is further up the tree.
fn declaration_node<'a>(&self, name_node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    name_node.parent()
}
```

This is a default-implemented trait method — each adapter can override it if needed, but the default `name_node.parent()` works for all current languages.

### 4.2 ParsedDeclaration Changes

**File:** `src-rust/src/reconciler.rs`

```rust
pub struct ParsedDeclaration {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    pub line: usize,
    pub source_text: String,  // ← NEW: full declaration source text
}
```

### 4.3 parse_file Changes

**File:** `src-rust/src/reconciler.rs`

In the declaration branch of the capture loop:

```rust
} else {
    let entity_type = if capture_name.starts_with("class") {
        "Class"
    } else {
        "Function"
    };

    let id = format!("{}:{}", file_path.display(), name);

    // Extract full source text from the enclosing declaration node.
    let source_text = adapter
        .declaration_node(node)
        .and_then(|decl_node| decl_node.utf8_text(source_code).ok())
        .map(|s| s.to_string())
        .unwrap_or_default();

    declarations.push(ParsedDeclaration {
        id,
        entity_type: entity_type.to_string(),
        name,
        line: node.start_position().row + 1,
        source_text,  // ← NEW
    });
}
```

### 4.4 Upsert Property Changes

**File:** `src-rust/src/reconciler.rs`

In the declaration upsert loop, add `content` to the entity properties:

```rust
for decl in declarations {
    let mut entity_props = HashMap::new();
    entity_props.insert("entity_type".to_string(), serde_json::Value::String(decl.entity_type.clone()));
    entity_props.insert("name".to_string(), serde_json::Value::String(decl.name.clone()));
    entity_props.insert("line".to_string(), serde_json::Value::Number(serde_json::Number::from(decl.line)));
    entity_props.insert("status".to_string(), serde_json::Value::String("active".to_string()));
    entity_props.insert("last_modified".to_string(), serde_json::Value::Number(serde_json::Number::from(timestamp)));
    
    // ← NEW: store full source text
    entity_props.insert(
        "content".to_string(),
        serde_json::Value::String(decl.source_text.clone()),
    );

    events.push(Event { ... });
    // ... DECLARED_IN edge (unchanged) ...
}
```

The `graph.rs` `upsert_node` function already extracts `content` from properties:

```rust
let content = extract_string_or(props, "content", "");
```

So the source text flows automatically into `MemoryNode.content` → returned by search and graph explore.

### 4.5 BM25 Text Changes

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
                parts.push(node.content.clone());
            } else if entity_type == "Function" || entity_type == "Class" {
                // ← NEW: index full source text for code entities
                if !node.content.is_empty() {
                    parts.push(node.content.clone());
                }
                // Also index docComment from metadata if present (existing behavior,
                // but now metadata is typically empty for code entities — source
                // text subsumes any doc comment that would have been there).
                if !node.metadata.is_empty() {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
                        if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
                            parts.push(doc.to_string());
                        }
                    }
                }
            } else {
                // File entities and other types: existing behavior
                if !node.metadata.is_empty() {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.metadata) {
                        if let Some(doc) = meta.get("docComment").and_then(|v| v.as_str()) {
                            parts.push(doc.to_string());
                        }
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
```

**Rationale for keeping the docComment path:** The metadata `docComment` path is currently dead code for code entities (metadata is never set), but keeping it ensures forward compatibility if we later add doc comment extraction. The source text includes any doc comments anyway, so there's no harm — at worst the same text is indexed twice, which BM25 handles fine (term frequency just increments).

### 4.6 Embedding Text Changes

**File:** `src-rust/src/rpc.rs` (`build_embedding_text_from_props`)

```rust
"Entity" => {
    let entity_type = props.get("entity_type").and_then(|v| v.as_str()).unwrap_or("");
    if entity_type == "Section" {
        let content = props.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if content.is_empty() {
            name.to_string()
        } else {
            format!("{} {}", name, content)
        }
    } else if entity_type == "Function" || entity_type == "Class" {
        // ← NEW: embed name + full source text
        let content = props.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if content.is_empty() {
            name.to_string()
        } else {
            format!("{} {}", name, content)
        }
    } else {
        // File entities and other types: existing behavior (name + docComment)
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
```

### 4.7 Embedding Computation: Use `embed_chunked` for All Entities

**File:** `src-rust/src/rpc.rs` (`handle_reconcile` and `handle_upsert_node`)

Currently, only `Section` entities use `embed_chunked`. With full source text, code entities can also exceed 400 tokens (a 50-line function is ~300-500 tokens). Change the branching to use `embed_chunked` for all entity types:

**Before:**

```rust
if entity_type == "Section" {
    match embedder.embed_chunked(&text, 400, 50) { ... }
} else {
    match embedder.embed(&text) { ... }  // ← single embedding, gets truncated
}
```

**After:**

```rust
// All entity types use embed_chunked — it falls back to a single
// embedding for text under max_tokens, so there's no overhead for
// short functions.
match embedder.embed_chunked(&text, 400, 50) {
    Ok(vectors) => {
        payload.properties.insert("embedding".to_string(), serde_json::json!(vectors));
    }
    Err(e) => {
        eprintln!("Failed to compute embedding for {}: {}", payload.id, e);
    }
}
```

This simplifies the code — no branching on entity type. `embed_chunked` already handles the short-text case efficiently (it calls `embed()` once and returns `vec![single_embedding]`).

**Apply this change in both embedding paths:**
1. `handle_reconcile` (line ~677)
2. `handle_upsert_node` (line ~234)

### 4.8 LanguageAdapter Trait Change

**File:** `src-rust/src/language_adapter.rs`

Add a default-implemented method to the `LanguageAdapter` trait:

```rust
/// Given a name capture node from a tree-sitter query, return the
/// enclosing declaration node whose full source text should be stored
/// as the entity's `content`.
///
/// Default implementation returns the direct parent, which is correct
/// for all current languages (TypeScript, Python, Rust) because the
/// query captures the `name` field of the declaration node.
fn declaration_node<'a>(
    &self,
    name_node: tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    name_node.parent()
}
```

No adapter overrides are needed for the current languages. The default `name_node.parent()` works because:

- **TypeScript `function.name`**: capture is `(identifier)` inside `(function_declaration name: (identifier) @function.name)` → parent is `function_declaration` ✅
- **TypeScript `method.name`**: capture is `(property_identifier)` inside `(method_definition name: (property_identifier) @method.name)` → parent is `method_definition` ✅
- **TypeScript `class.name`**: capture is `(type_identifier)` inside `(class_declaration name: (type_identifier) @class.name)` → parent is `class_declaration` ✅
- **TypeScript `variable.name`**: capture is `(identifier)` inside `(variable_declarator name: (identifier) @variable.name value: [(arrow_function) (function_expression)])` → parent is `variable_declarator` ✅ (includes the arrow function body)
- **Python `function.name`**: capture is `(identifier)` inside `(function_definition name: (identifier) @function.name)` → parent is `function_definition` ✅
- **Python `class.name`**: capture is `(identifier)` inside `(class_definition name: (identifier) @class.name)` → parent is `class_definition` ✅
- **Rust `function.name`**: capture is `(identifier)` inside `(function_item name: (identifier) @function.name)` → parent is `function_item` ✅
- **Rust `class.name` (struct)**: capture is `(type_identifier)` inside `(struct_item name: (type_identifier) @class.name)` → parent is `struct_item` ✅
- **Rust `class.name` (enum)**: same pattern → parent is `enum_item` ✅
- **Rust `class.name` (trait)**: same pattern → parent is `trait_item` ✅

---

## 5. File-by-File Change Inventory

| File | Change Type | Description |
|------|------------|-------------|
| `src-rust/src/language_adapter.rs` | Add method | `declaration_node()` default method on `LanguageAdapter` trait |
| `src-rust/src/reconciler.rs` | Modify struct | Add `source_text: String` field to `ParsedDeclaration` |
| `src-rust/src/reconciler.rs` | Modify function | `parse_file`: extract source text via `adapter.declaration_node(node)` |
| `src-rust/src/reconciler.rs` | Modify function | Declaration upsert loop: add `content` property from `decl.source_text` |
| `src-rust/src/rpc.rs` | Modify function | `build_bm25_text`: index `content` for Function/Class entities |
| `src-rust/src/rpc.rs` | Modify function | `build_embedding_text_from_props`: embed `content` for Function/Class entities |
| `src-rust/src/rpc.rs` | Modify function | `handle_reconcile`: use `embed_chunked` for all entity types (remove Section-only branching) |
| `src-rust/src/rpc.rs` | Modify function | `handle_upsert_node`: same `embed_chunked` change |

---

## 6. Data Flow (Before vs After)

### Before

```
parse_file()
  → ParsedDeclaration { name: "embed_chunked", line: 78 }
  → UpsertNode { properties: { name, entity_type, line, status, last_modified } }
  → MemoryNode { content: "" }
  → build_bm25_text → "embed_chunked" (2 tokens)
  → build_embedding_text → "embed_chunked" (2 tokens)
  → embed() → single vector from 2 tokens

Search result: { name: "embed_chunked", content: "" }
Model must: grep + read to see implementation
```

### After

```
parse_file()
  → ParsedDeclaration { name: "embed_chunked", line: 78, source_text: "pub fn embed_chunked(&self, text: &str, max_tokens: usize, ...) { ... }" }
  → UpsertNode { properties: { name, entity_type, line, status, last_modified, content: source_text } }
  → MemoryNode { content: "pub fn embed_chunked(&self, text: &str, ..." }
  → build_bm25_text → "embed_chunked pub fn embed_chunked(&self, text: &str, max_tokens: usize, overlap_tokens: usize) -> Result<Vec<Vec<f32>>... }"
  → build_embedding_text → "embed_chunked pub fn embed_chunked(&self, text: &str, max_tokens: usize, ..."
  → embed_chunked() → multiple vectors (if >400 tokens) or single vector

Search result: { name: "embed_chunked", content: "pub fn embed_chunked(&self, text: &str, ..." }
Model has: full implementation directly — no grep/read needed
```

---

## 7. Testing Plan

### 7.1 Source Text Extraction

- **Unit test:** Parse a Rust file with a `pub fn embed_chunked(...)` → `ParsedDeclaration.source_text` contains the full function body from `pub fn` to the closing `}`.
- **Unit test:** Parse a TypeScript file with `const foo = () => { ... }` → `source_text` contains the full variable declarator including the arrow function body.
- **Unit test:** Parse a Python file with `def foo(): ...` → `source_text` contains the full function definition including the body.
- **Unit test:** Parse a file with a class declaration → `source_text` contains the full class including all methods.
- **Unit test:** Parse a file with a function that has no body (e.g., a forward declaration or abstract method) → `source_text` is whatever the declaration node contains (may be just the signature).

### 7.2 BM25 Indexing

- **Unit test:** Create a `MemoryNode` with `entity_type: "Function"` and `content: "pub fn embed_chunked(&self, text: &str, max_tokens: usize, ..."` → `build_bm25_text` returns `"embed_chunked pub fn embed_chunked(&self, text: &str, max_tokens: usize, ..."`.
- **Integration test:** Reconcile a Rust file → search for "max_tokens overlap" → function containing those parameter names appears in results (previously would not because only the name was indexed).

### 7.3 Embedding

- **Unit test:** `build_embedding_text_from_props` for a Function entity with `content` set → returns `"name content"`.
- **Unit test:** `build_embedding_text_from_props` for a Function entity with empty `content` → returns just `"name"` (backward compatible).
- **Integration test:** Reconcile a file with a long function (>400 tokens) → node has multiple embedding vectors (chunked).

### 7.4 Search Retrieval

- **Integration test:** Reconcile a code file → `yaam_search` returns results with `content` field populated with full source text.
- **Integration test:** `yaam_graph_explore` with `return_fields: ["id", "name", "content"]` → `content` contains source text for Function/Class entities.
- **Integration test:** Search for a concept mentioned only in function body (not in function name) → function appears in results.

### 7.5 Backward Compatibility

- **Integration test:** Wipe graph, re-reconcile all files → search results for code queries return more relevant results than before (due to richer BM25 + embedding text).
- **Integration test:** Existing `yaam_graph_explore` queries that don't request `content` field → unaffected (content is only returned when requested or by default in search).

---

## 8. Implementation Sequencing

### Phase 1: Source Text Extraction
1. Add `declaration_node()` default method to `LanguageAdapter` trait.
2. Add `source_text: String` field to `ParsedDeclaration`.
3. Update `parse_file` to extract source text via `adapter.declaration_node(node)`.
4. Update declaration upsert loop to include `content` property.
5. Write unit tests for source text extraction (all 3 languages).

### Phase 2: Search and Embedding Enrichment
6. Update `build_bm25_text` to index `content` for Function/Class entities.
7. Update `build_embedding_text_from_props` to embed `content` for Function/Class entities.
8. Update `handle_reconcile` and `handle_upsert_node` to use `embed_chunked` for all entity types.
9. Wipe graph, re-reconcile, verify search finds functions by body content.

### Phase 3: Verification
10. Integration test: search for "chunking token limit" → `embed_chunked` appears in results.
11. Integration test: search result `content` field contains full function implementation.
12. Integration test: long functions get chunked embeddings (multiple vectors).
13. Verify no regressions in existing search behavior.

---

## 9. Review — Gaps and Edge Cases

### 9.1 [DEFERRED] Storing chunk text alongside chunk embeddings

**Context:** The user initially asked whether storing the text of each chunk (not just the embedding vector) would help. This spec stores the **full** source text in `content`, but does not store per-chunk text alongside per-chunk embeddings.

**Why deferred:** The full text is already in `content` and returned by search. Per-chunk text would enable snippet extraction ("this chunk matched because..."), but that's a UX enhancement, not a retrieval necessity. The model gets the full implementation from `content` regardless of which chunk matched.

**Future:** If snippet extraction becomes valuable, add a `chunk_texts: Vec<String>` field alongside `embedding: Vec<Vec<f32>>`, storing the text that produced each embedding vector.

### 9.2 [SHOULD ADDRESS] TypeScript arrow function variable declarations

**Problem:** The TypeScript query captures `variable.name` for `const foo = () => { ... }`. The parent node is `variable_declarator`, which includes the name, `=`, and the arrow function body. But it does **not** include the `const` keyword — that's on the parent `lexical_declaration` node.

**Impact:** The source text for arrow function variables will start with `foo = () => { ... }` instead of `const foo = () => { ... }`. This is a minor cosmetic issue — the implementation body is fully captured.

**Recommendation:** Accept for v1. The `const`/`let`/`var` keyword is not semantically significant for search or retrieval. If needed later, override `declaration_node` in `TypeScriptAdapter` to walk up two levels for variable declarations.

### 9.3 [SHOULD ADDRESS] Storage and memory impact

**Problem:** Storing full source text for ~220 code entities (current graph size) adds significant text to both the JSONL event log and in-memory graph. A typical function is 20-100 lines (~200-2000 bytes). 220 entities × ~1000 bytes average = ~220KB extra in memory and in the JSONL.

**Assessment:** Negligible. The JSONL file is already several MB with embeddings (each embedding is 384 floats × 4 bytes = ~1.5KB per entity). Source text adds ~10-20% to the JSONL size. In-memory, `MemoryNode.content` is already a `String` that's empty for code entities — it just gets longer.

### 9.4 [SHOULD ADDRESS] BM25 document length normalization

**Problem:** BM25 normalizes by document length. Adding full source text makes code entity documents much longer (from 2 tokens to 50-500 tokens). This changes the BM25 scoring landscape — short-named functions that happen to contain query terms in their body will score differently than before.

**Assessment:** This is a feature, not a bug. The current behavior (only name indexed) is strictly worse — it produces false negatives (relevant functions not found). Longer documents may slightly reduce BM25 scores for individual term matches (due to length normalization), but the hybrid search combines BM25 with cosine similarity, so the net effect should be improved recall without precision loss.

### 9.5 [SHOULD ADDRESS] Embedded code in source text

**Problem:** Function source text contains code syntax (`fn`, `pub`, `->`, `&self`, `{`, `}`, etc.) that will be tokenized by BM25. The BM25 tokenizer strips non-alphanumeric characters and splits on camelCase/snake_case, so `&self` becomes `self`, `Vec<Vec<f32>>` becomes `vec`, `f32` becomes `f32`, etc. This is noisy but not harmful — the signal (parameter names, function calls, variable names) outweighs the noise.

**Assessment:** Acceptable. The tokenizer already handles code identifiers well (camelCase splitting, snake_case splitting). Syntax keywords like `fn`, `pub`, `let` will become low-value tokens with low IDF scores (they appear in most documents), so they won't dominate search results.

### 9.6 [VERIFIED OK] graph.rs upsert_node already handles content

**Concern:** Does `graph.rs` `upsert_node` correctly extract and store the `content` property?

**Result:** Yes — line 73: `let content = extract_string_or(props, "content", "");`. This already works for Sections and will work identically for code entities. ✅

### 9.7 [VERIFIED OK] Search results already return content

**Concern:** Do `yaam_search` and `yaam_graph_explore` already return the `content` field?

**Result:** 
- `handle_search` (rpc.rs line 565): `"content": node.content` ✅
- `project_node` (query_dsl.rs line 296): `"content": node.content` ✅

Both already return `content` in their JSON response. No changes needed. ✅

### 9.8 [VERIFIED OK] embed_chunked handles short text

**Concern:** Using `embed_chunked` for all entities — does it add overhead for short functions?

**Result:** No — `embed_chunked` checks `count_tokens(text) <= max_tokens` first and returns `vec![self.embed(text)?]` immediately. The only overhead is one `count_tokens` call (tokenizer encoding without model inference). For a 10-line function, this is negligible. ✅

### 9.9 [VERIFIED OK] Existing docComment path is dead code

**Concern:** Will removing the docComment branch break anything?

**Result:** The `metadata` field is never populated for code entities during reconciliation (confirmed by grep — no code sets metadata for Function/Class entities). The docComment branch in `build_bm25_text` and `build_embedding_text_from_props` is dead code for code entities. Keeping it (as the spec does) is harmless — it's just a no-op since metadata is empty. Removing it would also be fine. ✅

### 9.10 [SHOULD ADDRESS] Reconciliation performance

**Problem:** With full source text, `embed_chunked` will be called for every code entity (currently ~220). Most functions are under 400 tokens and will get a single embedding (same as before). But some long functions will get multiple chunks, adding ONNX inference calls.

**Assessment:** Acceptable. The current graph has ~220 code entities. Even if 20% exceed 400 tokens and average 3 chunks each, that's ~44 extra ONNX calls at ~5-10ms each = ~0.5 seconds. Total reconciliation time goes from ~2 seconds to ~2.5 seconds. Negligible.

---

## 10. Summary of Review Findings

| # | Issue | Severity | Action |
|---|-------|----------|--------|
| 9.1 | Per-chunk text storage | DEFERRED | Store full text in `content` for now; per-chunk text is a future UX enhancement |
| 9.2 | TypeScript arrow function missing `const` keyword | SHOULD ADDRESS | Accept for v1 — cosmetic, implementation body is captured |
| 9.3 | Storage and memory impact | SHOULD ADDRESS | Negligible (~220KB extra) |
| 9.4 | BM25 document length normalization | SHOULD ADDRESS | Feature, not bug — improved recall |
| 9.5 | Code syntax in BM25 tokens | SHOULD ADDRESS | Acceptable — signal outweighs noise |
| 9.6 | graph.rs content extraction | VERIFIED OK | — |
| 9.7 | Search results return content | VERIFIED OK | — |
| 9.8 | embed_chunked overhead for short text | VERIFIED OK | — |
| 9.9 | docComment dead code | VERIFIED OK | — |
| 9.10 | Reconciliation performance | SHOULD ADDRESS | ~0.5s extra, negligible |

**Conclusion:** No must-fix items. All should-address items are acceptable for v1 with documented limitations. The change is low-risk — it reuses existing fields (`content`), existing infrastructure (`embed_chunked`), and existing return paths (search and graph explore already return `content`).