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

### 2. Recording Insights
As you discover nuances or make decisions, record them in the scratchpad.
- **Tool:** `yaam_workspace_append_note(workspace, content)`
- **Guideline:** Record "why" decisions, not just "what" was done.

### 3. Exploring Relationships
To understand how code components are linked, query the graph.
- **Tool:** `yaam_graph_explore(query)`
- **Example Queries:**

```cypher
-- Entity counts
MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC

-- Import dependencies
MATCH (src:Entity {type: 'File'})-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst:Entity {type: 'File'}) RETURN src.id, dst.id

-- Call graph
MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity) RETURN caller.id, callee.id

-- Inheritance
MATCH (sub:Entity)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup:Entity) RETURN sub.id, sup.id

-- Files tracked to active workspace
MATCH (w:Workspace {status: 'active'})-[:MAPPED_TO]->(e:Entity) RETURN e.id, e.type
```

### 4. Checking Status
Run the `/yaam` command to see entity counts, active workspace, recent notes, and reconciler state.

## Graph Schema

### Node Tables
| Table | Key | Description |
|-------|-----|-------------|
| `Entity` | `id` | Files, Functions, Classes |
| `Workspace` | `workspace_name` | Task contexts |
| `Scratchpad` | `id` | Notes/insights |

### Relationship Tables
| Table | Description |
|-------|-------------|
| `LINKED_TO` | `relationship_type`: CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM |
| `MAPPED_TO` | Workspace → Entity (file tracking) |
| `HAS_SCRATCHPAD` | Workspace → Scratchpad |

### Entity ID Format
| Type | Format | Example |
|------|--------|---------|
| File | `<path>` | `src/index.ts` |
| Function | `<file>::<function>` | `src/db.ts::sleep` |
| Method | `<file>::<Class>::<method>` | `src/db.ts::ConnectionManager::withConnection` |
| Class | `<file>::<Class>` | `test_py/a.py::DerivedClass` |

## Guardrails
- **Read-Only:** `yaam_graph_explore` is strictly read-only. Write operations (CREATE, MERGE, SET, DELETE, etc.) are blocked.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`. Read that file if directed.
- **Memory Decay:** Older notes lose relevance. Focus on the most recent context returned by retrieval tools.
- **Multi-Agent:** Multiple agents share the same database. Lock contention is handled via exponential backoff.