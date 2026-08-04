---
name: yaam-code-discovery
description: Use YAAM's hybrid search and graph exploration as the primary method for discovering and navigating code. Always search the graph first before manually reading files. This skill ensures you leverage the repo's own memory engine instead of blind grep/read cycles.
---

# YAAM Code Discovery

This skill provides procedural guidance for using YAAM's own search and graph tools as the **first and primary method** for code discovery, navigation, and impact analysis. The agent should default to `yaam_search` and `yaam_graph_explore` before falling back to manual `read`, `grep`, or `bash` file exploration.

## Why This Matters

YAAM maintains a live, in-memory graph of every function, class, file, and relationship in the repository — backed by tree-sitter parsing, LSP resolution, BM25 keyword indexing, and ONNX semantic embeddings. This graph is **automatically reconciled** after every file operation.

When you manually `grep` and `read` files to answer questions about the codebase, you're doing work the engine has already done — and doing it worse. The graph knows:

- **What functions exist** and where they're declared
- **Who calls whom** (cross-file, resolved via LSP)
- **What imports what** (file-level dependency graph)
- **Semantic similarity** between concepts (e.g., "embedding" matches "vectorize" even without keyword overlap)
- **DocComments and section bodies** indexed for keyword and semantic search

Using the graph is faster, more targeted, and produces better answers.

## Core Principle

> **Search the graph first. Read files only to fill gaps the graph can't cover.**

## Decision Flowchart

```
Question about the codebase?
│
├── "How does X work?" or "Where is X implemented?"
│   └──► yaam_search(text="X concept", snippet="auto")
│        └──► Read the specific files/functions surfaced by results
│             (not the entire codebase)
│
├── "Who calls X?" or "What's the impact of changing X?"
│   └──► yaam_graph_explore(match={id:"X"}, traverse={relationship:"CALLS", direction:"inbound"})
│
├── "What does X import / depend on?"
│   └──► yaam_graph_explore(match={id:"X"}, traverse={relationship:"IMPORTS", direction:"outbound"})
│
├── "What functions/classes are in file X?"
│   └──► yaam_graph_explore(match={label:"Entity", entity_type:"Function"}, where={edge_to:{id:"X", relationship:"DECLARED_IN"}})
│
├── "Give me an overview of the codebase"
│   └──► yaam_graph_explore(match={label:"Entity"}, aggregate={group_by:"type", count:true})
│        └──► Then yaam_search for specific subsystems
│
└── "What changed recently?" or "What's the active workspace?"
    └──► yaam_graph_explore(match={label:"Workspace", status:"active"})
         └──► yaam_graph_explore(traverse={relationship:"HAS_SCRATCHPAD", direction:"outbound"})
```

## Workflows

### 1. Understanding a Concept or Subsystem

**Instead of:** grepping for keywords and reading 5 files blindly.

**Do this:**
```
yaam_search(text="embedding chunking ONNX model", snippet="auto", top_k=10)
```
The `snippet="auto"` parameter extracts the best-matching passage from each result, so you get relevant code context directly in the search results — often enough to answer without reading the full file.

If the snippets are insufficient, read **only the specific files** surfaced by the top results.

### 2. Tracing Call Chains and Impact Analysis

**Instead of:** reading a function, manually searching for its name in other files, reading those files...

**Do this:**
```json
// Forward: what does this function call?
yaam_graph_explore({
  "match": {"id": "src-rust/src/rpc.rs::handle_search"},
  "traverse": {"relationship": "CALLS", "direction": "outbound", "max_depth": 3}
})

// Reverse: who calls this function? (impact analysis)
yaam_graph_explore({
  "match": {"id": "src-rust/src/embedding.rs::embed"},
  "traverse": {"relationship": "CALLS", "direction": "inbound", "max_depth": 2}
})
```

### 3. Finding Code by Natural Language

**Instead of:** guessing function names and grepping.

**Do this:**
```
yaam_search(text="file reconciliation logic", entity_types=["Function","Class"], snippet="auto")
```
This finds functions related to reconciliation even if their names don't contain that word — the ONNX semantic embeddings match by meaning.

### 4. Scoping to a Specific Area

**Instead of:** listing all files and reading each one.

