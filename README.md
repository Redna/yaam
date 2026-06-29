# YAAM (Yet Another Agent Memory)

A dual-layered hybrid semantic-episodic memory engine for local coding agents. It provides a physical code topology (Layer 0) augmented with cognitive workspaces (Layer 1).

YAAM runs as a standalone compiled **Rust daemon** communicating over `stdio` JSON-RPC 2.0, utilizing an append-only JSONL event stream, an in-memory graph, and a highly interactive web-based Cytoscape UI.

## Architecture

```
src-rust/
├── src/main.rs          Rust daemon entrypoint
├── src/rpc.rs           JSON-RPC 2.0 stdio server
├── src/graph.rs         In-memory graph engine (Arc<RwLock>)
├── src/storage.rs       Append-only JSONL event persistence (`fs2` file locks)
├── src/reconciler.rs    Tree-sitter AST parser
├── src/lsp_adapter.rs   Stdio client to typescript-language-server
├── src/embedding.rs     ONNX Inference (gte-small)
└── src/search.rs        BM25 Inverted Index
```

## How It Works

### Layer 0 — Physical Topology
Automatically tracks files, functions, classes, and cross-file relationships (CALLS, IMPORTS, INHERITS_FROM).
- **Tree-sitter Parsing:** Extracts local declarations and call expressions seamlessly.
- **LSP Resolution:** The Rust Reconciler delegates `textDocument/definition` requests to an active background `typescript-language-server` over stdio, resolving targets and generating accurate cross-file edges.
- **Deletion Tracking:** Orphaned entities and deleted files automatically issue `DELETE_NODE` payloads across the in-memory graph, preventing stale context.

### Layer 1 — Cognitive Context
Agent-defined workspaces containing chronological scratchpads for design rationale and decisions.

### Zero-Dependency Rust Engine
The backend operates entirely without heavy external DBs.
- **Append-Only JSONL Storage:** State is restored safely from a unified `events.jsonl` event log, protected from concurrent overwrites by OS-level locking.
- **In-Memory Speed:** Graph relationships and nodes reside exclusively in RAM, enabling instant DSL queries and recursive traversals.
- **Semantic + Keyword Hybrid Search:** `yaam-engine` ships with a 38M parameter ONNX embedding model (`thenlper/gte-small`) executed locally on CPU for dense semantic lookup, paired with a custom Unicode-aware BM25 token index.

## Web-Based Graph Visualizer
Running `/yaam viz` spins up a local Express backend serving an interactive UI at `http://localhost:3456`.
- **Cytoscape.js Frontend:** A premium, dark-mode GUI highlighting Code nodes, Workspaces, and Scratchpads natively.
- **Interactive Details:** Hover animations and sidebars explicitly map node attributes and metadata in real-time.

## Graph Schema

| Node Table | Key | Fields |
|------------|-----|--------|
| `Entity` | `id` | `type`, `status`, `last_modified`, `metadata` |
| `Workspace` | `workspace_name` | `description`, `status`, `closed_at` |
| `Scratchpad` | `id` | `content`, `created_at` |

| Rel Table | From → To | Properties |
|-----------|-----------|------------|
| `LINKED_TO` | Entity → Entity | `relationship_type` (CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM) |
| `MAPPED_TO` | Workspace → Entity | `created_at`, `invalidated_at`, `is_stale` |
| `HAS_SCRATCHPAD` | Workspace → Scratchpad | — |

## Pi Extension Tools

| Tool | Description |
|------|-------------|
| `yaam_graph_explore` | Executes a custom JSON DSL query against the Rust engine's in-memory graph. |
| `yaam_search` | Hybrid BM25 + ONNX semantic search across all memory nodes by natural language. |
| `yaam_workspace_initialize` | Create workspace, deactivate previous. Schedules full reconcile. |
| `yaam_workspace_append_note` | Append note to workspace scratchpad. |
| `/yaam viz` | Launches the interactive web-based topology visualizer. |

## Setup

1. **Install Model Weights**:
   Execute `cargo run --manifest-path src-rust/Cargo.toml -- setup` to pull down the required HuggingFace `gte-small` model and tokenizer.
2. **Build Release Binary**:
   Ensure Rust is installed, then compile the engine:
   ```bash
   cd src-rust && cargo build --release
   ```
3. **Compile TypeScript Extension**:
   ```bash
   npm run build
   ```

Loaded automatically by pi from `pi.extensions` in `package.json`.