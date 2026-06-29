import { YaamEngineClient } from './engine-client.js';
import * as path from 'path';

export class Reconciler {
  public isRunning = false;
  public progress: { current: number; total: number; detail: string } | null = null;
  private syncQueue: Set<string> = new Set();
  private debounceTimer: NodeJS.Timeout | null = null;

  constructor(private engine: YaamEngineClient) {}

  public scheduleIncremental(toolName: string, payload: any) {
    if (["write", "edit", "bash"].includes(toolName) && payload && payload.path) {
      this.syncQueue.add(payload.path);
      this.triggerSync();
    }
  }

  public scheduleFull() {
    const fs = require('fs');
    const walkPath = require('path');
    
    const walkSync = (dir: string, filelist: string[] = []) => {
      if (!fs.existsSync(dir)) return filelist;
      const files = fs.readdirSync(dir);
      for (const file of files) {
        const filepath = walkPath.join(dir, file);
        if (fs.statSync(filepath).isDirectory()) {
          if (!['node_modules', 'dist', '.git', 'src-rust', 'target', '.chunks'].includes(file)) {
            walkSync(filepath, filelist);
          }
        } else if (file.endsWith('.ts') || file.endsWith('.js')) {
          filelist.push(filepath);
        }
      }
      return filelist;
    };

    try {
      const allFiles = walkSync(process.cwd());
      const allFilesSet = new Set(allFiles.map((f: string) => walkPath.relative(process.cwd(), f)));
      for (const f of allFiles) {
        this.syncQueue.add(f);
      }

      this.engine.query({ match: { label: "Entity", type: "File" } })
        .then(async (fileNodes) => {
          for (const node of fileNodes) {
            if (!allFilesSet.has(node.id)) {
              console.log(`[YAAM] Removing stale file from graph: ${node.id}`);
              await this.engine.reconcile({ file_path: node.id, content: "" });
            }
          }
          this.triggerSync();
        })
        .catch(e => {
          console.error("Failed to query nodes during full sync:", e);
          this.triggerSync();
        });
    } catch (e) {
      console.error("Full sync error:", e);
    }
  }

  private triggerSync() {
    if (this.debounceTimer) clearTimeout(this.debounceTimer);
    this.debounceTimer = setTimeout(() => {
      this.runSync().catch(e => console.error("Reconciler error:", e));
    }, 1000);
  }

  private async runSync() {
    if (this.syncQueue.size === 0) return;
    this.isRunning = true;
    const filesToSync = Array.from(this.syncQueue);
    this.syncQueue.clear();

    this.progress = { current: 0, total: filesToSync.length, detail: "Processing..." };

    const fs = await import('fs');
    for (let i = 0; i < filesToSync.length; i++) {
      const file = filesToSync[i];
      this.progress.current = i + 1;
      this.progress.detail = file.length > 30 ? "..." + file.substring(file.length - 27) : file;

      try {
        const resolved = path.resolve(file);
        if (fs.existsSync(resolved)) {
          const content = fs.readFileSync(resolved, 'utf-8');
          const relPath = path.relative(process.cwd(), resolved);
          await this.engine.reconcile({ file_path: relPath, content });
        }
      } catch (e) {
        console.warn(`Failed to reconcile ${file}:`, e);
      }
    }

    this.progress = null;
    this.isRunning = false;
  }
}