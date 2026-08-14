import { YaamEngineClient } from './engine-client.js';
import * as path from 'path';
import * as crypto from 'crypto';

export class Reconciler {
  public isRunning = false;
  public progress: { current: number; total: number; detail: string } | null = null;
  private syncQueue: Set<string> = new Set();
  private debounceTimer: NodeJS.Timeout | null = null;
  /** True while scheduleFull() is running. Prevents scanForModifiedFiles()
   *  from queuing all files due to empty fileMtimes during the race window
   *  between session_start and the first full sync completing. */
  private isPriming = false;
  /** Maps relative file path → content hash of the last reconciled version.
   *  Used to skip files whose content hasn't changed since the last sync. */
  private contentHashes: Map<string, string> = new Map();
  /** Maps relative file path → mtime (ms) of the last reconciled version.
   *  Used to detect files modified externally (e.g., by bash commands). */
  private fileMtimes: Map<string, number> = new Map();

  constructor(private engine: YaamEngineClient) {}

  /** Compute a fast hash of file content for change detection. */
  private hashContent(content: string): string {
    return crypto.createHash('sha256').update(content).digest('hex').substring(0, 16);
  }

  public scheduleIncremental(toolName: string, payload: any) {
    if (["write", "edit"].includes(toolName) && payload && payload.path) {
      this.syncQueue.add(payload.path);
      this.triggerSync();
    } else if (toolName === "bash") {
      // Bash commands can modify any file — we can't know which ones from
      // the command string. Instead, scan supported files for mtime changes
      // since the last sync and queue only those.
      this.scanForModifiedFiles();
    }
  }

  /** Quick mtime scan: walk supported files and queue any whose mtime
   *  changed since the last reconciliation. This is much cheaper than a
   *  full reconcile because it only stats files (no read, no engine call).
   *  The hash check in runSync() provides a second layer of filtering.
   *  Skipped if scheduleFull() is running — it will handle modified files. */
  private scanForModifiedFiles() {
    if (this.isPriming) return; // Full sync in progress — it handles everything

    const fs = require('fs');
    const walkPath = require('path');

    const SUPPORTED_EXTENSIONS = ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs', '.md'];
    const SKIP_DIRS = [
      'node_modules', 'dist', '.git', 'target', '.chunks', '.yaam',
      '.local', '.cache', '.npm', '.cargo', '.docker', '.rustup',
      '.nvm', '.pyenv', 'venv', '.venv', '__pycache__', 'build', 'out'
    ];

    const walkSync = (dir: string, filelist: string[] = []) => {
      if (!fs.existsSync(dir)) return filelist;
      const files = fs.readdirSync(dir);
      for (const file of files) {
        const filepath = walkPath.join(dir, file);
        try {
          const stat = fs.statSync(filepath);
          if (stat.isDirectory()) {
            if (!SKIP_DIRS.includes(file)) {
              walkSync(filepath, filelist);
            }
          } else if (SUPPORTED_EXTENSIONS.some(ext => file.endsWith(ext))) {
            filelist.push(filepath);
          }
        } catch { /* skip unreadable files */ }
      }
      return filelist;
    };

    try {
      const allFiles = walkSync(process.cwd());
      let found = 0;
      for (const absPath of allFiles) {
        const relPath = walkPath.relative(process.cwd(), absPath);
        try {
          const stat = fs.statSync(absPath);
          const lastMtime = this.fileMtimes.get(relPath) ?? 0;
          if (stat.mtimeMs > lastMtime) {
            this.syncQueue.add(relPath);
            found++;
          }
        } catch { /* skip */ }
      }
      if (found > 0) {
        this.triggerSync();
      }
    } catch {
      // Scan failed — silently ignore
    }
  }

