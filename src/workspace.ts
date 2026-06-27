/**
 * workspace.ts — workspace initialization, note appending, and file tracking.
 * All functions receive a raw connection (used within ConnectionManager.withConnection).
 */

import * as path from 'path';

export async function initializeWorkspace(
  name: string,
  description: string,
  conn: any
): Promise<string> {
  // Deactivate all existing active workspaces
  await conn.query("MATCH (w:Workspace {status: 'active'}) SET w.status = 'inactive'");

  // Create the new workspace
  const query = "CREATE (:Workspace {workspace_name: $name, description: $description_val, status: 'active'})";
  const prepared = await conn.prepare(query);
  await conn.execute(prepared, { name, description_val: description });

  return `Workspace '${name}' initialized successfully.`;
}

export async function appendNote(
  workspace: string,
  content: string,
  conn: any
): Promise<string> {
  const noteId = `note_${Date.now()}`;
  const ts = Math.floor(Date.now() / 1000);

  const query1 = "CREATE (:Scratchpad {id: $id, content: $content, created_at: $ts})";
  const prep1 = await conn.prepare(query1);
  await conn.execute(prep1, { id: noteId, content, ts });

  const query2 = "MATCH (w:Workspace {workspace_name: $ws_name}), (s:Scratchpad {id: $n_id}) CREATE (w)-[:HAS_SCRATCHPAD]->(s)";
  const prep2 = await conn.prepare(query2);
  await conn.execute(prep2, { ws_name: workspace, n_id: noteId });

  return `Note added to workspace '${workspace}'.`;
}

/**
 * Track a file accessed by a pi tool to the active workspace.
 * Uses pi's actual tool names: read, write, edit.
 */
export async function trackAccessedFile(
  toolName: string,
  toolInput: any,
  conn: any
): Promise<void> {
  let filePath = '';

  if (toolName === 'read' && toolInput.path) {
    filePath = toolInput.path;
  } else if (toolName === 'write' && toolInput.path) {
    filePath = toolInput.path;
  } else if (toolName === 'edit' && toolInput.path) {
    filePath = toolInput.path;
  }

  if (!filePath) return;

  const relPath = path.relative(process.cwd(), path.resolve(filePath));
  if (relPath.startsWith('..')) return;

  // Find active workspace
  let wsName: string | null = null;
  try {
    const prep = await conn.prepare("MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name");
    const res = await conn.execute(prep);
    const rows = await res.getAll();
    if (rows.length > 0) {
      wsName = rows[0]['w.workspace_name'];
    }
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

    // Also map classes declared in this file
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