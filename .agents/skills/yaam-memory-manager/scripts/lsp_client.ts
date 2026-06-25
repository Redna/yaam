import { spawn, ChildProcess } from 'child_process';
import * as path from 'path';

export class LspClient {
  private process: ChildProcess;
  private buffer: Buffer = Buffer.alloc(0);
  private nextRequestId: number = 1;
  private pendingRequests = new Map<number, { resolve: (val: any) => void; reject: (err: any) => void }>();

  constructor(command: string, args: string[], rootDir: string) {
    this.process = spawn(command, args, { cwd: rootDir, stdio: ['pipe', 'pipe', 'inherit'] });
    this.process.stdout!.on('data', (chunk) => this.handleData(chunk));
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
    const id = this.nextRequestId++;
    const payload = {
      jsonrpc: '2.0',
      id,
      method,
      params
    };
    const body = JSON.stringify(payload);
    const header = `Content-Length: ${Buffer.byteLength(body, 'utf8')}\r\n\r\n`;

    return new Promise((resolve, reject) => {
      this.pendingRequests.set(id, { resolve, reject });
      this.process.stdin!.write(header + body);
    });
  }

  public sendNotification(method: string, params: any) {
    const payload = {
      jsonrpc: '2.0',
      method,
      params
    };
    const body = JSON.stringify(payload);
    const header = `Content-Length: ${Buffer.byteLength(body, 'utf8')}\r\n\r\n`;
    this.process.stdin!.write(header + body);
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
    this.process.kill();
  }
}
