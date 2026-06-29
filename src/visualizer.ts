/**
 * visualizer.ts — Terminal rendering of YAAM memory layers.
 *
 * Option A: Workspace-as-container view.
 * Layer 1 (cognitive context) forms the outer frame: the active workspace
 * with its description and chronological scratchpad notes. Layer 0 (physical
 * topology) appears inside as a tree of mapped files and their entities
 * (classes, functions, methods) with call annotations and metadata badges.
 * Mapped files are marked with *, stale mappings with !.
 *
 * Tree structure is built from entity ID hierarchy (split by ::), giving
 * accurate nesting for callbacks and closures without relying on
 * DECLARED_IN edges (which only connect to the immediate class, not
 * intermediate scope levels).
 *
 * Future Enhancement — Option C: Interactive split-pane TUI
 * An interactive explorer using blessed/ink with three panels:
 *   Left panel   — Layer 1: Workspace list (expandable to show notes &
 *                   file counts). Selecting a workspace highlights its
 *                   mapped entities in the right panel.
 *   Right panel  — Layer 0: Full code topology tree. Mapped entities
 *                   highlighted, unmapped dimmed. Supports expand/collapse
 *                   of subtrees via Enter key.
 *   Bottom panel — Entity detail: signature, params, return type,
 *                   docstring, call relationships (forward + reverse),
 *                   workspace mappings.
 * Keyboard navigation:
 *   - Arrow keys to traverse tree nodes
 *   - Enter to expand/collapse subtrees
 *   - Tab to switch between Layer 1 and Layer 0 panels
 *   - 'f' to filter by edge type (CALLS, IMPORTS, INHERITS_FROM)
 *   - 'd' to set call graph depth limit (1-5 hops)
 *   - 'w' to switch between workspaces
 *   - 't' to toggle timeline view (Option D) showing note → file mapping
 *                   history with stale markers on changed files
 *   - 's' to toggle stale-only filter (show only mappings with is_stale)
 * The interactive TUI would render in full-screen mode via blessed,
 * supporting colors, mouse events, and scroll regions. It would load the
 * full graph into memory for instant navigation without DB round-trips.
 * Each panel would be independently scrollable, with a status bar showing
 * the current selection, filter state, and graph statistics.
 */

const BOX_WIDTH = 72;
const NOTE_PREVIEW = 62;
const MAX_CALLS = 3;

// ─── Types ────────────────────────────────────────────────────────────────

interface NoteData {
  content: string;
  createdAt: number;
}

interface EntityData {
  id: string;
  type: string;
  metadata: any;
  calls: string[];
}

interface FileData {
  id: string;
  isStale: boolean;
  entities: EntityData[];
}

export interface WorkspaceViewData {
  name: string;
  description: string;
  notes: NoteData[];
  files: FileData[];
}

interface TreeNode {
  name: string;
  id: string;
  isEntity: boolean;
  type?: string;
  metadata?: any;
  calls: string[];
  children: TreeNode[];
}

// ─── Text Helpers ──────────────────────────────────────────────────────────

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  const h = d.getHours().toString().padStart(2, '0');
  const m = d.getMinutes().toString().padStart(2, '0');
  return `${h}:${m}`;
}

function truncate(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  if (maxLen < 3) return text.substring(0, maxLen);
  return text.substring(0, maxLen - 3) + '...';
}

function wrapText(text: string, width: number): string[] {
  if (!text) return [];
  const words = text.split(' ');
  const lines: string[] = [];
  let current = '';
  for (const word of words) {
    if (current.length + 1 + word.length > width) {
      if (current) lines.push(current);
      current = word;
    } else {
      current = current ? `${current} ${word}` : word;
    }
  }
  if (current) lines.push(current);
  return lines;
}

function shortName(entityId: string): string {
  const parts = entityId.split('::');
  return parts[parts.length - 1] || entityId;
}

/** Fit a string to an exact width: pad with spaces or truncate with '...'. */
function fitWidth(content: string, width: number): string {
  if (content.length > width) {
    if (width < 4) return content.substring(0, width);
    return content.substring(0, width - 3) + '...';
  }
  return content + ' '.repeat(width - content.length);
}

// ─── Data Gathering ────────────────────────────────────────────────────────

