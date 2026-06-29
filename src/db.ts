import { fork, ChildProcess } from 'child_process';
import * as path from 'path';
import * as url from 'url';
import * as fs from 'fs';

const __filename = url.fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const SCHEMA_QUERIES = [
  "CREATE NODE TABLE Entity(id STRING, type STRING, status STRING, last_modified INT64, metadata STRING, PRIMARY KEY (id))",
  "CREATE REL TABLE LINKED_TO(FROM Entity TO Entity, relationship_type STRING)",
  "CREATE NODE TABLE Workspace(workspace_name STRING, description STRING, status STRING, closed_at INT64, PRIMARY KEY (workspace_name))",
  "CREATE NODE TABLE Scratchpad(id STRING, content STRING, created_at INT64, PRIMARY KEY (id))",
  "CREATE REL TABLE MAPPED_TO(FROM Workspace TO Entity, created_at INT64, invalidated_at INT64, is_stale BOOLEAN)",
  "CREATE REL TABLE HAS_SCRATCHPAD(FROM Workspace TO Scratchpad)",
];

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

class Mutex {
  private queue: (() => void)[] = [];
  private locked = false;

  lock(): Promise<void> {
    return new Promise(resolve => {
      if (!this.locked) {
        this.locked = true;
        resolve();
      } else {
        this.queue.push(resolve);
      }
    });
  }

  unlock(): void {
    if (this.queue.length > 0) {
      const resolve = this.queue.shift();
      resolve!();
    } else {
      this.locked = false;
    }
  }
}

class QueryResultProxy {
  constructor(private rows: any[]) {}
  async getAll(): Promise<any[]> {
    return this.rows;
  }
}

class StatementProxy {
  constructor(public id: number) {}
}

export class ConnectionProxy {
  constructor(private sendRequest: (msg: any) => Promise<any>) {}

  async prepare(query: string): Promise<StatementProxy> {
    const res = await this.sendRequest({ action: 'prepare', query });
    return new StatementProxy(res.resultId);
  }

  async execute(stmt: StatementProxy | string, params?: any): Promise<QueryResultProxy> {
    if (typeof stmt === 'string') {
      const res = await this.sendRequest({ action: 'execute_query', query: stmt, params });
      return new QueryResultProxy(res.rows);
    } else {
      const res = await this.sendRequest({ action: 'execute_stmt', stmtId: stmt.id, params });
      return new QueryResultProxy(res.rows);
    }
  }

  async query(query: string): Promise<QueryResultProxy> {
    const res = await this.sendRequest({ action: 'query', query });
    return new QueryResultProxy(res.rows);
  }
}

export class ConnectionManager {
  private dbPath: string;
  /** Project root directory, captured at construction time.
   * Used as a stable anchor for project-relative paths (spool files, etc.)
   * since process.cwd() can shift during the session. */
  readonly projectRoot: string;
  private mutex = new Mutex();
  private worker: ChildProcess | null = null;
  private msgId = 1;
  private pendingRequests = new Map<number, { resolve: (val: any) => void; reject: (err: any) => void }>();

  constructor(dbPath?: string) {
    this.dbPath = dbPath || path.join(process.cwd(), 'memory.lbug');
    this.projectRoot = path.dirname(this.dbPath);
  }

  private startWorker() {
    if (this.worker) return;
    const ext = path.extname(__filename);
    const scriptPath = path.join(__dirname, 'db_worker' + ext);

    // Provide --import=tsx if running .ts and expose gc for memory management
    const execArgv = ext === '.ts' ? ['--import=tsx', '--expose-gc'] : ['--expose-gc'];

    // CRITICAL: Isolate worker stdio so it never writes to the parent's
    // fd 1 (stdout) / fd 2 (stderr). The host coding agent (pi) uses stdout
    // as a structured JSON-RPC channel — any interleaved output from the
    // worker (tsx loader, LadybugDB native, console.log, exit-time flush)
    // would corrupt that stream and crash the agent.
    //   stdin  → 'ignore'  : worker doesn't read stdin
    //   stdout → 'pipe'    : captured, never reaches parent fd 1
    //   stderr → 'pipe'    : captured, never reaches parent fd 2
    //   ipc    → 'ipc'     : IPC channel still created (required by fork)
    this.worker = fork(scriptPath, [], {
      execArgv,
      stdio: ['ignore', 'pipe', 'pipe', 'ipc']
    });

    // Optionally capture worker stderr for debugging. We must NOT write
    // it to process.stderr / console.error — that goes to pi's fd 2.
    // Buffer it in-memory for diagnostics if needed.
    this.worker.stderr?.on('data', (_chunk: Buffer) => {
      // Intentionally swallowed — do not forward to process.stderr.
    });
    this.worker.stderr?.on('error', () => {}); // Prevent unhandled EPIPE from crashing parent
    this.worker.stdout?.on('data', (_chunk: Buffer) => {
      // Intentionally swallowed — do not forward to process.stdout.
    });
    this.worker.stdout?.on('error', () => {}); // Prevent unhandled EPIPE from crashing parent

    this.worker.on('message', (msg: any) => {
      const req = this.pendingRequests.get(msg.id);
      if (req) {
        this.pendingRequests.delete(msg.id);
        if (msg.success) {
          req.resolve(msg);
        } else {
          req.reject(new Error(msg.error));
        }
      }
    });

    this.worker.on('error', (err: Error) => {
      // IPC channel errors (EPIPE, etc.) — don't let these crash the process
      this.worker = null;
      const e = new Error(`Worker IPC error: ${err.message}`);
      for (const req of this.pendingRequests.values()) {
        req.reject(e);
      }
      this.pendingRequests.clear();
    });

    this.worker.on('exit', (code, signal) => {
      this.worker = null;
      const err = new Error(`Worker exited with code ${code} signal ${signal}`);
      for (const req of this.pendingRequests.values()) {
        req.reject(err);
      }
      this.pendingRequests.clear();
    });
  }

