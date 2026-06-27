# YAAM Agent Onboarding & Instructions

This repository is equipped with **YAAM (Yet Another Agent Memory)**. All agents interacting with this codebase MUST use the memory engine to maintain continuity and structural awareness.

## 1. Environment & Tools

YAAM runs as a **first-class pi extension** with three LLM-callable tools and a `/yaam` status command. No CLI scripts or shell-outs are needed for pi agents.

- **Extension entry point:** `src/index.ts` (loaded via `pi.extensions` in `package.json`)
- **Core skill:** `yaam-memory-manager` (located at `skills/yaam-memory-manager/SKILL.md`)
- **CLI scripts** (for non-pi agents only): `skills/yaam-memory-manager/scripts/`

### Available Tools

| Tool | Purpose |
|------|---------|
| `yaam_graph_explore` | Run read-only Cypher queries against the memory graph |
| `yaam_workspace_initialize` | Create a new task tracking workspace |
| `yaam_workspace_append_note` | Record insights/decisions to a workspace scratchpad |
| `/yaam` command | Show memory status (entity counts, active workspace, recent notes) |

## 2. Mandatory Workflows

### Memory Exploration
Before making architectural changes, query the existing code relationships:
```
Tool: yaam_graph_explore(query="MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC")
```

### Task Initialization
When starting a new task, initialize a dedicated context:
```
Tool: yaam_workspace_initialize(name="your-active-task", description="What you're working on")
```

### Insight Capture
Record all "Why" decisions and learnings:
```
Tool: yaam_workspace_append_note(workspace="your-active-task", content="Decided X because Y")
```

## 3. Physical State Sync

The extension automatically reconciles the codebase in the background:

- **After tool use** (`read`, `write`, `edit`, `bash`): Schedules an incremental reconcile that parses changed files and updates the graph. Runs in the background — does not block the agent.
- **At agent end**: Schedules a full reconcile that scans the entire codebase, cleans up stale entities, and rebuilds the graph.
- **Stale cleanup**: When entities are deleted, their structural edges (CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM) are also removed.

No manual sync is needed. The reconciler uses the TypeScript Compiler API for TS/JS files and Pyright LSP for Python files.

## 4. Querying Examples

### Entity counts
```cypher
MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC
```

### Import dependencies
```cypher
MATCH (src:Entity {type: 'File'})-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst:Entity {type: 'File'}) RETURN src.id, dst.id
```

### Call graph
```cypher
MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity) RETURN caller.id, callee.id
```

### Inheritance
```cypher
MATCH (sub:Entity)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup:Entity) RETURN sub.id, sup.id
```

### Workspace notes
```cypher
MATCH (w:Workspace {status: 'active'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at DESC
```

### Files tracked to active workspace
```cypher
MATCH (w:Workspace {status: 'active'})-[:MAPPED_TO]->(e:Entity) RETURN e.id, e.type
```

## 5. Guardrails

- **Read-Only:** `yaam_graph_explore` is strictly read-only. Write operations are blocked.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`. Read that file if directed.
- **Memory Decay:** Older notes lose relevance. Focus on the most recent context.
- **Multi-Agent:** Multiple agents can share the same database. Lock contention is handled via exponential backoff.