export async function gatherWorkspaceData(conn: any): Promise<WorkspaceViewData | null> {
  // 1. Get active workspace
  const wsResult = await conn.query(
    "MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name, w.description"
  );
  const wsRows = await wsResult.getAll();
  if (wsRows.length === 0) return null;

  const wsName = wsRows[0]['w.workspace_name'];
  const wsDesc = wsRows[0]['w.description'] || '(no description)';

  // 2. Get notes (chronological)
  let notes: NoteData[] = [];
  try {
    const notePrep = await conn.prepare(
      "MATCH (w:Workspace {workspace_name: $name})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content, s.created_at ORDER BY s.created_at"
    );
    const noteRes = await conn.execute(notePrep, { name: wsName });
    const noteRows = await noteRes.getAll();
    notes = noteRows.map((r: any) => ({
      content: r['s.content'],
      createdAt: r['s.created_at'],
    }));
  } catch {}

  // 3. Get mapped files with stale status
  let fileRows: any[] = [];
  try {
    const filePrep = await conn.prepare(
      "MATCH (w:Workspace {workspace_name: $name})-[r:MAPPED_TO]->(e:Entity {type: 'File'}) RETURN e.id, r.is_stale ORDER BY e.id"
    );
    const fileRes = await conn.execute(filePrep, { name: wsName });
    fileRows = await fileRes.getAll();
  } catch {}

  // 4. For each file, gather entities and calls
  const files: FileData[] = [];
  for (const row of fileRows) {
    const fileId = row['e.id'];
    const isStale = row['r.is_stale'] === true;
    const entities = await gatherFileEntities(conn, fileId);
    files.push({ id: fileId, isStale, entities });
  }

  return { name: wsName, description: wsDesc, notes, files };
}

async function gatherFileEntities(conn: any, fileId: string): Promise<EntityData[]> {
  // Get all entities (functions + classes) whose ID starts with fileId::
  let entRows: any[] = [];
  try {
    const entPrep = await conn.prepare(
      "MATCH (e:Entity) WHERE e.id STARTS WITH $prefix AND e.type IN ['Function', 'Class'] RETURN e.id, e.type, e.metadata ORDER BY e.id"
    );
    const entRes = await conn.execute(entPrep, { prefix: `${fileId}::` });
    entRows = await entRes.getAll();
  } catch {}

  // Get all CALLS edges from entities in this file (batch query)
  let callRows: any[] = [];
  try {
    const callPrep = await conn.prepare(
      "MATCH (caller:Entity)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee:Entity) WHERE caller.id STARTS WITH $prefix RETURN caller.id, callee.id"
    );
    const callRes = await conn.execute(callPrep, { prefix: `${fileId}::` });
    callRows = await callRes.getAll();
  } catch {}

  // Group calls by caller — store short names for display
  const callsMap = new Map<string, string[]>();
  for (const row of callRows) {
    const callerId = row['caller.id'];
    const calleeId = row['callee.id'];
    if (!callsMap.has(callerId)) callsMap.set(callerId, []);
    callsMap.get(callerId)!.push(shortName(calleeId));
  }

  // Build entity data
  return entRows.map((row: any) => {
    let metadata: any = undefined;
    try { metadata = JSON.parse(row['e.metadata']); } catch {}
    return {
      id: row['e.id'],
      type: row['e.type'],
      metadata,
      calls: callsMap.get(row['e.id']) || [],
    };
  });
}

// ─── Tree Building ─────────────────────────────────────────────────────────

/**
 * Build a hierarchical tree from entity IDs by splitting on '::'.
 * This preserves the full nesting structure including intermediate scope
 * nodes (anonymous callbacks, closures) that don't have their own entity
 * in the database.
 */
function buildTree(entities: EntityData[]): TreeNode[] {
  const root: TreeNode = {
    name: '', id: '', isEntity: false, calls: [], children: [],
  };

  for (const ent of entities) {
    const parts = ent.id.split('::');
    // parts[0] = file path, parts[1..] = entity hierarchy
    let current = root;
    let currentPath = parts[0];

    for (let i = 1; i < parts.length; i++) {
      currentPath = `${currentPath}::${parts[i]}`;
      let child = current.children.find(c => c.name === parts[i]);
      if (!child) {
        child = {
          name: parts[i],
          id: currentPath,
          isEntity: false,
          calls: [],
          children: [],
        };
        current.children.push(child);
      }
      current = child;
    }

    // Mark the leaf as a real entity with metadata
    current.isEntity = true;
    current.type = ent.type;
    current.metadata = ent.metadata;
    current.calls = ent.calls;
  }

  // Sort: classes first, then functions, then scope nodes — alpha within each
  function sortChildren(node: TreeNode) {
    node.children.sort((a, b) => {
      const aPri = a.isEntity ? (a.type === 'Class' ? 0 : 1) : 2;
      const bPri = b.isEntity ? (b.type === 'Class' ? 0 : 1) : 2;
      if (aPri !== bPri) return aPri - bPri;
      return a.name.localeCompare(b.name);
    });
    for (const child of node.children) sortChildren(child);
  }
  sortChildren(root);

  return root.children;
}

// ─── Rendering ─────────────────────────────────────────────────────────────