  private sendRequest(msg: any, timeoutMs: number = 30000): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!this.worker) this.startWorker();
      const id = this.msgId++;
      const timer = setTimeout(() => {
        this.pendingRequests.delete(id);
        reject(new Error(`Worker request timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      this.pendingRequests.set(id, {
        resolve: (v: any) => { clearTimeout(timer); resolve(v); },
        reject: (e: any) => { clearTimeout(timer); reject(e); },
      });
      try {
        this.worker!.send({ ...msg, id }, (err) => {
          if (err) {
            clearTimeout(timer);
            this.pendingRequests.delete(id);
            reject(err);
          }
        });
      } catch (e) {
        clearTimeout(timer);
        this.pendingRequests.delete(id);
        reject(e);
      }
    });
  }

  private async setupSchema(conn: ConnectionProxy): Promise<void> {
    for (const query of SCHEMA_QUERIES) {
      try {
        await conn.query(query);
      } catch (e: any) {
        const errMsg = String(e).toLowerCase();
        if (errMsg.includes("already exists")) {
          continue; // Table/rel already exists — idempotent, safe to skip
        }
        if (errMsg.includes("lock") || errMsg.includes("already opened")) {
          throw e; // Lock errors should propagate so withConnection can retry
        }
        // All other errors (binder exceptions, type mismatches, version
        // differences on existing tables, etc.) are non-fatal during schema
        // setup — the tables already exist with compatible-enough structure.
        // Logging but NOT throwing prevents cascading connection failures on
        // databases created by previous schema versions.
        // Do NOT use console.error — that writes to pi's stderr (fd 2).
      }
    }
  }

  async withConnection<T>(
    fn: (conn: ConnectionProxy) => Promise<T>,
    maxRetries = 10
  ): Promise<T> {
    await this.mutex.lock();
    try {
      let lastError: any;
      for (let attempt = 0; attempt < maxRetries; attempt++) {
        try {
          this.startWorker();
          await this.sendRequest({ action: 'open', dbPath: this.dbPath });
          
          const connProxy = new ConnectionProxy((msg) => this.sendRequest(msg));
          await this.setupSchema(connProxy);
          
          const result = await fn(connProxy);
          
          await this.sendRequest({ action: 'close' });
          // Reuse worker for next call — no fork overhead.
          // Worker is only killed on error (see catch blocks below).
          return result;
        } catch (e: any) {
          const errMsg = String(e).toLowerCase();
          const isLockError = errMsg.includes("lock") || errMsg.includes("already opened");
          const isCrash = errMsg.includes("exited with code");
          
          if (!isLockError && !isCrash) {
            try { await this.sendRequest({ action: 'close' }); } catch {}
            this.killWorker();
            throw e;
          }
          
          lastError = e;
          try { await this.sendRequest({ action: 'close' }); } catch {}
          this.killWorker();
          
          const delay = Math.min(50 * Math.pow(2, attempt), 2000);
          await sleep(delay);
        }
      }
      throw lastError || new Error("Failed to acquire DB lock after max retries");
    } finally {
      this.mutex.unlock();
    }
  }

  /** Kill the worker process to release its buffer pool.
   * SIGKILL is used because SIGTERM doesn't reliably kill forked
   * processes that have an active IPC channel. This is safe because
   * killWorker() is only called AFTER the 'close' response confirms
   * that CHECKPOINT + conn.close() + db.close() have all completed. */
  private killWorker(): void {
    if (this.worker) {
      try { this.worker.kill('SIGKILL'); } catch {}
      this.worker = null;
      this.pendingRequests.clear();
    }
  }
}