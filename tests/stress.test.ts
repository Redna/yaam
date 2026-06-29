import test from 'node:test';
import assert from 'node:assert';
import fs from 'fs';
import path from 'path';
import { ConnectionManager } from '../src/db.js';

test('Stress Test - Parallel Connection Requests', async () => {
  const dbPath = 'test_parallel.lbug';
  if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
  if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });

  const cm = new ConnectionManager(dbPath);

  try {
    // 1. Initial setup to create the schema so queries don't fail
    await cm.withConnection(async (conn) => {
      // Schema setup happens automatically in withConnection
      await conn.query("CREATE NODE TABLE StressTest(id STRING, val INT64, PRIMARY KEY (id))");
    });

    // 2. Fire 50 concurrent writes
    const promises = [];
    const concurrency = 50;
    let completedCount = 0;

    for (let i = 0; i < concurrency; i++) {
      promises.push(
        cm.withConnection(async (conn) => {
          // Simulate some parallel reads and writes
          await conn.query(`MERGE (n:StressTest {id: 'node_${i}'}) SET n.val = ${i}`);
          completedCount++;
        })
      );
    }

    await Promise.all(promises);
    assert.strictEqual(completedCount, concurrency, 'All concurrent writes should complete');

    // 3. Verify all 50 writes persisted
    await cm.withConnection(async (conn) => {
      const res = await conn.query("MATCH (n:StressTest) RETURN count(n) AS cnt");
      const rows = await res.getAll();
      assert.strictEqual(Number(rows[0].cnt), concurrency, 'Database should contain exactly 50 nodes');
    });

  } finally {
    // Clean up
    (cm as any).killWorker();
    if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
    if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });
  }
});

test('Stress Test - Corrupted WAL/Database Recovery', async () => {
  const dbPath = 'test_corrupt.lbug';
  const walPath = dbPath + '.wal';
  
  if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
  if (fs.existsSync(walPath)) fs.rmSync(walPath, { force: true });

  // 1. Create a valid database first
  const cm = new ConnectionManager(dbPath);
  await cm.withConnection(async (conn) => {
    await conn.query("CREATE NODE TABLE Dummy(id STRING, PRIMARY KEY (id))");
    await conn.query("MERGE (n:Dummy {id: 'valid'})");
  });

  // 2. Corrupt the WAL file with random garbage bytes to simulate a broken tool mid-write
  const garbage = Buffer.alloc(1024 * 1024);
  for (let i = 0; i < garbage.length; i++) {
    garbage[i] = Math.floor(Math.random() * 256);
  }
  fs.writeFileSync(walPath, garbage);

  // 3. Try to connect. LadybugDB might crash the worker on a corrupted WAL.
  // Our connection manager should retry and eventually throw, but NOT crash the main process
  // and NOT delete the database (to prevent data loss, per the RCA fix).
  let threwError = false;
  try {
    await cm.withConnection(async (conn) => {
      await conn.query("MATCH (n:Dummy) RETURN n");
    }, 3); // Limit to 3 retries for the test to be faster
  } catch (e: any) {
    threwError = true;
    // Ensure the error was propagated gracefully
    assert.ok(
      e.message.includes('exited with code') || 
      e.message.includes('max retries') || 
      e.message.includes('corrupt') ||
      e.message.includes('Worker IPC error'),
      `Unexpected error message: ${e.message}`
    );
  }

  assert.ok(threwError, 'Connection manager should throw when database is fatally corrupted');
  
  // Verify the database file is still there! (We don't auto-delete per user requirements to avoid data loss)
  assert.ok(fs.existsSync(dbPath), 'Corrupted database file should NOT be automatically deleted');
  
  // Clean up
  (cm as any).killWorker();
  if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
  if (fs.existsSync(walPath)) fs.rmSync(walPath, { force: true });
});