function boxLine(content: string): string {
  return `║${content}║`;
}

function renderEntityLine(node: TreeNode, indent: string, connector: string): string {
  let line = `${indent}${connector}${node.name}`;

  if (node.isEntity && node.type === 'Class') {
    line += ' (class)';
  }

  // Metadata badges
  if (node.isEntity && node.metadata) {
    const badges: string[] = [];
    if (node.metadata.isAsync) badges.push('async');
    if (node.metadata.isExported) badges.push('exported');
    if (node.metadata.isStatic) badges.push('static');
    if (node.metadata.isAbstract) badges.push('abstract');
    if (badges.length > 0) {
      line += `  ${badges.join(' ')}`;
    }
  }

  // Call annotation
  if (node.isEntity && node.calls.length > 0) {
    const shown = node.calls.slice(0, MAX_CALLS);
    const more = node.calls.length > MAX_CALLS
      ? `, +${node.calls.length - MAX_CALLS}`
      : '';
    line += `  calls: ${shown.join(', ')}${more}`;
  }

  return fitWidth(line, BOX_WIDTH);
}

function renderTree(nodes: TreeNode[], prefix: string, lines: string[]): void {
  for (let i = 0; i < nodes.length; i++) {
    const node = nodes[i];
    const isLast = i === nodes.length - 1;
    const connector = isLast ? '└── ' : '├── ';
    const childPrefix = prefix + (isLast ? '    ' : '│   ');

    lines.push(boxLine(renderEntityLine(node, prefix, connector)));

    if (node.children.length > 0) {
      renderTree(node.children, childPrefix, lines);
    }
  }
}

export function renderWorkspaceView(data: WorkspaceViewData): string {
  const W = BOX_WIDTH;
  const lines: string[] = [];

  // ── Top border ──────────────────────────────────────────────────────
  lines.push(`╔${'═'.repeat(W)}╗`);

  // ── Workspace header (name left, status right) ──────────────────────
  const headerLeft = ` Workspace: ${data.name}`;
  const headerRight = 'active';
  lines.push(boxLine(fitWidth(headerLeft, W - headerRight.length) + headerRight));

  // ── Description (word-wrapped) ──────────────────────────────────────
  for (const descLine of wrapText(data.description, W - 1)) {
    lines.push(boxLine(fitWidth(` ${descLine}`, W)));
  }

  // ── Separator ───────────────────────────────────────────────────────
  lines.push(`╟${'─'.repeat(W)}╢`);

  // ── Notes section ───────────────────────────────────────────────────
  lines.push(boxLine(fitWidth('', W)));
  const noteLabel = ` Scratchpad (${data.notes.length} ${data.notes.length === 1 ? 'note' : 'notes'}):`;
  lines.push(boxLine(fitWidth(noteLabel, W)));
  lines.push(boxLine(fitWidth('', W)));

  if (data.notes.length === 0) {
    lines.push(boxLine(fitWidth('  (none)', W)));
  } else {
    for (const note of data.notes) {
      const time = formatTime(note.createdAt);
      const preview = truncate(note.content.replace(/\n/g, ' '), NOTE_PREVIEW - 8);
      lines.push(boxLine(fitWidth(`  [${time}] ${preview}`, W)));
    }
  }

  lines.push(boxLine(fitWidth('', W)));

  // ── Separator ───────────────────────────────────────────────────────
  lines.push(`╟${'─'.repeat(W)}╢`);

  // ── Mapped entities section ─────────────────────────────────────────
  lines.push(boxLine(fitWidth('', W)));
  const fileLabel = ` Mapped Entities (${data.files.length} ${data.files.length === 1 ? 'file' : 'files'}):`;
  lines.push(boxLine(fitWidth(fileLabel, W)));
  lines.push(boxLine(fitWidth('', W)));

  if (data.files.length === 0) {
    lines.push(boxLine(fitWidth('  (none)', W)));
  } else {
    for (const file of data.files) {
      // File header line with markers
      const staleMarker = file.isStale ? ' !' : '';
      lines.push(boxLine(fitWidth(`  ${file.id} *${staleMarker}`, W)));

      // Entity tree
      if (file.entities.length > 0) {
        const tree = buildTree(file.entities);
        const treeLines: string[] = [];
        renderTree(tree, '  ', treeLines);
        lines.push(...treeLines);
      }

      lines.push(boxLine(fitWidth('', W)));
    }
  }

  // ── Bottom border ───────────────────────────────────────────────────
  lines.push(`╚${'═'.repeat(W)}╝`);

  // ── Legend (outside box) ────────────────────────────────────────────
  lines.push('');
  lines.push('  * = mapped to this workspace    ! stale = file changed since mapping');

  return lines.join('\n');
}