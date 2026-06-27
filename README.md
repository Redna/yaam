# YAAM (Yet Another Agent Memory) — Cross-Agent Memory Engine

YAAM is a lightweight, 2-layered agent memory system designed to maintain continuity and structural awareness across different AI coding agents. It separates your physical file structures/AST (Layer 0) from the cognitive agent reasoning states/workspaces (Layer 1).

This engine is written **100% in TypeScript**, using the native TypeScript Compiler API for TS/JS AST analysis (including `ts.resolveModuleName()` for import resolution), and the Pyright Language Server via stdio JSON-RPC for Python codebase analysis. It utilizes **LadybugDB** as its underlying graph database.

---

## How it Works

* **Layer 0 (Physical Topology):** Automatically tracked files, class declarations, method and function definitions, call graphs, inheritance relationships, and import dependencies.
  * **TypeScript/JavaScript**: Extracted natively using the TypeScript Compiler API — `ts.createLanguageService()` for call/inheritance resolution, `ts.resolveModuleName()` for import resolution (handles `.js`→`.ts` mapping, index files, and bundler resolution rules).
  * **Python**: Extracted using the `pyright-langserver` via stdio JSON-RPC queries.
* **Layer 1 (Cognitive Context):** User/Agent-defined workspaces and chronological scratchpads that capture design rationale, insights, and decisions.
* **Automated Sync Hooks**: Runs incremental physical synchronization in the background after tool use and full reconciliation at agent boundaries.

---

## Architecture

YAAM runs as a **first-class pi extension** — all database operations happen in-process. No subprocess spawning, no shell-outs, no `npx tsx` per query.

### Lock-and-Release with Backoff

Instead of holding a persistent DB connection, each operation opens the database, does its work, and closes. If the database is locked by another process (another pi session, a Gemini hook, etc.), it retries with exponential backoff (50ms → 100ms → 200ms → ... → 2s, up to 10 retries). This enables **multi-agent coexistence** on the same project.

### Background Optimistic Reconciler

The reconciler runs in the background (fire-and-forget). It splits work into two phases:

1. **Phase 1 — Parse (no DB lock):** Read files, parse AST, extract entities, resolve calls and inheritance. This is CPU/IO-bound work that doesn't need the database. Runs in the next event loop tick after `setTimeout(resolve, 0)`, so the caller returns immediately.
2. **Phase 2 — Commit (brief DB lock):** Open the database, write all entities and relationships, close the database. Lock is held only during writes (~100ms), not during parsing (~3-7s).

Multiple reconcile requests coalesce — if an incremental is pending and a full is requested, the full takes priority.

### Stale Entity Cleanup

When entities are soft-deleted (functions/classes removed from files), their structural edges (`CALLS`, `DECLARED_IN`, `IMPORTS`, `INHERITS_FROM`) are deleted via Cypher `DELETE r`. This prevents stale edges from lingering in the graph.

### File Tracking

The extension hooks into pi's `tool_result` event. When the agent uses `read`, `write`, or `edit` tools, the accessed file path is extracted from the tool input and mapped to the active workspace via `MAPPED_TO` relationships.

### Directory Structure

```
src/
├── index.ts        — Pi extension entry point (tools, hooks, /yaam command)
├── db.ts           — ConnectionManager: lock-and-release with exponential backoff
├── reconciler.ts   — Background reconciler (parse phase + commit phase)
├── graph_explore.ts — Read-only Cypher query with write protection
└── workspace.ts     — Workspace init, note append, file tracking

skills/yaam-memory-manager/
├── SKILL.md                          — Onboarding skill for agents
└── scripts/                          — CLI scripts (for non-pi agents: Gemini, etc.)
    ├── db.ts                         — Standalone DB connection
    ├── graph_explore.ts              — CLI query tool
    ├── reconciler.ts                 — CLI reconciler
    ├── workspace_initialize.ts       — CLI workspace init
    ├── workspace_append_note.ts      — CLI note appender
    └── lsp_client.ts                — Pyright LSP client (shared with extension)
```

---

## Installation & Setup

```bash
npm install
```

The extension is loaded automatically by pi from the `pi.extensions` config in `package.json`.

---

## Pi Extension Tools

