---
name: yaam-memory-manager
description: Manage episodic agent memory using the YAAM (Yet Another Agent Memory) engine. Use this skill to initialize workspaces, record scratchpad notes, and explore the memory graph to maintain context across sessions.
---

# YAAM Memory Manager

This skill provides procedural guidance for interacting with the YAAM engine, which separates physical code structure (Layer 0) from cognitive agent context (Layer 1).

## Core Concepts

- **Layer 0 (Physical):** Automatically tracked files, functions, classes, call graphs, inheritance, and import dependencies. Use `yaam_graph_explore` to query.
- **Layer 1 (Cognitive):** Agent-defined workspaces and scratchpads for recording decisions and rationale.
- **Reconciliation:** The system automatically syncs with the filesystem in the background after every tool operation (write, edit, bash, read). The graph always reflects the **live state** of the repository. Uses tree-sitter for AST parsing and `typescript-language-server` for cross-file resolution.

## Architecture Overview

YAAM runs as a first-class pi extension. The backend is a compiled Rust daemon communicating over stdio JSON-RPC 2.0, using an append-only JSONL event log and an in-memory graph engine. State is restored on startup by replaying events.

The reconciler runs in the background (fire-and-forget):
1. **Parse phase:** Reads files, parses AST with tree-sitter, resolves calls/inheritance/imports via LSP.
2. **Commit phase:** Generates events (UPSERT_NODE, LINK_NODES, DELETE_NODE) and applies them to the JSONL log and in-memory graph.

**The graph is trustworthy.** Every `write`, `edit`, `bash`, and `read` tool result triggers a background reconcile that updates the graph to match the filesystem. Use the graph as your primary navigation and impact-analysis tool.

## Workflows

### 1. Initializing a Task
When starting a new feature or bug fix, always initialize a workspace to group your thoughts.
- **Tool:** `yaam_workspace_initialize(name, description)`
- **Guideline:** Use descriptive names like `auth-fix` or `ui-refactor`.
- **Note:** This deactivates any previously active workspace. Only one workspace can be active at a time.

### 2. Recording Insights
As you discover nuances or make decisions, record them in the scratchpad.
- **Tool:** `yaam_workspace_append_note(workspace, content)`
- **Guideline:** Record "why" decisions, not just "what" was done. Notes persist across sessions.

### 3. Exploring Relationships
To understand how code components are linked, query the graph.
- **Tool:** `yaam_graph_explore(query)`
- The tool description includes an inline JSON DSL schema and examples for quick reference.
- Detailed query patterns are documented below in **Query Cookbook**.

### 4. Checking Status
Run the `/yaam` command to see entity counts, active workspace, recent notes, and reconciler state.

## Complete Graph Schema

### Node Properties

**Entity** — code constructs (files, functions, classes)
| Property | Type | Description |
|----------|------|-------------|
| `id` | string | Hierarchical path identifier (see ID Format below) |
| `entity_type` | string | `"File"`, `"Function"`, or `"Class"` |
| `status` | string | `"active"` (entity exists in codebase) |
| `last_modified` | int | Unix timestamp of last reconciliation |
| `metadata` | string (JSON) | Rich metadata for Functions/Classes (see below); empty for Files |

**Workspace** — task contexts
| Property | Type | Description |
|----------|------|-------------|
| `id` | string | Unique identifier (workspace name) |
| `description` | string | Human-readable task description |
| `status` | string | `"active"` or `"inactive"` |
| `closed_at` | timestamp | When workspace was closed; `null` if still open |

**Scratchpad** — notes/insights
| Property | Type | Description |
|----------|------|-------------|
| `id` | string | Unique identifier |
| `content` | string | The note text |
| `created_at` | timestamp | When the note was written |

### Relationship Properties

**`LINKED_TO`** — Entity → Entity (code relationships)
| Property | Type | Description |
|----------|------|-------------|
| `relationship` | string | `"CALLS"`, `"DECLARED_IN"`, `"IMPORTS"`, `"INHERITS_FROM"` |

**`MAPPED_TO`** — Workspace → Entity (files tracked to a workspace)
| Property | Type | Description |
|----------|------|-------------|
| `created_at` | timestamp | When the mapping was established |
| `invalidated_at` | timestamp | When the mapping became stale; `null` if still valid |
| `is_stale` | bool | Whether this mapping is stale |

**`HAS_SCRATCHPAD`** — Workspace → Scratchpad
> No properties on these edges.

