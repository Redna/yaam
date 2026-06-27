import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { ConnectionManager } from "./db.js";
import { Reconciler } from "./reconciler.js";
import { exploreGraph } from "./graph_explore.js";
import { initializeWorkspace, appendNote } from "./workspace.js";

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

SCHEMA:
  Nodes:    Entity(id, type, status, last_modified, metadata)
            Workspace(workspace_name, description, status, closed_at)
            Scratchpad(id, content, created_at)
  Edges:    -[:LINKED_TO {relationship_type}]->     (Entity → Entity)
            -[:MAPPED_TO {created_at, is_stale}]->  (Workspace → Entity)
            -[:HAS_SCRATCHPAD]->                    (Workspace → Scratchpad)

  Entity.type = "File" | "Function" | "Class"
  Entity.id format: "path/to/file.ts" or "path/to/file.ts::functionName"
  Entity.metadata = JSON string like {"line": 42} (Functions/Classes only)
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
      "Use yaam_graph_explore to query the YAAM memory graph before making architectural changes. It tracks files, functions, classes, call graphs, inheritance, and workspace scratchpads.",
    ],
    parameters: Type.Object({
      query: Type.String({ description: "The Cypher query to execute (read-only)." }),
    }),
    async execute(_toolCallId, params) {
      try {
        const result = await connMgr.withConnection(async (conn) => {
          return await exploreGraph(params.query, conn);
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
      `Executes a read-only Cypher query to explore code relationships (Layer 0)
and agent memories (Layer 1) in LadybugDB.

SCHEMA:
  Nodes:    Entity(id, type, status, last_modified, metadata)
            Workspace(workspace_name, description, status, closed_at)
            Scratchpad(id, content, created_at)
  Edges:    -[:LINKED_TO {relationship_type}]->     (Entity → Entity)
            -[:MAPPED_TO {created_at, is_stale}]->  (Workspace → Entity)
            -[:HAS_SCRATCHPAD]->                    (Workspace → Scratchpad)

  Entity.type = "File" | "Function" | "Class"
  Entity.id format: "path/to/file.ts" or "path/to/file.ts::functionName"
  Entity.metadata = JSON string like {"line": 42} (Functions/Classes only)
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
    promptSnippet: "Initialize a YAAM workspace for task tracking",
    promptGuidelines: [
      "Use yaam_workspace_initialize at the start of a new feature or refactor to create a task context for recording decisions.",
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
      `Executes a read-only Cypher query to explore code relationships (Layer 0)
and agent memories (Layer 1) in LadybugDB.

SCHEMA:
  Nodes:    Entity(id, type, status, last_modified, metadata)
            Workspace(workspace_name, description, status, closed_at)
            Scratchpad(id, content, created_at)
  Edges:    -[:LINKED_TO {relationship_type}]->     (Entity → Entity)
            -[:MAPPED_TO {created_at, is_stale}]->  (Workspace → Entity)
            -[:HAS_SCRATCHPAD]->                    (Workspace → Scratchpad)

  Entity.type = "File" | "Function" | "Class"
  Entity.id format: "path/to/file.ts" or "path/to/file.ts::functionName"
  Entity.metadata = JSON string like {"line": 42} (Functions/Classes only)
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
    promptSnippet: "Record an insight or decision to YAAM workspace",
    promptGuidelines: [
      "Use yaam_workspace_append_note to record 'why' decisions and architectural rationale, not just 'what' was done.",
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
    description: "Show YAAM memory status (entity counts, active workspace, recent notes)",
    async handler(_args, ctx) {
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