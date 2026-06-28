import { getConn, setupDatabase, DB_PATH } from './db.js';
import { Command } from 'commander';
import { exec } from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as util from 'util';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const execPromise = util.promisify(exec);

async function runReconciler() {
  try {
    const tsReconcilerPath = path.join(__dirname, 'reconciler.ts');
    await execPromise(`npx tsx "${tsReconcilerPath}" < /dev/null`);
  } catch (e) {
    console.error('YAAM reconciler failed:', e);
  }
}

async function main() {
  const program = new Command();
  program
    .name('graph_explore')
    .description('Run read-only Cypher query against LadybugDB')
    .argument('<query>', 'Cypher query to run')
    .action(async (query: string) => {
      await runReconciler();
      await setupDatabase();

      const cleanQuery = query.trim();
      const forbidden = ["CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DROP", "ALTER"];
      const hasForbidden = forbidden.some(k => new RegExp(`\\b${k}\\b`, "i").test(cleanQuery));
      if (hasForbidden) {
        console.error("ERROR: Write operations forbidden via this tool. Use workspace mutation tools to alter memory.");
        process.exit(1);
      }

      let constrainedQuery = cleanQuery;
      if (!constrainedQuery.toUpperCase().includes("LIMIT")) {
        constrainedQuery = constrainedQuery.replace(/(RETURN\s+.+)/i, "$1 LIMIT 500");
        if (!constrainedQuery.toUpperCase().includes("LIMIT 500")) {
          constrainedQuery += " LIMIT 500";
        }
      }

      const { db, conn } = getConn();
      try {
        const queryResult = (await conn.query(constrainedQuery)) as any;
        const rows = await queryResult.getAll();

        if (rows.length === 0) {
          console.log("Query completed successfully. Zero rows returned.");
          return;
        }

        if (rows.length > 20) {
          const tmpDir = path.join(path.dirname(DB_PATH), '.chunks', 'memory_dumps');
          if (!fs.existsSync(tmpDir)) {
            fs.mkdirSync(tmpDir, { recursive: true });
          }
          const outputFile = path.join(tmpDir, 'query_out.txt');
          const fileContent = `Source Query: ${query}\n${"=".repeat(40)}\n` + 
            rows.map((r: any) => JSON.stringify(r)).join("\n");
          fs.writeFileSync(outputFile, fileContent, "utf-8");
          console.log(`SUCCESS: Query returned ${rows.length} rows. Results spooled to: '${outputFile}'.`);
          return;
        }

        console.log("Results:");
        for (const row of rows) {
          console.log(`- ${JSON.stringify(row)}`);
        }
      } catch (err: any) {
        console.error(`Database Syntax Error: ${err.message || String(err)}`);
        process.exit(1);
      } finally {
        await conn.close();
        await db.close();
      }
    });

  await program.parseAsync(process.argv);
}

main();
