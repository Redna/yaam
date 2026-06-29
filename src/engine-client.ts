import { spawn, ChildProcess } from 'node:child_process';
import * as readline from 'node:readline';
import * as path from 'node:path';

export interface RpcRequest {
  jsonrpc: "2.0";
  method: string;
  params: any;
  id: string | number | null;
}

export interface RpcResponse {
  jsonrpc: "2.0";
  result?: any;
  error?: {
    code: number;
    message: string;
    data?: any;
  };
  id: string | number | null;
}

export class YaamEngineClient {
  private proc: ChildProcess | null = null;
  private pendingRequests = new Map<string, { resolve: (res: any) => void; reject: (err: any) => void }>();
  private nextRequestId = 1;

  constructor(private eventsPath: string) {}

  public async start(): Promise<void> {
    const cargoTomlPath = path.resolve(process.cwd(), 'src-rust', 'Cargo.toml');
    const binPath = path.resolve(process.cwd(), 'src-rust', 'target', 'release', 'yaam-engine');
    const fs = require('fs');

    if (fs.existsSync(binPath)) {
      this.proc = spawn(binPath, [this.eventsPath], {
        stdio: ['pipe', 'pipe', 'inherit'],
      });
    } else {
      const cargoCmd = process.env.HOME ? path.join(process.env.HOME, '.cargo', 'bin', 'cargo') : 'cargo';
      // Spawn the Rust engine using cargo run.
      this.proc = spawn(cargoCmd, ['run', '--manifest-path', cargoTomlPath, '--release', '--', this.eventsPath], {
        stdio: ['pipe', 'pipe', 'inherit'], // inherit stderr for logs
      });
    }

    if (!this.proc.stdout || !this.proc.stdin) {
      throw new Error("Failed to initialize stdio for YAAM engine");
    }

    const rl = readline.createInterface({
      input: this.proc.stdout,
      terminal: false,
    });

    rl.on('line', (line) => {
      try {
        const response: RpcResponse = JSON.parse(line);
        if (response.id !== null) {
          const idStr = String(response.id);
          const handlers = this.pendingRequests.get(idStr);
          if (handlers) {
            this.pendingRequests.delete(idStr);
            if (response.error) {
              handlers.reject(new Error(`RPC Error [${response.error.code}]: ${response.error.message}`));
            } else {
              handlers.resolve(response.result);
            }
          }
        }
      } catch (err) {
        console.error("Failed to parse YAAM engine response:", line);
      }
    });

    this.proc.on('exit', (code) => {
      this.proc = null;
      for (const [id, handlers] of this.pendingRequests.entries()) {
        handlers.reject(new Error("Engine exited"));
      }
      this.pendingRequests.clear();
    });
  }

  public stop(): void {
    if (this.proc) {
      this.call('shutdown', {}).catch(() => {});
      this.proc.kill();
      this.proc = null;
    }
  }

  private call(method: string, params: any): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!this.proc || !this.proc.stdin) {
        return reject(new Error("Engine not running"));
      }
      
      const id = String(this.nextRequestId++);
      const request: RpcRequest = {
        jsonrpc: "2.0",
        method,
        params,
        id,
      };

      this.pendingRequests.set(id, { resolve, reject });
      this.proc.stdin.write(JSON.stringify(request) + '\n');
    });
  }

  // ─── API Methods ─────────────────────────────────────────────────────────

  public async upsertNode(payload: { id: string; label: string; properties: any }): Promise<{ id: string }> {
    return this.call('upsert_node', payload);
  }

  public async linkNodes(payload: { from_id: string; to_id: string; relationship: string; properties: any }): Promise<void> {
    await this.call('link_nodes', payload);
  }

  public async deleteNode(payload: { id: string }): Promise<void> {
    await this.call('delete_node', payload);
  }

  public async deleteEdges(payload: { from_id: string; direction: string }): Promise<void> {
    await this.call('delete_edges', payload);
  }

  public async query(dsl: any): Promise<any> {
    return this.call('query', dsl);
  }

  public async search(payload: { text: string; top_k?: number; workspace?: string }): Promise<any[]> {
    return this.call('search', payload);
  }

  public async reconcile(payload: { file_path: string; content: string }): Promise<{ upserted_nodes: string[] }> {
    return this.call('reconcile', payload);
  }
}
