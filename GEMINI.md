# YAAM Agent Onboarding & Instructions

This repository is equipped with **YAAM (Yet Another Agent Memory)**. All agents interacting with this codebase MUST use the memory engine to maintain continuity and structural awareness.

## 1. Environment & Tools
- **MCP Server:** `yaam_memory` (launched automatically via `.gemini/settings.json`).
- **Core Skill:** `yaam-memory-manager` (provides procedural workflows).
- **Python Env:** Always use the local virtual environment at `./.venv`.

## 2. Mandatory Workflows
- **Memory Exploration:** Before making architectural changes, use `graph_explore` to query the existing Layer 0 (Physical) and Layer 1 (Cognitive) relationships.
- **Task Initialization:** When starting a new task, always run `workspace_initialize` to create a dedicated context.
- **Insight Capture:** Record all "Why" decisions and non-obvious learnings using `workspace_append_note`.

## 3. Physical State Sync
The system relies on a **PostToolUse** reconciler. If you make file system changes manually or via tools, ensure you run the reconciler to keep the graph in sync:
```bash
./.venv/bin/python3 reconciler.py
```

## 4. Querying Examples
To see your thoughts in the current workspace:
```cypher
MATCH (w:Workspace {workspace_name: 'your-active-task'})-[:HAS_SCRATCHPAD]->(s:Scratchpad)
RETURN s.content, s.created_at ORDER BY s.created_at DESC
```
