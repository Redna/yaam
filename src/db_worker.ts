import ladybug from '@ladybugdb/core';
import * as fs from 'fs';
import * as path from 'path';

let db: any = null;
let conn: any = null;
let stmts = new Map<number, any>();
let nextStmtId = 1;

function sleep(ms: number) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

async function safeOpenDatabase(dbPath: string): Promise<any> {
  const lockDir = dbPath + '.yaam_probe_lock';

  let lockAcquired = false;
  try {
    fs.mkdirSync(lockDir);
    lockAcquired = true;
  } catch (e) {
    let waited = 0;
    while (fs.existsSync(lockDir) && waited < 15000) {
      await sleep(100);
      waited += 100;
    }
    try {
      fs.mkdirSync(lockDir);
      lockAcquired = true;
    } catch {}
  }

  try {
    // 32MB buffer pool is plenty for code topology graphs and prevents VM OOM.
    return new ladybug.Database(dbPath, 32 * 1024 * 1024);
  } finally {
    if (lockAcquired) {
      try { fs.rmdirSync(lockDir); } catch {}
    }
  }
}

process.on('message', async (msg: any) => {
  try {
    if (msg.action === 'open') {
      if (db) throw new Error("Already open");
      db = await safeOpenDatabase(msg.dbPath);
      conn = new ladybug.Connection(db);
      safeSend({ id: msg.id, success: true });

    } else if (msg.action === 'close') {
      if (conn) {
        try { await conn.query("CHECKPOINT"); } catch {}
        try { await conn.close(); } catch {}
      }
      if (db) {
        try { await db.close(); } catch {}
      }
      db = null;
      conn = null;
      stmts.clear();
      if (global.gc) global.gc(); // Force V8 to run destructors and unmap 8TB sparse memory
      safeSend({ id: msg.id, success: true });

    } else if (msg.action === 'prepare') {
      if (!conn) throw new Error("Connection not open");
      const stmt = await conn.prepare(msg.query);
      const id = nextStmtId++;
      stmts.set(id, stmt);
      safeSend({ id: msg.id, success: true, resultId: id });

    } else if (msg.action === 'execute_stmt') {
      if (!conn) throw new Error("Connection not open");
      const stmt = stmts.get(msg.stmtId);
      if (!stmt) throw new Error("Statement not found");
      const res = await conn.execute(stmt, msg.params || {});
      const rows = await res.getAll();
      safeSend({ id: msg.id, success: true, rows });

    } else if (msg.action === 'execute_query') {
      if (!conn) throw new Error("Connection not open");
      const prep = await conn.prepare(msg.query);
      const res = await conn.execute(prep, msg.params || {});
      const rows = await res.getAll();
      safeSend({ id: msg.id, success: true, rows });

    } else if (msg.action === 'query') {
      if (!conn) throw new Error("Connection not open");
      const res = await conn.query(msg.query);
      const rows = await res.getAll();
      safeSend({ id: msg.id, success: true, rows });
    }
  } catch (err: any) {
    // Only exit if the IPC channel is truly gone (parent died).
    // Transient send errors (EPIPE from full buffer, momentary congestion)
    // must NOT kill the worker — that creates a race where the callback
    // fires mid-next-operation, corrupting DB state and leaving the
    // parent's pending request hanging forever.
    if (!process.connected || !process.send) {
      process.exit(1);
    }
    try {
      process.send!({ id: msg.id, success: false, error: err.message || String(err) });
    } catch {
      // Send failed but IPC still connected — don't exit.
      // Parent will detect the missing response via its exit/error handlers.
    }
  }
});

// Helper for safe sends — only exits if IPC is truly disconnected.
// Transient send errors are swallowed to prevent killing the worker
// mid-operation (which corrupts DB state and hangs the parent's mutex).
function safeSend(payload: any) {
  if (!process.connected || !process.send) {
    process.exit(1);
  }
  try {
    process.send!(payload);
  } catch {
    // Transient error — don't exit. Parent handles missing responses.
  }
}
