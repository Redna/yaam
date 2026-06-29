import test from 'node:test';
import assert from 'node:assert';
import fs from 'fs';
import path from 'path';
import { ConnectionManager } from '../src/db.js';

test('ConnectionManager - sendRequest timeout', async () => {
  const cm = new ConnectionManager('test_timeout.lbug');
  // Mock startWorker to create a dummy worker that does not respond
  (cm as any).startWorker = function () {
    this.worker = {
      send: (msg: any, cb: (err?: Error) => void) => {
        // Do not call cb, do not send response back. This simulates a hung worker.
      },
      on: () => {},
      stderr: { on: () => {} },
      stdout: { on: () => {} },
      kill: () => {},
    };
  };

  const start = Date.now();
  try {
    // Override timeout to 100ms for fast testing
    await (cm as any).sendRequest({ action: 'open' }, 100);
    assert.fail('Should have timed out');
  } catch (e: any) {
    assert.ok(e.message.includes('Worker request timed out after 100ms'), 'Error should be a timeout');
  }
  const duration = Date.now() - start;
  assert.ok(duration >= 100 && duration < 1000, 'Timeout should take around 100ms');
});

test('ConnectionManager - isCrash logic ignores SIGKILL/SIGTERM', async () => {
  const dbPath = 'test_crash.lbug';
  fs.writeFileSync(dbPath, 'dummy data');
  const cm = new ConnectionManager(dbPath);
  
  let attempts = 0;
  (cm as any).startWorker = function () {
    this.worker = {
      send: (msg: any, cb: (err?: Error) => void) => {
        cb(); // Success send
      },
      on: () => {},
      stderr: { on: () => {} },
      stdout: { on: () => {} },
      kill: () => {},
    };
  };

  // Mock sendRequest to simulate a crash response first, then success
  (cm as any).sendRequest = async function(msg: any) {
    if (msg.action === 'open') {
      attempts++;
      if (attempts <= 2) {
        // Simulate a SIGKILL error from the worker
        throw new Error("Worker exited with code null signal SIGKILL");
      }
      return { success: true }; // Succeed on 3rd attempt
    }
    if (msg.action === 'close') return { success: true };
    return { success: true };
  };

  try {
    await cm.withConnection(async () => {
      // should succeed on attempt 3
    }, 5); // 5 max retries
  } catch (e) {
    assert.fail('Should have retried and succeeded');
  }

  // Verify that the DB file was NOT deleted (we had 2 "SIGKILL" errors, but it should not trigger the deletion logic anymore anyway)
  assert.ok(fs.existsSync(dbPath), 'Database file should not be deleted');
  fs.rmSync(dbPath);
});

test('ConnectionManager - setupSchema ignores non-fatal errors', async () => {
  const cm = new ConnectionManager('test_schema.lbug');
  
  let queriesRun = 0;
  // Mock sendRequest to throw specific errors for queries
  (cm as any).sendRequest = async function(msg: any) {
    if (msg.action === 'open') return { success: true };
    if (msg.action === 'close') return { success: true };
    if (msg.action === 'query') {
      queriesRun++;
      if (queriesRun === 1) {
        throw new Error("Table already exists");
      } else if (queriesRun === 2) {
        throw new Error("Binder Error: Type mismatch");
      } else if (queriesRun >= 3) {
        throw new Error("already opened"); // This should trigger lock error retries!
      }
      return { success: true, rows: [] };
    }
    return { success: true };
  };

  (cm as any).startWorker = function () {
    this.worker = { kill: () => {} };
  };

  try {
    await cm.withConnection(async () => {}, 2); // only 2 retries
    assert.fail('Should have thrown after max retries');
  } catch (e: any) {
    if (!e.message.includes('already opened')) {
      assert.fail(`Expected 'already opened' error, got: ${e.message}`);
    }
  }
});
