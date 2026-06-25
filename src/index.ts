import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import * as path from "path";
import { exec } from "child_process";
import * as util from "util";

const execPromise = util.promisify(exec);

export default function yaamExtension(pi: ExtensionAPI) {
  async function runReconciler(full: boolean = false) {
    try {
      const reconcilerPath = path.join(
        process.cwd(),
        ".agents",
        "skills",
        "yaam-memory-manager",
        "scripts",
        "reconciler.ts"
      );
      const cmd = `npx tsx "${reconcilerPath}"${full ? " --full" : ""} < /dev/null`;
      await execPromise(cmd);
    } catch (e) {
      console.error("YAAM reconciler failed:", e);
    }
  }

  // Hook into tool_result to automatically run incremental reconciliation
  pi.on("tool_result", async (event: any, ctx: any) => {
    await runReconciler(false);
  });

  // Hook into agent_end to automatically run full reconciliation
  pi.on("agent_end", async (event: any, ctx: any) => {
    await runReconciler(true);
  });
}
