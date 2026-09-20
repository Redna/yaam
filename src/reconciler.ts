import { YaamEngineClient } from './engine-client.js';
import * as path from 'path';
import * as crypto from 'crypto';

/**
 * Coverage facts read from the engine's reconcile cache
 * (`<workspace>/.yaam/reconcile-cache.json`, owned by the Rust daemon).
 *
 * `known` is false only when the cache is switched off
 * (`YAAM_RECONCILE_CACHE=off`): the engine is bypassing the cache, so file
 * completeness cannot be judged from it and the full sync falls back to the
 * mtime-only fast-path. When the cache is enabled but absent/unreadable the
 * result is an empty coverage set — nothing is provably complete.
 */
interface CacheCoverage {
  known: boolean;
  /** Relative paths that have a cache entry (complete or not). */
  present: Set<string>;
  /** Relative path → references still awaiting resolution in the engine queue. */
  pending: Map<string, number>;
}

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
  /** Files whose content hash is known but whose graph coverage is incomplete
   *  (no cache entry, or references still pending). These must bypass the
   *  content-hash fast-path in runSync() so the engine re-parses and re-queues
   *  their references instead of silently skipping them. */
  private forceReconcile: Set<string> = new Set();

  constructor(private engine: YaamEngineClient) {}

  /** Compute a fast hash of file content for change detection. */
  private hashContent(content: string): string {
    return crypto.createHash('sha256').update(content).digest('hex').substring(0, 16);
  }

  public scheduleIncremental(toolName: string, payload: any) {
    if (["write", "edit"].includes(toolName) && payload && payload.path) {
      const walkPath = path;
      const resolvedPath = walkPath.resolve(process.cwd(), payload.path);
      const relPath = walkPath.relative(process.cwd(), resolvedPath);
      
      // Ignore files outside the project root or in hidden internal folders
      if (relPath.startsWith('..') || relPath.startsWith('.yaam') || relPath.startsWith('.pi')) {
        return;
      }
      
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
  private async scanForModifiedFiles() {
    if (this.isPriming) return; // Full sync in progress — it handles everything

    const fs = await import('fs/promises');
    const walkPath = path;
    const { existsSync } = await import('fs');

    // Documents are opt-in: indexing every .md in a repo dominates the graph
    // (a doc-heavy workspace reached 1,809 Section nodes) and with it the RSS.
    const SUPPORTED_EXTENSIONS = process.env.YAAM_INDEX_DOCS === 'true'
      ? ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs', '.md']
      : ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs'];
    const SKIP_DIRS = [
      'node_modules', 'dist', '.git', 'target', '.chunks', '.yaam',
      '.local', '.cache', '.npm', '.cargo', '.docker', '.rustup',
      '.nvm', '.pyenv', 'venv', '.venv', '__pycache__', 'build', 'out',
      '.pi', '.pi-web', '.claude', '.vscode', '.idea', 'coverage', '.turbo',
      '.next', '.parcel-cache', 'screenshots'
    ];

    const walkAsync = async (dir: string, filelist: string[] = []): Promise<string[]> => {
      if (!existsSync(dir)) return filelist;
      let files;
      try {
        files = await fs.readdir(dir);
      } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err);
        return filelist;
      }
      
      for (const file of files) {
        const filepath = walkPath.join(dir, file);
        try {
          const stat = await fs.lstat(filepath);
          if (stat.isDirectory()) {
            if (!SKIP_DIRS.includes(file) && !file.startsWith('session-logs')) {
              await walkAsync(filepath, filelist);
            }
          } else if (SUPPORTED_EXTENSIONS.some(ext => file.endsWith(ext))) {
            filelist.push(filepath);
          }
        } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err); /* skip unreadable files */ }
      }
      return filelist;
    };

    try {
      const allFiles = await walkAsync(process.cwd());
      let found = 0;
      for (const absPath of allFiles) {
        const relPath = walkPath.relative(process.cwd(), absPath);
        try {
          const stat = await fs.stat(absPath);
          const lastMtime = this.fileMtimes.get(relPath) ?? 0;
          if (stat.mtimeMs > lastMtime) {
            this.syncQueue.add(relPath);
            found++;
          }
        } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err); /* skip */ }
      }
      if (found > 0) {
        this.triggerSync();
      }
    } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err);
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
  public async syncGithubContext() {
    const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN || process.env.GITHUB_PAT;
    if (!token) {
      console.log("[YAAM Reconciler] No GITHUB_TOKEN found, skipping GitHub context ingestion.");
      return;
    }

    try {
      console.log("[YAAM Reconciler] Fetching GitHub context...");
      const res = await fetch("https://api.github.com/repos/Redna/evol-hive/issues?state=all&per_page=100", {
        headers: {
          "Authorization": `Bearer ${token}`,
          "Accept": "application/vnd.github.v3+json",
          "User-Agent": "YAAM-Engine"
        }
      });

      if (!res.ok) {
        console.error(`[YAAM Reconciler] Failed to fetch GitHub context: ${res.statusText}`);
        return;
      }

      const issues = await res.json();
      let ingested = 0;

      for (const item of issues) {
        const isPr = !!item.pull_request;
        const label = isPr ? "PullRequest" : "Issue";
        const id = isPr ? `pr-${item.number}` : `issue-${item.number}`;
        
        await this.engine.upsertNode({
          id,
          label,
          properties: {
            title: item.title,
            status: item.state,
            created_at: Math.floor(new Date(item.created_at).getTime() / 1000),
            name: `${label} #${item.number}`,
            content: item.body || ""
          }
        });

        if (isPr && item.body) {
          const resolveMatch = item.body.match(/(?:closes|fixes|resolves) #(\d+)/i);
          if (resolveMatch) {
            const issueId = `issue-${resolveMatch[1]}`;
            await this.engine.linkNodes({
              from_id: id,
              to_id: issueId,
              relationship: "RESOLVES",
              properties: {}
            });
          }
        }
        ingested++;
      }
      console.log(`[YAAM Reconciler] Successfully ingested ${ingested} GitHub issues/PRs.`);
    } catch (err) {
      console.error("[YAAM Reconciler] Error during GitHub context sync:", err);
    }
  }

  /**
   * Read the engine's reconcile cache for coverage facts. Best-effort: an
   * absent or malformed cache yields an *empty* coverage set (nothing is
   * provably complete, so everything is re-queued) — that is exactly the state
   * a fresh workspace or a kill-before-flush leaves behind.
   *
   * Only `YAAM_RECONCILE_CACHE=off` gives `known:false`: the engine is bypassing
   * the cache, so file completeness cannot be judged from it and the full sync
   * falls back to the mtime-only fast-path instead of re-parsing everything.
   */
  private async readCacheCoverage(): Promise<CacheCoverage> {
    if ((process.env.YAAM_RECONCILE_CACHE ?? '').toLowerCase() === 'off') {
      return { known: false, present: new Set(), pending: new Map() };
    }

    const empty: CacheCoverage = { known: true, present: new Set(), pending: new Map() };
    const dir = process.env.YAAM_RECONCILE_CACHE_DIR || path.resolve(process.cwd(), '.yaam');
    try {
      const fs = await import('fs/promises');
      const doc = JSON.parse(await fs.readFile(path.resolve(dir, 'reconcile-cache.json'), 'utf-8'));
      const files = doc?.files;
      if (!files || typeof files !== 'object') return empty;

      const present = new Set<string>();
      const pending = new Map<string, number>();
      for (const [rel, entry] of Object.entries(files as Record<string, any>)) {
        present.add(rel);
        const n = Number((entry as any)?.refs_pending ?? 0);
        pending.set(rel, Number.isFinite(n) && n > 0 ? n : 0);
      }
      return { known: true, present, pending };
    } catch {
      return empty;
    }
  }

  public async scheduleFull() {
    this.isPriming = true;
    try {
      const fs = await import('fs/promises');
      const walkPath = path;
      const { existsSync } = await import('fs');

      // Documents are opt-in: indexing every .md in a repo dominates the graph
    // (a doc-heavy workspace reached 1,809 Section nodes) and with it the RSS.
    const SUPPORTED_EXTENSIONS = process.env.YAAM_INDEX_DOCS === 'true'
      ? ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs', '.md']
      : ['.ts', '.tsx', '.js', '.jsx', '.py', '.rs'];
      const SKIP_DIRS = [
        'node_modules', 'dist', '.git', 'target', '.chunks', '.yaam',
        '.local', '.cache', '.npm', '.cargo', '.docker', '.rustup',
        '.nvm', '.pyenv', 'venv', '.venv', '__pycache__', 'build', 'out',
        '.pi', '.pi-web', '.claude', '.vscode', '.idea', 'coverage', '.turbo',
      '.next', '.parcel-cache', 'screenshots'
      ];

      const walkAsync = async (dir: string, filelist: string[] = []): Promise<string[]> => {
        if (!existsSync(dir)) return filelist;
        let files;
        try {
          files = await fs.readdir(dir);
        } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err);
          return filelist;
        }
        
        for (const file of files) {
          const filepath = walkPath.join(dir, file);
          try {
            const stat = await fs.lstat(filepath);
            if (stat.isDirectory()) {
              if (!SKIP_DIRS.includes(file) && !file.startsWith('session-logs')) {
                await walkAsync(filepath, filelist);
              }
            } else if (SUPPORTED_EXTENSIONS.some(ext => file.endsWith(ext))) {
              filelist.push(filepath);
            }
          } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err); /* skip unreadable files */ }
        }
        return filelist;
      };

      // Query the graph for existing File entities and their last_modified timestamps.
      let graphFiles: Map<string, number> = new Map();
      try {
        const fileNodes = await this.engine.query({
          match: { label: "Entity", entity_type: "File" }
        });
        for (const node of fileNodes) {
          const lastMod = node.label?.last_modified ?? 0;
          graphFiles.set(node.id, lastMod);
        }
      } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err);
        // If query fails, graphFiles stays empty — all files will be queued
      }

      console.log(`[YAAM Reconciler] Loaded ${graphFiles.size} File entities from graph.`);

      const allFiles = await walkAsync(process.cwd());
      const allFilesSet = new Set(allFiles.map((f: string) => walkPath.relative(process.cwd(), f)));

      // Ask the engine's reconcile cache which files are provably complete.
      // The graph query above only tells us the engine *knows* a file; it does
      // not tell us whether that file's references were fully resolved. The
      // cache's `refs_pending` is the only such signal the daemon exposes, and
      // it is the one Leg 1 invalidates on (entry_is_valid rejects pending>0).
      const cache = await this.readCacheCoverage();

      let reconciledCount = 0;
      let unresolvedRefs = 0;
      for (const rel of allFilesSet) {
        if (cache.known) {
          const pending = cache.pending.get(rel) ?? 0;
          unresolvedRefs += pending;
          if (pending === 0 && cache.present.has(rel) && graphFiles.has(rel)) reconciledCount++;
        } else if (graphFiles.has(rel)) {
          reconciledCount++;
        }
      }

      let queued = 0;
      let primed = 0;
      let requeuedIncomplete = 0;

      // Process file stats sequentially to prevent Node.js OOM
      for (const absPath of allFiles) {
        const relPath = walkPath.relative(process.cwd(), absPath);
        try {
          const stat = await fs.stat(absPath);
          const lastReconciled = graphFiles.get(relPath) ?? 0;

          // Prevent OOM from parsing/reading massive files
          if (stat.size > 1_000_000) {
            this.fileMtimes.set(relPath, stat.mtimeMs);
            continue;
          }

          // Coverage-aware skip: only a file that is *provably* complete may be
          // skipped. Complete means the graph knows it (a File node with an
          // mtime) and, when the cache is readable, its entry exists with zero
          // pending references. This is what makes "who calls X" truthful: an
          // engine restart mid-queue drops unresolved refs in memory, and those
          // files are re-queued here even though their mtime never changed.
          const pendingRefs = cache.known ? (cache.pending.get(relPath) ?? 0) : 0;
          const cacheComplete = !cache.known || (cache.present.has(relPath) && pendingRefs === 0);
          const graphKnowsFile = graphFiles.has(relPath);
          const mtimeChanged = stat.mtimeMs > lastReconciled;

          if (mtimeChanged || !cacheComplete || !graphKnowsFile) {
            if (!mtimeChanged && (!cacheComplete || !graphKnowsFile)) {
              // Unchanged content but incomplete coverage: drop the primed hash
              // and force the file through runSync()'s hash fast-path.
              this.contentHashes.delete(relPath);
              this.forceReconcile.add(relPath);
              requeuedIncomplete++;
            }
            this.syncQueue.add(relPath);
            queued++;
          } else {
            // File unchanged and covered — prime the hash so runSync() skips it
            const content = await fs.readFile(absPath, 'utf-8');
            this.contentHashes.set(relPath, this.hashContent(content));
            primed++;
          }
          // Always update mtime baseline
          this.fileMtimes.set(relPath, stat.mtimeMs);
        } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err); /* skip */ }
      }

      // One line per full sync so an incomplete graph is visible instead of
      // silent. `reconciled` counts discovered files that are provably complete
      // (graph File node + cache entry with zero pending refs); `unresolved` is
      // the engine's pending reference count across those files.
      if (cache.known) {
        console.log(`[YAAM Reconciler] coverage: ${reconciledCount}/${allFilesSet.size} files, ${unresolvedRefs} refs pending`);
      } else {
        console.log(`[YAAM Reconciler] coverage: ${reconciledCount}/${allFilesSet.size} files, refs pending unknown (reconcile cache off/unreadable)`);
      }
      console.log(`[YAAM Reconciler] scheduleFull completed. Queued: ${queued} (${requeuedIncomplete} re-queued for coverage), Primed (Skipped): ${primed}`);

      // Delete stale files (in graph but not on disk).
      for (const [fileId, _] of graphFiles) {
        if (!allFilesSet.has(fileId)) {
          try {
            await this.engine.reconcile({ file_path: fileId, content: "" });
            this.contentHashes.delete(fileId);
          } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err); /* ignore */ }
        }
      }

      await this.syncGithubContext();

      if (queued > 0 || this.syncQueue.size > 0) {
        this.triggerSync();
      }
    } catch (err) {
      console.error("[YAAM Reconciler] Full sync error:", err);
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
    this.debounceTimer.unref();
  }

  private async runSync() {
    if (this.syncQueue.size === 0) return;
    this.isRunning = true;
    const filesToSync = Array.from(this.syncQueue);
    this.syncQueue.clear();

    this.progress = { current: 0, total: filesToSync.length, detail: "Processing..." };

    const fs = await import('fs/promises');
    const { existsSync } = await import('fs');
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
        if (existsSync(resolved)) {
          const stat = await fs.stat(resolved);
          const relPath = path.relative(process.cwd(), resolved);
          
          // Prevent OOM from parsing massive files (e.g. minified JS > 1MB)
          if (stat.size > 1_000_000) {
            skipped++;
            this.fileMtimes.set(relPath, stat.mtimeMs);
            return;
          }

          const content = await fs.readFile(resolved, 'utf-8');

          // Skip if content hasn't changed since last reconciliation.
          const newHash = this.hashContent(content);
          // A file queued for coverage (not an mtime change) must not be skipped
          // by the content-hash fast-path — the engine needs to re-parse it and
          // re-queue its unresolved references.
          const forced = this.forceReconcile.delete(relPath);
          const existingHash = this.contentHashes.get(relPath);
          if (!forced && existingHash === newHash) {
            skipped++;
            const newStat = await fs.stat(resolved);
            this.fileMtimes.set(relPath, newStat.mtimeMs);
            return;
          }

          await this.engine.reconcile({ file_path: relPath, content });
          this.contentHashes.set(relPath, newHash);
          const finalStat = await fs.stat(resolved);
          this.fileMtimes.set(relPath, finalStat.mtimeMs);
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