import { getConn, setupDatabase } from './db.js';
import { Command } from 'commander';
import { exec } from 'child_process';
import * as path from 'path';
import * as util from 'util';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const execPromise = util.promisify(exec);

async function runReconciler(full: boolean = false) {
  try {
    const tsReconcilerPath = path.join(__dirname, 'reconciler.ts');
    const cmd = `npx tsx "${tsReconcilerPath}"${full ? ' --full' : ''} < /dev/null`;
    await execPromise(cmd);
  } catch (e) {
    console.error('YAAM reconciler failed:', e);
  }
}

async function main() {
  const program = new Command();
  program
    .name('workspace_initialize')
    .description('Initialize a new task tracking workspace')
    .requiredOption('--name <name>', 'Workspace name')
    .requiredOption('--description <desc>', 'Workspace description')
    .action(async (options) => {
      const { name, description } = options;

      // 1. Run full reconciliation to ensure physical topology is complete
      await runReconciler(true);
      await setupDatabase();

      const { db, conn } = getConn();
      try {
        // 2. Set all other active workspaces to inactive
        await conn.query("MATCH (w:Workspace {status: 'active'}) SET w.status = 'inactive'");

        // 3. Create the new workspace
        const query = "CREATE (:Workspace {workspace_name: $name, description: $description_val, status: 'active'})";
        const prepared = await conn.prepare(query);
        await conn.execute(prepared, { name, description_val: description });

        console.log(`Workspace '${name}' initialized successfully.`);
      } catch (err: any) {
        console.error(`Error initializing workspace: ${err.message || String(err)}`);
        process.exit(1);
      } finally {
        await conn.close();
        await db.close();
      }
    });

  await program.parseAsync(process.argv);
}

main();
