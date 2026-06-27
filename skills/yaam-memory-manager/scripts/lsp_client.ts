import { spawn, ChildProcess } from 'child_process';
import * as path from 'path';

export class LspClient {
  private process: ChildProcess | null = null;
  private buffer: Buffer = Buffer.alloc(0);
  private nextRequestId: number = 1;
  private pendingRequests = new Map<number, { resolve: (val: any) => void; reject: (err: any) => void }>();
  private alive = false;

  constructor(command: string, args: string[], rootDir: string) {
    try {
      this.process = spawn(command, args, { cwd: rootDir, stdio: ['pipe', 'pipe', 'inherit'] });
      this.alive = true;

      this.process.stdout!.on('data', (chunk) => this.handleData(chunk));

      this.process.on('error', (err) => {
        console.error('LSP process error:', err);
        this.alive = false;
        this.rejectAll(err);
      });

      this.process.on('exit', (code, signal) => {
        this.alive = false;
        this.process = null;
        if (this.pendingRequests.size > 0) {
          this.rejectAll(new Error(`LSP process exited (code=${code}, signal=${signal})`));
        }
      });

      // Handle stdin errors (broken pipe, etc.)
      this.process.stdin?.on('error', (err) => {
        this.alive = false;
        this.rejectAll(err);
      });
    } catch (e) {
      this.alive = false;
      throw e;
    }
  }

  get isAlive(): boolean {
    return this.alive && this.process !== null;
  }

  private rejectAll(err: any): void {
    for (const { reject } of this.pendingRequests.values()) {
      try { reject(err); } catch {}
    }
    this.pendingRequests.clear();
  }

  private handleData(chunk: Buffer) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    while (true) {
      const str = this.buffer.toString('utf8');
      const headerIndex = str.indexOf('\r\n\r\n');
      if (headerIndex === -1) break;

      const headerPart = str.substring(0, headerIndex);
      const match = headerPart.match(/Content-Length:\s*(\d+)/i);
      if (!match) {
        this.buffer = this.buffer.subarray(1);
        continue;
      }

      const contentLength = parseInt(match[1], 10);
      const headerLength = Buffer.byteLength(headerPart, 'utf8') + 4;

      if (this.buffer.length < headerLength + contentLength) {
        break;
      }

      const jsonPayload = this.buffer.subarray(headerLength, headerLength + contentLength).toString('utf8');
      this.buffer = this.buffer.subarray(headerLength + contentLength);

      try {
        const message = JSON.parse(jsonPayload);
        this.handleMessage(message);
      } catch (e) {
        console.error("Failed to parse LSP message:", e);
      }
    }
  }

  private handleMessage(msg: any) {
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const pending = this.pendingRequests.get(msg.id);
      if (pending) {
        this.pendingRequests.delete(msg.id);
        if (msg.error) {
          pending.reject(msg.error);
        } else {
          pending.resolve(msg.result);
        }
      }
    }
  }

  public sendRequest(method: string, params: any): Promise<any> {
    if (!this.isAlive) {
      return Promise.reject(new Error(`LSP process not alive (method: ${method})`));
    }

    const id = this.nextRequestId++;
    const payload = { jsonrpc: '2.0', id, method, params };
    const body = JSON.stringify(payload);
    const header = `Content-Length: ${Buffer.byteLength(body, 'utf8')}\r\n\r\n`;

    return new Promise((resolve, reject) => {
      this.pendingRequests.set(id, { resolve, reject });
      try {
        this.process!.stdin!.write(header + body);
      } catch (e) {
        this.pendingRequests.delete(id);
        reject(e);
      }
    });
  }

  public sendNotification(method: string, params: any) {
    if (!this.isAlive) return;
    const payload = { jsonrpc: '2.0', method, params };
    const body = JSON.stringify(payload);
    const header = `Content-Length: ${Buffer.byteLength(body, 'utf8')}\r\n\r\n`;
    try {
      this.process!.stdin!.write(header + body);
    } catch {}
  }

  public async initialize(rootDir: string) {
    const rootUri = `file://${path.resolve(rootDir)}`;
    await this.sendRequest('initialize', {
      processId: process.pid,
      rootPath: rootDir,
      rootUri,
      capabilities: {
        textDocument: {
          definition: { dynamicRegistration: true },
          documentSymbol: { dynamicRegistration: true, hierarchicalDocumentSymbolSupport: true }
        }
      },
      initializationOptions: {}
    });
    this.sendNotification('initialized', {});
  }

  public stop() {
    if (!this.process) return;
    this.alive = false;

    // Best-effort graceful shutdown
    try {
      const body = JSON.stringify({ jsonrpc: '2.0', method: 'shutdown', params: null });
      const header = `Content-Length: ${Buffer.byteLength(body, 'utf8')}\r\n\r\n`;
      this.process.stdin?.write(header + body);
    } catch {}

    // Give it a moment to process shutdown, then SIGTERM
    setTimeout(() => {
      try { this.process?.kill('SIGTERM'); } catch {}
      this.process = null;
    }, 100);

    this.pendingRequests.clear();
  }
}