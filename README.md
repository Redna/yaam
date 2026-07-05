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
| Rust | `.rs` | `rust-analyzer` | `tree-sitter-rust` |

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
| `search` | Hybrid BM25 + ONNX semantic search. Supports filtering by `entity_types`, `include_paths`, `exclude_paths`. Results include `category` (module/library). |
| `upsert_node` / `link_nodes` / `delete_node` / `delete_edges` | Graph mutations. |
| `languages.list` | List all registered languages with extensions, LSP command, and running status. |
| `initialize` / `shutdown` | Daemon lifecycle. |

## Setup

### Quick Start (Recommended)

```bash
npm run setup
```

This runs the interactive bootstrap script which:
1. Checks prerequisites (Rust, Node.js, npm)
2. Installs npm dependencies
3. Builds the Rust daemon (`cargo build --release`)
4. Downloads the gte-small ONNX model weights (for semantic search)
5. Installs LSP servers for all enabled languages
6. Builds the TypeScript extension

For non-interactive use (CI, Docker):
```bash
npm run setup:yes
```

Flags:
```bash
./scripts/setup.sh --no-lsp      # Skip LSP installation
./scripts/setup.sh --no-build    # Skip Rust + TS build (LSP only)
./scripts/setup.sh --yes         # Install everything without prompts
```

### Manual Setup

If you prefer to do it step by step:

1. **Install Model Weights**:
   ```bash
   cd src-rust && cargo run --release -- setup
   ```
   Downloads the required HuggingFace `gte-small` model and tokenizer for semantic search.

2. **Build Release Binary**:
   ```bash
   cd src-rust && cargo build --release
   ```

3. **Compile TypeScript Extension**:
   ```bash
   npm install && npm run build
   ```

4. **Install Language Servers** (optional but recommended):
   LSP servers are started lazily on first use. Install only the ones you need:
   ```bash
   npm install -g typescript-language-server typescript   # TypeScript / JavaScript
   pip install python-lsp-server                            # Python
   rustup component add rust-analyzer                        # Rust
   ```
   Without LSP servers, tree-sitter parsing still works (declarations are extracted),
   but cross-file `CALLS` and `IMPORTS` edges won't be resolved.

### Language Configuration

Enabled languages are configured in [`.yaam/config.json`](.yaam/config.json).
To disable a language you don't need, set `"enabled": false`:

```json
{
  "languages": {
    "python": { "enabled": false }
  }
}
```

The setup script reads this file and only installs LSP servers for enabled languages.

Loaded automatically by pi from `pi.extensions` in `package.json`.

## Adding a New Language

YAAM uses a **language adapter pattern** (strategy pattern) for multi-language support. Each language is a self-contained adapter that provides the tree-sitter grammar, query, LSP command, and AST-walking logic. Adding a new language is a 4-step process.

### Option A: Scaffolding Script (recommended)

```bash
# Generate and print boilerplate code with instructions
./scripts/add-language.sh Go go tree-sitter-go 0.21 gopls serve

# Or auto-insert the code directly into source files
./scripts/add-language.sh --apply Go go tree-sitter-go 0.21 gopls serve
```

**Arguments:**

| Argument | Description | Example |
|----------|-------------|---------|
| `name` | Human-readable language name | `Go`, `Java`, `Ruby` |
| `extensions` | Comma-separated file extensions (no dots) | `go`, `java,gradle` |
| `tree-sitter-crate` | crates.io name of the grammar | `tree-sitter-go` |
| `crate-version` | crates.io version | `0.21` |
| `lsp-command` | LSP server executable, or `none` | `gopls`, `jdtls`, `none` |
| `lsp-args` | Optional arguments for the LSP server | `--stdio`, `serve` |

The script generates a complete adapter struct with TODO templates for `query_source()` and `find_enclosing_function()`. Customize those, then build and install the LSP server.

### Option B: Manual

1. **Add tree-sitter grammar** to `src-rust/Cargo.toml`:
   ```toml
   tree-sitter-go = "0.21"
   ```

