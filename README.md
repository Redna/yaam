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

Depending on your agent environment, you can install YAAM either as a native agent plugin or manually as a project-level server.

### Option A: Native Plugin Installation (Recommended)

Since YAAM bundles native plugin manifests, you can install it directly using your agent's native extension mechanisms.

#### 1. Claude Code (via `/plugin install`)
Run the native plugin installer inside Claude Code. Since this is a private repository, ensure your local Git SSH keys are configured:
```bash
/plugin install git@github.com:Redna/yaam.git
```
* **How it works:** Claude Code clones the repo into its isolated storage, reads [.claude-plugin/plugin.json](file:///.claude-plugin/plugin.json), and starts the server.

#### 2. Antigravity CLI (via plugins folder)
Clone the repository directly into the Antigravity CLI user-global plugins directory:
```bash
git clone git@github.com:Redna/yaam.git ~/.gemini/antigravity-cli/plugins/yaam-memory
```
* **How it works:** Antigravity CLI reads the root [plugin.json](file:///plugin.json) manifest, registers the `PostToolUse` lifecycle hook, and executes the server under its native OS sandbox (`nsjail` / `sandbox-exec`).

---

### Option B: Local Setup or Manual Project Setup

If you prefer to configure it for a specific project directory only (rather than installing it globally):

#### 1. In a new project, copy the YAAM files:
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

#### 2. Run the installer locally:
Run the installer with the `--local` (or `-l`) flag to restrict configurations strictly to this directory:
```bash
chmod +x install.py
./install.py --local
```
* **How it works:** This builds the local `.venv`, installs requirements, and initializes the local database at `.agents/memory.lbug` without modifying your global settings.

---

## Agent Usage Guide

### 1. Instructions and Onboarding
* **Claude Code:** Ask Claude to load the instructions using:
  ```
  /prompt use_yaam_memory
  ```
* **Antigravity CLI:** Automatically loads the onboarding markdown skill from `.agents/skills/yaam-memory-manager/SKILL.md` (or through the global symlink).

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
