import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { YaamEngineClient } from "./engine-client.js";
import { Reconciler } from "./reconciler.js";
import { exploreGraph } from "./graph_explore.js";
import { initializeWorkspace, appendNote, trackAccessedFile } from "./workspace.js";
import { startServerIfNeeded } from "./visualizer.js";
import * as path from "path";

export default function yaamExtension(pi: ExtensionAPI) {
  const eventsPath = path.resolve(process.cwd(), "events.jsonl");
  const engine = new YaamEngineClient(eventsPath);
  const reconciler = new Reconciler(engine);

  // ─── Cached memory context (refreshed at session start) ──────────────────
  let memoryContext = "";

  // ─── UI helpers (safe in all modes) ─────────────────────────────────────
  let lastCtx: any = null;
  let statusTimer: ReturnType<typeof setInterval> | null = null;

  function setStatus(ctx: any, key: string, text: string) {
    try { ctx?.ui?.setStatus?.(key, text); } catch {}
  }

  function progressBar(pct: number): string {
    const filled = Math.max(0, Math.min(10, Math.round(pct / 10)));
    return '█'.repeat(filled) + '░'.repeat(10 - filled);
  }

  /** Poll reconciler progress and update the status bar every 250ms. */
  function startStatusPolling(ctx: any) {
    lastCtx = ctx;
    if (statusTimer) clearInterval(statusTimer);
    statusTimer = setInterval(() => {
      if (!reconciler.isRunning) {
        setStatus(lastCtx, "yaam", "Ready ✅");
        if (statusTimer) { clearInterval(statusTimer); statusTimer = null; }
        return;
      }
      const p = reconciler.progress;
      if (!p) {
        setStatus(lastCtx, "yaam", "Sync 🔄 working…");
        return;
      }
      const pct = p.total > 0 ? Math.round((p.current / p.total) * 100) : 0;
      const bar = progressBar(pct);
      setStatus(lastCtx, "yaam", `Sync 🔄 ${p.detail} ${bar} ${p.current}/${p.total}`);
    }, 250);
  }

  // ─── Session lifecycle ───────────────────────────────────────────────────

  async function refreshMemoryContext() {
    try {
      const [typeRows, wsRows] = await Promise.all([
        engine.query({
          match: { label: "Entity" },
          aggregate: { group_by: "type", count: true }
        }),
        engine.query({
          match: { label: "Workspace", status: "active" } }),
      ]);

      const parts: string[] = [];

      // Entity counts
      if (typeRows.length > 0) {
        const counts = typeRows.map((r: any) => `${r.count} ${r.type}`).join(", ");
        parts.push(`Graph: ${counts}`);
      }

      // Active workspace + recent notes
      if (wsRows.length > 0) {
        const ws = wsRows[0];
        parts.push(`Active workspace: ${ws.id}`);
        if (ws.properties?.description) {
          parts.push(`  Task: ${ws.properties.description}`);
        }

        // Fetch recent scratchpad notes
        try {
          const notes = await engine.query({
            match: { label: "Workspace", id: ws.id },
            traverse: { relationship: "HAS_SCRATCHPAD", direction: "outbound", max_depth: 1 },
            limit: 3,
          });
          if (notes.length > 0) {
            const noteSummaries = notes.map((n: any) => {
              const preview = (n.content || "").substring(0, 150);
              return `  - ${preview}`;
            }).join("\n");
            parts.push(`Recent decisions:\n${noteSummaries}`);
          }
        } catch {}
      } else {
        parts.push("No active workspace.");
      }

      memoryContext = parts.join("\n");
    } catch {
      memoryContext = "";
    }
  }

  pi.on("session_start", async (_event, ctx) => {
    lastCtx = ctx;
    try {
      await engine.start();
      setStatus(ctx, "yaam", "Ready ✅");
      reconciler.scheduleFull();
      // Refresh memory context and inject as a one-time message.
      // Do NOT modify the system prompt — that would invalidate the prompt cache.
      // Instead, send the context as a new message that the LLM sees at the start.
      setTimeout(() => {
        refreshMemoryContext().then(() => {
          if (memoryContext) {
            pi.sendMessage({
              customType: "yaam_memory_context",
              content: `[YAAM Memory Context]\n${memoryContext}`,
              display: true,
            }, { deliverAs: "nextTurn" });
          }
        });
      }, 3000);
    } catch (e: any) {
      setStatus(ctx, "yaam", `Error ❌`);
      ctx.ui.notify(`Failed to start YAAM Engine: ${e.message}`, "error");
    }
  });

  // NOTE: Do NOT modify the system prompt via before_agent_start.
  // The system prompt must remain stable across turns to preserve the
  // LLM provider's prompt cache. Dynamic memory context is delivered
  // as one-time messages via pi.sendMessage instead.

  pi.on("session_shutdown", async () => {
    if (statusTimer) { clearInterval(statusTimer); statusTimer = null; }
    engine.stop();
  });

  pi.on("turn_start", async (_event, ctx) => {
    lastCtx = ctx;
    if (reconciler.isRunning) {
      startStatusPolling(ctx);
    } else {
      setStatus(ctx, "yaam", "Idle 💤");
    }
  });

  // ─── Reconciliation hooks (fire-and-forget) ─────────────────────────────

  pi.on("tool_result", async (event, ctx) => {
    const toolName = (event as any).toolName;
    const toolInput = (event as any).input;

    if (["write", "edit", "bash", "read"].includes(toolName)) {
      startStatusPolling(ctx);
      await trackAccessedFile(toolName, toolInput, engine, process.cwd());
    }
  });

  pi.on("agent_end", async (_event, ctx) => {
    startStatusPolling(ctx);
    // reconciler.scheduleFull(connMgr); (Disabled for now as Rust reconciler handles file-by-file)
  });

  // ─── Tool: yaam_graph_explore ────────────────────────────────────────────

  pi.registerTool({
    name: "yaam_graph_explore",
    label: "YAAM Graph Explore",
    description:
      `Executes a read-only JSON Query DSL against the Rust graph engine.

The graph is automatically reconciled after every file operation and reflects the LIVE state of the repository.

JSON DSL STRUCTURE:
{
  "match": { // Filter initial candidate nodes
    "label": "Entity" | "Workspace" | "Scratchpad",
    "entity_type": "File" | "Function" | "Class",
    "id": "node_id",
    "status": "active" | "closed",
    "name_contains": "substring"
  },
  "where": { // Filter candidates by edge constraints
    "edge_to": { "id": "target_id", "relationship": "DECLARED_IN" | "CALLS" | "IMPORTS" },
    "edge_from": { "id": "source_id", "relationship": "CALLS" }
  },
  "traverse": { // BFS graph walk from candidates
    "relationship": "CALLS" | "IMPORTS" | "HAS_SCRATCHPAD" | "DECLARED_IN",
    "direction": "outbound" | "inbound" | "both",
    "max_depth": 1-5
  },
  "aggregate": { "group_by": "type" | "label" | "status", "count": true },
  "limit": 20,
  "return_fields": ["id", "name", "label", "content", "metadata"]
}

EXAMPLES:
1. Find all functions in a file:
{"match":{"label":"Entity","entity_type":"Function"}, "where":{"edge_to":{"id":"src/index.ts","relationship":"DECLARED_IN"}}}

2. Trace who calls a function (reverse call graph):
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"inbound","max_depth":2}}

3. Multi-hop impact analysis (outbound):
{"match":{"id":"src/reconciler.ts::reconcile"}, "traverse":{"relationship":"CALLS","direction":"outbound","max_depth":3}}
`,
    promptSnippet: "Query YAAM memory graph with JSON DSL (read-only)",
    promptGuidelines: [
      "The YAAM graph is automatically reconciled. Use yaam_graph_explore with the JSON Query DSL to query files, functions, call graphs, imports, and workspace scratchpads.",
    ],
    parameters: Type.Object({
      query: Type.Any({ description: "The JSON Query DSL object (NOT Cypher!) specifying the match, where, and traverse parameters." }),
    }),
    async execute(_toolCallId, params) {
      try {
        const result = await exploreGraph(params.query, engine, process.cwd());
        return {
          content: [{ type: "text" as const, text: result.text }],
          details: { spooledTo: result.spooledTo },
        };
      } catch (e: any) {
        return {
          content: [{ type: "text" as const, text: `Error: ${e.message || String(e)}` }],
          details: undefined,
        };
      }
    },
  });

  // ─── Tool: yaam_workspace_initialize ────────────────────────────────────

  pi.registerTool({
    name: "yaam_workspace_initialize",
    label: "YAAM Workspace Initialize",
    description:
      "Initializes a new workspace context in YAAM memory for task tracking. Deactivates any existing active workspace. Use at the start of a new feature or bug fix to create a persistent context for recording decisions.",
    promptSnippet: "Initialize a YAAM workspace for task tracking",
    promptGuidelines: [
      "Use yaam_workspace_initialize at the start of a new feature or refactor to create a task context for recording decisions. The graph is automatically reconciled after file operations, so workspace entity mappings reflect the live codebase.",
    ],
    parameters: Type.Object({
      name: Type.String({ description: "The unique name of the workspace (e.g. 'auth-fix', 'ui-refactor')." }),
      description: Type.String({ description: "Detailed description of the task." }),
    }),
    async execute(_toolCallId, params) {
      try {
        // Schedule a full reconcile to ensure topology is current (non-blocking)
        reconciler.scheduleFull();

        // Optimistic background save
        initializeWorkspace(params.name, params.description, engine)
          .catch(e => {}); // Workspace init error suppressed
        
        return {
          content: [{ type: "text" as const, text: `Workspace '${params.name}' initialized successfully (background save).` }],
          details: undefined,
        };
      } catch (e: any) {
        return {
          content: [{ type: "text" as const, text: `Error: ${e.message || String(e)}` }],
          details: undefined,
        };
      }
    },
  });

  // ─── Tool: yaam_workspace_append_note ────────────────────────────────────

  pi.registerTool({
    name: "yaam_workspace_append_note",
    label: "YAAM Workspace Append Note",
    description:
      "Appends a new insight or note to the active workspace scratchpad in YAAM memory. Notes persist across sessions. Record \"why\" decisions and architectural rationale, not just \"what\" was done.",
    promptSnippet: "Record an insight or decision to YAAM workspace",
    promptGuidelines: [
      "Use yaam_workspace_append_note to record 'why' decisions and architectural rationale, not just 'what' was done. Notes persist across sessions alongside the live code graph.",
    ],
    parameters: Type.Object({
      workspace: Type.String({ description: "The name of the active workspace." }),
      content: Type.String({ description: "The insight content to record." }),
    }),
    async execute(_toolCallId, params) {
      try {
        // Optimistic background save
        appendNote(params.workspace, params.content, engine)
          .catch(e => {}); // Workspace append note error suppressed

        return {
          content: [{ type: "text" as const, text: `Note added to workspace '${params.workspace}' (background save).` }],
          details: undefined,
        };
      } catch (e: any) {
        return {
          content: [{ type: "text" as const, text: `Error: ${e.message || String(e)}` }],
          details: undefined,
        };
      }
    },
  });

  // ─── Tool: yaam_search ───────────────────────────────────────────────────

  pi.registerTool({
    name: "yaam_search",
    label: "YAAM Hybrid Search",
    description:
      `Performs a hybrid semantic + keyword search across the YAAM memory graph.\n\nCombines BM25 keyword matching with dense ONNX embeddings (gte-small) to find relevant code entities, workspaces, and scratchpad notes by natural language meaning — not just exact keyword matches.\n\nUse this when you need to find code by concept or behavior (e.g. "file reconciliation logic", "workspace tracking") rather than exact names. Results are ranked by combined BM25 + cosine similarity score.\n\nParameters:\n- text (required): Natural language or keyword query\n- top_k (optional): Max results to return (default: 10)\n- workspace (optional): Scope results to entities mapped to a specific workspace\n- entity_types (optional): Filter by entity type (e.g. ["Function", "Class", "File"]\n- include_paths (optional): Include only results whose path starts with one of these prefixes (e.g. ["src/"]\n- exclude_paths (optional): Exclude results whose path starts with one of these prefixes (e.g. ["node_modules/", ".venv/"]\n\nResults include a \"category\" field: "module" for project source code, "library" for dependencies.\nUse exclude_paths to scope searches to your own code.`,
    promptSnippet: "Search YAAM memory by semantic meaning + keywords",
    promptGuidelines: [
      "Use yaam_search for natural-language discovery of code entities and notes — it combines BM25 keyword search with dense semantic embeddings to find relevant nodes by meaning, not just exact text matches.",
    ],
    parameters: Type.Object({
      text: Type.String({ description: "The natural language or keyword search query." }),
      top_k: Type.Optional(Type.Number({ description: "Maximum number of results to return (default: 10)." })),
      workspace: Type.Optional(Type.String({ description: "Optional: scope results to entities mapped to a specific workspace." })),
      entity_types: Type.Optional(Type.Array(Type.String(), { description: "Optional: filter results by entity type (e.g. [\"Function\", \"Class\"])." })),
      include_paths: Type.Optional(Type.Array(Type.String(), { description: "Optional: include only results whose path starts with one of these prefixes (e.g. [\"src/\"])." })),
      exclude_paths: Type.Optional(Type.Array(Type.String(), { description: "Optional: exclude results whose path starts with one of these prefixes (e.g. [\"node_modules/\", \".venv/\"])." })),
    }),
    async execute(_toolCallId, params) {
      try {
        const results = await engine.search({
          text: params.text,
          top_k: params.top_k,
          workspace: params.workspace,
          entity_types: params.entity_types,
          include_paths: params.include_paths,
          exclude_paths: params.exclude_paths,
        });

        if (!results || results.length === 0) {
          return {
            content: [{ type: "text" as const, text: "Search completed. No results found." }],
            details: undefined,
          };
        }

        const formatted = results.map((r: any) =>
          `[${r.type}] ${r.id} (score: ${typeof r.score === 'number' ? r.score.toFixed(4) : r.score})${r.name ? ' — ' + r.name : ''}${r.content ? '\n  ' + r.content.substring(0, 200) : ''}`
        ).join('\n');

        return {
          content: [{ type: "text" as const, text: `Search results for "${params.text}":\n\n${formatted}` }],
          details: { resultCount: results.length },
        };
      } catch (e: any) {
        return {
          content: [{ type: "text" as const, text: `Error: ${e.message || String(e)}` }],
          details: undefined,
        };
      }
    },
  });

  // ─── Command: /yaam ─────────────────────────────────────────────────────

  pi.registerCommand("yaam", {
    description: "Show YAAM memory status. Use '/yaam viz' for a visual workspace graph view.",
    async handler(args, ctx) {
      // ─── Subcommand: viz ──────────────────────────────────────────────
      if (typeof args === 'string' && args.trim() === 'viz') {
        try {
          ctx.ui.notify("Launching YAAM Visualizer... 🚀", "info");
          const url = await startServerIfNeeded(engine, 3456);
          ctx.ui.notify(`YAAM Graph Visualizer is running at: ${url}`, "info");
        } catch (e: any) {
          ctx.ui.notify(`YAAM visualization error: ${e.message || String(e)}`, "error");
        }
        return;
      }

      // ─── Default: status display ──────────────────────────────────────
      try {
        const typeRows = await engine.query({
          match: { label: "Entity" },
          aggregate: { group_by: "type", count: true }
        });

        const wsRows = await engine.query({
          match: { label: "Workspace" },
          where: { field: "status", op: "eq", value: "active" }
        });

        const activeWs = wsRows.length > 0 ? wsRows[0].id : null;
        let notesRows = [];

        if (activeWs) {
          notesRows = await engine.query({
            match: { label: "Workspace" },
            where: { field: "id", op: "eq", value: activeWs },
            traverse: { relationship: "HAS_SCRATCHPAD", direction: "outbound" },
            limit: 5
          });
        }

        let output = "═══ YAAM Memory Status ═══\n\n";

        output += "📊 Entities:\n";
        if (typeRows.length === 0) {
          output += "  (empty)\n";
        } else {
          // The DSL aggregate returns an array of { group, value }
          for (const row of typeRows) {
            output += `  ${row.group}: ${row.value}\n`;
          }
        }

        output += "\n📝 Active Workspace:\n";
        if (wsRows.length === 0) {
          output += "  (none active)\n";
        } else {
          const ws = wsRows[0];
          output += `  Name: ${ws.id}\n`;
          output += `  Description: ${ws.properties.description}\n`;
        }

        output += "\n🗒️  Recent Notes:\n";
        if (notesRows.length === 0) {
          output += "  (none)\n";
        } else {
          for (const note of notesRows) {
            const date = new Date(note.properties.created_at * 1000).toLocaleString();
            const preview = note.properties.content.substring(0, 80);
            output += `  [${date}] ${preview}...\n`;
          }
        }

        output += `\n🔄 Reconciler: ${reconciler.isRunning ? "running" : "idle"}`;

        ctx.ui.notify(output, "info");
      } catch (e: any) {
        ctx.ui.notify(`YAAM status error: ${e.message || String(e)}`, "error");
      }
    },
  });
}