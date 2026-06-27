---
name: yaam-memory-manager
description: Manage episodic agent memory using the YAAM (Yet Another Agent Memory) engine. Use this skill to initialize workspaces, record scratchpad notes, and explore the memory graph to maintain context across sessions.
---

# YAAM Memory Manager

This skill provides procedural guidance for interacting with the YAAM engine, which separates physical code structure (Layer 0) from cognitive agent context (Layer 1).

## Core Concepts

- **Layer 0 (Physical):** Automatically tracked files, functions, classes, call graphs, inheritance, and import dependencies. Use `yaam_graph_explore` to query.
- **Layer 1 (Cognitive):** Agent-defined workspaces and scratchpads for recording decisions and rationale.
- **Reconciliation:** The system automatically syncs with the filesystem in the background after tool use. Uses the TypeScript Compiler API for TS/JS and Pyright LSP for Python.

## Architecture Overview

YAAM runs as a first-class pi extension. All database operations happen in-process using a lock-and-release pattern with exponential backoff — no subprocess spawning or CLI shell-outs. This enables multi-agent coexistence on the same project.

The reconciler runs in the background (fire-and-forget):
1. **Parse phase** (no DB lock): Reads files, parses AST, resolves calls/inheritance/imports.
2. **Commit phase** (brief DB lock): Writes entities and relationships to LadybugDB.

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
- The tool description includes an inline schema and examples for quick reference.
- Detailed query patterns are documented below in **Query Cookbook**.

### 4. Checking Status
Run the `/yaam` command to see entity counts, active workspace, recent notes, and reconciler state.

## Complete Graph Schema

### Node Properties

**Entity** — code constructs (files, functions, classes)
| Property | Type | Description |
|----------|------|-------------|
| `id` | string | Hierarchical path identifier (see ID Format below) |
| `type` | string | `"File"`, `"Function"`, or `"Class"` |
| `status` | string | `"active"` (entity exists in codebase) |
| `last_modified` | int | Unix timestamp of last reconciliation |
| `metadata` | string (JSON) | `{"line": N}` for Functions/Classes; `null` for Files |

**Workspace** — task contexts
| Property | Type | Description |
|----------|------|-------------|
| `workspace_name` | string | Unique identifier (e.g. `"auth-fix"`) |
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
| `relationship_type` | string | `"CALLS"`, `"DECLARED_IN"`, `"IMPORTS"`, `"INHERITS_FROM"` |

> This is the ONLY property on `LINKED_TO` edges. Do not access other properties on these edges.

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
| Function | `<file>::<function>` | `src/db.ts::sleep` |
| Method | `<file>::<Class>::<method>` | `src/db.ts::ConnectionManager::withConnection` |
| Callback | `<file>::<parent>::<callback>` | `src/db.ts::main::action() callback` |
| Class | `<file>::<Class>` | `test_py/a.py::DerivedClass` |

> Nested callbacks and closures are tracked hierarchically: `file::outerFn::innerFn::callback`. This lets you trace callback scope chains.

## Query Cookbook

### Entity Discovery