2. **Implement the `LanguageAdapter` trait** in `src-rust/src/language_adapter.rs` (see [Complete Example](#complete-adapter-example) below).

3. **Register in `get_adapter()`** — add a match arm for the file extension:
   ```rust
   "go" => Some(Box::new(GoAdapter)),
   ```

4. **Register in `list_languages()`** — add a `LanguageInfo` entry:
   ```rust
   LanguageInfo {
       name: "Go".to_string(),
       extensions: vec!["go".to_string()],
       language_id: "go".to_string(),
       lsp_command: GoAdapter.lsp_command(),
   },
   ```

### Complete Adapter Example

Here is the full `PythonAdapter` implementation as a reference. A new adapter follows the same structure:

```rust
pub struct PythonAdapter;

impl LanguageAdapter for PythonAdapter {
    // 1. Return the tree-sitter Language for this grammar.
    fn language(&self) -> Language {
        tree_sitter_python::language()
    }

    // 2. Tree-sitter query that captures declarations, calls, and imports.
    //    MUST be a &'static str (string literal).
    //    Capture names determine entity classification by prefix (see table below).
    fn query_source(&self) -> &'static str {
        r#"
        (class_definition name: (identifier) @class.name)
        (function_definition name: (identifier) @function.name)
        (call function: (identifier) @call.name)
        (call function: (attribute attribute: (identifier) @call.name))
        (import_statement (dotted_name) @import.name)
        (import_from_statement (dotted_name) @import.name)
        "#
    }

    // 3. LSP languageId string for textDocument/didOpen.
    fn language_id(&self) -> &'static str {
        "python"
    }

    // 4. Walk up the tree-sitter AST from a call/import node to find the
    //    enclosing function or class. Returns "file_path:name" or None.
    //    Node kinds are language-specific — check the grammar's node-types.json.
    fn find_enclosing_function(
        &self,
        node: Node,
        source_code: &[u8],
        file_path: &Path,
    ) -> Option<String> {
        let mut current = node.parent();
        while let Some(parent) = current {
            match parent.kind() {
                // These node kind strings come from the tree-sitter grammar.
                // For Python: "function_definition", "class_definition"
                // For Rust: "function_item"
                // For TypeScript: "function_declaration", "class_declaration", "method_definition"
                "function_definition" | "class_definition" => {
                    // "name" is the field name in the grammar for the
                    // function/class identifier node.
                    if let Some(name_node) = parent.child_by_field_name("name") {
                        let name = name_node.utf8_text(source_code).ok()?.to_string();
                        return Some(format!("{}:{}", file_path.display(), name));
                    }
                }
                _ => {}
            }
            current = parent.parent();
        }
        None
    }

    // 5. LSP server command. Return None if no LSP is available.
    fn lsp_command(&self) -> Option<LspCommand> {
        Some(LspCommand {
            command: "pylsp".to_string(),
            args: vec![],
        })
    }
}
```

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

**How to find node types for your language:**

1. Use the [tree-sitter playground](https://tree-sitter.github.io/tree-sitter/playground) to parse a sample file and inspect the AST.
2. Or check the grammar's `node-types.json` file in the crate source:
   ```bash
   # Find the node types for your tree-sitter grammar
   find ~/.cargo/registry/src -path "*/tree-sitter-XXX*/node-types.json"
   # List function/class node types and their fields
   python3 -c "import json; [print(f'{t[\"type\"]}: fields={[f for f in t.get(\"fields\",{})]}') for t in json.load(open('PATH')) if 'function' in t.get('type','').lower() or 'class' in t.get('type','').lower()]"
   ```

### Troubleshooting

| Problem | Cause | Fix |
|---------|-------|-----|
| **Runtime panic: `QueryError ... kind: Structure`** | Tree-sitter query references a node type or field that doesn't exist in the grammar | Check `node-types.json` for the correct type/field names. E.g. Rust `trait_item` uses `type_identifier` for `name`, not `identifier`. |
| **LSP warning: `Failed to start LSP server`** | LSP server not installed on the system | Install it (e.g. `pip install python-lsp-server`, `rustup component add rust-analyzer`). Declarations still work without LSP; only cross-file CALLS/IMPORTS edges are missing. |
| **LSP starts but no CALLS edges created** | LSP server can't resolve definitions (file not part of a project, or needs indexing time) | For rust-analyzer: file must be part of a Cargo project. For pylsp: file should be on disk. Try reconciling the file a second time after a few seconds to give the LSP time to index. |
| **CALLS edges attributed to file instead of function** | `find_enclosing_function()` returns `None` — wrong node kind string | Check the grammar's `node-types.json` for the correct function node kind (e.g. `function_item` for Rust, `function_definition` for Python). |
| **`query_source()` won't compile** | Return type is `&'static str` — query must be a string literal | Use `r#"..."#` raw string literal, not `String::from()` or `format!()`. |