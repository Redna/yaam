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
  const walPath = dbPath + '.wal';
  const lockDir = dbPath + '.yaam_probe_lock';

  if (fs.existsSync(walPath)) {
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
    }

    if (lockAcquired) {
      try {
        // If we acquired the lock and WAL exists, we could check if it crashes.
        // We rely on the worker dying if it segfaults.
      } finally {
        try { fs.rmdirSync(lockDir); } catch {}
      }
    }
  }

  // Default buffer pool is ~8TB which causes Mmap failures.
  // 256MB is plenty for YAAM's graph size.
  return new ladybug.Database(dbPath, 256 * 1024 * 1024);
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
    if (process.connected && process.send) {
      try { 
        process.send({ id: msg.id, success: false, error: err.message || String(err) }, (e) => {
          if (e) process.exit(1);
        }); 
      } catch (e) { process.exit(1); }
    } else {
      process.exit(1);
    }
  }
});

// Helper for safe sends
function safeSend(payload: any) {
  if (process.connected && process.send) {
    try { 
      process.send(payload, (e) => {
        if (e) process.exit(1);
      }); 
    } catch (e) { process.exit(1); }
  } else {
    process.exit(1);
  }
}
