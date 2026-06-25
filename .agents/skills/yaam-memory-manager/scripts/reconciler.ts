import { getConn, setupDatabase } from './db.js';
import { Command } from 'commander';
import { execSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import * as ts from 'typescript';
import { fileURLToPath } from 'url';

function getGitStatus(): { status: string; path: string }[] {
  try {
    const stdout = execSync('git status --porcelain', { encoding: 'utf-8' });
    return stdout.split('\n')
      .filter(line => line.trim())
      .map(line => {
        const status = line.substring(0, 2).trim();
        const filepath = line.substring(3).trim();
        return { status, path: filepath };
      });
  } catch (e) {
    return [];
  }
}

function getAllFiles(dir = '.'): string[] {
  const ignoredDirs = new Set([
    '.git', '.venv', '__pycache__', '.pytest_cache', '.claude', '.gemini', 
    'node_modules', 'llm_logs', 'xray_data', 'reports', 'docs', '.agents', '.pi'
  ]);
  const files: string[] = [];
  function traverse(currentDir: string) {
    const list = fs.readdirSync(currentDir);
    for (const item of list) {
      const itemPath = path.join(currentDir, item);
      const stat = fs.statSync(itemPath);
      if (stat.isDirectory()) {
        if (!ignoredDirs.has(item)) {
          traverse(itemPath);
        }
      } else {
        files.push(path.relative('.', itemPath));
      }
    }
  }
  traverse(dir);
  return files;
}

interface ParsedEntity {
  id: string;
  name: string;
  type: 'Class' | 'Function';
  startLine: number;
  endLine: number;
  superclasses?: string[];
}

interface ParsedCall {
  callerId: string;
  name: string;
  line: number;
  col: number;
}

function extractTSEntities(filePath: string) {
  const fileContent = fs.readFileSync(filePath, 'utf-8');
  const sourceFile = ts.createSourceFile(filePath, fileContent, ts.ScriptTarget.Latest, true);

  const classes: ParsedEntity[] = [];
  const functions: ParsedEntity[] = [];
  const imports: string[] = [];
  const calls: ParsedCall[] = [];

  let currentClass: string | null = null;
  let currentFunction: string | null = null;

  function getLineAndChar(pos: number) {
    const { line, character } = sourceFile.getLineAndCharacterOfPosition(pos);
    return { line: line + 1, col: character };
  }

  function visit(node: ts.Node) {
    if (ts.isImportDeclaration(node)) {
      if (node.moduleSpecifier) {
        const moduleSpecifier = node.moduleSpecifier.getText(sourceFile).replace(/['"]/g, '');
        imports.push(moduleSpecifier);
      }
    } else if (ts.isClassDeclaration(node) && node.name) {
      const className = node.name.text;
      const { line: startLine } = getLineAndChar(node.getStart(sourceFile));
      const { line: endLine } = getLineAndChar(node.getEnd());
      
      const superclasses: string[] = [];
      if (node.heritageClauses) {
        for (const clause of node.heritageClauses) {
          for (const typeNode of clause.types) {
            superclasses.push(typeNode.expression.getText(sourceFile));
          }
        }
      }

      const classId = `${filePath}::${className}`;
      classes.push({
        id: classId,
        name: className,
        type: 'Class',
        startLine,
        endLine,
        superclasses
      });

      const oldClass = currentClass;
      currentClass = className;
      ts.forEachChild(node, visit);
      currentClass = oldClass;
      return;
    } else if (ts.isMethodDeclaration(node) && node.name && currentClass) {
      const methodName = node.name.getText(sourceFile);
      const { line: startLine } = getLineAndChar(node.getStart(sourceFile));
      const { line: endLine } = getLineAndChar(node.getEnd());
      const methodId = `${filePath}::${currentClass}::${methodName}`;

      functions.push({
        id: methodId,
        name: methodName,
        type: 'Function',
        startLine,
        endLine
      });

      const oldFunc = currentFunction;
      currentFunction = methodId;
      ts.forEachChild(node, visit);
      currentFunction = oldFunc;
      return;
    } else if (ts.isFunctionDeclaration(node) && node.name) {
      const funcName = node.name.text;
      const { line: startLine } = getLineAndChar(node.getStart(sourceFile));
      const { line: endLine } = getLineAndChar(node.getEnd());
      const funcId = `${filePath}::${funcName}`;

      functions.push({
        id: funcId,
        name: funcName,
        type: 'Function',
        startLine,
        endLine
      });

      const oldFunc = currentFunction;
      currentFunction = funcId;
      ts.forEachChild(node, visit);
      currentFunction = oldFunc;
      return;
    } else if (ts.isCallExpression(node) && currentFunction) {
      const expression = node.expression;
      const callName = expression.getText(sourceFile);
      const { line, col } = getLineAndChar(expression.getStart(sourceFile));
      calls.push({
        callerId: currentFunction,
        name: callName,
        line,
        col
      });
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);
  return { classes, functions, imports, calls };
}

async function cleanupStaleEntities(conn: any, filePath: string, parsedIds: Set<string>, ts: number) {
  const prefix = `${filePath}::`;
  let dbEntities: string[] = [];
  try {
    const prep = await conn.prepare("MATCH (e:Entity) WHERE e.id STARTS WITH $prefix AND e.type IN ['Function', 'Class'] RETURN e.id");
    const res = await conn.execute(prep, { prefix });
    const rows = await res.getAll();
    dbEntities = rows.map((r: any) => r['e.id']);
  } catch (e) {}

  for (const entId of dbEntities) {
    if (!parsedIds.has(entId)) {
      const prep = await conn.prepare("MATCH (e:Entity {id: $eid}) SET e.status = 'deleted', e.last_modified = $ts_val");
      await conn.execute(prep, { eid: entId, ts_val: ts });
    }
  }
}

async function processFileEntities(
  conn: any,
  filePath: string,
  entities: ReturnType<typeof extractTSEntities>,
  ts: number,
  funcLookup: Map<string, string[]>,
  classLookup: Map<string, string[]>
) {
  const parsedIds = new Set<string>();

  // 1. Handle File Imports
  for (const impPath of entities.imports) {
    let targetPath: string | null = null;
    if (impPath.startsWith('.')) {
      const resolved = path.join(path.dirname(filePath), impPath);
      for (const ext of ['.ts', '.js']) {
        if (fs.existsSync(resolved + ext)) {
          targetPath = resolved + ext;
          break;
        }
      }
    }

    if (targetPath && targetPath !== filePath) {
      const prep1 = await conn.prepare("MERGE (target:Entity {id: $target_id}) SET target.type = 'File'");
      await conn.execute(prep1, { target_id: targetPath });
      const prep2 = await conn.prepare("MATCH (src:Entity {id: $src_id}), (dst:Entity {id: $dst_id}) MERGE (src)-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst)");
      await conn.execute(prep2, { src_id: filePath, dst_id: targetPath });
    }
  }

  // 2. Class definitions
  for (const c of entities.classes) {
    parsedIds.add(c.id);
    const metadata = JSON.stringify({ line: c.startLine });
    const prep1 = await conn.prepare("MERGE (cls:Entity {id: $cid}) SET cls.type = 'Class', cls.status = 'active', cls.last_modified = $ts_val, cls.metadata = $meta_val");
    await conn.execute(prep1, { cid: c.id, ts_val: ts, meta_val: metadata });
    const prep2 = await conn.prepare("MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)");
    await conn.execute(prep2, { pid: filePath, cid: c.id });

    if (c.superclasses) {
      for (const baseClass of c.superclasses) {
        let superId = null;
        if (classLookup.has(baseClass)) {
          superId = classLookup.get(baseClass)![0];
        }
        if (superId) {
          const prepInherit = await conn.prepare("MATCH (sub:Entity {id: $sub_id}), (sup:Entity {id: $sup_id}) MERGE (sub)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup)");
          await conn.execute(prepInherit, { sub_id: c.id, sup_id: superId });
        }
      }
    }
  }

  // 3. Functions / Methods
  for (const f of entities.functions) {
    parsedIds.add(f.id);
    const metadata = JSON.stringify({ line: f.startLine });
    const prep1 = await conn.prepare("MERGE (func:Entity {id: $fid}) SET func.type = 'Function', func.status = 'active', func.last_modified = $ts_val, func.metadata = $meta_val");
    await conn.execute(prep1, { fid: f.id, ts_val: ts, meta_val: metadata });

    const parts = f.id.split('::');
    if (parts.length > 2) {
      const classId = `${parts[0]}::${parts[1]}`;
      const prep2 = await conn.prepare("MATCH (cls:Entity {id: $cid}), (method:Entity {id: $mid}) MERGE (method)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(cls)");
      await conn.execute(prep2, { cid: classId, mid: f.id });
    } else {
      const prep2 = await conn.prepare("MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)");
      await conn.execute(prep2, { pid: filePath, fid: f.id });
    }
  }

  // 4. Resolve Calls (Call Graph)
  for (const call of entities.calls) {
    let targetId: string | null = null;

    const parts = call.callerId.split('::');
    if (parts.length > 2) {
      const classId = `${parts[0]}::${parts[1]}`;
      if (call.name.startsWith('this.')) {
        const methodName = call.name.substring(5);
        targetId = `${classId}::${methodName}`;
      }
    }

    if (!targetId) {
      const lastName = call.name.split('.').pop() || '';
      if (funcLookup.has(lastName)) {
        targetId = funcLookup.get(lastName)![0];
      }
    }

    if (targetId) {
      const prep1 = await conn.prepare("MERGE (target:Entity {id: $target_id}) SET target.type = 'Function'");
      await conn.execute(prep1, { target_id: targetId });
      const prep2 = await conn.prepare("MATCH (caller:Entity {id: $caller_id}), (callee:Entity {id: $callee_id}) MERGE (caller)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee)");
      await conn.execute(prep2, { caller_id: call.callerId, callee_id: targetId });
    }
  }

  await cleanupStaleEntities(conn, filePath, parsedIds, ts);
}

async function trackAccessedFile(conn: any, toolName: string, toolInput: any) {
  let filePath = '';
  if (toolName === 'view_file' && toolInput.AbsolutePath) {
    filePath = toolInput.AbsolutePath;
  } else if (['replace_file_content', 'multi_replace_file_content', 'write_to_file'].includes(toolName) && toolInput.TargetFile) {
    filePath = toolInput.TargetFile;
  }

  if (!filePath) return;

  const relPath = path.relative(process.cwd(), filePath);
  if (relPath.startsWith('..')) return;

  let wsName = null;
  try {
    const prep = await conn.prepare("MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name");
    const res = await conn.execute(prep);
    const rows = await res.getAll();
    if (rows.length > 0) {
      wsName = rows[0]['w.workspace_name'];
    }
  } catch (e) {}

  if (!wsName) return;

  const ts = Math.floor(Date.now() / 1000);
  try {
    const prep1 = await conn.prepare("MERGE (e:Entity {id: $eid}) SET e.type = 'File'");
    await conn.execute(prep1, { eid: relPath });
    const prep2 = await conn.prepare("MATCH (w:Workspace {workspace_name: $ws}), (e:Entity {id: $eid}) MERGE (w)-[:MAPPED_TO {created_at: $ts_val, is_stale: false}]->(e)");
    await conn.execute(prep2, { ws: wsName, eid: relPath, ts_val: ts });

    const prepClasses = await conn.prepare("MATCH (c:Entity {type: 'Class'})-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(f:Entity {id: $fid}) RETURN c.id");
    const resClasses = await conn.execute(prepClasses, { fid: relPath });
    const classRows = await resClasses.getAll();
    for (const row of classRows) {
      const cid = row['c.id'];
      const prepMap = await conn.prepare("MATCH (w:Workspace {workspace_name: $ws}), (e:Entity {id: $eid}) MERGE (w)-[:MAPPED_TO {created_at: $ts_val, is_stale: false}]->(e)");
      await conn.execute(prepMap, { ws: wsName, eid: cid, ts_val: ts });
    }
  } catch (e) {
    console.error("Error mapping workspace to file:", e);
  }
}

async function reconcile(full = false) {
  const { db, conn } = getConn();
  const ts = Math.floor(Date.now() / 1000);

  try {
    await setupDatabase();

    let filesToReconcile: string[] = [];
    let diskFiles: string[] = [];
    if (full) {
      diskFiles = getAllFiles();
      filesToReconcile = diskFiles.filter(p => p.endsWith('.ts') || p.endsWith('.js'));
    } else {
      const statusMap = getGitStatus();
      filesToReconcile = statusMap
        .filter(item => item.status !== 'D' && (item.path.endsWith('.ts') || item.path.endsWith('.js')))
        .map(item => item.path);
    }

    let existingFuncs: string[] = [];
    let existingClasses: string[] = [];
    try {
      const prepFunc = await conn.prepare("MATCH (f:Entity {type: 'Function', status: 'active'}) RETURN f.id");
      const resFunc = await conn.execute(prepFunc);
      existingFuncs = (await (resFunc as any).getAll()).map((r: any) => r['f.id']);

      const prepClass = await conn.prepare("MATCH (c:Entity {type: 'Class', status: 'active'}) RETURN c.id");
      const resClass = await conn.execute(prepClass);
      existingClasses = (await (resClass as any).getAll()).map((r: any) => r['c.id']);
    } catch (e) {}

    const reconcilePrefixes = filesToReconcile.map(p => `${p}::`);
    const isReconciling = (id: string) => reconcilePrefixes.some(pref => id.startsWith(pref));

    const funcLookup = new Map<string, string[]>();
    for (const fid of existingFuncs) {
      if (isReconciling(fid)) continue;
      const shortName = fid.split('::').pop() || '';
      if (!funcLookup.has(shortName)) funcLookup.set(shortName, []);
      funcLookup.get(shortName)!.push(fid);
    }

    const classLookup = new Map<string, string[]>();
    for (const cid of existingClasses) {
      if (isReconciling(cid)) continue;
      const shortName = cid.split('::').pop() || '';
      if (!classLookup.has(shortName)) classLookup.set(shortName, []);
      classLookup.get(shortName)!.push(cid);
    }

    const parsedCache = new Map<string, ReturnType<typeof extractTSEntities>>();

    for (const filePath of filesToReconcile) {
      try {
        const entities = extractTSEntities(filePath);
        parsedCache.set(filePath, entities);

        for (const sym of entities.functions) {
          if (!funcLookup.has(sym.name)) funcLookup.set(sym.name, []);
          funcLookup.get(sym.name)!.push(sym.id);
        }
        for (const sym of entities.classes) {
          if (!classLookup.has(sym.name)) classLookup.set(sym.name, []);
          classLookup.get(sym.name)!.push(sym.id);
        }
      } catch (e) {
        console.error(`Error parsing ${filePath}:`, e);
      }
    }

    if (full) {
      console.log(`Processing ${diskFiles.length} files on disk...`);
      for (let i = 0; i < diskFiles.length; i++) {
        const filePath = diskFiles[i];
        const prep = await conn.prepare("MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = 'active', e.last_modified = $ts_val");
        await conn.execute(prep, { path_val: filePath, ts_val: ts });

        if (parsedCache.has(filePath)) {
          const entities = parsedCache.get(filePath)!;
          try {
            await processFileEntities(conn, filePath, entities, ts, funcLookup, classLookup);
          } catch (e) {
            console.error(`Error processing ${filePath}:`, e);
          }
        }
      }

      let dbFiles: string[] = [];
      try {
        const prep = await conn.prepare("MATCH (e:Entity {type: 'File'}) RETURN e.id");
        const res = await conn.execute(prep);
        dbFiles = (await (res as any).getAll()).map((r: any) => r['e.id']);
      } catch (e) {}

      const diskFilesSet = new Set(diskFiles);
      const staleFiles = dbFiles.filter(f => !diskFilesSet.has(f));
      if (staleFiles.length > 0) {
        console.log(`Soft-deleting ${staleFiles.length} stale files...`);
        const staleFilesSet = new Set(staleFiles);
        const funcsToDelete = existingFuncs.filter(fid => staleFilesSet.has(fid.split('::')[0]));
        const classesToDelete = existingClasses.filter(cid => staleFilesSet.has(cid.split('::')[0]));
        const allToDelete = [...funcsToDelete, ...classesToDelete];

        const chunkSize = 500;
        for (let i = 0; i < allToDelete.length; i += chunkSize) {
          const chunk = allToDelete.slice(i, i + chunkSize);
          const prep = await conn.prepare("MATCH (e:Entity) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val");
          await conn.execute(prep, { chunk, ts_val: ts });
        }

        for (let i = 0; i < staleFiles.length; i += chunkSize) {
          const chunk = staleFiles.slice(i, i + chunkSize);
          const prep = await conn.prepare("MATCH (e:Entity {type: 'File'}) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val");
          await conn.execute(prep, { chunk, ts_val: ts });
        }
      }
    } else {
      const statusMap = getGitStatus();
      for (const item of statusMap) {
        const filePath = item.path;
        const entityStatus = item.status === 'D' ? 'deleted' : 'active';

        const prep = await conn.prepare("MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = $status_val, e.last_modified = $ts_val");
        await conn.execute(prep, { path_val: filePath, status_val: entityStatus, ts_val: ts });

        if (entityStatus === 'active' && parsedCache.has(filePath)) {
          const entities = parsedCache.get(filePath)!;
          try {
            await processFileEntities(conn, filePath, entities, ts, funcLookup, classLookup);
          } catch (e) {
            console.error(`Error processing ${filePath}:`, e);
          }
        }

        if (entityStatus === 'deleted') {
          const prepInvalidate = await conn.prepare("MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity) WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix SET r.is_stale = true, r.invalidated_at = $ts_val");
          await conn.execute(prepInvalidate, { path_val: filePath, path_prefix: `${filePath}::`, ts_val: ts });
        }
      }
    }
  } finally {
    await conn.close();
    await db.close();
  }
}

async function main() {
  const program = new Command();
  program
    .name('reconciler')
    .description('Reconcile physical codebase files to LadybugDB')
    .option('--full', 'Perform full codebase scan')
    .action(async (options) => {
      console.log("Starting database reconciliation...");
      await reconcile(options.full);
      console.log("Reconciliation complete.");
    });

  if (!process.stdin.isTTY) {
    let inputData = '';
    process.stdin.on('data', chunk => {
      inputData += chunk;
    });
    process.stdin.on('end', async () => {
      try {
        if (inputData.trim()) {
          const payload = JSON.parse(inputData);
          const toolName = payload.tool_name || payload.tool;
          const toolInput = payload.tool_input || payload.args || {};
          
          await reconcile(false);
          if (toolName) {
            const { db, conn } = getConn();
            try {
              await trackAccessedFile(conn, toolName, toolInput);
            } finally {
              await conn.close();
              await db.close();
            }
          }
        }
      } catch (e) {
        console.error("Error running reconciler hook:", e);
      }
      console.log(JSON.stringify({ decision: 'allow' }));
      process.exit(0);
    });
  } else {
    await program.parseAsync(process.argv);
  }
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1]);
if (isMain) {
  main();
}
