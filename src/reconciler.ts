/**
 * reconciler.ts — in-process codebase reconciler with background scheduling.
 *
 * Architecture:
 *   Phase 1 (parseCodebase): Parse files, extract entities, resolve calls/inheritance.
 *     No DB connection needed — runs in background without holding any lock.
 *   Phase 2 (commit): Open DB (with backoff), write all results, close DB.
 *     Brief lock hold — only during writes.
 *
 * The reconciler runs in the background (fire-and-forget). Multiple tool_result
 * events don't cause overlapping reconciles — pending requests coalesce.
 * Multiple agents can share the DB since locks are held briefly.
 */

import * as fs from 'fs';
import * as path from 'path';
import * as ts from 'typescript';
import { fileURLToPath } from 'url';
import { execFile } from 'child_process';
import { promisify } from 'util';

const execFileAsync = promisify(execFile);

/** Yield to the event loop so other tasks (developer tools, I/O) can run. */
function yieldToEventLoop(): Promise<void> {
  return new Promise(resolve => setImmediate(resolve));
}
import { LspClient } from '../skills/yaam-memory-manager/scripts/lsp_client.js';
import type { ConnectionManager } from './db.js';

// ─── Types ──────────────────────────────────────────────────────────────────

interface EntityRange {
  id: string;
  startPos: number;
  endPos: number;
  pyRange?: { start: { line: number; character: number }; end: { line: number; character: number } };
}

interface RichMeta {
  signature?: string;
  isAsync?: boolean;
  isExported?: boolean;
  isStatic?: boolean;
  isAbstract?: boolean;
  params?: { name: string; type?: string; default?: string }[];
  returnType?: string;
  docComment?: string;
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
  pyRange?: { start: { line: number; character: number }; end: { line: number; character: number } };
  richMeta?: RichMeta;
}

interface UnresolvedCall {
  callerId: string;
  pos: number;
  name: string;
  line: number;
  col: number;
}

interface ParsedFile {
  classes: ParsedEntity[];
  functions: ParsedEntity[];
  imports: string[];
  unresolvedCalls: UnresolvedCall[];
  ranges: EntityRange[];
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

/** Result of the parse phase — all data needed for the commit phase. */
interface ReconcileResult {
  isFull: boolean;
  diskFiles: string[];
  gitStatus: { status: string; path: string }[];
  parsedCache: Map<string, { classes: ParsedEntity[]; functions: ParsedEntity[]; imports: string[]; unresolvedCalls: UnresolvedCall[] }>;
  entitiesInFiles: Map<string, EntityRange[]>;
  resolvedCalls: { callerId: string; targetId: string }[];
  resolvedInherits: { subId: string; supId: string }[];
  fileAccess?: { toolName: string; toolInput: any };
}

/** Progress report from the reconciler for UI display. */
export interface ReconcileProgress {
  phase: 'scanning' | 'parsing' | 'resolving' | 'committing' | 'done';
  detail: string;
  current: number;
  total: number;
}

// ─── Settings ───────────────────────────────────────────────────────────────

interface YaamSettings {
  frequency: string;
  languages: Record<string, { extensions: string[]; command: string; args: string[] }>;
}

function loadSettings(projectRoot: string): YaamSettings {
  const defaults: YaamSettings = {
    frequency: 'incremental',
    languages: {
      python: {
        extensions: ['.py'],
        command: 'npx',
        args: ['--package=pyright', 'pyright-langserver', '--stdio'],
      },
    },
  };

  let merged = { ...defaults };

  const paths = [
    path.join(process.env.HOME || '', '.pi', 'agent', 'settings.json'),
    path.join(projectRoot, '.pi', 'settings.json'),
    path.join(projectRoot, '.gemini', 'settings.json'),
    path.join(projectRoot, '.agents', 'settings.json'),
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
            languages: { ...(merged.languages || {}), ...(parsed.yaam.languages || {}) },
          };
        }
      } catch {}
    }
  }

  if (process.env.YAAM_SETTINGS) {
    try {
      const parsed = JSON.parse(process.env.YAAM_SETTINGS);
      if (parsed) {
        merged = {
          ...merged,
          ...parsed,
          languages: { ...(merged.languages || {}), ...(parsed.languages || {}) },
        };
      }
    } catch {}
  }

  return merged;
}

function getLspLanguages(projectRoot: string): Record<string, LspLangConfig> {
  const settings = loadSettings(projectRoot);
  const registry: Record<string, LspLangConfig> = {};

  const defaultTemplates: Record<string, Partial<LspLangConfig>> = {
    python: {
      callRegex: /\b([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\b\s*\(/g,
      importRegexes: [
        /^\s*import\s+([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)/,
        /^\s*from\s+([a-zA-Z_]\w*(?:\.[a-zA-Z_]\w*)*)\s+import/,
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
            superclasses.push({ name: trimmed, line: startLineIdx, col: startChar });
          }
          currentOffset += part.length + 1;
        }
        return superclasses;
      },
      resolveImport: (filePath: string, impPath: string) => {
        const relativeResolved = path.join(path.dirname(filePath), impPath.replace(/\./g, '/'));
        const absoluteResolved = path.join(projectRoot, impPath.replace(/\./g, '/'));
        const candidatePaths = [
          relativeResolved + '.py',
          path.join(relativeResolved, '__init__.py'),
          absoluteResolved + '.py',
          path.join(absoluteResolved, '__init__.py'),
        ];
        for (const candidate of candidatePaths) {
          if (fs.existsSync(candidate)) return path.relative(projectRoot, candidate);
        }
        return null;
      },
    },
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
          resolveImport: template.resolveImport,
        };
      }
    }
  }

  return registry;
}

// ─── File Discovery ──────────────────────────────────────────────────────────

async function getGitStatus(): Promise<{ status: string; path: string }[]> {
  try {
    const { stdout } = await execFileAsync('git', ['status', '--porcelain'], { encoding: 'utf-8' });
    return stdout
      .split('\n')
      .filter((line: string) => line.trim())
      .map((line: string) => ({
        status: line.substring(0, 2).trim(),
        path: line.substring(3).trim(),
      }));
  } catch {
    return [];
  }
}

const IGNORED_DIRS = new Set([
  '.git', '.venv', '__pycache__', '.pytest_cache', '.claude', '.gemini',
  'node_modules', 'llm_logs', 'xray_data', 'reports', 'docs', '.agents', '.pi',
  'dist', 'build', 'out', '.next', 'coverage', 'lib'
]);

