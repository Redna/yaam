# YAAM (Yet Another Agent Memory)

2-layered agent memory system: physical code topology (Layer 0) + cognitive workspaces (Layer 1). Written in TypeScript, backed by LadybugDB.

## Architecture

```
src/
├── index.ts          Pi extension: tools, hooks, /yaam command, progress bar UI
├── db.ts             ConnectionManager: Mutex + worker IPC + lock-and-release
├── db_worker.ts       Forked worker process: LadybugDB native isolation, safe IPC
├── reconciler.ts     Background reconciler: parse phase (no lock) + commit phase (brief lock)
├── graph_explore.ts  Read-only Cypher with write protection + result spooling
└── workspace.ts      Workspace init, note append, file tracking

skills/yaam-memory-manager/
├── SKILL.md              Agent onboarding skill
└── scripts/              CLI scripts for non-pi agents (Gemini, etc.)
    ├── db.ts             Standalone DB connection
    ├── graph_explore.ts  CLI query tool
    ├── reconciler.ts     CLI reconciler
    ├── workspace_*.ts    CLI workspace tools
    └── lsp_client.ts     Pyright LSP client (shared with extension)
```

## How It Works

### Layer 0 — Physical Topology
Automatically tracks files, functions, classes, call graphs, inheritance, and imports.
- **TS/JS:** TypeScript Compiler API (`createLanguageService`, `resolveModuleName`)
- **Python:** Pyright LSP via stdio JSON-RPC

### Layer 1 — Cognitive Context
Agent-defined workspaces with chronological scratchpads for design rationale and decisions.

### Database Isolation
LadybugDB uses native C++ with mmap buffer pools. A native segfault kills the process. All DB operations run in a **forked worker process** (`db_worker.ts`) to isolate crashes.

- **Buffer pool:** 256 MB (default ~8 TB causes mmap failures)
- **CHECKPOINT** before every close (prevents WAL corruption)
- **Worker reuse** on success (no fork overhead); **SIGKILL** on error (clean buffer pool)
- **Mutex** serializes `withConnection()` calls — only one DB operation at a time
- **IPC:** `safeSend()` with error callbacks prevents EPIPE crashes on both sides

### Background Reconciler
Fire-and-forget, two phases:
1. **Parse** (no DB lock): scan files, extract AST, resolve calls/inheritance. Yields via `setImmediate()` between files — never blocks the developer.
2. **Commit** (brief DB lock): write entities + edges, clean stale entries. Also yields between files.

Progress is reported via a `progress` getter and rendered as a Unicode bar in the status bar: `Sync 🔄 Parsing files ████░░░░░░ 12/42` (250ms polling).

### Optimistic Saves
Workspace tools (`yaam_workspace_initialize`, `yaam_workspace_append_note`) fire DB writes without awaiting — they return immediately. Write completes in the background.

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
| `yaam_graph_explore` | Read-only Cypher query. Write ops blocked. Results > 20 rows spooled to file. |
| `yaam_workspace_initialize` | Create workspace, deactivate previous. Schedules full reconcile. |
| `yaam_workspace_append_note` | Append note to workspace scratchpad. |
| `/yaam` | Status command: entity counts, workspace, notes, reconciler state. |

## Lifecycle Hooks

| Event | Action |
|-------|--------|
| `session_start` | Status: "Ready ✅" |
| `session_shutdown` | Kill status timer, shutdown LSP clients |
| `turn_start` | Resume progress polling if reconciler running |
| `tool_result` | Schedule incremental reconcile (write/edit/bash) |
| `agent_end` | Schedule full reconcile |

## Setup

```bash
npm install
```

Loaded automatically by pi from `pi.extensions` in `package.json`.

## Multi-Agent Support

All agents share `memory.lbug`. Lock contention handled by exponential backoff (50ms → 2s, 10 retries). Each agent holds the lock only during the commit phase (~100ms).

- **pi:** In-process extension tools
- **Gemini/Antigravity:** CLI scripts via `.gemini/settings.json` hooks
- **Other agents:** CLI scripts via `.agents/hooks.json` hooks