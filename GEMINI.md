# YAAM Agent Onboarding & Instructions (Antigravity / Gemini CLI)

This repository is equipped with **YAAM (Yet Another Agent Memory)**. All agents interacting with this codebase MUST use the memory engine to maintain continuity and structural awareness.

## 1. Environment & Tools

YAAM runs as a **first-class pi extension** when used with pi. For Gemini/Antigravity and other agents, CLI scripts are available.

- **Pi extension:** Tools (`yaam_graph_explore`, `yaam_workspace_initialize`, `yaam_workspace_append_note`) and `/yaam` command are registered automatically.
- **CLI scripts:** Located at `skills/yaam-memory-manager/scripts/` (for non-pi agents).
- **Node Env:** Run CLI scripts via `npx tsx skills/yaam-memory-manager/scripts/<script>.ts`.

## 2. Mandatory Workflows

### Memory Exploration
Before making architectural changes, query the existing relationships:

**Pi (in-process tool):**
```
yaam_graph_explore(query="MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC")
```

**Gemini/CLI:**
```bash
npx tsx skills/yaam-memory-manager/scripts/graph_explore.ts "MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC"
```

### Task Initialization
When starting a new task, initialize a dedicated context:

**Pi:**
```
yaam_workspace_initialize(name="your-active-task", description="What you're working on")
```

**Gemini/CLI:**
```bash
npx tsx skills/yaam-memory-manager/scripts/workspace_initialize.ts --name "your-active-task" --description "What you're working on"
```

### Insight Capture
Record all "Why" decisions and learnings:

**Pi:**
```
yaam_workspace_append_note(workspace="your-active-task", content="Decided X because Y")
```

**Gemini/CLI:**
```bash
npx tsx skills/yaam-memory-manager/scripts/workspace_append_note.ts --workspace "your-active-task" --content "Decided X because Y"
```

## 3. Physical State Sync

### Pi agents
The extension automatically reconciles the codebase in the background after tool use and at agent end. No manual sync is needed.

### Gemini / other agents
The system relies on hooks configured in `.gemini/settings.json` (`AfterTool`) and `.agents/hooks.json` (`PostToolUse`). These trigger the reconciler CLI script after tool use. If manual sync is needed:
```bash
npx tsx skills/yaam-memory-manager/scripts/reconciler.ts          # incremental
npx tsx skills/yaam-memory-manager/scripts/reconciler.ts --full  # full scan
```

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

## 5. Multi-Agent Coexistence

Multiple agents can share the same `memory.lbug` database file. Lock contention is handled via exponential backoff (50ms → 2s, up to 10 retries). Each agent holds the database lock only for milliseconds during queries or reconciliation writes.

## 6. Guardrails

- **Read-Only:** `graph_explore` is strictly read-only. Write operations are blocked.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`.
- **Memory Decay:** Older notes lose relevance. Focus on the most recent context.