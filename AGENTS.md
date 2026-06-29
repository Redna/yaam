# YAAM — Agent Instructions

All agents working in this repo MUST use YAAM tools to maintain continuity and structural awareness.

## Tools

| Tool | Purpose |
|------|---------|
| `yaam_graph_explore` | Read-only JSON DSL queries against the in-memory Rust graph engine |
| `yaam_workspace_initialize` | Create a task tracking workspace (deactivates previous) |
| `yaam_workspace_append_note` | Record decisions/insights to a workspace scratchpad |
| `/yaam` command | Show memory status (entity counts, workspace, notes, reconciler) |
| `/yaam viz` | Launch interactive web-based topology visualizer at localhost:3456 |

## Mandatory Workflows

1. **Before architectural changes:** query the graph to understand existing structure.
2. **When starting a task:** initialize a workspace.
3. **After key decisions:** append a note explaining "why", not just "what".

## Architecture

### Rust Engine (Standalone Daemon)

The backend is a compiled Rust binary (`src-rust/`) communicating over `stdio` JSON-RPC 2.0. It uses:

- **Append-only JSONL storage** (`events.jsonl`) with `fs2` file locks for concurrent safety.
- **In-memory graph** (`Arc<RwLock<MemoryEngine>`) — all nodes and edges reside in RAM for instant queries.
- **Tree-sitter + LSP reconciliation** — parses TypeScript/JavaScript ASTs and resolves cross-file references via `typescript-language-server`.
- **Hybrid search** — BM25 inverted index + ONNX `gte-small` dense embeddings (38M params, CPU-only).

### TypeScript Extension (Pi)

`src/index.ts` is the pi extension entry point. It spawns the Rust engine as a child process and communicates via JSON-RPC over stdio.

- **`src/engine-client.ts`** — `YaamEngineClient` wraps the stdio JSON-RPC protocol.
- **`src/reconciler.ts`** — Debounced file sync orchestrator. Walks the project for `.ts`/`.js` files and sends them to the Rust engine for parsing.
- **`src/workspace.ts`** — Workspace initialization, note appending, and file-access tracking.
- **`src/graph_explore.ts`** — Thin wrapper that sends DSL queries to the engine and handles result spooling.
- **`src/visualizer.ts`** — Express + Cytoscape.js web UI served at localhost:3456.

### Reconciliation

The TypeScript `Reconciler` class debounces file changes (1s) and sends file contents to the Rust engine's `reconcile` RPC method. The Rust reconciler:

1. **Parses** the file with tree-sitter to extract declarations (functions, classes) and references (calls, imports).
2. **Resolves** cross-file references via LSP `textDocument/definition` requests.
3. **Generates** `UPSERT_NODE`, `LINK_NODES`, `DELETE_NODE` events.
4. **Applies** events to both the JSONL log and the in-memory graph.

Progress is polled every 250ms and displayed in the pi status bar.

### Optimistic Writes

`yaam_workspace_initialize` and `yaam_workspace_append_note` return immediately — the actual JSON-RPC call to the engine happens in the background (fire-and-forget).

## Graph Schema

### Nodes

| Label | Key | Fields |
|-------|-----|--------|
| `Entity` | `id` | `entity_type` ("File", "Function", "Class"), `status`, `last_modified`, `metadata` |
| `Workspace` | `id` (workspace name) | `description`, `status` ("active"/"inactive"), `closed_at` |
| `Scratchpad` | `id` | `content`, `created_at` |

### Edges

| Relationship | From → To | Properties |
|--------------|-----------|------------|
| `LINKED_TO` | Entity → Entity | `relationship_type`: "CALLS", "DECLARED_IN", "IMPORTS", "INHERITS_FROM" |
| `MAPPED_TO` | Workspace → Entity | `created_at`, `invalidated_at`, `is_stale` |
| `HAS_SCRATCHPAD` | Workspace → Scratchpad | — |

### Entity ID Format

- File: `src/index.ts`
- Function: `src/reconciler.ts::walkSync`
- Method: `src/engine-client.ts::YaamEngineClient::start`
- Class: `src/reconciler.ts::Reconciler`

## Query DSL

The `yaam_graph_explore` tool accepts a JSON Query DSL (not Cypher). See the tool description for full schema and examples.

### Quick Reference

```json
// Entity counts by type
{"match":{"label":"Entity"}, "aggregate":{"group_by":"type","count":true}}

// All functions in a file
{"match":{"label":"Entity","entity_type":"Function"}, "where":{"edge_to":{"id":"src/index.ts","relationship":"DECLARED_IN"}}}

// Reverse call graph (who calls X?)
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"inbound","max_depth":2}}

// Forward call graph (what does X call?)
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"outbound","max_depth":3}}
```

## Gotchas

- **Results > 20 rows** are spooled to `.chunks/memory_dumps/query_out.txt`. Read that file if directed.
- **`yaam_graph_explore` is read-only.** Write operations are not supported via the query tool.
- **The graph is auto-reconciled** after every `write`, `edit`, `bash`, and `read` tool result. Trust it as your primary source of code structure.