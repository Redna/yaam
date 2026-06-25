import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import * as path from "path";
import * as fs from "fs";
import { exec, execFile } from "child_process";
import * as util from "util";
import { fileURLToPath } from "url";
import { Type } from "typebox";

const execPromise = util.promisify(exec);
const execFilePromise = util.promisify(execFile);

function loadYaamSettings(): any {
  let merged: any = {
    frequency: 'incremental',
    languages: {
      python: {
        extensions: ['.py'],
        command: 'npx',
        args: ['--package=pyright', 'pyright-langserver', '--stdio']
      }
    }
  };

  const paths = [
    path.join(process.cwd(), '.gemini/settings.json'),
    path.join(process.cwd(), '.agents/settings.json'),
    path.join(process.env.HOME || '', '.gemini/settings.json'),
    path.join(process.env.HOME || '', '.agents/settings.json')
  ];

  for (const settingsPath of paths) {
    if (fs.existsSync(settingsPath)) {
      try {
        const raw = fs.readFileSync(settingsPath, 'utf-8');
        const parsed = JSON.parse(raw);
        if (parsed.yaam) {
          merged = {
            ...merged,
            ...parsed.yaam,
            languages: {
              ...(merged.languages || {}),
              ...(parsed.yaam.languages || {})
            }
          };
        }
      } catch (e) {}
    }
  }

  return merged;
}

export default function yaamExtension(pi: ExtensionAPI) {
  async function runReconciler(full: boolean = false, ctx?: any) {
    if (ctx && ctx.ui && typeof ctx.ui.setStatus === "function") {
      ctx.ui.setStatus("yaam", "Syncing 🔄");
    }
    try {
      const filename = fileURLToPath(import.meta.url);
      const dirname = path.dirname(filename);
      const reconcilerPath = path.resolve(
        dirname,
        "../skills/yaam-memory-manager/scripts/reconciler.ts"
      );
      const config = loadYaamSettings();
      const cmd = `npx tsx "${reconcilerPath}"${full ? " --full" : ""} < /dev/null`;
      await execPromise(cmd, {
        env: {
          ...process.env,
          YAAM_SETTINGS: JSON.stringify(config)
        }
      });
      if (ctx && ctx.ui && typeof ctx.ui.setStatus === "function") {
        ctx.ui.setStatus("yaam", "Synced ✅");
      }
    } catch (e) {
      console.error("YAAM reconciler failed:", e);
      if (ctx && ctx.ui && typeof ctx.ui.setStatus === "function") {
        ctx.ui.setStatus("yaam", "Sync Error ❌");
      }
    }
  }

  // Initialize status on session and turn starts
  pi.on("turn_start", async (event: any, ctx: any) => {
    if (ctx && ctx.ui && typeof ctx.ui.setStatus === "function") {
      ctx.ui.setStatus("yaam", "Idle 💤");
    }
  });

  pi.on("session_start", async (event: any, ctx: any) => {
    if (ctx && ctx.ui && typeof ctx.ui.setStatus === "function") {
      ctx.ui.setStatus("yaam", "Idle 💤");
    }
  });

  // Hook into tool_result to automatically run incremental reconciliation
  pi.on("tool_result", async (event: any, ctx: any) => {
    const config = loadYaamSettings();
    if (config.frequency === "incremental") {
      await runReconciler(false, ctx);
    }
  });

  // Hook into agent_end to automatically run full reconciliation
  pi.on("agent_end", async (event: any, ctx: any) => {
    const config = loadYaamSettings();
    if (config.frequency !== "disabled") {
      await runReconciler(true, ctx);
    }
  });

  // Register yaam_graph_explore tool
  pi.registerTool({
    name: "yaam_graph_explore",
    label: "YAAM Graph Explore",
    description: "Executes a read-only Cypher query to explore code relationships (Layer 0) and agent memories (Layer 1) in LadybugDB.",
    parameters: Type.Object({
      query: Type.String({ description: "The Cypher query to execute." })
    }),
    async execute(toolCallId: string, params: { query: string }) {
      try {
        const filename = fileURLToPath(import.meta.url);
        const dirname = path.dirname(filename);
        const scriptPath = path.resolve(
          dirname,
          "../skills/yaam-memory-manager/scripts/graph_explore.ts"
        );
        const { stdout, stderr } = await execFilePromise("npx", ["tsx", scriptPath, params.query]);
        return { content: [{ type: "text", text: stdout || stderr }], details: undefined };
      } catch (e: any) {
        return { content: [{ type: "text", text: `Error: ${e.stdout || e.stderr || e.message}` }], details: undefined };
      }
    }
  });

  // Register yaam_workspace_initialize tool
  pi.registerTool({
    name: "yaam_workspace_initialize",
    label: "YAAM Workspace Initialize",
    description: "Initializes a new workspace context in YAAM memory for task tracking.",
    parameters: Type.Object({
      name: Type.String({ description: "The unique name of the workspace." }),
      description: Type.String({ description: "Detailed description of the task." })
    }),
    async execute(toolCallId: string, params: { name: string; description: string }) {
      try {
        const filename = fileURLToPath(import.meta.url);
        const dirname = path.dirname(filename);
        const scriptPath = path.resolve(
          dirname,
          "../skills/yaam-memory-manager/scripts/workspace_initialize.ts"
        );
        const { stdout, stderr } = await execFilePromise("npx", [
          "tsx",
          scriptPath,
          "--name",
          params.name,
          "--description",
          params.description
        ]);
        return { content: [{ type: "text", text: stdout || stderr }], details: undefined };
      } catch (e: any) {
        return { content: [{ type: "text", text: `Error: ${e.stdout || e.stderr || e.message}` }], details: undefined };
      }
    }
  });

  // Register yaam_workspace_append_note tool
  pi.registerTool({
    name: "yaam_workspace_append_note",
    label: "YAAM Workspace Append Note",
    description: "Appends a new insight or note to the active workspace scratchpad in YAAM memory.",
    parameters: Type.Object({
      workspace: Type.String({ description: "The name of the active workspace." }),
      content: Type.String({ description: "The insight content to record." })
    }),
    async execute(toolCallId: string, params: { workspace: string; content: string }) {
      try {
        const filename = fileURLToPath(import.meta.url);
        const dirname = path.dirname(filename);
        const scriptPath = path.resolve(
          dirname,
          "../skills/yaam-memory-manager/scripts/workspace_append_note.ts"
        );
        const { stdout, stderr } = await execFilePromise("npx", [
          "tsx",
          scriptPath,
          "--workspace",
          params.workspace,
          "--content",
          params.content
        ]);
        return { content: [{ type: "text", text: stdout || stderr }], details: undefined };
      } catch (e: any) {
        return { content: [{ type: "text", text: `Error: ${e.stdout || e.stderr || e.message}` }], details: undefined };
      }
    }
  });
}
