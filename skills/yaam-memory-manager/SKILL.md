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
| `metadata` | string (JSON) | Rich metadata for Functions/Classes (see below); `null` for Files |

**`metadata` JSON fields (Functions/Classes):**

| Field | Type | Description | Example |
|-------|------|-------------|---------|
| `line` | int | Start line number | `42` |
| `endLine` | int | End line number | `96` |
| `signature` | string | Full function/class signature | `"withConnection(fn, maxRetries): Promise<T>"` |
| `isAsync` | bool | Whether the function is async | `true` |
| `isExported` | bool | Whether it's exported (TS/JS) | `true` |
| `isStatic` | bool | Whether it's a static method | `false` |
| `isAbstract` | bool | Whether it's abstract | `false` |
| `params` | array | Parameter list: `{name, type?, default?}` | `[{"name":"fn","type":"..."}]` |
| `returnType` | string | Return type annotation | `"Promise<T>"` |
| `docComment` | string | JSDoc/Python docstring text | `"Acquires a DB connection..."` |

> Not all fields are present for every entity. Use `WHERE metadata CONTAINS '"isAsync":true'` to filter.
> For Files, `metadata` is `null`.

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

```cypher
-- All files in the project
MATCH (f:Entity {type: 'File'}) RETURN f.id AS file

-- All files matching a pattern
MATCH (f:Entity {type: 'File'}) WHERE f.id CONTAINS '.ts' RETURN f.id

-- All functions in a specific file
MATCH (fn:Entity)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file:Entity {type: 'File'})
WHERE file.id CONTAINS 'reconciler.ts'
RETURN fn.id AS function, fn.metadata AS meta
```

### Call Graph Analysis

```cypher
-- What does function X call? (forward call graph)
MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity)
WHERE caller.id CONTAINS 'reconcile'
RETURN caller.id AS caller, callee.id AS callee

-- Who calls function X? (reverse call graph / impact analysis)
MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity)
WHERE callee.id CONTAINS 'getConn'
RETURN caller.id AS caller

-- Multi-hop transitive call chain (1-3 levels)
MATCH path = (src:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}*1..3]->(dst:Entity)
WHERE src.id CONTAINS 'reconcile'
RETURN src.id AS source, dst.id AS target, length(path) AS depth

-- Import dependencies between files
MATCH (src:Entity {type: 'File'})-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst:Entity {type: 'File'})
RETURN src.id AS importer, dst.id AS imported
```

### Inheritance

```cypher
-- Class inheritance hierarchy
MATCH (sub:Entity)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup:Entity)
RETURN sub.id AS subclass, sup.id AS superclass
```

### Workspace & Memory Queries

```cypher
-- Active workspace
MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name, w.description

-- All workspaces and their status
MATCH (w:Workspace) RETURN w.workspace_name AS name, w.status AS status, w.closed_at AS closed

-- Notes in a specific workspace
MATCH (w:Workspace {workspace_name: 'auth-fix'})-[:HAS_SCRATCHPAD]->(s:Scratchpad)
RETURN s.content AS note, s.created_at AS created

-- Files mapped to a workspace
MATCH (w:Workspace {workspace_name: 'auth-fix'})-[:MAPPED_TO]->(e:Entity)
WHERE e.type = 'File'
RETURN e.id AS file

-- All functions in files mapped to a workspace
MATCH (w:Workspace {workspace_name: 'auth-fix'})-[:MAPPED_TO]->(file:Entity {type: 'File'})
MATCH (fn:Entity)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)
RETURN fn.id AS function
```

### Metadata & Staleness

```cypher
-- Entity counts by type
MATCH (n:Entity) RETURN n.type AS type, count(n) AS count ORDER BY count DESC

-- Recently modified entities
MATCH (e:Entity) RETURN e.id, e.last_modified ORDER BY e.last_modified DESC LIMIT 10

-- Stale workspace-to-entity mappings
MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity) WHERE r.is_stale = true
RETURN w.workspace_name AS workspace, e.id AS entity
```

### Rich Metadata Queries

The `metadata` field is a JSON string containing signatures, docstrings, and structural
metadata. Query it with `CONTAINS` (the DB does not support JSON path access):

