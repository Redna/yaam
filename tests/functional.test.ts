import test from 'node:test';
import assert from 'node:assert';
import fs from 'fs';
import path from 'path';
import { ConnectionManager } from '../src/db.js';
import { initializeWorkspace, appendNote } from '../src/workspace.js';
import { exploreGraph } from '../src/graph_explore.js';

test('Functional Test - Workspace Initialization and Notes', async () => {
  const dbPath = 'test_functional.lbug';
  if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
  if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });

  const cm = new ConnectionManager(dbPath);
  
  try {
    // 1. Setup workspace
    await cm.withConnection(async (conn) => {
      await initializeWorkspace('test_ws', 'A test workspace', conn);
      
      const res = await conn.query("MATCH (w:Workspace) RETURN w.workspace_name, w.status");
      const rows = await res.getAll();
      assert.strictEqual(rows.length, 1);
      assert.strictEqual(rows[0]['w.workspace_name'], 'test_ws');
      assert.strictEqual(rows[0]['w.status'], 'active');
    });

    // 2. Append note to workspace
    await cm.withConnection(async (conn) => {
      await appendNote('test_ws', 'My super secret note', conn);
      
      // Verify note exists and is linked
      const res = await conn.query("MATCH (w:Workspace)-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content");
      const rows = await res.getAll();
      assert.strictEqual(rows.length, 1);
      assert.strictEqual(rows[0]['s.content'], 'My super secret note');
    });
  } finally {
    (cm as any).killWorker();
    if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
    if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });
  }
});

test('Functional Test - File Decay and Stale Marking (Reconciler Logic)', async () => {
  const dbPath = 'test_decay.lbug';
  if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
  if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });

  const cm = new ConnectionManager(dbPath);
  
  try {
    await cm.withConnection(async (conn) => {
      await initializeWorkspace('decay_ws', 'Decay Workspace', conn);
      
      // Create a mock file and map it
      await conn.query("CREATE (:Entity {id: 'deleted_file.ts', type: 'File', status: 'active'})");
      await conn.query(`
        MATCH (w:Workspace {workspace_name: 'decay_ws'}), (e:Entity {id: 'deleted_file.ts'}) 
        CREATE (w)-[:MAPPED_TO {created_at: 1000, is_stale: false}]->(e)
      `);
      
      // Simulate file removal and stale cleanup (this mimics Reconciler's commit logic for stale files)
      const ts = Math.floor(Date.now() / 1000);
      await conn.query("MATCH (e:Entity {id: 'deleted_file.ts'}) SET e.status = 'deleted'");
      await conn.query(`
        MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity) 
        WHERE e.id = 'deleted_file.ts' 
        SET r.is_stale = true, r.invalidated_at = $ts
      `, { ts });
      
      // Verify it was decayed properly
      const res = await conn.query("MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity) RETURN e.status, r.is_stale");
      const rows = await res.getAll();
      assert.strictEqual(rows.length, 1);
      assert.strictEqual(rows[0]['e.status'], 'deleted');
      assert.strictEqual(rows[0]['r.is_stale'], true);
    });
  } finally {
    (cm as any).killWorker();
    if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
    if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });
  }
});

test('Functional Test - Graph Explore Spooling', async () => {
  const dbPath = 'test_spool.lbug';
  if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
  if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });

  const cm = new ConnectionManager(dbPath);
  const tmpDir = path.join(process.cwd(), '.chunks', 'memory_dumps');
  
  try {
    await cm.withConnection(async (conn) => {
      // Create 25 entities (more than 20 to trigger spooling)
      for (let i = 0; i < 25; i++) {
        await conn.query(`CREATE (:Entity {id: 'node_${i}', type: 'Test'})`);
      }
      
      // Explore graph
      const result = await exploreGraph("MATCH (n:Entity {type: 'Test'}) RETURN n.id", conn, process.cwd());
      
      // Verify spooling logic
      assert.ok(result.spooledTo !== undefined, 'Result should be spooled to a file');
      assert.ok(result.text.includes('SUCCESS: Query returned 25 rows'), 'Text should mention spooling success');
      
      // Verify file contents
      const fileContent = fs.readFileSync(result.spooledTo, 'utf-8');
      assert.ok(fileContent.includes("node_0"), 'Spooled file should contain node_0');
      assert.ok(fileContent.includes("node_24"), 'Spooled file should contain node_24');
    });
  } finally {
    (cm as any).killWorker();
    if (fs.existsSync(dbPath)) fs.rmSync(dbPath, { force: true });
    if (fs.existsSync(dbPath + '.wal')) fs.rmSync(dbPath + '.wal', { force: true });
    if (fs.existsSync(tmpDir)) fs.rmSync(tmpDir, { recursive: true, force: true });
  }
});