function getAllFiles(baseDir: string): string[] {
  const files: string[] = [];
  function traverse(currentDir: string) {
    const list = fs.readdirSync(currentDir);
    for (const item of list) {
      const itemPath = path.join(currentDir, item);
      const stat = fs.statSync(itemPath);
      if (stat.isDirectory()) {
        if (!IGNORED_DIRS.has(item)) traverse(itemPath);
      } else {
        files.push(path.relative(baseDir, itemPath));
      }
    }
  }
  traverse(baseDir);
  return files;
}

// ─── LSP Position Helpers ───────────────────────────────────────────────────

function isPositionInRange(
  pos: { line: number; character: number },
  range: { start: { line: number; character: number }; end: { line: number; character: number } }
): boolean {
  if (pos.line < range.start.line || pos.line > range.end.line) return false;
  if (pos.line === range.start.line && pos.character < range.start.character) return false;
  if (pos.line === range.end.line && pos.character > range.end.character) return false;
  return true;
}

function findEnclosingFunction(pos: { line: number; character: number }, functions: ParsedEntity[]): ParsedEntity | null {
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
    const richMeta: RichMeta | undefined = sym.detail ? { signature: sym.detail } : undefined;
    if (sym.kind === 5) {
      const currentPath = [...parentPath, sym.name];
      const classId = `${filePath}::${currentPath.join('::')}`;
      result.push({
        id: classId, name: sym.name, type: 'Class',
        startLine: sym.range.start.line + 1, endLine: sym.range.end.line + 1,
        startPos: 0, endPos: 0, pyRange: sym.range,
        richMeta,
      });
      if (sym.children) result.push(...traverseSymbols(sym.children, filePath, currentPath));
    } else if (sym.kind === 6 || sym.kind === 12) {
      const currentPath = [...parentPath, sym.name];
      const funcId = `${filePath}::${currentPath.join('::')}`;
      result.push({
        id: funcId, name: sym.name, type: 'Function',
        startLine: sym.range.start.line + 1, endLine: sym.range.end.line + 1,
        startPos: 0, endPos: 0, pyRange: sym.range,
        richMeta,
      });
      if (sym.children) result.push(...traverseSymbols(sym.children, filePath, currentPath));
    } else {
      if (sym.children) result.push(...traverseSymbols(sym.children, filePath, parentPath));
    }
  }
  return result;
}

// ─── Entity Extraction ──────────────────────────────────────────────────────

