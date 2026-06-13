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

To achieve zero-friction setup without nesting Git repositories, use the following one-liner commands.

### Option 1: Global Plugin Mode (Recommended - Cleanest Setup)

In this mode, the YAAM codebase is stored in a single, central directory on your machine. No plugin source files are copied or cloned into your active project repositories.

#### A. Claude Code (via `/plugin install`)
Install the plugin globally using your private repository SSH URL:
```bash
/plugin install git@github.com:Redna/yaam.git
```
* **How it works:** Claude Code clones this repository once into its global user plugins directory. The server runs globally but operates relative to whichever local workspace folder you open Claude in.

#### B. Antigravity CLI (via plugins folder)
Run this one-liner to clone the repository once directly into the Antigravity global plugins directory and run the installation script:
```bash
git clone git@github.com:Redna/yaam.git ~/.gemini/antigravity-cli/plugins/yaam-memory && cd ~/.gemini/antigravity-cli/plugins/yaam-memory && ./install.py
```

---

### Option 2: Project-Local Mode (No Repository Nesting)

If you need the YAAM files directly inside a specific project repository, run this single command from your target repository root:

```bash
git archive --remote=git@github.com:Redna/yaam.git main | tar -x && ./install.py --local
```

* **How it works:**
  1. `git archive` downloads all the codebase files from the remote `main` branch via SSH.
  2. `tar -x` extracts them directly into your current directory (avoiding any nested Git repository clutter).
  3. `./install.py --local` initializes the project-local `.venv` and database inside the active directory.

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