### Entity ID Format

| Type | Format | Example |
|------|--------|---------|
| File | `<path>` | `src/index.ts` |
| Function | `<file>::<function>` | `src/reconciler.ts::reconcile` |
| Method | `<file>::<Class>::<method>` | `src/engine-client.ts::YaamEngineClient::start` |
| Class | `<file>::<Class>` | `src/reconciler.ts::Reconciler` |

## Query Cookbook (JSON DSL)

The `yaam_graph_explore` tool accepts a JSON Query DSL object. Below are common patterns.

### Entity Discovery

```json
// All entities
{"match":{"label":"Entity"}}

// All files
{"match":{"label":"Entity","entity_type":"File"}}

// All functions
{"match":{"label":"Entity","entity_type":"Function"}}

// All functions in a specific file
{"match":{"label":"Entity","entity_type":"Function"}, "where":{"edge_to":{"id":"src/index.ts","relationship":"DECLARED_IN"}}}

// Find entities by name substring
{"match":{"label":"Entity","name_contains":"reconcile"}}
```

### Call Graph Analysis

```json
// What does function X call? (forward call graph, 3 hops)
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"outbound","max_depth":3}}

// Who calls function X? (reverse call graph / impact analysis, 2 hops)
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"inbound","max_depth":2}}
```

### Import Dependencies

```json
// What does a file import?
{"match":{"id":"src/index.ts"}, "traverse":{"relationship":"IMPORTS","direction":"outbound","max_depth":1}}
```

### Aggregation

```json
// Entity counts by type
{"match":{"label":"Entity"}, "aggregate":{"group_by":"type","count":true}}

// Entity counts by status
{"match":{"label":"Entity"}, "aggregate":{"group_by":"status","count":true}}
```

### Workspace & Memory Queries

```json
// Active workspace
{"match":{"label":"Workspace","status":"active"}}

// Notes in active workspace
{"match":{"label":"Workspace","status":"active"}, "traverse":{"relationship":"HAS_SCRATCHPAD","direction":"outbound","max_depth":1}}
```

### Result Projection

```json
// Return only specific fields
{"match":{"label":"Entity","entity_type":"Function"}, "return_fields":["id","name"], "limit":10}
```

## DSL Structure Reference

```json
{
  "match": {
    "label": "Entity" | "Workspace" | "Scratchpad",
    "entity_type": "File" | "Function" | "Class",
    "id": "node_id",
    "status": "active" | "inactive",
    "name_contains": "substring"
  },
  "where": {
    "edge_to": { "id": "target_id", "relationship": "DECLARED_IN" | "CALLS" | "IMPORTS" },
    "edge_from": { "id": "source_id", "relationship": "CALLS" }
  },
  "traverse": {
    "relationship": "CALLS" | "IMPORTS" | "HAS_SCRATCHPAD" | "DECLARED_IN",
    "direction": "outbound" | "inbound" | "both",
    "max_depth": 1
  },
  "aggregate": { "group_by": "type" | "label" | "status", "count": true },
  "limit": 20,
  "return_fields": ["id", "name", "label", "content", "metadata"]
}
```

## Best Practices

1. **Always initialize a workspace** before starting substantive work.

2. **Use `name_contains` for fuzzy matching** instead of exact IDs. Entity IDs can be long hierarchical paths:
   ```json
   {"match":{"label":"Entity","name_contains":"reconcile"}}
   ```

3. **Query the reverse call graph for impact analysis.** Before modifying a function, find all callers:
   ```json
   {"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"inbound","max_depth":2}}
   ```

4. **Record "why" not "what".** Scratchpad notes should capture decisions and rationale, not actions.

5. **Use multi-hop traversal for transitive analysis.** The DSL supports `max_depth` up to 5:
   ```json
   {"match":{"id":"src/index.ts"}, "traverse":{"relationship":"CALLS","direction":"outbound","max_depth":3}}
   ```

6. **Use `aggregate` for quick summaries.** Get entity counts without retrieving full nodes:
   ```json
   {"match":{"label":"Entity"}, "aggregate":{"group_by":"type","count":true}}
   ```

## Guardrails

- **Live Reconciliation:** The graph is automatically reconciled after every file operation and reflects the current state of the repository.
- **Read-Only:** `yaam_graph_explore` is strictly read-only.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`. Read that file if directed.
- **Max Traversal Depth:** 5 hops maximum.