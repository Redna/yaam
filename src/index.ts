import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { ConnectionManager } from "./db.js";
import { Reconciler } from "./reconciler.js";
import { exploreGraph } from "./graph_explore.js";
import { initializeWorkspace, appendNote } from "./workspace.js";
import { gatherWorkspaceData, renderWorkspaceView } from "./visualizer.js";

export default function yaamExtension(pi: ExtensionAPI) {
  const connMgr = new ConnectionManager();
  const reconciler = new Reconciler();

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

  pi.on("session_start", async (_event, ctx) => {
    lastCtx = ctx;
    setStatus(ctx, "yaam", "Ready ✅");
  });

  pi.on("session_shutdown", async () => {
    if (statusTimer) { clearInterval(statusTimer); statusTimer = null; }
    reconciler.shutdownLspClients();
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

    // Schedule background reconcile + file tracking (non-blocking)
    if (["write", "edit", "bash"].includes(toolName)) {
      startStatusPolling(ctx);
      reconciler.scheduleIncremental(connMgr, { toolName, toolInput });
    }
  });

  pi.on("agent_end", async (_event, ctx) => {
    startStatusPolling(ctx);
    reconciler.scheduleFull(connMgr);
  });

  // ─── Tool: yaam_graph_explore ────────────────────────────────────────────

  pi.registerTool({
    name: "yaam_graph_explore",
    label: "YAAM Graph Explore",
    description:
      `Executes a read-only Cypher query to explore code relationships (Layer 0)
and agent memories (Layer 1) in LadybugDB.

The graph is automatically reconciled after every file operation (write, edit, bash)
and reflects the LIVE state of the repository. Entity topology — files, functions,
classes, call graphs, imports, and inheritance — is current as of the last tool
invocation. You can trust this information is accurate without manual verification.

SCHEMA:
  Nodes:    Entity(id, type, status, last_modified, metadata)
            Workspace(workspace_name, description, status, closed_at)
            Scratchpad(id, content, created_at)
  Edges:    -[:LINKED_TO {relationship_type}]->     (Entity → Entity)
            -[:MAPPED_TO {created_at, is_stale}]->  (Workspace → Entity)
            -[:HAS_SCRATCHPAD]->                    (Workspace → Scratchpad)

  Entity.type = "File" | "Function" | "Class"
  Entity.id format: "path/to/file.ts" or "path/to/file.ts::functionName"
  Entity.last_modified = unix epoch int (e.g. 1782562666, NOT a date string)
  Entity.metadata = JSON string with rich fields (Functions/Classes only): {"line":42,"endLine":96,"signature":"foo(x: string): void","isAsync":true,"isExported":false,"isStatic":false,"params":[...],"returnType":"void","docComment":"Does the thing"}
  LINKED_TO.relationship_type = "CALLS" | "DECLARED_IN" | "IMPORTS" | "INHERITS_FROM"

EXAMPLES:
  -- Find all functions in a file:
  MATCH (f:Entity)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file:Entity {type: 'File'})
  WHERE file.id CONTAINS 'db.ts' RETURN f.id

  -- Trace who calls a function (reverse call graph):
  MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity)
  WHERE callee.id CONTAINS 'getConn' RETURN caller.id

  -- Multi-hop impact analysis:
  MATCH path = (src:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}*1..3]->(dst:Entity)
  WHERE src.id CONTAINS 'reconcile' RETURN src.id, dst.id, length(path)

PITFALLS:
  - \`type(r)\` does NOT work. Use \`r.relationship_type\` instead.
  - Property access is strict: accessing \`n.prop\` throws a Binder exception
    if \`prop\` doesn't exist on ALL matched nodes. Use \`keys(n)\` to discover
    available properties first.
  - Results > 20 rows are spooled to \`.chunks/memory_dumps/query_out.txt\`.`,
    promptSnippet: "Query YAAM memory graph with Cypher (read-only)",
    promptGuidelines: [
      "The YAAM graph is automatically reconciled after every file operation (write, edit, bash) and reflects the live repository state. Use yaam_graph_explore to query files, functions, classes, call graphs, imports, inheritance, and workspace scratchpads — the information is current and trustworthy.",
    ],
    parameters: Type.Object({
      query: Type.String({ description: "Read-only Cypher query. Use [:LINKED_TO {relationship_type: 'CALLS'|'DECLARED_IN'|'IMPORTS'|'INHERITS_FROM'}] for Entity edges. Filter by label (:Entity, :Workspace, :Scratchpad) before accessing properties to avoid Binder exceptions." }),
    }),
    async execute(_toolCallId, params) {
      try {
        const result = await connMgr.withConnection(async (conn) => {
          return await exploreGraph(params.query, conn, connMgr.projectRoot);
        });
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
        reconciler.scheduleFull(connMgr);

        // Optimistic background save
        connMgr.withConnection(async (conn) => {
          return await initializeWorkspace(params.name, params.description, conn);
        }).catch(e => console.error("Workspace init error:", e));
        
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
        connMgr.withConnection(async (conn) => {
          return await appendNote(params.workspace, params.content, conn);
        }).catch(e => console.error("Workspace append note error:", e));

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

  // ─── Command: /yaam ─────────────────────────────────────────────────────

  pi.registerCommand("yaam", {
    description: "Show YAAM memory status. Use '/yaam viz' for a visual workspace graph view.",
    async handler(args, ctx) {
      // ─── Subcommand: viz ──────────────────────────────────────────────
      if (typeof args === 'string' && args.trim() === 'viz') {
        try {
          const data = await connMgr.withConnection(async (conn) => {
            return await gatherWorkspaceData(conn);
          });

          if (!data) {
            ctx.ui.notify(
              "No active workspace. Use yaam_workspace_initialize to create one.",
              "info",
            );
            return;
          }

          const output = renderWorkspaceView(data);
          ctx.ui.notify(output, "info");
        } catch (e: any) {
          ctx.ui.notify(`YAAM visualization error: ${e.message || String(e)}`, "error");
        }
        return;
      }

      // ─── Default: status display ──────────────────────────────────────
      try {
        const stats = await connMgr.withConnection(async (conn) => {
          const typeResult = await conn.query(
            "MATCH (n:Entity) RETURN n.type, count(n) AS count ORDER BY count DESC"
          );
          const typeRows = await typeResult.getAll();

          const wsResult = await conn.query(
            "MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name, w.description"
          );
          const wsRows = await wsResult.getAll();

          const notesResult = await conn.query(
            "MATCH (w:Workspace {status: 'active'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at DESC LIMIT 5"
          );
          const notesRows = await notesResult.getAll();

          return { typeRows, wsRows, notesRows };
        });

        let output = "═══ YAAM Memory Status ═══\n\n";

        output += "📊 Entities:\n";
        if (stats.typeRows.length === 0) {
          output += "  (empty)\n";
        } else {
          for (const row of stats.typeRows) {
            output += `  ${row["n.type"]}: ${row["count"]}\n`;
          }
        }

        output += "\n📝 Active Workspace:\n";
        if (stats.wsRows.length === 0) {
          output += "  (none active)\n";
        } else {
          const ws = stats.wsRows[0];
          output += `  Name: ${ws["w.workspace_name"]}\n`;
          output += `  Description: ${ws["w.description"]}\n`;
        }

        output += "\n🗒️  Recent Notes:\n";
        if (stats.notesRows.length === 0) {
          output += "  (none)\n";
        } else {
          for (const note of stats.notesRows) {
            const date = new Date(note["s.created_at"] * 1000).toLocaleString();
            const preview = note["s.content"].substring(0, 80);
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