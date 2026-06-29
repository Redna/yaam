# YAAM Agent Onboarding & Instructions (Gemini / Antigravity / Other Agents)

This repository is equipped with **YAAM (Yet Another Agent Memory)**. All agents interacting with this codebase SHOULD use the memory engine to maintain continuity and structural awareness.

## 1. Environment & Tools

YAAM runs as a **first-class pi extension**. The Rust engine daemon (`src-rust/`) is spawned automatically by the extension and communicates over stdio JSON-RPC 2.0.

- **Pi extension:** Tools (`yaam_graph_explore`, `yaam_search`, `yaam_workspace_initialize`, `yaam_workspace_append_note`) and `/yaam` command are registered automatically when the extension loads.
- **Non-pi agents:** The YAAM tools are available only within pi. If you are a non-pi agent, you can still benefit from the graph data by reading the `events.jsonl` log or asking a pi agent to run queries on your behalf.

## 2. Mandatory Workflows

### Memory Exploration
Before making architectural changes, query the existing relationships:

```
yaam_graph_explore(query={"match":{"label":"Entity"}, "aggregate":{"group_by":"type","count":true}})
```

### Task Initialization
When starting a task, initialize a dedicated context:

```
yaam_workspace_initialize(name="your-active-task", description="What you're working on")
```

### Insight Capture
Record all "Why" decisions and learnings:

```
yaam_workspace_append_note(workspace="your-active-task", content="Decided X because Y")
```

## 3. Physical State Sync

The extension automatically reconciles the codebase in the background after every tool use (`write`, `edit`, `bash`, `read`). The Rust engine parses files with tree-sitter and resolves cross-file references via `typescript-language-server`. No manual sync is needed.

## 4. Querying Examples (JSON DSL)

The `yaam_graph_explore` tool accepts a JSON Query DSL, not Cypher.

### Entity counts by type
```json
{"match":{"label":"Entity"}, "aggregate":{"group_by":"type","count":true}}
```

### All functions in a file
```json
{"match":{"label":"Entity","entity_type":"Function"}, "where":{"edge_to":{"id":"src/index.ts","relationship":"DECLARED_IN"}}}
```

### Reverse call graph (impact analysis)
```json
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"inbound","max_depth":2}}
```

### Forward call graph
```json
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"outbound","max_depth":3}}
```

### Import dependencies
```json
{"match":{"label":"Entity","entity_type":"File"}, "traverse":{"relationship":"IMPORTS","direction":"outbound","max_depth":1}}
```

### Active workspace notes
```json
{"match":{"label":"Workspace","status":"active"}, "traverse":{"relationship":"HAS_SCRATCHPAD","direction":"outbound","max_depth":1}}
```

## 5. Architecture

YAAM uses a dual-layer model:

- **Layer 0 (Physical):** Files, functions, classes, call graphs, imports, and inheritance — automatically tracked via tree-sitter + LSP.
- **Layer 1 (Cognitive):** Agent-defined workspaces with scratchpad notes for recording decisions.

State is persisted as an append-only `events.jsonl` log. On startup, events are replayed into an in-memory graph (`Arc<RwLock<MemoryEngine>`) for instant queries.

## 6. Guardrails

- **Read-Only:** `yaam_graph_explore` is strictly read-only.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`.
- **Auto-Reconciliation:** The graph reflects the live state of the repository after every tool invocation.