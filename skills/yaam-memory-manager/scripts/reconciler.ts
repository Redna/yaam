import { getConn, setupDatabase } from './db.js';
import { Command } from 'commander';
import { execSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import * as ts from 'typescript';
import { fileURLToPath } from 'url';
import { LspClient } from './lsp_client.js';

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

interface EntityRange {
  id: string;
  startPos: number;
  endPos: number;
  pyRange?: {
    start: { line: number; character: number };
    end: { line: number; character: number };
  };
}

interface ParsedEntity {
  id: string;
  name: string;
  type: 'Class' | 'Function';
  startLine: number;
  endLine: number;
  startPos: number;
  endPos: number;
  superclasses?: ({ name: string; pos: number } | { name: string; line: number; col: number })[];
  pyRange?: {
    start: { line: number; character: number };
    end: { line: number; character: number };
  };
}

interface UnresolvedCall {
  callerId: string;
  pos: number;
  name: string;
  line: number;
  col: number;
}

interface LspLangConfig {
  languageId: string;
  command: string;
  args: string[];
  callRegex: RegExp;
  importRegexes: RegExp[];
  defKeywords?: string[];
  extractSuperclasses?: (line: string, startLineIdx: number) => { name: string; line: number; col: number }[];
  resolveImport?: (filePath: string, impPath: string) => string | null;
}

function loadSettings(): {
  frequency: string;
  languages: Record<string, { extensions: string[]; command: string; args: string[] }>;
} {
  const defaults = {
    frequency: 'incremental',
    languages: {
      python: {
        extensions: ['.py'],
        command: 'npx',
        args: ['--package=pyright', 'pyright-langserver', '--stdio']
      }
    }
  };

  let merged = { ...defaults };

  const paths = [
    path.join(process.env.HOME || '', '.pi', 'agent', 'settings.json'),
    path.join(process.cwd(), '.pi', 'settings.json')
  ];

  for (const settingsPath of paths) {
    if (fs.existsSync(settingsPath)) {
      try {
        const raw = fs.readFileSync(settingsPath, 'utf-8');
        const parsed = JSON.parse(raw);
        if (parsed.yaam) {
          merged = {
            ...merged,
            ...parsed.yaam,
            languages: {
              ...(merged.languages || {}),
              ...(parsed.yaam.languages || {})
            }
          };
        }
      } catch (e) {}
    }
  }

  if (process.env.YAAM_SETTINGS) {
    try {
      const parsed = JSON.parse(process.env.YAAM_SETTINGS);
      if (parsed) {
        merged = {
          ...merged,
          ...parsed,
          languages: {
            ...(merged.languages || {}),
            ...(parsed.languages || {})
          }
        };
      }
    } catch (e) {}
  }

  return merged;
}

function getLspLanguages(): Record<string, LspLangConfig> {
  const settings = loadSettings();
  const registry: Record<string, LspLangConfig> = {};

  const defaultTemplates: Record<string, Partial<LspLangConfig>> = {
    'python': {
      callRegex: /\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\b\s*\(/g,
      importRegexes: [
        /^\s*import\s+([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)/,
        /^\s*from\s+([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s+import/
      ],
      defKeywords: ['def', 'class'],
      extractSuperclasses: (line: string, startLineIdx: number) => {
        const classMatch = line.match(/class\s+(\w+)\s*\(([^)]+)\)/);
        if (!classMatch) return [];
        const superclassesStr = classMatch[2];
        const baseOffset = line.indexOf(superclassesStr);
        const parts = superclassesStr.split(',');
        let currentOffset = baseOffset;
        const superclasses: any[] = [];
        for (const part of parts) {
          const trimmed = part.trim();
          if (trimmed) {
            const partIndex = part.indexOf(trimmed);
            const startChar = currentOffset + partIndex;
            superclasses.push({
              name: trimmed,
              line: startLineIdx,
              col: startChar
            });
          }
          currentOffset += part.length + 1;
        }
        return superclasses;
      },
      resolveImport: (filePath: string, impPath: string) => {
        const relativeResolved = path.join(path.dirname(filePath), impPath.replace(/\./g, '/'));
        const absoluteResolved = path.join(process.cwd(), impPath.replace(/\./g, '/'));
        const candidatePaths = [
          relativeResolved + '.py',
          path.join(relativeResolved, '__init__.py'),
          absoluteResolved + '.py',
          path.join(absoluteResolved, '__init__.py')
        ];
        for (const candidate of candidatePaths) {
          if (fs.existsSync(candidate)) {
            return path.relative(process.cwd(), candidate);
          }
        }
        return null;
      }
    }
  };

  if (settings.languages) {
    for (const [langName, langConfig] of Object.entries(settings.languages)) {
      const template = defaultTemplates[langName] || {
        callRegex: /\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\b\s*\(/g,
        importRegexes: [],
      };

      for (const ext of langConfig.extensions) {
        registry[ext] = {
          languageId: langName,
          command: langConfig.command,
          args: langConfig.args,
          callRegex: template.callRegex!,
          importRegexes: template.importRegexes || [],
          defKeywords: template.defKeywords,
          extractSuperclasses: template.extractSuperclasses,
          resolveImport: template.resolveImport
        };
      }
    }
  }

  return registry;
}

const LSP_LANGUAGES = getLspLanguages();

function isPositionInRange(
  pos: { line: number; character: number },
  range: { start: { line: number; character: number }; end: { line: number; character: number } }
): boolean {
  if (pos.line < range.start.line || pos.line > range.end.line) return false;
  if (pos.line === range.start.line && pos.character < range.start.character) return false;
  if (pos.line === range.end.line && pos.character > range.end.character) return false;
  return true;
}

function findEnclosingFunction(
  pos: { line: number; character: number },
  functions: ParsedEntity[]
): ParsedEntity | null {
  let bestFunc: ParsedEntity | null = null;
  let smallestSpan = Infinity;
  for (const func of functions) {
    const range = func.pyRange;
    if (range && isPositionInRange(pos, range)) {
      const span = range.end.line - range.start.line;
      if (span < smallestSpan) {
        smallestSpan = span;
        bestFunc = func;
      }
    }
  }
  return bestFunc;
}

function traverseSymbols(symbols: any[], filePath: string, parentPath: string[] = []): ParsedEntity[] {
  const result: ParsedEntity[] = [];
  for (const sym of symbols) {
    if (sym.kind === 5) { // Class
      const currentPath = [...parentPath, sym.name];
      const classId = `${filePath}::${currentPath.join('::')}`;
      result.push({
        id: classId,
        name: sym.name,
        type: 'Class',
        startLine: sym.range.start.line + 1,
        endLine: sym.range.end.line + 1,
        startPos: 0,
        endPos: 0,
        pyRange: sym.range
      });
      if (sym.children) {
        result.push(...traverseSymbols(sym.children, filePath, currentPath));
      }
    } else if (sym.kind === 6 || sym.kind === 12) { // Method or Function
      const currentPath = [...parentPath, sym.name];
      const funcId = `${filePath}::${currentPath.join('::')}`;
      result.push({
        id: funcId,
        name: sym.name,
        type: 'Function',
        startLine: sym.range.start.line + 1,
        endLine: sym.range.end.line + 1,
        startPos: 0,
        endPos: 0,
        pyRange: sym.range
      });
      if (sym.children) {
        result.push(...traverseSymbols(sym.children, filePath, currentPath));
      }
    } else {
      if (sym.children) {
        result.push(...traverseSymbols(sym.children, filePath, parentPath));
      }
    }
  }
  return result;
}

async function extractLspEntities(
  filePath: string,
  lspClient: LspClient,
  config: LspLangConfig
): Promise<{
  classes: ParsedEntity[];
  functions: ParsedEntity[];
  imports: string[];
  unresolvedCalls: UnresolvedCall[];
  ranges: EntityRange[];
}> {
  const content = fs.readFileSync(filePath, 'utf-8');
  const uri = `file://${path.resolve(filePath)}`;
  
  const symbols = await lspClient.sendRequest('textDocument/documentSymbol', {
    textDocument: { uri }
  });

  const classes: ParsedEntity[] = [];
  const functions: ParsedEntity[] = [];
  const imports: string[] = [];
  const unresolvedCalls: UnresolvedCall[] = [];
  const ranges: EntityRange[] = [];

  if (symbols && Array.isArray(symbols)) {
    const parsed = traverseSymbols(symbols, filePath);
    for (const ent of parsed) {
      if (ent.type === 'Class') {
        classes.push(ent);
      } else {
        functions.push(ent);
      }
      ranges.push({
        id: ent.id,
        startPos: 0,
        endPos: 0,
        pyRange: ent.pyRange
      });
    }
  }

  const lines = content.split('\n');
  if (config.extractSuperclasses) {
    for (const cls of classes) {
      const range = cls.pyRange;
      if (range) {
        const startLineIdx = range.start.line;
        if (startLineIdx < lines.length) {
          const line = lines[startLineIdx];
          cls.superclasses = config.extractSuperclasses(line, startLineIdx);
        }
      }
    }
  }

  for (const line of lines) {
    for (const regex of config.importRegexes) {
      const match = line.match(regex);
      if (match) {
        imports.push(match[1]);
        break;
      }
    }
  }

  for (let lineIdx = 0; lineIdx < lines.length; lineIdx++) {
    const line = lines[lineIdx];
    const skipRegex = config.defKeywords ? new RegExp(`^\\s*(${config.defKeywords.join('|')})\\b`) : null;
    if (skipRegex && skipRegex.test(line)) {
      continue;
    }
    const regex = new RegExp(config.callRegex.source, config.callRegex.flags);
    let match;
    while ((match = regex.exec(line)) !== null) {
      const callName = match[1];
      const matchIndex = match.index;
      
      let targetCol = matchIndex;
      const lastDot = callName.lastIndexOf('.');
      if (lastDot !== -1) {
        targetCol += lastDot + 1;
      }
      
      const pos = { line: lineIdx, character: targetCol };
      const enclosingFunc = findEnclosingFunction(pos, functions);
      if (enclosingFunc) {
        unresolvedCalls.push({
          callerId: enclosingFunc.id,
          pos: 0,
          name: callName,
          line: lineIdx,
          col: targetCol
        });
      }
    }
  }

  return { classes, functions, imports, unresolvedCalls, ranges };
}

function extractTSEntities(filePath: string, sourceFile: ts.SourceFile) {
  const classes: ParsedEntity[] = [];
  const functions: ParsedEntity[] = [];
  const imports: string[] = [];
  const unresolvedCalls: UnresolvedCall[] = [];
  const ranges: EntityRange[] = [];

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
      const startPos = node.getStart(sourceFile);
      const endPos = node.getEnd();
      const { line: startLine } = getLineAndChar(startPos);
      const { line: endLine } = getLineAndChar(endPos);
      
      const superclasses: { name: string; pos: number }[] = [];
      if (node.heritageClauses) {
        for (const clause of node.heritageClauses) {
          for (const typeNode of clause.types) {
            superclasses.push({
              name: typeNode.expression.getText(sourceFile),
              pos: typeNode.expression.getStart(sourceFile)
            });
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
        startPos,
        endPos,
        superclasses
      });
      ranges.push({ id: classId, startPos, endPos });

      const oldClass = currentClass;
      currentClass = className;
      ts.forEachChild(node, visit);
      currentClass = oldClass;
      return;
    } else if (ts.isMethodDeclaration(node) && node.name && currentClass) {
      const methodName = node.name.getText(sourceFile);
      const startPos = node.getStart(sourceFile);
      const endPos = node.getEnd();
      const { line: startLine } = getLineAndChar(startPos);
      const { line: endLine } = getLineAndChar(endPos);
      const methodId = `${filePath}::${currentClass}::${methodName}`;

      functions.push({
        id: methodId,
        name: methodName,
        type: 'Function',
        startLine,
        endLine,
        startPos,
        endPos
      });
      ranges.push({ id: methodId, startPos, endPos });

      const oldFunc = currentFunction;
      currentFunction = methodId;
      ts.forEachChild(node, visit);
      currentFunction = oldFunc;
      return;
    } else if (ts.isFunctionDeclaration(node) && node.name) {
      const funcName = node.name.text;
      const startPos = node.getStart(sourceFile);
      const endPos = node.getEnd();
      const { line: startLine } = getLineAndChar(startPos);
      const { line: endLine } = getLineAndChar(endPos);
      const funcId = `${filePath}::${funcName}`;

      functions.push({
        id: funcId,
        name: funcName,
        type: 'Function',
        startLine,
        endLine,
        startPos,
        endPos
      });
      ranges.push({ id: funcId, startPos, endPos });

      const oldFunc = currentFunction;
      currentFunction = funcId;
      ts.forEachChild(node, visit);
      currentFunction = oldFunc;
      return;
    } else if (ts.isCallExpression(node) && currentFunction) {
      const expression = node.expression;
      const callName = expression.getText(sourceFile);
      const startPos = expression.getStart(sourceFile);
      const { line, col } = getLineAndChar(startPos);
      unresolvedCalls.push({
        callerId: currentFunction,
        pos: startPos,
        name: callName,
        line,
        col
      });
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);
  return { classes, functions, imports, unresolvedCalls, ranges };
}

async function cleanupStaleEntities(conn: any, filePath: string, parsedIds: Set<string>, timestamp: number) {
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
      await conn.execute(prep, { eid: entId, ts_val: timestamp });
    }
  }
}

async function processFileEntities(
  conn: any,
  filePath: string,
  entities: { classes: ParsedEntity[]; functions: ParsedEntity[]; imports: string[] },
  timestamp: number
) {
  const parsedIds = new Set<string>();

  // 1. Handle File Imports
  for (const impPath of entities.imports) {
    let targetPath: string | null = null;
    const ext = path.extname(filePath);
    const config = LSP_LANGUAGES[ext];
    if (config && config.resolveImport) {
      targetPath = config.resolveImport(filePath, impPath);
    } else {
      if (impPath.startsWith('.')) {
        const resolved = path.join(path.dirname(filePath), impPath);
        for (const ext of ['.ts', '.js']) {
          if (fs.existsSync(resolved + ext)) {
            targetPath = resolved + ext;
            break;
          }
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
    await conn.execute(prep1, { cid: c.id, ts_val: timestamp, meta_val: metadata });
    const prep2 = await conn.prepare("MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)");
    await conn.execute(prep2, { pid: filePath, cid: c.id });
  }

  // 3. Functions / Methods
  for (const f of entities.functions) {
    parsedIds.add(f.id);
    const metadata = JSON.stringify({ line: f.startLine });
    const prep1 = await conn.prepare("MERGE (func:Entity {id: $fid}) SET func.type = 'Function', func.status = 'active', func.last_modified = $ts_val, func.metadata = $meta_val");
    await conn.execute(prep1, { fid: f.id, ts_val: timestamp, meta_val: metadata });

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

  await cleanupStaleEntities(conn, filePath, parsedIds, timestamp);
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

  const timestamp = Math.floor(Date.now() / 1000);
  try {
    const prep1 = await conn.prepare("MERGE (e:Entity {id: $eid}) SET e.type = 'File'");
    await conn.execute(prep1, { eid: relPath });
    const prep2 = await conn.prepare("MATCH (w:Workspace {workspace_name: $ws}), (e:Entity {id: $eid}) MERGE (w)-[:MAPPED_TO {created_at: $ts_val, is_stale: false}]->(e)");
    await conn.execute(prep2, { ws: wsName, eid: relPath, ts_val: timestamp });

    const prepClasses = await conn.prepare("MATCH (c:Entity {type: 'Class'})-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(f:Entity {id: $fid}) RETURN c.id");
    const resClasses = await conn.execute(prepClasses, { fid: relPath });
    const classRows = await resClasses.getAll();
    for (const row of classRows) {
      const cid = row['c.id'];
      const prepMap = await conn.prepare("MATCH (w:Workspace {workspace_name: $ws}), (e:Entity {id: $eid}) MERGE (w)-[:MAPPED_TO {created_at: $ts_val, is_stale: false}]->(e)");
      await conn.execute(prepMap, { ws: wsName, eid: cid, ts_val: timestamp });
    }
  } catch (e) {
    console.error("Error mapping workspace to file:", e);
  }
}

async function reconcile(full = false) {
  const { db, conn } = getConn();
  const nowTs = Math.floor(Date.now() / 1000);
  const activeLspClients = new Map<string, LspClient>();

  try {
    await setupDatabase();

    let filesToReconcile: string[] = [];
    let diskFiles: string[] = [];
    if (full) {
      diskFiles = getAllFiles();
      filesToReconcile = diskFiles.filter(p => p.endsWith('.ts') || p.endsWith('.js') || Object.keys(LSP_LANGUAGES).some(ext => p.endsWith(ext)));
    } else {
      const statusMap = getGitStatus();
      filesToReconcile = statusMap
        .filter(item => item.status !== 'D' && (item.path.endsWith('.ts') || item.path.endsWith('.js') || Object.keys(LSP_LANGUAGES).some(ext => item.path.endsWith(ext))))
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

    // Initialize TypeScript Language Service (Programmatic LSP)
    const allDiskFiles = getAllFiles();
    const tsFiles = allDiskFiles.filter(p => p.endsWith('.ts') || p.endsWith('.js'));

    const filesMap = new Map<string, { version: number, content: string }>();
    for (const file of tsFiles) {
      try {
        filesMap.set(path.resolve(file), { version: 0, content: fs.readFileSync(file, 'utf-8') });
      } catch (e) {}
    }

    const servicesHost: ts.LanguageServiceHost = {
      getScriptFileNames: () => Array.from(filesMap.keys()),
      getScriptVersion: (fileName) => {
        const entry = filesMap.get(path.resolve(fileName));
        return entry ? String(entry.version) : "0";
      },
      getScriptSnapshot: (fileName) => {
        const norm = path.resolve(fileName);
        if (!filesMap.has(norm)) {
          if (fs.existsSync(norm)) {
            filesMap.set(norm, { version: 0, content: fs.readFileSync(norm, 'utf-8') });
          } else {
            return undefined;
          }
        }
        return ts.ScriptSnapshot.fromString(filesMap.get(norm)!.content);
      },
      getCurrentDirectory: () => process.cwd(),
      getCompilationSettings: () => ({
        target: ts.ScriptTarget.ES2022,
        module: ts.ModuleKind.ESNext,
        moduleResolution: ts.ModuleResolutionKind.Bundler,
        allowJs: true,
        checkJs: true,
      }),
      getDefaultLibFileName: (options) => ts.getDefaultLibFilePath(options),
      fileExists: ts.sys.fileExists,
      readFile: ts.sys.readFile,
      readDirectory: ts.sys.readDirectory,
      directoryExists: ts.sys.directoryExists,
      getDirectories: ts.sys.getDirectories,
    };

    const documentRegistry = ts.createDocumentRegistry();
    const service = ts.createLanguageService(servicesHost, documentRegistry);

    // Initialize active LSP clients for extensions present in reconciliation set
    for (const file of filesToReconcile) {
      const ext = path.extname(file);
      const config = LSP_LANGUAGES[ext];
      if (config && !activeLspClients.has(ext)) {
        console.log(`Launching LSP Server for ${ext} (${config.languageId})...`);
        const client = new LspClient(config.command, config.args, process.cwd());
        await client.initialize(process.cwd());
        activeLspClients.set(ext, client);
      }
    }

    // Send didOpen notification to appropriate LSP clients for all matching workspace files
    for (const [ext, client] of activeLspClients.entries()) {
      const matchingDiskFiles = allDiskFiles.filter(p => p.endsWith(ext));
      for (const diskFile of matchingDiskFiles) {
        try {
          const content = fs.readFileSync(diskFile, 'utf-8');
          client.sendNotification('textDocument/didOpen', {
            textDocument: {
              uri: `file://${path.resolve(diskFile)}`,
              languageId: LSP_LANGUAGES[ext].languageId,
              version: 1,
              text: content
            }
          });
        } catch (e) {}
      }
    }

    if (activeLspClients.size > 0) {
      await new Promise(resolve => setTimeout(resolve, 1000));
    }

    const parsedCache = new Map<string, {
      classes: ParsedEntity[];
      functions: ParsedEntity[];
      imports: string[];
      unresolvedCalls: UnresolvedCall[];
    }>();
    const entitiesInFiles = new Map<string, EntityRange[]>();

    for (const filePath of filesToReconcile) {
      try {
        const ext = path.extname(filePath);
        const config = LSP_LANGUAGES[ext];
        const lspClient = activeLspClients.get(ext);

        if (config && lspClient) {
          const result = await extractLspEntities(filePath, lspClient, config);
          parsedCache.set(filePath, {
            classes: result.classes,
            functions: result.functions,
            imports: result.imports,
            unresolvedCalls: result.unresolvedCalls,
          });
          entitiesInFiles.set(filePath, result.ranges);
        } else {
          const content = fs.readFileSync(filePath, 'utf-8');
          const sourceFile = ts.createSourceFile(filePath, content, ts.ScriptTarget.Latest, true);
          const result = extractTSEntities(filePath, sourceFile);

          parsedCache.set(filePath, {
            classes: result.classes,
            functions: result.functions,
            imports: result.imports,
            unresolvedCalls: result.unresolvedCalls,
          });
          entitiesInFiles.set(filePath, result.ranges);
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
        await conn.execute(prep, { path_val: filePath, ts_val: nowTs });

        if (parsedCache.has(filePath)) {
          const entities = parsedCache.get(filePath)!;
          try {
            await processFileEntities(conn, filePath, entities, nowTs);
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
          await conn.execute(prep, { chunk, ts_val: nowTs });
        }

        for (let i = 0; i < staleFiles.length; i += chunkSize) {
          const chunk = staleFiles.slice(i, i + chunkSize);
          const prep = await conn.prepare("MATCH (e:Entity {type: 'File'}) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val");
          await conn.execute(prep, { chunk, ts_val: nowTs });
        }
      }
    } else {
      const statusMap = getGitStatus();
      for (const item of statusMap) {
        const filePath = item.path;
        const entityStatus = item.status === 'D' ? 'deleted' : 'active';

        const prep = await conn.prepare("MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = $status_val, e.last_modified = $ts_val");
        await conn.execute(prep, { path_val: filePath, status_val: entityStatus, ts_val: nowTs });

        if (entityStatus === 'active' && parsedCache.has(filePath)) {
          const entities = parsedCache.get(filePath)!;
          try {
            await processFileEntities(conn, filePath, entities, nowTs);
          } catch (e) {
            console.error(`Error processing ${filePath}:`, e);
          }
        }

        if (entityStatus === 'deleted') {
          const prepInvalidate = await conn.prepare("MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity) WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix SET r.is_stale = true, r.invalidated_at = $ts_val");
          await conn.execute(prepInvalidate, { path_val: filePath, path_prefix: `${filePath}::`, ts_val: nowTs });
        }
      }
    }

    // Resolve calls and inheritance via Language Service (LSP)
    const resolvedCalls: { callerId: string; targetId: string }[] = [];
    const resolvedInherits: { subId: string; supId: string }[] = [];

    for (const [filePath, fileData] of parsedCache.entries()) {
      const absPath = path.resolve(filePath);
      const ext = path.extname(filePath);
      const lspClient = activeLspClients.get(ext);
      
      // 1. Resolve Calls
      for (const call of fileData.unresolvedCalls) {
        try {
          let defs: any = null;
          if (lspClient) {
            defs = await lspClient.sendRequest('textDocument/definition', {
              textDocument: { uri: `file://${absPath}` },
              position: { line: call.line, character: call.col }
            });
          } else {
            defs = service.getDefinitionAtPosition(absPath, call.pos);
          }

          if (defs) {
            const defLocations = Array.isArray(defs) ? defs : [defs];
            if (defLocations.length > 0) {
              const def = defLocations[0];
              if (lspClient) {
                if (def.uri.startsWith('file://')) {
                  const targetAbsPath = fileURLToPath(def.uri);
                  const relTarget = path.relative(process.cwd(), targetAbsPath);
                  if (!relTarget.startsWith('..') && !path.isAbsolute(relTarget)) {
                    const targets = entitiesInFiles.get(relTarget) || [];
                    let bestTarget: EntityRange | null = null;
                    let smallestSpan = Infinity;
                    const defStart = def.range.start;
                    for (const t of targets) {
                      const range = t.pyRange;
                      if (range && isPositionInRange(defStart, range)) {
                        const span = range.end.line - range.start.line;
                        if (span < smallestSpan) {
                          smallestSpan = span;
                          bestTarget = t;
                        }
                      }
                    }
                    if (bestTarget) {
                      resolvedCalls.push({
                        callerId: call.callerId,
                        targetId: bestTarget.id,
                      });
                    }
                  }
                }
              } else {
                const tsDef = def as ts.DefinitionInfo;
                const relTarget = path.relative(process.cwd(), tsDef.fileName);
                if (!relTarget.startsWith('..') && !path.isAbsolute(relTarget)) {
                  const targets = entitiesInFiles.get(relTarget) || [];
                  let bestTarget: EntityRange | null = null;
                  let smallestSpan = Infinity;
                  for (const t of targets) {
                    if (tsDef.textSpan.start >= t.startPos && tsDef.textSpan.start <= t.endPos) {
                      const span = t.endPos - t.startPos;
                      if (span < smallestSpan) {
                        smallestSpan = span;
                        bestTarget = t;
                      }
                    }
                  }
                  if (bestTarget) {
                    resolvedCalls.push({
                      callerId: call.callerId,
                      targetId: bestTarget.id,
                    });
                  }
                }
              }
            }
          }
        } catch (e) {}
      }

      // 2. Resolve Inheritance
      for (const cls of fileData.classes) {
        if (cls.superclasses) {
          for (const superclass of cls.superclasses) {
            try {
              let defs: any = null;
              if (lspClient) {
                const sClass = superclass as any;
                defs = await lspClient.sendRequest('textDocument/definition', {
                  textDocument: { uri: `file://${absPath}` },
                  position: { line: sClass.line, character: sClass.col }
                });
              } else {
                const sClass = superclass as { name: string; pos: number };
                defs = service.getDefinitionAtPosition(absPath, sClass.pos);
              }

              if (defs) {
                const defLocations = Array.isArray(defs) ? defs : [defs];
                if (defLocations.length > 0) {
                  const def = defLocations[0];
                  if (lspClient) {
                    if (def.uri.startsWith('file://')) {
                      const targetAbsPath = fileURLToPath(def.uri);
                      const relTarget = path.relative(process.cwd(), targetAbsPath);
                      if (!relTarget.startsWith('..') && !path.isAbsolute(relTarget)) {
                        const targets = entitiesInFiles.get(relTarget) || [];
                        let bestTarget: EntityRange | null = null;
                        let smallestSpan = Infinity;
                        const defStart = def.range.start;
                        for (const t of targets) {
                          const range = t.pyRange;
                          if (range && isPositionInRange(defStart, range)) {
                            const span = range.end.line - range.start.line;
                            if (span < smallestSpan) {
                              smallestSpan = span;
                              bestTarget = t;
                            }
                          }
                        }
                        if (bestTarget) {
                          resolvedInherits.push({
                            subId: cls.id,
                            supId: bestTarget.id,
                          });
                        }
                      }
                    }
                  } else {
                    const tsDef = def as ts.DefinitionInfo;
                    const relTarget = path.relative(process.cwd(), tsDef.fileName);
                    if (!relTarget.startsWith('..') && !path.isAbsolute(relTarget)) {
                      const targets = entitiesInFiles.get(relTarget) || [];
                      let bestTarget: EntityRange | null = null;
                      let smallestSpan = Infinity;
                      for (const t of targets) {
                        if (tsDef.textSpan.start >= t.startPos && tsDef.textSpan.start <= t.endPos) {
                          const span = t.endPos - t.startPos;
                          if (span < smallestSpan) {
                            smallestSpan = span;
                            bestTarget = t;
                          }
                        }
                      }
                      if (bestTarget) {
                        resolvedInherits.push({
                          subId: cls.id,
                          supId: bestTarget.id,
                        });
                      }
                    }
                  }
                }
              }
            } catch (e) {}
          }
        }
      }
    }

    // Write Resolved Calls to Database
    console.log(`Writing ${resolvedCalls.length} resolved call relationships...`);
    for (const call of resolvedCalls) {
      try {
        const prep1 = await conn.prepare("MERGE (target:Entity {id: $target_id}) SET target.type = 'Function'");
        await conn.execute(prep1, { target_id: call.targetId });
        const prep2 = await conn.prepare("MATCH (caller:Entity {id: $caller_id}), (callee:Entity {id: $callee_id}) MERGE (caller)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee)");
        await conn.execute(prep2, { caller_id: call.callerId, callee_id: call.targetId });
      } catch (e) {
        console.error(`Error saving call link: ${call.callerId} -> ${call.targetId}`, e);
      }
    }

    // Write Resolved Inheritance to Database
    console.log(`Writing ${resolvedInherits.length} resolved inherits relationships...`);
    for (const link of resolvedInherits) {
      try {
        const prepInherit = await conn.prepare("MATCH (sub:Entity {id: $sub_id}), (sup:Entity {id: $sup_id}) MERGE (sub)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup)");
        await conn.execute(prepInherit, { sub_id: link.subId, sup_id: link.supId });
      } catch (e) {
        console.error(`Error saving inherits link: ${link.subId} -> ${link.supId}`, e);
      }
    }

  } finally {
    for (const client of activeLspClients.values()) {
      try {
        client.stop();
      } catch (e) {}
    }
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