**Do this:**
```
// All functions in a specific file
yaam_graph_explore({
  "match": {"label": "Entity", "entity_type": "Function"},
  "where": {"edge_to": {"id": "src-rust/src/search.rs", "relationship": "DECLARED_IN"}}
})

// Search within a directory only
yaam_search(text="error handling", include_paths=["src-rust/"], snippet="auto")
```

### 5. Getting a Codebase Overview

**Instead of:** reading the README and listing files.

**Do this:**
```json
// Entity counts by type
yaam_graph_explore({
  "match": {"label": "Entity"},
  "aggregate": {"group_by": "type", "count": true}
})

// All files
yaam_graph_explore({
  "match": {"label": "Entity", "entity_type": "File"},
  "return_fields": ["id", "name"]
})
```

### 6. Combined Search + Graph Traversal

For deep investigation, combine both tools:

```
Step 1: yaam_search(text="RPC request handling", snippet="auto")
Step 2: yaam_graph_explore({
  "match": {"id": "<top result from search>"},
  "traverse": {"relationship": "CALLS", "direction": "outbound", "max_depth": 2}
})
```

This gives you both **what** (semantic match) and **how** (call graph context).

## When to Fall Back to Manual Reading

The graph is not a replacement for reading code — it's a **navigation accelerator**. You should still read files when:

1. **You need to see exact line numbers and surrounding context** that snippets don't provide
2. **You're about to edit a file** — always read the current state before editing
3. **The graph hasn't reconciled yet** for a file you just created (though reconciliation is automatic and fast)
4. **You need to see non-code files** (config, data, etc.) that aren't indexed

But even in these cases, **use the graph first to identify which files to read** — don't grep blindly.

## Anti-Patterns to Avoid

| ❌ Don't | ✅ Do |
|---------|-------|
| `grep -rn "embed" src/` | `yaam_search(text="embed", snippet="auto")` |
| Read 5 files to find where a concept lives | `yaam_search(text="concept", snippet="auto")` then read 1-2 files |
| Manually trace function calls by reading each file | `yaam_graph_explore` with `CALLS` traversal |
| List all files and read each to understand structure | `yaam_graph_explore` with `aggregate` or `DECLARED_IN` queries |
| Guess function names and search for them | `yaam_search` with natural language |

## Best Practices

1. **Always start with `yaam_search`** when asked "how does X work" or "where is X". The snippet parameter gives you immediate context.

2. **Use `yaam_graph_explore` for structural questions** — call graphs, import dependencies, entity counts. Search finds *what*; the graph finds *how they connect*.

3. **Chain the tools**: search to find relevant entities → graph explore to understand their relationships → read only the specific functions you need to modify.

4. **Use `entity_types` filter** to narrow results: `["Function"]` for implementations, `["Class"]` for types/structs, `["Section"]` for documentation.

5. **Use `include_paths` / `exclude_paths`** to scope to your own code: `exclude_paths=["node_modules/", "target/", ".venv/"]`.

6. **Use `traverse` in search** to get graph context alongside search results — neighbor nodes are included directly in the response.

7. **Use `diversity_lambda`** when you want diverse results: `0.5` balances relevance with avoiding multiple results from the same file.

## Quick Reference

| Question Type | Tool | Example |
|---------------|------|---------|
| "How does X work?" | `yaam_search` | `text="X", snippet="auto"` |
| "Where is X defined?" | `yaam_search` | `text="X", entity_types=["Function","Class"]` |
| "Who calls X?" | `yaam_graph_explore` | `match={id:"X"}, traverse={relationship:"CALLS", direction:"inbound"}` |
| "What does X call?" | `yaam_graph_explore` | `match={id:"X"}, traverse={relationship:"CALLS", direction:"outbound"}` |
| "What's in file X?" | `yaam_graph_explore` | `match={entity_type:"Function"}, where={edge_to:{id:"X", relationship:"DECLARED_IN"}}` |
| "Codebase overview" | `yaam_graph_explore` | `match={label:"Entity"}, aggregate={group_by:"type", count:true}` |
| "Find by concept" | `yaam_search` | `text="error handling pattern", snippet="auto"` |
| "Active workspace?" | `yaam_graph_explore` | `match={label:"Workspace", status:"active"}` |