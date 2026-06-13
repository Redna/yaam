# YAAM (Yet Another Agent Memory) — Cross-Agent Memory Engine

YAAM is a lightweight, 2-layered agent memory system designed to maintain continuity and structural awareness across different AI coding agents. It separates your physical file structures/AST (Layer 0) from the cognitive agent reasoning states/workspaces (Layer 1).

This engine is optimized for **cross-agent compatibility**, supporting both **Claude Code** and **Antigravity CLI** seamlessly.

---

## How it Works

* **Layer 0 (Physical Topology):** Automatically tracked files, function declarations, and Git changes. 
* **Layer 1 (Cognitive Context):** User/Agent-defined workspaces and chronological scratchpads that capture design rationale and decisions.
* **Passive Reconciliation:** For agents that do not support native client-side lifecycle hooks (like Claude Code), YAAM automatically runs git-status physical synchronization internally on the server before executing any memory tools.

---

## Installation Guide

To avoid polluting your repositories with duplicate setup and execution scripts, **Global Installation** is the recommended method. 

### Option 1: Global Plugin Mode (Recommended - Clean Setup)

In this mode, the YAAM codebase is stored in a single, central directory on your machine. No plugin source files are copied or cloned into your active project repositories.

#### A. Claude Code (via `/plugin install`)
Install the plugin globally using your private repository SSH URL:
```bash
/plugin install git@github.com:Redna/yaam.git
```
* **How it works:** Claude Code clones this repository once into its global user plugins directory. The server runs globally but operates relative to whichever local workspace folder you open Claude in.

#### B. Antigravity CLI (via plugins folder)
Clone the repository once directly into the Antigravity CLI global plugins directory:
```bash
git clone git@github.com:Redna/yaam.git ~/.gemini/antigravity-cli/plugins/yaam-memory
```
* **How it works:** Antigravity CLI parses the `plugin.json` manifest globally, registering tools and hooks, and executes the server under its native OS sandbox (`nsjail` / `sandbox-exec`) on the active workspace.

---

### Option 2: Project-Local Mode (Legacy/Manual Fallback)

If you explicitly need to bundle the plugin inside a specific repository or want to customize it locally:

1. **Copy YAAM files into your project root:**
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

2. **Run the installer locally:**
   ```bash
   chmod +x install.py
   ./install.py --local
   ```
   *This initializes a local `.venv` and database at `.agents/memory.lbug` without modifying global files.*

---

## Agent Usage Guide

### 1. Instructions and Onboarding
* **Claude Code:** Ask Claude to load the instructions using:
  ```
  /prompt use_yaam_memory
  ```
* **Antigravity CLI:** Automatically loads the onboarding markdown skill from `.agents/skills/yaam-memory-manager/SKILL.md`.

### 2. Core Workflows (For Agents)

#### Task Initialization
Initialize a workspace when starting a new task:
```cypher
workspace_initialize(name="refactor-auth", description="Refactoring authentication layer")
```

#### Recording Key Rationale
Capture "Why" decisions and architectural learnings:
```cypher
workspace_append_note(workspace_name="refactor-auth", content="Switched to JWT token storage to support cross-agent sessions.")
```

#### Exploring Memory Relationships
Query the physical and cognitive graph database using Cypher:
```cypher
graph_explore("MATCH (w:Workspace {workspace_name: 'refactor-auth'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at DESC")
```
