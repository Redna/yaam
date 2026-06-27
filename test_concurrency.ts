import { ConnectionManager } from './src/db.js';

const NUM_CONCURRENT = 5;

async function workerLoop(id: number) {
  const cm = new ConnectionManager();
  for (let i = 0; i < 3; i++) {
    try {
      console.log(`[Worker ${id}] Attempt ${i} starting`);
      await cm.withConnection(async (conn) => {
        // Run a simple query
        await conn.query("MATCH (n:Entity) RETURN count(n) LIMIT 1");
        console.log(`[Worker ${id}] Attempt ${i} succeeded`);
      });
    } catch (e) {
      console.log(`[Worker ${id}] Attempt ${i} failed:`, e);
    }
  }
}

async function main() {
  const promises = [];
  for (let i = 0; i < NUM_CONCURRENT; i++) {
    promises.push(workerLoop(i));
  }
  await Promise.all(promises);
  console.log("All done!");
}

main();