  /**
   * Smart full sync: only reconcile files that actually changed.
   *
   * The Rust daemon already loaded the full graph from events.jsonl on startup.
   * This method compares each on-disk file's mtime to the graph's last_modified
   * timestamp for that file. Only files with mtime > last_modified (or new files
   * not yet in the graph) are queued for reconciliation. Unchanged files have
   * their content hashes primed so runSync() will skip them via the hash check.
   *
   * Stale files (in the graph but no longer on disk) are deleted.
   */
  public async scheduleFull() {
    this.isPriming = true;
    try {
      const fs = require('fs');
    const walkPath = require('path');

    const SUPPORTED_EXTENSIONS = ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs', '.md'];
    const SKIP_DIRS = [
      'node_modules', 'dist', '.git', 'target', '.chunks', '.yaam',
      '.local', '.cache', '.npm', '.cargo', '.docker', '.rustup',
      '.nvm', '.pyenv', 'venv', '.venv', '__pycache__', 'build', 'out'
    ];

    const walkSync = (dir: string, filelist: string[] = []) => {
      if (!fs.existsSync(dir)) return filelist;
      const files = fs.readdirSync(dir);
      for (const file of files) {
        const filepath = walkPath.join(dir, file);
        try {
          const stat = fs.statSync(filepath);
          if (stat.isDirectory()) {
            if (!SKIP_DIRS.includes(file)) {
              walkSync(filepath, filelist);
            }
          } else if (SUPPORTED_EXTENSIONS.some(ext => file.endsWith(ext))) {
            filelist.push(filepath);
          }
        } catch { /* skip unreadable files */ }
      }
      return filelist;
    };

    // Query the graph for existing File entities and their last_modified timestamps.
    // Uses entity_type (not type) — the DSL field is entity_type.
    let graphFiles: Map<string, number> = new Map();
    try {
      const fileNodes = await this.engine.query({
        match: { label: "Entity", entity_type: "File" }
      });
      for (const node of fileNodes) {
        const lastMod = node.label?.last_modified ?? 0;
        graphFiles.set(node.id, lastMod);
      }
    } catch {
      // If query fails, graphFiles stays empty — all files will be queued
    }

    const allFiles = walkSync(process.cwd());
    const allFilesSet = new Set(allFiles.map((f: string) => walkPath.relative(process.cwd(), f)));

    let queued = 0;
    let primed = 0;

    for (const absPath of allFiles) {
      const relPath = walkPath.relative(process.cwd(), absPath);
      try {
        const stat = fs.statSync(absPath);
        const lastReconciled = graphFiles.get(relPath) ?? 0;

        if (stat.mtimeMs > lastReconciled) {
          // File changed since last reconciliation (or is new) — queue it
          this.syncQueue.add(relPath);
          queued++;
        } else {
          // File unchanged — prime the hash so runSync() skips it
          const content = fs.readFileSync(absPath, 'utf-8');
          this.contentHashes.set(relPath, this.hashContent(content));
          primed++;
        }
        // Always update mtime baseline
        this.fileMtimes.set(relPath, stat.mtimeMs);
      } catch { /* skip */ }
    }

    // Delete stale files (in graph but not on disk).
    // Only iterates File entities — not Functions/Classes/Sections.
    for (const [fileId, _] of graphFiles) {
      if (!allFilesSet.has(fileId)) {
        try {
          await this.engine.reconcile({ file_path: fileId, content: "" });
          this.contentHashes.delete(fileId);
        } catch { /* ignore */ }
      }
    }

    if (queued > 0 || this.syncQueue.size > 0) {
      this.triggerSync();
    }
    } catch {
      // Full sync error — trigger sync as fallback
      this.triggerSync();
    } finally {
      this.isPriming = false;
    }
  }

  private triggerSync() {
    if (this.debounceTimer) clearTimeout(this.debounceTimer);
    this.debounceTimer = setTimeout(() => {
      this.runSync().catch(e => {}); // Reconciler error suppressed
    }, 1000);
  }

  private async runSync() {
    if (this.syncQueue.size === 0) return;
    this.isRunning = true;
    const filesToSync = Array.from(this.syncQueue);
    this.syncQueue.clear();

    this.progress = { current: 0, total: filesToSync.length, detail: "Processing..." };

    const fs = await import('fs');
    let skipped = 0;
    let reconciled = 0;
    let completed = 0;

    // Process files concurrently to avoid sequential blocking on slow
    // reconcile RPCs (tree-sitter + LSP + ONNX embeddings). The daemon
    // handles concurrent connections, and the LSP mutex serializes
    // same-language LSP resolution internally.
    const CONCURRENCY = 4;

    const processFile = async (file: string) => {
      try {
        const resolved = path.resolve(file);
        if (fs.existsSync(resolved)) {
          const content = fs.readFileSync(resolved, 'utf-8');
          const relPath = path.relative(process.cwd(), resolved);

          // Skip if content hasn't changed since last reconciliation.
          const newHash = this.hashContent(content);
          const existingHash = this.contentHashes.get(relPath);
          if (existingHash === newHash) {
            skipped++;
            const stat = fs.statSync(resolved);
            this.fileMtimes.set(relPath, stat.mtimeMs);
            return;
          }

          await this.engine.reconcile({ file_path: relPath, content });
          this.contentHashes.set(relPath, newHash);
          const stat = fs.statSync(resolved);
          this.fileMtimes.set(relPath, stat.mtimeMs);
          reconciled++;
        }
      } catch (e) {
        // Failed to reconcile ${file}
      } finally {
        completed++;
        if (this.progress) {
          this.progress.current = completed;
          this.progress.detail = file.length > 30 ? "..." + file.substring(file.length - 27) : file;
        }
      }
    };

    // Simple concurrency limiter: process CONCURRENCY files at a time.
    const queue = [...filesToSync];
    const workers: Promise<void>[] = [];
    for (let w = 0; w < Math.min(CONCURRENCY, queue.length); w++) {
      workers.push((async () => {
        while (queue.length > 0) {
          const file = queue.shift()!;
          await processFile(file);
        }
      })());
    }
    await Promise.all(workers);

    if (skipped > 0 || reconciled > 0) {
      console.log(`[YAAM Reconciler] ${reconciled} reconciled, ${skipped} unchanged (skipped)`);
    }

    this.progress = null;
    this.isRunning = false;
  }
}