### yaam_graph_explore
Executes a read-only Cypher query against the YAAM memory graph. Write operations (`CREATE`, `MERGE`, `SET`, `DELETE`, `REMOVE`, `DROP`, `ALTER`) are blocked. Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`.

### yaam_workspace_initialize
Creates a new workspace context. Deactivates any existing active workspace and schedules a full reconciliation to ensure topology is current.

### yaam_workspace_append_note
Appends an insight/note to a workspace's scratchpad.

### /yaam command
Shows a status summary: entity counts, active workspace, recent notes, and reconciler state.

---

## Lifecycle Hooks

| Event | Action |
|-------|--------|
| `session_start` | Set status to "Ready ✅" |
| `session_shutdown` | Shutdown LSP clients (graceful `shutdown` request + `SIGTERM`) |
| `turn_start` | Update status (syncing/idle) |
| `tool_result` | Schedule background incremental reconcile + file tracking (fire-and-forget) |
| `agent_end` | Schedule background full reconcile (fire-and-forget) |

---

## Usage Guide (For Agents)

### 1. Initialize a Workspace
When beginning a new feature or refactor, initialize a task tracking workspace:
```
Tool: yaam_workspace_initialize(name="auth-fix", description="Fixing authentication flow")
```

### 2. Append Key Insights
Record "Why" decisions and architectural learnings to the active workspace scratchpad:
```
Tool: yaam_workspace_append_note(workspace="auth-fix", content="Decided to use JWT with refresh tokens because...")
```

### 3. Explore Code & Memory Relationships
Query the database using read-only Cypher queries to understand code linkages or recall past thoughts:
```
Tool: yaam_graph_explore(query="MATCH (f:Entity {type: 'Function'})-[:LINKED_TO {relationship_type: 'CALLS'}]->(g) RETURN f.id, g.id LIMIT 10")
```

**Useful queries:**

```cypher
-- Entity counts
MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC

-- Import graph
MATCH (src:Entity {type: 'File'})-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst:Entity {type: 'File'}) RETURN src.id, dst.id

-- Call graph
MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity) RETURN caller.id, callee.id

-- Inheritance
MATCH (sub:Entity)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup:Entity) RETURN sub.id, sup.id

-- Recent workspace notes
MATCH (w:Workspace {status: 'active'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at DESC
```

### 4. Check Status
Run the `/yaam` command to see current memory state.

---

## Graph Schema

### Node Tables

| Table | Key | Fields | Description |
|-------|-----|--------|-------------|
| `Entity` | `id` | `type`, `status`, `last_modified`, `metadata` | Files, Functions, Classes |
| `Workspace` | `workspace_name` | `description`, `status`, `closed_at` | Task contexts |
| `Scratchpad` | `id` | `content`, `created_at` | Notes/insights |

### Relationship Tables

| Table | From → To | Properties | Description |
|-------|-----------|------------|-------------|
| `LINKED_TO` | Entity → Entity | `relationship_type` | CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM |
| `MAPPED_TO` | Workspace → Entity | `created_at`, `invalidated_at`, `is_stale` | Files tracked to a workspace |
| `HAS_SCRATCHPAD` | Workspace → Scratchpad | — | Notes belonging to a workspace |

### Entity ID Format

| Type | Format | Example |
|------|--------|---------|
| File | `<path>` | `src/index.ts` |
| Function | `<file>::<function>` | `src/db.ts::sleep` |
| Method | `<file>::<Class>::<method>` | `src/db.ts::ConnectionManager::withConnection` |
| Class | `<file>::<Class>` | `test_py/a.py::DerivedClass` |

---

## Multi-Agent Support

YAAM supports multiple agents working on the same project simultaneously:

- **pi extension:** Uses in-process `ConnectionManager` with lock-and-release + backoff
- **Gemini/Antigravity:** Uses CLI scripts via `.gemini/settings.json` `AfterTool` hooks
- **Other agents:** Uses CLI scripts via `.agents/hooks.json` `PostToolUse` hooks

All agents share the same `memory.lbug` file. Lock contention is handled by exponential backoff — each agent holds the lock only for milliseconds during queries or the commit phase of reconciliation.

---

## Performance

| Operation | CLI scripts (old) | In-process (current) |
|-----------|-------------------|---------------------|
| `yaam_graph_explore` | ~600ms (shell-out) | ~75ms (lock-and-release) |
| `yaam_workspace_initialize` | ~600ms+ | ~40ms |
| `yaam_workspace_append_note` | ~600ms+ | ~43ms |
| Reconcile scheduling | blocked event loop | 0ms (fire-and-forget) |
| Reconcile parse phase | N/A (in script) | ~3-7s (background, no lock) |
| Reconcile commit phase | N/A (in script) | ~100ms (brief lock) |
| DB locking | race conditions | eliminated (backoff) |
| Multi-agent | ✗ (lock contention) | ✓ (lock-and-release) |