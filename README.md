# YAAM (Yet Another Agent Memory) — Cross-Agent Memory Engine

YAAM is a lightweight, 2-layered agent memory system designed to maintain continuity and structural awareness across different AI coding agents. It separates your physical file structures/AST (Layer 0) from the cognitive agent reasoning states/workspaces (Layer 1).

This engine is optimized for **cross-agent compatibility**, supporting both **Claude Code** and **Antigravity CLI** seamlessly.

---

## How it Works

* **Layer 0 (Physical Topology):** Tracked files, function declarations, and Git changes. These are updated automatically after tools run.
* **Layer 1 (Cognitive Context):** User/Agent-defined workspaces and chronological scratchpads that capture design rationale and decisions.

---

## Quick Start: Using YAAM in a New Repository

Follow these steps to integrate YAAM memory into a new codebase or project:

### Step 1: Copy YAAM Files into your New Repository
Copy the core YAAM files and configurations into your new project directory:
```bash
# Core execution scripts
cp /path/to/yaam/server.py /new/repo/
cp /path/to/yaam/reconciler.py /new/repo/
cp /path/to/yaam/db.py /new/repo/
cp /path/to/yaam/parser.py /new/repo/
cp /path/to/yaam/logger.py /new/repo/
cp /path/to/yaam/requirements.txt /new/repo/
cp /path/to/yaam/install.py /new/repo/

# Configs & Skills
cp -R /path/to/yaam/.agents /new/repo/
cp /path/to/yaam/.mcp.json /new/repo/
```

### Step 2: Run the Installer
Run the setup script inside the new repository root:
```bash
chmod +x install.py
./install.py
```
This script automatically:
1. Creates a local virtual environment (`.venv`) and installs database/AST parsing dependencies.
2. Initializes a new local database (`memory.lbug`) at `.agents/memory.lbug`.
3. Sets up global configurations so both **Claude Code** and **Antigravity CLI** recognize the new project-level server.

---

## Agent Usage Guide

### 1. Claude Code
Claude Code reads the root-level `.mcp.json` automatically. When starting Claude Code in the repository:
* **Load Instructions:** Ask Claude to run the prompt:
  ```
  /prompt use_yaam_memory
  ```
* **Reconciliation:** Claude Code runs *Passive Reconciliation* internally on the server. Whenever Claude calls any YAAM memory tool, the database automatically syncs any filesystem changes.

### 2. Antigravity CLI
Antigravity CLI reads `.agents/mcp_config.json` and `.agents/hooks.json` automatically:
* **Reconciliation:** Whenever you edit or create files, the Antigravity `PostToolUse` lifecycle hook runs `reconciler.py` automatically.

---

## Core Memory Workflows (For Agents)

Always use these memory tools to track tasks:

### Task Initialization
Initialize a workspace when starting a new task:
```cypher
workspace_initialize(name="refactor-auth", description="Refactoring authentication layer")
```

### Recording Key Rationale
Capture "Why" decisions and architectural learnings:
```cypher
workspace_append_note(workspace_name="refactor-auth", content="Switched to JWT token storage to support cross-agent sessions.")
```

### Exploring Memory Relationships
Query the physical and cognitive graph database using Cypher:
```cypher
// Query all active scratchpad notes for a task
graph_explore("MATCH (w:Workspace {workspace_name: 'refactor-auth'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at DESC")
```