async function extractLspEntities(
  filePath: string,
  lspClient: LspClient,
  config: LspLangConfig
): Promise<ParsedFile> {
  const content = fs.readFileSync(filePath, 'utf-8');
  const uri = `file://${path.resolve(filePath)}`;

  const symbols = await lspClient.sendRequest('textDocument/documentSymbol', {
    textDocument: { uri },
  });

  const classes: ParsedEntity[] = [];
  const functions: ParsedEntity[] = [];
  const imports: string[] = [];
  const unresolvedCalls: UnresolvedCall[] = [];
  const ranges: EntityRange[] = [];

  if (symbols && Array.isArray(symbols)) {
    const parsed = traverseSymbols(symbols, filePath);
    for (const ent of parsed) {
      if (ent.type === 'Class') classes.push(ent);
      else functions.push(ent);
      ranges.push({ id: ent.id, startPos: 0, endPos: 0, pyRange: ent.pyRange });
    }
  }

  const lines = content.split('\n');

  if (config.extractSuperclasses) {
    for (const cls of classes) {
      const range = cls.pyRange;
      if (range) {
        const startLineIdx = range.start.line;
        if (startLineIdx < lines.length) {
          cls.superclasses = config.extractSuperclasses(lines[startLineIdx], startLineIdx);
        }
      }
    }
  }

  // Extract rich metadata from source lines for Python entities
  for (const ent of [...classes, ...functions]) {
    const range = ent.pyRange;
    if (!range) continue;
    const defLineIdx = range.start.line;
    if (defLineIdx >= lines.length) continue;
    const defLine = lines[defLineIdx];

    if (!ent.richMeta) ent.richMeta = {};

    // Detect async
    if (/^\s*async\s+def\b/.test(defLine)) {
      ent.richMeta.isAsync = true;
    }

    // Detect decorators (check lines above the def)
    for (let i = defLineIdx - 1; i >= 0; i--) {
      const line = lines[i].trim();
      if (line.startsWith('@')) {
        if (line.includes('staticmethod')) ent.richMeta.isStatic = true;
        if (line.includes('classmethod')) ent.richMeta.isStatic = false;
        // Keep checking above for more decorators
      } else if (line === '' || line.startsWith('#')) {
        continue; // Skip blank lines and comments
      } else {
        break; // Hit non-decorator code
      }
    }

    // Extract docstring (first string literal after def)
    for (let i = defLineIdx + 1; i < Math.min(defLineIdx + 10, lines.length); i++) {
      const line = lines[i].trim();
      if (line === '' || line.startsWith('#')) continue;
      // Triple-quoted docstring
      const tripleMatch = line.match(/^['"]{3}([\s\S]*?)['"]{3}$/);
      if (tripleMatch) {
        ent.richMeta.docComment = tripleMatch[1].trim() || undefined;
        break;
      }
      // Start of multi-line triple-quoted string
      const startMatch = line.match(/^['"]{3}(.*)$/);
      if (startMatch) {
        const parts: string[] = [];
        if (startMatch[1].trim()) parts.push(startMatch[1].trim());
        for (let j = i + 1; j < Math.min(i + 50, lines.length); j++) {
          const endMatch = lines[j].match(/^(.*?)['"]{3}$/);
          if (endMatch) {
            if (endMatch[1].trim()) parts.push(endMatch[1].trim());
            break;
          }
          parts.push(lines[j].trim());
        }
        ent.richMeta.docComment = parts.join('\n').trim() || undefined;
        break;
      }
      // Not a string — stop looking
      break;
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
    if (skipRegex && skipRegex.test(line)) continue;
    const regex = new RegExp(config.callRegex.source, config.callRegex.flags);
    let match;
    while ((match = regex.exec(line)) !== null) {
      const callName = match[1];
      const matchIndex = match.index;
      let targetCol = matchIndex;
      const lastDot = callName.lastIndexOf('.');
      if (lastDot !== -1) targetCol += lastDot + 1;
      const pos = { line: lineIdx, character: targetCol };
      const enclosingFunc = findEnclosingFunction(pos, functions);
      if (enclosingFunc) {
        unresolvedCalls.push({
          callerId: enclosingFunc.id, pos: 0, name: callName, line: lineIdx, col: targetCol,
        });
      }
    }
  }

  return { classes, functions, imports, unresolvedCalls, ranges };
}

// ─── Rich Metadata Extraction (TS) ──────────────────────────────────────────

/** Extract JSDoc comment text from a TS node. */
function extractTsDocComment(node: ts.Node): string | undefined {
  const docs = ts.getJSDocCommentsAndTags(node);
  const parts: string[] = [];
  for (const doc of docs) {
    if (ts.isJSDoc(doc)) {
      if (typeof doc.comment === 'string') {
        parts.push(doc.comment);
      } else if (Array.isArray(doc.comment)) {
        for (const part of doc.comment) {
          parts.push(part.text);
        }
      }
    }
  }
  return parts.length > 0 ? parts.join('\n').trim() : undefined;
}

/** Extract rich metadata from a TS function/method/constructor declaration. */
function extractTsFunctionMeta(
  node: ts.FunctionDeclaration | ts.MethodDeclaration | ts.ConstructorDeclaration,
  sourceFile: ts.SourceFile
): RichMeta {
  const modifiers = ts.canHaveModifiers(node) ? ts.getModifiers(node) : undefined;
  const isAsync = modifiers?.some(m => m.kind === ts.SyntaxKind.AsyncKeyword) ?? false;
  const isStatic = modifiers?.some(m => m.kind === ts.SyntaxKind.StaticKeyword) ?? false;
  const isAbstract = modifiers?.some(m => m.kind === ts.SyntaxKind.AbstractKeyword) ?? false;
  const isExported = modifiers?.some(m => m.kind === ts.SyntaxKind.ExportKeyword) ?? false;

  // Parameters
  const params = node.parameters.map(p => ({
    name: p.name.getText(sourceFile),
    type: p.type ? p.type.getText(sourceFile) : undefined,
    default: p.initializer ? p.initializer.getText(sourceFile) : undefined,
  }));

  // Return type
  const returnType = node.type ? node.type.getText(sourceFile) : undefined;

  // Signature string
  const name = node.name ? node.name.getText(sourceFile) : (ts.isConstructorDeclaration(node) ? 'constructor' : 'anonymous');
  const paramList = params.map(p => {
    let s = p.name;
    if (p.type) s += `: ${p.type}`;
    if (p.default) s += ` = ${p.default}`;
    return s;
  }).join(', ');
  const signature = `${isAsync ? 'async ' : ''}${name}(${paramList})${returnType ? `: ${returnType}` : ''}`;

  // Doc comment
  const docComment = extractTsDocComment(node);

  return { signature, isAsync, isExported, isStatic, isAbstract, params, returnType, docComment };
}

/** Extract rich metadata from a TS class declaration. */
function extractTsClassMeta(
  node: ts.ClassDeclaration,
  sourceFile: ts.SourceFile
): RichMeta {
  const modifiers = ts.getModifiers(node) ?? [];
  const isExported = modifiers.some(m => m.kind === ts.SyntaxKind.ExportKeyword);
  const isAbstract = modifiers.some(m => m.kind === ts.SyntaxKind.AbstractKeyword);

  const name = node.name?.text ?? 'anonymous';
  const heritage = node.heritageClauses?.map(h => h.getText(sourceFile)).join(' ') ?? '';
  const signature = `class ${name}${heritage ? ' ' + heritage : ''}`;

  const docComment = extractTsDocComment(node);

  return { signature, isExported, isAbstract, docComment };
}

/**
 * Extract TS/JS entities using the TypeScript Language Service's
 * getNavigationTree() — same pattern as Python's LSP documentSymbol.
 *
 * The Language Service provides type-aware symbol classification:
 *   "class" → Class, "function"/"method"/"constructor" → Function.
 *
 * Imports, call expressions, and superclass positions are still parsed
 * from the AST (not available in the navigation tree).
 */
function extractTsServiceEntities(
  filePath: string,
  service: ts.LanguageService
): ParsedFile {
  const absPath = path.resolve(filePath);
  const content = fs.readFileSync(filePath, 'utf-8');
  const sourceFile = ts.createSourceFile(filePath, content, ts.ScriptTarget.Latest, true);

  const classes: ParsedEntity[] = [];
  const functions: ParsedEntity[] = [];
  const imports: string[] = [];
  const unresolvedCalls: UnresolvedCall[] = [];
  const ranges: EntityRange[] = [];

  // ── Entity extraction via Navigation Tree ──────────────────────────────
  const navTree = service.getNavigationTree(absPath);

  function getLine(pos: number): number {
    return sourceFile.getLineAndCharacterOfPosition(pos).line + 1;
  }

  // ScriptElementKind values we care about
  const CLASS_KINDS = new Set(['class', 'local class']);
  const FUNCTION_KINDS = new Set(['function', 'local function', 'method', 'constructor']);

  function traverseNav(item: ts.NavigationTree, parentPath: string[]): void {
    const name = item.text;

    if (CLASS_KINDS.has(item.kind)) {
      const currentPath = [...parentPath, name];
      const classId = `${filePath}::${currentPath.join('::')}`;
      const span = item.spans[0];
      if (span) {
        const startPos = span.start;
        const endPos = span.start + span.length;
        classes.push({
          id: classId, name, type: 'Class',
          startLine: getLine(startPos), endLine: getLine(endPos),
          startPos, endPos,
        });
        ranges.push({ id: classId, startPos, endPos });
      }
      if (item.childItems) {
        for (const child of item.childItems) traverseNav(child, currentPath);
      }
      return;
    }

    if (FUNCTION_KINDS.has(item.kind)) {
      const currentPath = [...parentPath, name];
      const funcId = `${filePath}::${currentPath.join('::')}`;
      const span = item.spans[0];
      if (span) {
        const startPos = span.start;
        const endPos = span.start + span.length;
        functions.push({
          id: funcId, name, type: 'Function',
          startLine: getLine(startPos), endLine: getLine(endPos),
          startPos, endPos,
        });
        ranges.push({ id: funcId, startPos, endPos });
      }
      if (item.childItems) {
        for (const child of item.childItems) traverseNav(child, currentPath);
      }
      return;
    }

    // Other kinds (variables, properties, etc.) — just recurse
    if (item.childItems) {
      for (const child of item.childItems) traverseNav(child, parentPath);
    }
  }

  // navTree is the file-level module; its children are top-level declarations
  if (navTree?.childItems) {
    for (const item of navTree.childItems) traverseNav(item, []);
  }

  // ── Import + call + superclass extraction via AST ──────────────────────
  // These aren't in the navigation tree, so we still parse the source.
  function visit(node: ts.Node) {
    if (ts.isImportDeclaration(node) && node.moduleSpecifier) {
      imports.push(node.moduleSpecifier.getText(sourceFile).replace(/['"]/g, ''));

    } else if (ts.isClassDeclaration(node) && node.name) {
      // Attach superclass positions to the Class entity (for inheritance resolution).
      // Match by source position (not constructed ID) to correctly handle nested classes.
      const nodeStart = node.getStart(sourceFile);
      const cls = classes.find(c => c.startPos === nodeStart);
      if (cls) {
        if (node.heritageClauses) {
          const superclasses: { name: string; pos: number }[] = [];
          for (const clause of node.heritageClauses) {
            // Only track extends (class inheritance), not implements (interface conformance).
            // implements clauses point to interfaces which are not in the entity graph
            // and would produce incorrect/unresolvable INHERITS_FROM edges.
            if (clause.token === ts.SyntaxKind.ExtendsKeyword) {
              for (const typeNode of clause.types) {
                superclasses.push({
                  name: typeNode.expression.getText(sourceFile),
                  pos: typeNode.expression.getStart(sourceFile),
                });
              }
            }
          }
          if (superclasses.length > 0) cls.superclasses = superclasses;
        }
        // Rich metadata
        if (!cls.richMeta) {
          cls.richMeta = extractTsClassMeta(node, sourceFile);
        }
      }

    } else if (ts.isCallExpression(node)) {
      // Find the enclosing function by checking which range contains the call
      const callPos = node.expression.getStart(sourceFile);
      let bestFunc: ParsedEntity | null = null;
      let smallestSpan = Infinity;
      for (const func of functions) {
        if (callPos >= func.startPos && callPos <= func.endPos) {
          const span = func.endPos - func.startPos;
          if (span < smallestSpan) {
            smallestSpan = span;
            bestFunc = func;
          }
        }
      }
      if (bestFunc) {
        unresolvedCalls.push({
          callerId: bestFunc.id,
          pos: callPos,
          name: node.expression.getText(sourceFile),
          line: 0, col: 0, // TS resolution uses character offset, not line/col
        });
      }
    } else if (ts.isFunctionDeclaration(node) || ts.isMethodDeclaration(node) || ts.isConstructorDeclaration(node)) {
      // Extract rich metadata for this function/method/constructor
      const fn = node as ts.FunctionDeclaration | ts.MethodDeclaration | ts.ConstructorDeclaration;
      const nodeStart = fn.getStart(sourceFile);
      let bestMatch: ParsedEntity | null = null;
      let smallestSpan = Infinity;
      for (const func of functions) {
        if (nodeStart >= func.startPos && nodeStart <= func.endPos) {
          const span = func.endPos - func.startPos;
          if (span < smallestSpan) {
            smallestSpan = span;
            bestMatch = func;
          }
        }
      }
      if (bestMatch && !bestMatch.richMeta) {
        bestMatch.richMeta = extractTsFunctionMeta(fn, sourceFile);
      }
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);
  return { classes, functions, imports, unresolvedCalls, ranges };
}

// ─── Prepared statements (prepared once, executed many times) ─────────────

interface PreparedStatements {
  // Entity merges
  mergeFileEntity: any;
  mergeClassEntity: any;
  mergeFuncEntity: any;
  // DECLARED_IN edges
  mergeDeclaredInClass: any;
  mergeDeclaredInMethod: any;
  mergeDeclaredInFunc: any;
  // IMPORTS edge
  mergeImportEdge: any;
  // CALLS edge
  mergeCallTarget: any;
  mergeCallEdge: any;
  // INHERITS edge
  mergeInheritEdge: any;
  // Stale cleanup
  getEntitiesByPrefix: any;
  deleteEntityEdgesOut: any;
  deleteEntityEdgesIn: any;
  markEntityDeleted: any;
  // File entity
  mergeFileActive: any;
  // Query existing entities
  getActiveFuncs: any;
  getActiveClasses: any;
  getAllFiles: any;
}

async function prepareStatements(conn: any): Promise<PreparedStatements> {
  return {
    mergeFileEntity: await conn.prepare("MERGE (target:Entity {id: $target_id}) SET target.type = 'File'"),
    mergeClassEntity: await conn.prepare("MERGE (cls:Entity {id: $cid}) SET cls.type = 'Class', cls.status = 'active', cls.last_modified = $ts_val, cls.metadata = $meta_val"),
    mergeFuncEntity: await conn.prepare("MERGE (func:Entity {id: $fid}) SET func.type = 'Function', func.status = 'active', func.last_modified = $ts_val, func.metadata = $meta_val"),
    mergeDeclaredInClass: await conn.prepare("MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)"),
    mergeDeclaredInMethod: await conn.prepare("MATCH (cls:Entity {id: $cid}), (method:Entity {id: $mid}) MERGE (method)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(cls)"),
    mergeDeclaredInFunc: await conn.prepare("MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)"),
    mergeImportEdge: await conn.prepare("MATCH (src:Entity {id: $src_id}), (dst:Entity {id: $dst_id}) MERGE (src)-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst)"),
    mergeCallTarget: await conn.prepare("MERGE (target:Entity {id: $target_id}) SET target.type = 'Function'"),
    mergeCallEdge: await conn.prepare("MATCH (caller:Entity {id: $caller_id}), (callee:Entity {id: $callee_id}) MERGE (caller)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee)"),
    mergeInheritEdge: await conn.prepare("MATCH (sub:Entity {id: $sub_id}), (sup:Entity {id: $sup_id}) MERGE (sub)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup)"),
    getEntitiesByPrefix: await conn.prepare("MATCH (e:Entity) WHERE e.id STARTS WITH $prefix AND e.type IN ['Function', 'Class'] RETURN e.id"),
    deleteEntityEdgesOut: await conn.prepare("MATCH (e:Entity {id: $eid})-[r:LINKED_TO]->() DELETE r"),
    deleteEntityEdgesIn: await conn.prepare("MATCH (e:Entity {id: $eid})<-[r:LINKED_TO]-() DELETE r"),
    markEntityDeleted: await conn.prepare("MATCH (e:Entity {id: $eid}) SET e.status = 'deleted', e.last_modified = $ts_val"),
    mergeFileActive: await conn.prepare("MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = 'active', e.last_modified = $ts_val"),
    getActiveFuncs: await conn.prepare("MATCH (f:Entity {type: 'Function', status: 'active'}) RETURN f.id"),
    getActiveClasses: await conn.prepare("MATCH (c:Entity {type: 'Class', status: 'active'}) RETURN c.id"),
    getAllFiles: await conn.prepare("MATCH (e:Entity {type: 'File'}) RETURN e.id"),
  };
}

// ─── DB Write Helpers (used in commit phase) ─────────────────────────────────

async function cleanupStaleEntities(conn: any, filePath: string, parsedIds: Set<string>, timestamp: number, stmts: PreparedStatements) {
  const prefix = `${filePath}::`;
  let dbEntities: string[] = [];
  try {
    const res = await conn.execute(stmts.getEntitiesByPrefix, { prefix });
    const rows = await res.getAll();
    dbEntities = rows.map((r: any) => r['e.id']);
  } catch {}

  for (const entId of dbEntities) {
    if (!parsedIds.has(entId)) {
      // Delete structural edges (CALLS, DECLARED_IN, IMPORTS, INHERITS_FROM)
      try {
        await conn.execute(stmts.deleteEntityEdgesOut, { eid: entId });
        await conn.execute(stmts.deleteEntityEdgesIn, { eid: entId });
      } catch {}
      await conn.execute(stmts.markEntityDeleted, { eid: entId, ts_val: timestamp });
    }
  }
}

async function processFileEntities(
  conn: any,
  filePath: string,
  entities: { classes: ParsedEntity[]; functions: ParsedEntity[]; imports: string[] },
  timestamp: number,
  stmts: PreparedStatements,
  lspLanguages: Record<string, LspLangConfig>,
  projectRoot: string
) {
  const parsedIds = new Set<string>();

  // 1. File imports
  for (const impPath of entities.imports) {
    let targetPath: string | null = null;
    const ext = path.extname(filePath);
    const config = lspLanguages[ext];
    if (config && config.resolveImport) {
      targetPath = config.resolveImport(filePath, impPath);
    } else {
      targetPath = resolveTsImport(filePath, impPath, projectRoot);
    }

    if (targetPath && targetPath !== filePath) {
      await conn.execute(stmts.mergeFileEntity, { target_id: targetPath });
      await conn.execute(stmts.mergeImportEdge, { src_id: filePath, dst_id: targetPath });
    }
  }

  // 2. Classes
  for (const c of entities.classes) {
    parsedIds.add(c.id);
    const metadata = JSON.stringify({
      line: c.startLine,
      endLine: c.endLine,
      ...c.richMeta,
    });
    await conn.execute(stmts.mergeClassEntity, { cid: c.id, ts_val: timestamp, meta_val: metadata });
    await conn.execute(stmts.mergeDeclaredInClass, { pid: filePath, cid: c.id });
  }

  // 3. Functions / Methods
  for (const f of entities.functions) {
    parsedIds.add(f.id);
    const metadata = JSON.stringify({
      line: f.startLine,
      endLine: f.endLine,
      ...f.richMeta,
    });
    await conn.execute(stmts.mergeFuncEntity, { fid: f.id, ts_val: timestamp, meta_val: metadata });

    const parts = f.id.split('::');
    if (parts.length > 2) {
      const classId = `${parts[0]}::${parts[1]}`;
      await conn.execute(stmts.mergeDeclaredInMethod, { cid: classId, mid: f.id });
    } else {
      await conn.execute(stmts.mergeDeclaredInFunc, { pid: filePath, fid: f.id });
    }
  }

  await cleanupStaleEntities(conn, filePath, parsedIds, timestamp, stmts);
}

// ─── TypeScript Module Resolution ───────────────────────────────────────────

const TS_COMPILER_OPTIONS: ts.CompilerOptions = {
  target: ts.ScriptTarget.ES2022,
  module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  allowJs: true,
};

const TS_MODULE_HOST: ts.ModuleResolutionHost = {
  fileExists: (fileName: string) => fs.existsSync(fileName),
  readFile: (fileName: string) => {
    try { return fs.readFileSync(fileName, 'utf-8'); } catch { return undefined; }
  },
  directoryExists: (dirName: string) => {
    try { return fs.statSync(dirName).isDirectory(); } catch { return false; }
  },
  realpath: (p: string) => {
    try { return fs.realpathSync(p); } catch { return p; }
  },
};

/**
 * Resolve a TS/JS import path to a relative file path using the TypeScript
 * Compiler API's module resolution. Handles .js → .ts mapping, index files,
 * and all bundler resolution rules.
 */
function resolveTsImport(filePath: string, importPath: string, projectRoot: string): string | null {
  if (!importPath.startsWith('.')) return null;

  const result = ts.resolveModuleName(
    importPath,
    path.resolve(filePath),
    TS_COMPILER_OPTIONS,
    TS_MODULE_HOST
  );

  if (result.resolvedModule) {
    const resolved = result.resolvedModule.resolvedFileName;
    const rel = path.relative(projectRoot, resolved);
    if (!rel.startsWith('..') && !path.isAbsolute(rel)) {
      return rel;
    }
  }
  return null;
}

// ─── Reconciler Class ───────────────────────────────────────────────────────

export class Reconciler {
  private activeLspClients = new Map<string, LspClient>();
  private running = false;
  private pendingMode: 'incremental' | 'full' | null = null;
  private pendingFileAccess: { toolName: string; toolInput: any } | null = null;
  private _progress: ReconcileProgress | null = null;
  /** Project root, captured from ConnectionManager on first schedule call. */
  private projectRoot: string = '';
  /** LSP language configs, initialized lazily with projectRoot. */
  private lspLanguages: Record<string, LspLangConfig> = {};

  constructor() {
    process.on('exit', () => this.shutdownLspClients());
  }

  get isRunning(): boolean {
    return this.running;
  }

  get progress(): ReconcileProgress | null {
    return this._progress;
  }

  private setProgress(
    phase: ReconcileProgress['phase'],
    detail: string,
    current: number,
    total: number,
  ): void {
    this._progress = { phase, detail, current, total };
  }

  shutdownLspClients(): void {
    for (const client of this.activeLspClients.values()) {
      try {
        // Send shutdown request first, then kill
        client.stop();
      } catch {}
    }
    this.activeLspClients.clear();
  }

  /**
   * Schedule an incremental reconcile. Returns immediately — the actual
   * work happens in the background. If a full reconcile is already pending,
   * it takes priority. Coalesces multiple incremental requests.
   */
  scheduleIncremental(connMgr: ConnectionManager, fileAccess?: { toolName: string; toolInput: any }): void {
    if (this.pendingMode === 'full') return; // full already pending
    this.pendingMode = 'incremental';
    if (fileAccess) this.pendingFileAccess = fileAccess;
    this.tryRun(connMgr);
  }

  /**
   * Schedule a full reconcile. Always upgrades — even if an incremental
   * is pending, we switch to full. Returns immediately.
   */
  scheduleFull(connMgr: ConnectionManager): void {
    this.pendingMode = 'full';
    this.tryRun(connMgr);
  }

  private tryRun(connMgr: ConnectionManager): void {
    if (this.running) return; // Will pick up pending when current run finishes
    this.running = true;

    // Capture project root from ConnectionManager (stable, captured at construction).
    // Initialize LSP languages lazily with the correct project root.
    if (!this.projectRoot) {
      this.projectRoot = connMgr.projectRoot;
      this.lspLanguages = getLspLanguages(this.projectRoot);
    }

    // Fire and forget — runs in background
    this.runReconcile(connMgr).catch((e) => {
      console.error("Reconciler error:", e);
    });
  }

  private async runReconcile(connMgr: ConnectionManager): Promise<void> {
    try {
      const mode = this.pendingMode;
      const fileAccess = this.pendingFileAccess;
      this.pendingMode = null;
      this.pendingFileAccess = null;
      if (!mode) return;

      this.setProgress('scanning', 'Scanning files', 0, 0);

      // Yield to event loop before heavy synchronous parse work.
      // This lets scheduleIncremental() return immediately to the caller.
      await new Promise((resolve) => setTimeout(resolve, 0));

      // ─── Phase 1: Parse (NO DB LOCK) ──────────────────────────────
      const dbFileTimestamps = new Map<string, number>();
      if (mode === 'full') {
        try {
          await connMgr.withConnection(async (conn) => {
            try {
              const res = await conn.query("MATCH (e:Entity {type: 'File'}) RETURN e.id, e.last_modified");
              const rows = await res.getAll();
              for (const row of rows) {
                dbFileTimestamps.set(row['e.id'], Number(row['e.last_modified']));
              }
            } catch (e) {
              console.error("Failed to load db file timestamps", e);
            }
          });
        } catch {}
      }

      const result = await this.parseCodebase(mode === 'full', fileAccess, dbFileTimestamps);

      // ─── Phase 2: Commit (BRIEF DB LOCK with backoff) ──────────────
      await connMgr.withConnection(async (conn) => {
        await this.commit(conn, result);
      });
    } finally {
      this.running = false;
      this._progress = null;
      if (this.pendingMode) {
        // A new request came in while we were running — handle it
        this.tryRun(connMgr);
      }
    }
  }

  // ─── Phase 1: Parse ─────────────────────────────────────────────────────

  private async parseCodebase(
    full: boolean,
    fileAccess: { toolName: string; toolInput: any } | null,
    dbFileTimestamps: Map<string, number>
  ): Promise<ReconcileResult> {
    let filesToReconcile: string[] = [];
    const diskFiles = getAllFiles(this.projectRoot);
    const gitStatus = await getGitStatus();

    if (full) {
      filesToReconcile = diskFiles.filter((p) => {
        if (!(p.endsWith('.ts') || p.endsWith('.js') || Object.keys(this.lspLanguages).some((ext) => p.endsWith(ext)))) {
          return false;
        }
        try {
          const stat = fs.statSync(p);
          const mtimeSec = Math.floor(stat.mtimeMs / 1000);
          const dbTime = dbFileTimestamps.get(p);
          if (dbTime && mtimeSec <= dbTime) {
            return false;
          }
          return true;
        } catch {
          return true;
        }
      });
    } else {
      filesToReconcile = gitStatus
        .filter(
          (item) =>
            item.status !== 'D' &&
            (item.path.endsWith('.ts') ||
              item.path.endsWith('.js') ||
              Object.keys(this.lspLanguages).some((ext) => item.path.endsWith(ext)))
        )
        .map((item) => item.path);
    }

    // Initialize TypeScript Language Service
    const tsFiles = diskFiles.filter((p) => p.endsWith('.ts') || p.endsWith('.js'));

    const filesMap = new Map<string, { version: number; content: string }>();

    const absTsFiles = tsFiles.map(f => path.resolve(f));

    const servicesHost: ts.LanguageServiceHost = {
      getScriptFileNames: () => absTsFiles,
      getScriptVersion: (fileName) => '0',
      getScriptSnapshot: (fileName) => {
        const norm = path.resolve(fileName);
        if (fs.existsSync(norm)) {
          return ts.ScriptSnapshot.fromString(fs.readFileSync(norm, 'utf-8'));
        }
        return undefined;
      },
      getCurrentDirectory: () => this.projectRoot,
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

    // Initialize LSP clients for files needing LSP analysis
    for (const file of filesToReconcile) {
      const ext = path.extname(file);
      const config = this.lspLanguages[ext];
      if (config && !this.activeLspClients.has(ext)) {
        const client = new LspClient(config.command, config.args, this.projectRoot);
        await client.initialize(this.projectRoot);
        this.activeLspClients.set(ext, client);
      }
    }

    // Send didOpen to LSP clients
    for (const [ext, client] of this.activeLspClients.entries()) {
      const matchingFiles = diskFiles.filter((p) => p.endsWith(ext));
      for (const diskFile of matchingFiles) {
        try {
          const content = fs.readFileSync(diskFile, 'utf-8');
          client.sendNotification('textDocument/didOpen', {
            textDocument: {
              uri: `file://${path.resolve(diskFile)}`,
              languageId: this.lspLanguages[ext].languageId,
              version: 1,
              text: content,
            },
          });
        } catch {}
      }
    }

    if (this.activeLspClients.size > 0) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }

    // Parse all files
    const parsedCache = new Map<string, { classes: ParsedEntity[]; functions: ParsedEntity[]; imports: string[]; unresolvedCalls: UnresolvedCall[] }>();
    const entitiesInFiles = new Map<string, EntityRange[]>();
    const totalToParse = filesToReconcile.length;
    let parsedIdx = 0;

    for (const filePath of filesToReconcile) {
      this.setProgress('parsing', 'Parsing files', parsedIdx, totalToParse);
      parsedIdx++;
      await yieldToEventLoop();
      try {
        const ext = path.extname(filePath);
        const config = this.lspLanguages[ext];
        const lspClient = this.activeLspClients.get(ext);

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
          // TS/JS: use the TypeScript Language Service for entity extraction
          const result = extractTsServiceEntities(filePath, service);
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

    // Resolve calls and inheritance (no DB needed)
    const resolvedCalls: { callerId: string; targetId: string }[] = [];
    const resolvedInherits: { subId: string; supId: string }[] = [];
    const resolveEntries = Array.from(parsedCache.entries());
    const totalResolve = resolveEntries.length;
    let resolveIdx = 0;

    for (const [filePath, fileData] of resolveEntries) {
      this.setProgress('resolving', 'Resolving topology', resolveIdx, totalResolve);
      resolveIdx++;
      await yieldToEventLoop();
      const absPath = path.resolve(filePath);
      const ext = path.extname(filePath);
      const lspClient = this.activeLspClients.get(ext);

      // Resolve calls
      for (const call of fileData.unresolvedCalls) {
        try {
          let defs: any = null;
          if (lspClient) {
            defs = await lspClient.sendRequest('textDocument/definition', {
              textDocument: { uri: `file://${absPath}` },
              position: { line: call.line, character: call.col },
            });
          } else {
            defs = service.getDefinitionAtPosition(absPath, call.pos);
          }

          if (defs) {
            const defLocations = Array.isArray(defs) ? defs : [defs];
            if (defLocations.length > 0) {
              const def = defLocations[0];
              if (lspClient) {
                if (def.uri?.startsWith('file://')) {
                  const targetAbsPath = fileURLToPath(def.uri);
                  const relTarget = path.relative(this.projectRoot, targetAbsPath);
                  if (!relTarget.startsWith('..') && !path.isAbsolute(relTarget)) {
                    const targets = entitiesInFiles.get(relTarget) || [];
                    let bestTarget: EntityRange | null = null;
                    let smallestSpan = Infinity;
                    for (const t of targets) {
                      const range = t.pyRange;
                      if (range && isPositionInRange(def.range.start, range)) {
                        const span = range.end.line - range.start.line;
                        if (span < smallestSpan) {
                          smallestSpan = span;
                          bestTarget = t;
                        }
                      }
                    }
                    if (bestTarget) resolvedCalls.push({ callerId: call.callerId, targetId: bestTarget.id });
                  }
                }
              } else {
                const tsDef = def as ts.DefinitionInfo;
                const relTarget = path.relative(this.projectRoot, tsDef.fileName);
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
                  if (bestTarget) resolvedCalls.push({ callerId: call.callerId, targetId: bestTarget.id });
                }
              }
            }
          }
        } catch {}
      }

      // Resolve inheritance
      for (const cls of fileData.classes) {
        if (cls.superclasses) {
          for (const superclass of cls.superclasses) {
            try {
              let defs: any = null;
              if (lspClient) {
                const sClass = superclass as any;
                defs = await lspClient.sendRequest('textDocument/definition', {
                  textDocument: { uri: `file://${absPath}` },
                  position: { line: sClass.line, character: sClass.col },
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
                    if (def.uri?.startsWith('file://')) {
                      const targetAbsPath = fileURLToPath(def.uri);
                      const relTarget = path.relative(this.projectRoot, targetAbsPath);
                      if (!relTarget.startsWith('..') && !path.isAbsolute(relTarget)) {
                        const targets = entitiesInFiles.get(relTarget) || [];
                        let bestTarget: EntityRange | null = null;
                        let smallestSpan = Infinity;
                        for (const t of targets) {
                          const range = t.pyRange;
                          if (range && isPositionInRange(def.range.start, range)) {
                            const span = range.end.line - range.start.line;
                            if (span < smallestSpan) {
                              smallestSpan = span;
                              bestTarget = t;
                            }
                          }
                        }
                        if (bestTarget) resolvedInherits.push({ subId: cls.id, supId: bestTarget.id });
                      }
                    }
                  } else {
                    const tsDef = def as ts.DefinitionInfo;
                    const relTarget = path.relative(this.projectRoot, tsDef.fileName);
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
                      if (bestTarget) resolvedInherits.push({ subId: cls.id, supId: bestTarget.id });
                    }
                  }
                }
              }
            } catch {}
          }
        }
      }
    }

    return {
      isFull: full,
      diskFiles,
      gitStatus,
      parsedCache,
      entitiesInFiles,
      resolvedCalls,
      resolvedInherits,
      fileAccess: fileAccess || undefined,
    };
  }

  // ─── Phase 2: Commit ─────────────────────────────────────────────────────

  private async commit(conn: any, result: ReconcileResult): Promise<void> {
    const nowTs = Math.floor(Date.now() / 1000);

    const commitTotal = result.isFull ? result.diskFiles.length : result.gitStatus.length;
    this.setProgress('committing', 'Writing entities', 0, commitTotal);

    // Prepare all statements once — reuse for all executions in this commit
    const stmts = await prepareStatements(conn);

    // Get existing entities for stale detection
    let existingFuncs: string[] = [];
    let existingClasses: string[] = [];
    try {
      const resFunc = await conn.execute(stmts.getActiveFuncs);
      existingFuncs = (await resFunc.getAll()).map((r: any) => r['f.id']);

      const resClass = await conn.execute(stmts.getActiveClasses);
      existingClasses = (await resClass.getAll()).map((r: any) => r['c.id']);
    } catch {}

    // Track file access from tool (if provided)
    if (result.fileAccess) {
      await this.trackFile(conn, result.fileAccess.toolName, result.fileAccess.toolInput, nowTs);
    }

    if (result.isFull) {
      // Write all disk files as File entities
      let commitIdx = 0;
      for (const filePath of result.diskFiles) {
        this.setProgress('committing', 'Writing entities', commitIdx, result.diskFiles.length);
        commitIdx++;
        await yieldToEventLoop();
        await conn.execute(stmts.mergeFileActive, { path_val: filePath, ts_val: nowTs });

        if (result.parsedCache.has(filePath)) {
          try {
            await processFileEntities(conn, filePath, result.parsedCache.get(filePath)!, nowTs, stmts, this.lspLanguages, this.projectRoot);
          } catch (e) {
            console.error(`Error processing ${filePath}:`, e);
          }
        }
        
        if (commitIdx % 100 === 0) {
          try { await conn.query("CHECKPOINT"); } catch {}
        }
      }

      // Soft-delete stale files
      let dbFiles: string[] = [];
      try {
        const res = await conn.execute(stmts.getAllFiles);
        dbFiles = (await res.getAll()).map((r: any) => r['e.id']);
      } catch {}

      const diskFilesSet = new Set(result.diskFiles);
      const staleFiles = dbFiles.filter((f) => !diskFilesSet.has(f));
      if (staleFiles.length > 0) {
        const staleFilesSet = new Set(staleFiles);
        const funcsToDelete = existingFuncs.filter((fid) => staleFilesSet.has(fid.split('::')[0]));
        const classesToDelete = existingClasses.filter((cid) => staleFilesSet.has(cid.split('::')[0]));
        const allToDelete = [...funcsToDelete, ...classesToDelete];

        const chunkSize = 500;
        for (let i = 0; i < allToDelete.length; i += chunkSize) {
          const chunk = allToDelete.slice(i, i + chunkSize);
          const prep = await conn.prepare("MATCH (e:Entity) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val");
          await conn.execute(prep, { chunk, ts_val: nowTs });
          // Delete outgoing structural edges for these entities
          try {
            const prepDelOut = await conn.prepare("MATCH (e:Entity) WHERE e.id IN $chunk MATCH (e)-[r:LINKED_TO]->() DELETE r");
            await conn.execute(prepDelOut, { chunk });
          } catch {}
          // Delete incoming structural edges
          try {
            const prepDelIn = await conn.prepare("MATCH (e:Entity) WHERE e.id IN $chunk MATCH ()-[r:LINKED_TO]->(e) DELETE r");
            await conn.execute(prepDelIn, { chunk });
          } catch {}
        }
        for (let i = 0; i < staleFiles.length; i += chunkSize) {
          const chunk = staleFiles.slice(i, i + chunkSize);
          const prep = await conn.prepare("MATCH (e:Entity {type: 'File'}) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val");
          await conn.execute(prep, { chunk, ts_val: nowTs });
          // Delete outgoing structural edges for stale files
          try {
            const prepDelOut = await conn.prepare("MATCH (e:Entity) WHERE e.id IN $chunk MATCH (e)-[r:LINKED_TO]->() DELETE r");
            await conn.execute(prepDelOut, { chunk });
          } catch {}
          // Delete incoming structural edges for stale files
          try {
            const prepDelIn = await conn.prepare("MATCH (e:Entity) WHERE e.id IN $chunk MATCH ()-[r:LINKED_TO]->(e) DELETE r");
            await conn.execute(prepDelIn, { chunk });
          } catch {}
          // Delete edges for child entities (functions/classes in stale files).
          // NOTE: We loop per-file instead of using ANY(f IN $chunk WHERE ...)
          // because the ANY() lambda pattern causes a native segfault in LadybugDB
          // when accessing outer-scope variables inside the lambda.
          const prepChildOut = await conn.prepare("MATCH (e:Entity) WHERE e.id STARTS WITH $prefix MATCH (e)-[r:LINKED_TO]->() DELETE r");
          const prepChildIn = await conn.prepare("MATCH (e:Entity) WHERE e.id STARTS WITH $prefix MATCH ()-[r:LINKED_TO]->(e) DELETE r");
          for (const filePath of chunk) {
            const prefix = `${filePath}::`;
            try { await conn.execute(prepChildOut, { prefix }); } catch {}
            try { await conn.execute(prepChildIn, { prefix }); } catch {}
          }
        }
      }
    } else {
      // Incremental: process git status changes
      let commitIdx = 0;
      for (const item of result.gitStatus) {
        this.setProgress('committing', 'Writing entities', commitIdx, result.gitStatus.length);
        commitIdx++;
        await yieldToEventLoop();
        const filePath = item.path;
        const entityStatus = item.status === 'D' ? 'deleted' : 'active';

        const prepFileStatus = await conn.prepare("MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = $status_val, e.last_modified = $ts_val");
        await conn.execute(prepFileStatus, { path_val: filePath, status_val: entityStatus, ts_val: nowTs });

        if (entityStatus === 'active' && result.parsedCache.has(filePath)) {
          try {
            await processFileEntities(conn, filePath, result.parsedCache.get(filePath)!, nowTs, stmts, this.lspLanguages, this.projectRoot);
          } catch (e) {
            console.error(`Error processing ${filePath}:`, e);
          }
        }
        
        if (commitIdx % 100 === 0) {
          try { await conn.query("CHECKPOINT"); } catch {}
        }

        if (entityStatus === 'deleted') {
          // Invalidate workspace mappings for this file
          const prepInvalidate = await conn.prepare("MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity) WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix SET r.is_stale = true, r.invalidated_at = $ts_val");
          await conn.execute(prepInvalidate, { path_val: filePath, path_prefix: `${filePath}::`, ts_val: nowTs });
          // Delete outgoing structural edges for deleted file and its child entities
          try {
            const prepDelOut = await conn.prepare("MATCH (e:Entity) WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix MATCH (e)-[r:LINKED_TO]->() DELETE r");
            await conn.execute(prepDelOut, { path_val: filePath, path_prefix: `${filePath}::` });
          } catch {}
          // Delete incoming structural edges
          try {
            const prepDelIn = await conn.prepare("MATCH (e:Entity) WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix MATCH ()-[r:LINKED_TO]->(e) DELETE r");
            await conn.execute(prepDelIn, { path_val: filePath, path_prefix: `${filePath}::` });
          } catch {}
          // Mark entities as deleted
          try {
            const prepDel2 = await conn.prepare("MATCH (e:Entity) WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix SET e.status = 'deleted', e.last_modified = $ts_val");
            await conn.execute(prepDel2, { path_val: filePath, path_prefix: `${filePath}::`, ts_val: nowTs });
          } catch {}
        }
      }
    }

    // Write resolved calls
    for (const call of result.resolvedCalls) {
      try {
        await conn.execute(stmts.mergeCallTarget, { target_id: call.targetId });
        await conn.execute(stmts.mergeCallEdge, { caller_id: call.callerId, callee_id: call.targetId });
      } catch (e) {
        console.error(`Error saving call link: ${call.callerId} -> ${call.targetId}`, e);
      }
    }

    // Write resolved inheritance
    for (const link of result.resolvedInherits) {
      try {
        await conn.execute(stmts.mergeInheritEdge, { sub_id: link.subId, sup_id: link.supId });
      } catch (e) {
        console.error(`Error saving inherits link: ${link.subId} -> ${link.supId}`, e);
      }
    }
  }

  /** Track a file accessed by a pi tool. Inline version for the commit phase. */
  private async trackFile(conn: any, toolName: string, toolInput: any, _timestamp: number): Promise<void> {
    let filePath = '';
    if (toolName === 'read' && toolInput.path) filePath = toolInput.path;
    else if (toolName === 'write' && toolInput.path) filePath = toolInput.path;
    else if (toolName === 'edit' && toolInput.path) filePath = toolInput.path;

    if (!filePath) return;

    const relPath = path.relative(this.projectRoot, path.resolve(filePath));
    if (relPath.startsWith('..')) return;

    let wsName: string | null = null;
    try {
      const prep = await conn.prepare("MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name");
      const res = await conn.execute(prep);
      const rows = await res.getAll();
      if (rows.length > 0) wsName = rows[0]['w.workspace_name'];
    } catch {
      return;
    }

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
}