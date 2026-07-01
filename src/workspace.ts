import * as path from 'path';
import * as fs from 'fs';
import { YaamEngineClient } from './engine-client.js';

export async function initializeWorkspace(
  name: string,
  description: string,
  client: YaamEngineClient
): Promise<string> {
  // Deactivate all existing active workspaces
  const dsl = {
    match: { label: "Workspace" },
    where: { field: "status", op: "eq", value: "active" }
  };
  const activeWorkspaces = await client.query(dsl);
  
  for (const ws of activeWorkspaces) {
    await client.upsertNode({
      id: ws.id,
      label: "Workspace",
      properties: { ...ws.properties, status: "inactive" }
    });
  }

  // Create the new workspace
  await client.upsertNode({
    id: name,
    label: "Workspace",
    properties: { description, status: "active" }
  });

  return `Workspace '${name}' initialized successfully.`;
}

export async function appendNote(
  workspace: string,
  content: string,
  client: YaamEngineClient
): Promise<string> {
  const noteId = `note_${Date.now()}`;
  const ts = Math.floor(Date.now() / 1000);

  await client.upsertNode({
    id: noteId,
    label: "Scratchpad",
    properties: { content, created_at: ts }
  });

  await client.linkNodes({
    from_id: workspace,
    to_id: noteId,
    relationship: "HAS_SCRATCHPAD",
    properties: {}
  });

  return `Note added to workspace '${workspace}'.`;
}

/**
 * Track a file accessed by a pi tool to the active workspace.
 * Uses pi's actual tool names: read, write, edit.
 */
export async function trackAccessedFile(
  toolName: string,
  toolInput: any,
  client: YaamEngineClient,
  projectRoot: string
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

  const resolvedPath = path.resolve(filePath);
  const relPath = path.relative(projectRoot, resolvedPath);
  if (relPath.startsWith('..')) return;

  // Find active workspace
  let wsName: string | null = null;
  try {
    const dsl = {
      match: { label: "Workspace" },
      where: { field: "status", op: "eq", value: "active" }
    };
    const active = await client.query(dsl);
    if (active.length > 0) {
      wsName = active[0].id;
    }
  } catch {
    return;
  }

  if (!wsName) return;

  // Reconcile the file content via AST if it exists
  try {
    if (fs.existsSync(resolvedPath)) {
      const content = fs.readFileSync(resolvedPath, 'utf-8');
      const res = await client.reconcile({ file_path: relPath, content });
      
      // Link the workspace to the reconciled entities
      const timestamp = Math.floor(Date.now() / 1000);
      for (const entityId of res.upserted_nodes) {
        await client.linkNodes({
          from_id: wsName,
          to_id: entityId,
          relationship: "MAPPED_TO",
          properties: { created_at: timestamp, is_stale: false }
        });
      }
    }
  } catch (e) {
    // Error mapping workspace to file
  }
}