import { getConn, setupDatabase } from './db.js';
import { Command } from 'commander';
import { exec } from 'child_process';
import * as path from 'path';
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
    .name('workspace_append_note')
    .description('Append a new note to the workspace scratchpad')
    .requiredOption('--workspace <workspace>', 'Workspace name')
    .requiredOption('--content <content>', 'Note content')
    .action(async (options) => {
      const { workspace, content } = options;

      await runReconciler();
      await setupDatabase();

      const noteId = `note_${Date.now()}`;
      const ts = Math.floor(Date.now() / 1000);

      const { db, conn } = getConn();
      try {
        // 1. Create the Scratchpad node
        const query1 = "CREATE (:Scratchpad {id: $id, content: $content, created_at: $ts})";
        const prep1 = await conn.prepare(query1);
        await conn.execute(prep1, { id: noteId, content, ts });

        // 2. Link the Scratchpad to the Workspace
        const query2 = "MATCH (w:Workspace {workspace_name: $ws_name}), (s:Scratchpad {id: $n_id}) CREATE (w)-[:HAS_SCRATCHPAD]->(s)";
        const prep2 = await conn.prepare(query2);
        await conn.execute(prep2, { ws_name: workspace, n_id: noteId });

        console.log(`Note added to workspace '${workspace}'.`);
      } catch (err: any) {
        console.error(`Error appending note: ${err.message || String(err)}`);
        process.exit(1);
      } finally {
        await conn.close();
        await db.close();
      }
    });

  await program.parseAsync(process.argv);
}

main();
