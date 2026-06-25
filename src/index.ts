import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import * as path from "path";
import { exec } from "child_process";
import * as util from "util";
import { fileURLToPath } from "url";

const execPromise = util.promisify(exec);

export default function yaamExtension(pi: ExtensionAPI) {
  const epi = pi as any;

  // Register configuration schema for TUI
  epi.settings.register({
    namespace: "yaam",
    schema: {
      type: "object",
      properties: {
        frequency: {
          type: "string",
          enum: ["incremental", "boundaries", "disabled"],
          default: "incremental",
          description: "Reconciliation frequency: incremental (after tool use), boundaries (end of turns), or disabled"
        },
        languages: {
          type: "object",
          description: "Language Server configurations by language name",
          additionalProperties: {
            type: "object",
            properties: {
              extensions: {
                type: "array",
                items: { type: "string" }
              },
              command: { type: "string" },
              args: {
                type: "array",
                items: { type: "string" }
              }
            },
            required: ["extensions", "command", "args"]
          }
        }
      }
    },
    defaults: {
      frequency: "incremental",
      languages: {
        python: {
          extensions: [".py"],
          command: "npx",
          args: ["--package=pyright", "pyright-langserver", "--stdio"]
        }
      }
    }
  });

  async function runReconciler(full: boolean = false, ctx?: any) {
    if (ctx && ctx.ui && typeof ctx.ui.setStatus === "function") {
      ctx.ui.setStatus("yaam", "Syncing 🔄");
    }
    try {
      const filename = fileURLToPath(import.meta.url);
      const dirname = path.dirname(filename);
      const reconcilerPath = path.resolve(
        dirname,
        "../.agents/skills/yaam-memory-manager/scripts/reconciler.ts"
      );
      const config = epi.settings.get("yaam");
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
    const config = epi.settings.get("yaam") || {};
    if (config.frequency === "incremental") {
      await runReconciler(false, ctx);
    }
  });

  // Hook into agent_end to automatically run full reconciliation
  pi.on("agent_end", async (event: any, ctx: any) => {
    const config = epi.settings.get("yaam") || {};
    if (config.frequency !== "disabled") {
      await runReconciler(true, ctx);
    }
  });
}
