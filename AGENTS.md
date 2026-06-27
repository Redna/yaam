# YAAM — Agent Instructions

All agents working in this repo MUST use YAAM tools to maintain continuity and structural awareness.

## Tools

| Tool | Purpose |
|------|---------|
| `yaam_graph_explore` | Read-only Cypher queries against the memory graph |
| `yaam_workspace_initialize` | Create a task tracking workspace (deactivates previous) |
| `yaam_workspace_append_note` | Record decisions/insights to a workspace scratchpad |
| `/yaam` command | Show memory status (entity counts, workspace, notes) |

## Mandatory Workflows

1. **Before architectural changes:** query the graph to understand existing structure.
2. **When starting a task:** initialize a workspace.
3. **After key decisions:** append a note explaining "why", not just "what".

## Architecture (Deep Knowledge)

### Database Layer: Worker Process Isolation

LadybugDB (`@ladybugdb/core` v0.17.1) uses native C++ with mmap for its buffer pool. A segfault in the native layer crashes the entire Node.js process. To isolate this, all DB operations run in a **forked worker process** (`src/db_worker.ts`).

**`ConnectionManager` (`src/db.ts`):**
- Serializes access via an in-process `Mutex` — only one `withConnection()` runs at a time.
- Each `withConnection()`: forks worker → open DB → run schema setup → execute fn → CHECKPOINT → close DB → return.
- **Worker reuse:** On success, the worker stays alive for the next call (no fork overhead). On error, the worker is SIGKILLed so the next call gets a fresh process with a clean buffer pool.
- **IPC:** Parent sends messages (`open`, `close`, `prepare`, `execute_stmt`, `query`), worker responds via `process.send()`. Both sides use callback-based error handling to prevent EPIPE crashes.
- **Buffer pool:** Set to 256 MB (`new ladybug.Database(path, 256 * 1024 * 1024)`). The default is ~8 TB which causes mmap failures on systems with limited address space.
- **CHECKPOINT:** Always runs before `conn.close()` in the worker's `close` handler. Without it, the WAL file is left on disk and the next open segfaults during WAL replay.

**`db_worker.ts`:**
- Single-threaded message handler. Receives one action at a time.
- `safeSend()` wraps `process.send()` with `process.connected` check and error callback → `process.exit(1)`. Prevents EPIPE from crashing the parent.
- `safeOpenDatabase()`: checks for stale WAL file, uses a filesystem lock dir (`.yaam_probe_lock`) for concurrent-open protection.

### Reconciler: Background Code Topology Sync

**`reconciler.ts`** runs fire-and-forget in the background. Two phases:

1. **Parse phase** (no DB lock): scans files, parses AST, resolves calls/inheritance/imports. Yields to the event loop via `setImmediate()` between each file so the developer's tools are never blocked. Uses `execFile` (async) for git status, not `execSync`.
2. **Commit phase** (brief DB lock via `ConnectionManager`): writes entities, edges, cleans up stale entries. Also yields between files.

**Progress reporting:** The reconciler exposes a `progress` getter (`{ phase, detail, current, total }`). The extension polls this every 250ms and renders a Unicode progress bar in the status bar: `Sync 🔄 Parsing files ████░░░░░░ 12/42`.

**Coalescing:** Multiple reconcile requests coalesce. If a full is pending, it takes priority over incremental.

### Extension Entry Point (`index.ts`)

- **Optimistic saves:** `yaam_workspace_initialize` and `yaam_workspace_append_note` fire `connMgr.withConnection()` without awaiting — they return immediately. DB write happens in the background.
- **Status polling:** A 250ms `setInterval` updates the status bar with reconciler progress. Auto-stops when the reconciler finishes.
- **Hooks:** `tool_result` (write/edit/bash) → incremental reconcile. `agent_end` → full reconcile. Both fire-and-forget.

## Graph Schema

### Nodes

| Table | Key | Fields |
|-------|-----|--------|
| `Entity` | `id` (STRING) | `type`, `status`, `last_modified`, `metadata` |
| `Workspace` | `workspace_name` (STRING) | `description`, `status`, `closed_at` |
| `Scratchpad` | `id` (STRING) | `content`, `created_at` |

### Relationships

| Table | From → To | Properties |
|-------|-----------|------------|
| `LINKED_TO` | Entity → Entity | `relationship_type` (CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM) |
| `MAPPED_TO` | Workspace → Entity | `created_at`, `invalidated_at`, `is_stale` |
| `HAS_SCRATCHPAD` | Workspace → Scratchpad | — |

### Entity ID Format

- File: `src/index.ts`
- Function: `src/db.ts::sleep`
- Method: `src/db.ts::ConnectionManager::withConnection`
- Class: `src/reconciler.ts::Reconciler`

## Query Examples

```cypher
-- Entity counts
MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC

-- Call graph
MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity) RETURN caller.id, callee.id

-- Inheritance
MATCH (sub:Entity)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup:Entity) RETURN sub.id, sup.id

-- Import dependencies
MATCH (src:Entity {type: 'File'})-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst:Entity {type: 'File'}) RETURN src.id, dst.id

-- Workspace notes
MATCH (w:Workspace {status: 'active'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at DESC
```

## Gotchas

- **Results > 20 rows** are spooled to `.chunks/memory_dumps/query_out.txt`. Read that file if directed.
- **`yaam_graph_explore` is read-only.** Write keywords (CREATE, MERGE, SET, DELETE, DROP, ALTER) are blocked.
- **If the DB is corrupted** (IO exception with multi-TB position), delete `memory.lbug` and `memory.lbug.wal`. The extension will recreate them on the next operation.
- **Multiple agents** share `memory.lbug`. Lock contention is handled by exponential backoff (50ms → 2s, 10 retries).
- **Stale workers** can accumulate if SIGTERM doesn't kill them. SIGKILL is used in `killWorker()` for reliability.