```cypher
-- Find all async functions
MATCH (f:Entity {type: 'Function'})
WHERE f.metadata CONTAINS '"isAsync":true'
RETURN f.id, f.metadata

-- Find all exported functions
MATCH (f:Entity {type: 'Function'})
WHERE f.metadata CONTAINS '"isExported":true'
RETURN f.id

-- Find functions that take a specific parameter type
MATCH (f:Entity {type: 'Function'})
WHERE f.metadata CONTAINS 'ConnectionProxy'
RETURN f.id

-- Find functions with docstrings mentioning a keyword
MATCH (f:Entity {type: 'Function'})
WHERE f.metadata CONTAINS '"docComment"' AND f.metadata CONTAINS 'lock'
RETURN f.id

-- View full signature and metadata for a function
MATCH (f:Entity {type: 'Function'})
WHERE f.id CONTAINS 'withConnection'
RETURN f.id, f.metadata

-- Find all functions without docstrings (no docComment field)
MATCH (f:Entity {type: 'Function'})
WHERE NOT f.metadata CONTAINS '"docComment"'
RETURN f.id
```

## Cypher Dialect Pitfalls

This system uses a DuckDB-backed Cypher engine. The following are known differences from Neo4j/openCypher:

### 1. `type(r)` does not exist

```
❌ MATCH ()-[r]->() RETURN type(r)
   → Error: function TYPE does not exist

✅ MATCH ()-[r:LINKED_TO]->() RETURN r.relationship_type
```

### 2. Strict property binding

Accessing a property that doesn't exist on **all** matched nodes throws a Binder exception instead of returning `null`:

```
❌ MATCH (n) RETURN n.id
   → Error: Binder exception: Cannot find property id for n
   (because Workspace nodes don't have an `id` property)

✅ MATCH (n:Entity) RETURN n.id
   (filter by label first so all matched nodes have the property)

✅ MATCH (n) RETURN keys(n) AS props
   (use keys() to discover what properties exist before accessing them)
```

### 3. Relationship table names must match exactly

```
❌ MATCH (w:Workspace)-[:HAS_ENTITY]->(e:Entity)
   → Error: Table HAS_ENTITY does not exist

✅ MATCH (w:Workspace)-[:MAPPED_TO]->(e:Entity)
```

### 4. Large result sets are spooled

Results > 20 rows are not returned inline. They are written to:
`.chunks/memory_dumps/query_out.txt`

If a query returns a "spooled" message, read that file to see full results.

## Best Practices

1. **Always initialize a workspace** before starting substantive work. This creates a persistent context container.

2. **Use `CONTAINS` for fuzzy entity matching** instead of exact IDs. Entity IDs can be long hierarchical paths, and you often only know part of the name:
   ```cypher
   WHERE e.id CONTAINS 'getConn'    -- instead of exact match
   ```

3. **Query the reverse call graph for impact analysis.** Before modifying a function, find all callers:
   ```cypher
   MATCH (caller)-[:LINKED_TO {relationship_type: 'CALLS'}]->(target)
   WHERE target.id CONTAINS 'functionYouWantToChange'
   RETURN caller.id
   ```

4. **Record "why" not "what".** Scratchpad notes should capture decisions and rationale, not actions. The code diff already shows what changed — notes should explain why.

5. **Discover schema with `keys()` before deep queries.** If you're unsure what properties a node type has, run `keys(n)` first:
   ```cypher
   MATCH (n:Entity) RETURN keys(n) AS props LIMIT 1
   ```

6. **Use multi-hop paths for transitive analysis.** The graph supports `[r*1..N]` traversal which is powerful for understanding cascading impacts:
   ```cypher
   MATCH path = (src)-[:LINKED_TO {relationship_type: 'CALLS'}*1..3]->(dst)
   ```

7. **Filter by label before accessing properties.** The strict binder means you should always specify node labels (`:Entity`, `:Workspace`, `:Scratchpad`) to ensure property compatibility.

## Guardrails

- **Read-Only:** `yaam_graph_explore` is strictly read-only. Write operations (CREATE, MERGE, SET, DELETE, etc.) are blocked.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`. Read that file if directed.
- **Memory Decay:** Older notes lose relevance. Focus on the most recent context returned by retrieval tools.
- **Multi-Agent:** Multiple agents share the same database. Lock contention is handled via exponential backoff.
