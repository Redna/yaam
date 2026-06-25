# YAAM Agent Onboarding & Instructions (Antigravity CLI)

This repository is equipped with **YAAM (Yet Another Agent Memory)**. All agents interacting with this codebase MUST use the memory engine to maintain continuity and structural awareness.

## 1. Environment & Tools
- **CLI/Extension:** Local TypeScript scripts inside the agentskill, or the `pi.dev` extension. (The Python MCP server has been disabled).
- **Core Skill:** `yaam-memory-manager` (located at `.agents/skills/`).
- **Node Env:** Run scripts via `npx tsx .agents/skills/yaam-memory-manager/scripts/<script>.ts`.


## 2. Mandatory Workflows
- **Memory Exploration:** Before making architectural changes, query the existing relationships using:
  ```bash
  npx tsx .agents/skills/yaam-memory-manager/scripts/graph_explore.ts "<query>"
  ```
- **Task Initialization:** When starting a new task, initialize a dedicated context:
  ```bash
  npx tsx .agents/skills/yaam-memory-manager/scripts/workspace_initialize.ts --name "<your-active-task>" --description "<description>"
  ```
- **Insight Capture:** Record all "Why" decisions and learnings:
  ```bash
  npx tsx .agents/skills/yaam-memory-manager/scripts/workspace_append_note.ts --workspace "<your-active-task>" --content "<insight>"
  ```

## 3. Physical State Sync
The system relies on a **PostToolUse** reconciler (configured in `.agents/hooks.json`). If you make file system changes manually or via tools, the reconciler should keep the graph in sync automatically. If manual sync is needed:
```bash
npx tsx .agents/skills/yaam-memory-manager/scripts/reconciler.ts
```

## 4. Querying Examples
To see your thoughts in the current workspace:
```cypher
MATCH (w:Workspace {workspace_name: 'your-active-task'})-[:HAS_SCRATCHPAD]->(s:Scratchpad)
RETURN s.content, s.created_at ORDER BY s.created_at DESC
```
