---
name: yaam-memory-manager
description: Manage episodic agent memory using the YAAM (Yet Another Agent Memory) engine. Use this skill to initialize workspaces, record scratchpad notes, and explore the memory graph to maintain context across sessions.
---

# YAAM Memory Manager

This skill provides procedural guidance for interacting with the YAAM engine, which separates physical code structure (Layer 0) from cognitive agent context (Layer 1).

## Core Concepts

- **Layer 0 (Physical):** Automatically tracked files and functions. Use `graph_explore` to query.
- **Layer 1 (Cognitive):** Agent-defined workspaces and scratchpads.
- **Reconciliation:** The system automatically syncs with the filesystem after tool use.

## Workflows

### 1. Initializing a Task
When starting a new feature or bug fix, always initialize a workspace to group your thoughts.
- **Tool:** `workspace_initialize(name, description)`
- **Guideline:** Use descriptive names like `auth-fix` or `ui-refactor`.

### 2. Recording Insights
As you discover nuances or make decisions, record them in the scratchpad.
- **Tool:** `workspace_append_note(workspace_name, content)`
- **Guideline:** Record "why" decisions, not just "what" was done.

### 3. Exploring Relationships
To understand how code components are linked, query the graph.
- **Tool:** `graph_explore(query)`
- **Example Query:**
  ```cypher
  MATCH (f:Entity {type: 'Function'})-[:LINKED_TO]->(file:Entity)
  RETURN f.id, file.id LIMIT 10
  ```

## Guardrails
- **Read-Only:** `graph_explore` is strictly read-only.
- **Context Protection:** Results > 20 rows are spooled to `.chunks/memory_dumps/`. Use `grep` or `cat` on those files if directed.
- **Memory Decay:** Older notes lose relevance. Focus on the most recent context returned by retrieval tools.
