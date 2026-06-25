---
name: yaam-memory-manager
description: Manage episodic agent memory using the YAAM (Yet Another Agent Memory) engine. Use this skill to initialize workspaces, record scratchpad notes, and explore the memory graph to maintain context across sessions.
---

# YAAM Memory Manager

This skill provides procedural guidance for interacting with the YAAM engine, which separates physical code structure (Layer 0) from cognitive agent context (Layer 1).

## Core Concepts

- **Layer 0 (Physical):** Automatically tracked files and functions. Use `graph_explore.ts` to query.
- **Layer 1 (Cognitive):** Agent-defined workspaces and scratchpads.
- **Reconciliation:** The system automatically syncs with the filesystem after tool use.

## Workflows

Since the MCP server is disabled, you must interact with the memory graph by executing TypeScript scripts with Node.js/tsx.

### 1. Initializing a Task
When starting a new feature or bug fix, always initialize a workspace to group your thoughts.
- **Script Command:** `npx tsx .agents/skills/yaam-memory-manager/scripts/workspace_initialize.ts --name <workspace-name> --description <description>`
- **Guideline:** Use descriptive names like `auth-fix` or `ui-refactor`.

### 2. Recording Insights
As you discover nuances or make decisions, record them in the scratchpad.
- **Script Command:** `npx tsx .agents/skills/yaam-memory-manager/scripts/workspace_append_note.ts --workspace <workspace-name> --content <content>`
- **Guideline:** Record "why" decisions, not just "what" was done.

### 3. Exploring Relationships
To understand how code components are linked, query the graph.
- **Script Command:** `npx tsx .agents/skills/yaam-memory-manager/scripts/graph_explore.ts "<query>"`
- **Example Query:**
  ```bash
  npx tsx .agents/skills/yaam-memory-manager/scripts/graph_explore.ts "MATCH (f:Entity {type: 'Function'})-[:LINKED_TO]->(file:Entity) RETURN f.id, file.id LIMIT 10"
  ```

## Guardrails
- **Read-Only Queries:** `graph_explore.ts` is strictly read-only.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/query_out.txt`. Use `grep` or `cat` on those files if directed.
- **Memory Decay:** Older notes lose relevance. Focus on the most recent context returned by retrieval tools.
