# YAAM (Yet Another Agent Memory)

A dual-layered hybrid semantic-episodic memory engine for local coding agents. It provides a physical code topology (Layer 0) augmented with cognitive workspaces (Layer 1).

YAAM runs as a standalone compiled **Rust daemon** communicating over `stdio` JSON-RPC 2.0, utilizing an append-only JSONL event stream, an in-memory graph, and a highly interactive web-based Cytoscape UI.

## Architecture

```
src-rust/
├── src/main.rs              Rust daemon entrypoint
├── src/rpc.rs               JSON-RPC 2.0 TCP server
├── src/graph.rs             In-memory graph engine (Arc<RwLock>)
├── src/storage.rs           Append-only JSONL event persistence (`fs2` file locks)
├── src/reconciler.rs        Generic tree-sitter AST parser (adapter-driven)
├── src/language_adapter.rs  Language adapter trait + TypeScript/Python adapters
├── src/lsp_adapter.rs       Stdio LSP client (shared by all languages)
├── src/embedding.rs         ONNX Inference (gte-small)
└── src/search.rs            BM25 Inverted Index

scripts/
└── add-language.sh          Scaffolding utility for adding new languages
```

## How It Works

### Layer 0 — Physical Topology
Automatically tracks files, functions, classes, and cross-file relationships (CALLS, IMPORTS, INHERITS_FROM).
- **Multi-Language Tree-sitter Parsing:** Each language is handled by a `LanguageAdapter` (strategy pattern) that provides the tree-sitter grammar, query source, and AST-walking logic. TypeScript/JavaScript and Python are supported out of the box. Adding a new language requires only implementing the adapter trait and registering it (see [Adding a New Language](#adding-a-new-language)).
- **Per-Language LSP Resolution:** Each language adapter declares its own LSP server command. LSP servers are **lazily started** — only when the first file of that language is reconciled — and run concurrently as managed child processes. Cross-file `CALLS` and `IMPORTS` edges are resolved via `textDocument/definition` requests.
- **Deletion Tracking:** Orphaned entities and deleted files automatically issue `DELETE_NODE` payloads across the in-memory graph, preventing stale context.

#### Supported Languages

| Language | Extensions | LSP Server | Tree-sitter Grammar |
|----------|-----------|------------|---------------------|
| TypeScript / JavaScript | `.ts` `.tsx` `.js` `.jsx` | `typescript-language-server` | `tree-sitter-typescript` |
| Python | `.py` | `pylsp` | `tree-sitter-python` |

Query registered languages at runtime via the `languages.list` RPC method.

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

## RPC Methods

| Method | Description |
|--------|-------------|
| `reconcile` | Parse a file with tree-sitter, resolve references via LSP, upsert entities. |
| `query` | Execute a JSON DSL query against the in-memory graph. |
| `search` | Hybrid BM25 + ONNX semantic search. |
| `upsert_node` / `link_nodes` / `delete_node` / `delete_edges` | Graph mutations. |
| `languages.list` | List all registered languages with extensions, LSP command, and running status. |
| `initialize` / `shutdown` | Daemon lifecycle. |

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
4. **(Optional) Install Language Servers**:
   LSP servers are started lazily on first use. Install only the ones you need:
   ```bash
   npm install -g typescript-language-server typescript   # TypeScript
   pip install python-lsp-server                            # Python
   ```

Loaded automatically by pi from `pi.extensions` in `package.json`.

## Adding a New Language

YAAM uses a **language adapter pattern** (strategy pattern) for multi-language support. Adding a new language is a 4-step process:

### Option A: Scaffolding Script (recommended)

```bash
# Generate and print boilerplate code with instructions
./scripts/add-language.sh Rust rs tree-sitter-rust 0.21 rust-analyzer

# Or auto-insert the code directly into source files
./scripts/add-language.sh --apply Rust rs tree-sitter-rust 0.21 rust-analyzer
```

Then customize the `query_source()` and `find_enclosing_function()` TODO sections, build, and install the LSP server.

### Option B: Manual

1. **Add tree-sitter grammar** to `src-rust/Cargo.toml`:
   ```toml
   tree-sitter-rust = "0.21"
   ```
2. **Implement the `LanguageAdapter` trait** in `src-rust/src/language_adapter.rs`:
   - `language()` — return the tree-sitter `Language`
   - `query_source()` — tree-sitter query capturing declarations, calls, and imports
   - `language_id()` — LSP `languageId` string
   - `find_enclosing_function()` — walk the AST to attribute CALLS edges
   - `lsp_command()` — return the LSP server command (or `None`)
3. **Register in `get_adapter()`** — add a match arm for the file extension
4. **Register in `list_languages()`** — add a `LanguageInfo` entry for introspection

### Tree-sitter Query Capture Conventions

Capture names are categorized by prefix in `parse_file()`:

| Capture Prefix | Entity Type | Example |
|----------------|-------------|---------|
| `@class.name` | Class declaration | `(class_definition name: (identifier) @class.name)` |
| `@function.name` | Function declaration | `(function_definition name: (identifier) @function.name)` |
| `@method.name` | Function (method) | `(method_definition name: ... @method.name)` |
| `@variable.name` | Function (variable holding function) | `(variable_declarator name: ... @variable.name)` |
| `@call.name` | CALLS reference | `(call function: (identifier) @call.name)` |
| `@import.name` | IMPORTS reference | `(import_statement ... @import.name)` |

Use the [tree-sitter playground](https://tree-sitter.github.io/tree-sitter/playground) to find the correct node types for your language.