import { spawn } from 'node:child_process';
import * as net from 'node:net';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as readline from 'node:readline';

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
  private socket: net.Socket | null = null;
  private pendingRequests = new Map<string, { resolve: (res: any) => void; reject: (err: any) => void }>();
  private nextRequestId = 1;
  private currentPort: number | null = null;
  private reconnecting: Promise<void> | null = null;

  constructor(private eventsPath: string) {}

  public async start(): Promise<void> {
    const port = await this.ensureDaemonRunning();
    await this.connectToDaemon(port);
  }

  private async ensureDaemonRunning(): Promise<number> {
    const portFilePath = path.resolve(process.cwd(), '.yaam', 'daemon.port');

    // Check if daemon is already running
    if (fs.existsSync(portFilePath)) {
      const portStr = fs.readFileSync(portFilePath, 'utf-8').trim();
      const port = parseInt(portStr, 10);
      if (!isNaN(port)) {
        // Test connection
        try {
          await this.testConnection(port);
          return port; // Successfully connected to existing daemon
        } catch (e) {
          console.log("Stale daemon port file detected, starting new daemon...");
          fs.unlinkSync(portFilePath);
        }
      }
    }

    // Spawn new daemon
    // Starting YAAM daemon in the background
    const cargoTomlPath = path.resolve(process.cwd(), 'src-rust', 'Cargo.toml');
    const binPath = path.resolve(process.cwd(), 'src-rust', 'target', 'release', 'yaam-engine');

    if (fs.existsSync(binPath)) {
      spawn(binPath, [this.eventsPath], {
        detached: true,
        stdio: 'ignore',
      }).unref();
    } else {
      const cargoCmd = process.env.HOME ? path.join(process.env.HOME, '.cargo', 'bin', 'cargo') : 'cargo';
      spawn(cargoCmd, ['run', '--manifest-path', cargoTomlPath, '--release', '--', this.eventsPath], {
        detached: true,
        stdio: 'ignore',
      }).unref();
    }

    // Wait for the port file to be written
    let retries = 0;
    while (retries < 50) { // 5 seconds max
      if (fs.existsSync(portFilePath)) {
        const portStr = fs.readFileSync(portFilePath, 'utf-8').trim();
        const port = parseInt(portStr, 10);
        if (!isNaN(port)) {
          return port;
        }
      }
      await new Promise(r => setTimeout(r, 100));
      retries++;
    }
    throw new Error("Failed to start YAAM daemon: Timed out waiting for .yaam/daemon.port");
  }

  private testConnection(port: number): Promise<void> {
    return new Promise((resolve, reject) => {
      const socket = new net.Socket();
      socket.setTimeout(1000);
      socket.on('connect', () => {
        socket.destroy();
        resolve();
      });
      socket.on('error', reject);
      socket.on('timeout', () => {
        socket.destroy();
        reject(new Error("Timeout"));
      });
      socket.connect(port, '127.0.0.1');
    });
  }

  private connectToDaemon(port: number): Promise<void> {
    return new Promise((resolve, reject) => {
      this.socket = new net.Socket();
      let connected = false;
      
      this.socket.on('connect', () => {
        connected = true;
        resolve();
        
        // Setup readline on socket
        const rl = readline.createInterface({
          input: this.socket as any,
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
            // Failed to parse YAAM engine response
          }
        });
      });

      this.socket.on('error', (err) => {
        if (!connected) {
          reject(err);
        } else {
          console.error("YAAM daemon connection error:", err);
        }
      });

      this.socket.on('close', () => {
        this.socket = null;
        for (const [id, handlers] of this.pendingRequests.entries()) {
          handlers.reject(new Error("Daemon connection closed"));
        }
        this.pendingRequests.clear();
      });

      this.socket.connect(port, '127.0.0.1');
    });
  }

  public stop(): void {
    if (this.socket) {
      this.socket.destroy();
      this.socket = null;
    }
  }

  /**
   * Attempt to reconnect to the daemon. If the daemon has died, a new one
   * will be spawned. Returns a resolved promise once connected.
   * Uses a singleton reconnecting promise to prevent multiple concurrent reconnection attempts.
   */
  private async reconnect(): Promise<void> {
    if (this.socket) return; // Already reconnected by another caller
    if (this.reconnecting) return this.reconnecting;

    this.reconnecting = (async () => {
      try {
        const port = await this.ensureDaemonRunning();
        this.currentPort = port;
        await this.connectToDaemon(port);
      } finally {
        this.reconnecting = null;
      }
    })();

    return this.reconnecting;
  }

  private call(method: string, params: any): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!this.socket) {
        // Attempt auto-reconnect, then retry the call
        this.reconnect()
          .then(() => {
            if (!this.socket) {
              return reject(new Error("Daemon not connected after reconnect attempt"));
            }
            this.dispatchCall(method, params, resolve, reject);
          })
          .catch(reject);
        return;
      }
      this.dispatchCall(method, params, resolve, reject);
    });
  }

  private dispatchCall(
    method: string,
    params: any,
    resolve: (res: any) => void,
    reject: (err: any) => void,
  ): void {
    if (!this.socket) {
      return reject(new Error("Daemon not connected"));
    }

    const id = String(this.nextRequestId++);
    const request: RpcRequest = {
      jsonrpc: "2.0",
      method,
      params,
      id,
    };

    this.pendingRequests.set(id, { resolve, reject });
    this.socket.write(JSON.stringify(request) + '\n');
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
