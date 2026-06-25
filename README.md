# YAAM (Yet Another Agent Memory) — Cross-Agent Memory Engine

YAAM is a lightweight, 2-layered agent memory system designed to maintain continuity and structural awareness across different AI coding agents. It separates your physical file structures/AST (Layer 0) from the cognitive agent reasoning states/workspaces (Layer 1).

This engine is written **100% in TypeScript**, using the native TypeScript Compiler API for TS/JS AST analysis, and the Pyright Language Server via stdio JSON-RPC for Python codebase analysis. It utilizes **LadybugDB** as its underlying graph database and supports direct CLI script executions and lifecycle hooks (e.g. `pi.dev` extensions).

---

## How it Works

* **Layer 0 (Physical Topology):** Automatically tracked files, class declarations, method and function definitions, and call/inheritance graphs.
  * **TypeScript/JavaScript**: Extracted natively using the programmatic TypeScript Language Service API.
  * **Python**: Extracted using the `pyright-langserver` via stdio JSON-RPC queries.
* **Layer 1 (Cognitive Context):** User/Agent-defined workspaces and chronological scratchpads that capture design rationale, insights, and decisions.
* **Automated Sync Hooks**: Runs incremental physical synchronization automatically after tool use (via post-tool hooks) and at turn/agent boundaries (via the `pi.dev` extension hooks).

---

## Installation & Setup

Set up the project dependencies using standard Node.js package managers:

```bash
# Install dependencies (LadybugDB, TS, tsx compiler runner)
npm install
```

---

## Agent Integration & Customizations

This workspace is configured with customizations loaded by the agent:

* **Workspace Customizations Root**: `.agents/` (contains `AGENTS.md` rules and the onboarding skill).
* **Onboarding Skill**: Loaded from `.agents/skills/yaam-memory-manager/SKILL.md`.
* **Agent Hooks**:
  * **Gemini/Antigravity**: Configured in `.gemini/settings.json` under `AfterTool` hooks to trigger `npx tsx reconciler.ts`.
  * **Other Agent Runners**: Configured in `.agents/hooks.json` under `PostToolUse` hooks.
  * **pi.dev Extension**: The extension in `.pi/extensions/yaam/index.ts` subscribes to `turn_end` and `agent_end` events to automatically execute the reconciler in the background.

---

## Usage Guide (For Agents)

Agents interact with the memory engine using local CLI scripts executed with Node.js/tsx.

### 1. Initialize a Workspace
When beginning a new feature or refactor, initialize a task tracking workspace:
```bash
npx tsx .agents/skills/yaam-memory-manager/scripts/workspace_initialize.ts --name "<workspace-name>" --description "<description>"
```

### 2. Append Key Insights
Record "Why" decisions and architectural learnings to the active workspace scratchpad:
```bash
npx tsx .agents/skills/yaam-memory-manager/scripts/workspace_append_note.ts --workspace "<workspace-name>" --content "<insight>"
```

### 3. Explore Code & Memory Relationships
Query the database using read-only Cypher queries to understand code linkages or recall past thoughts:
```bash
npx tsx .agents/skills/yaam-memory-manager/scripts/graph_explore.ts "MATCH (n:Entity) RETURN n.type, count(n)"
```

### 4. Run Manual Sync
If manual codebase reconciliation is needed, execute the reconciler script:
```bash
npx tsx .agents/skills/yaam-memory-manager/scripts/reconciler.ts
```
* Use `--full` to force a complete codebase scan:
  ```bash
  npx tsx .agents/skills/yaam-memory-manager/scripts/reconciler.ts --full
  ```
