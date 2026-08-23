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
    match: { label: "Workspace", status: "active" }
  };
  const activeWorkspaces = await client.query(dsl);
  
  for (const ws of activeWorkspaces) {
    await client.upsertNode({
      id: ws.id,
      label: "Workspace",
      properties: {
        description: ws.label?.description ?? "",
        status: "inactive",
        closed_at: ws.label?.closed_at ?? null
      }
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

  try {
    // 1. Link to files mentioned by path or @filename
    const fileEntities = await client.query({ match: { label: "Entity", entity_type: "File" }, return: ["id"] });
    for (const f of fileEntities) {
      if (!f.id) continue;
      const basename = path.basename(f.id);
      if (content.includes(f.id) || content.includes(`@${basename}`)) {
        await client.linkNodes({
          from_id: noteId,
          to_id: f.id,
          relationship: "ATTACHED_TO",
          properties: {}
        });
      }
    }

    // 2. Link to entities mentioned by #Entity or @Entity
    const entityRefs = new Set<string>();
    const entRegex = /[@#]([a-zA-Z_]\w+)/g;
    let match;
    while ((match = entRegex.exec(content)) !== null) {
      entityRefs.add(match[1]);
    }

    if (entityRefs.size > 0) {
      const allEntities = await client.query({ match: { label: "Entity" }, return: ["id", "name"] });
      for (const ent of allEntities) {
        if (ent.name && entityRefs.has(ent.name)) {
          await client.linkNodes({
            from_id: noteId,
            to_id: ent.id,
            relationship: "REFERENCES",
            properties: {}
          });
        }
      }
    }
  } catch (e) {
    // Fail silently so we don't break note saving if semantic linking fails
    console.error("[YAAM] Semantic linking failed:", e);
  }

  return `Note added to workspace '${workspace}'.`;
}

/** Minimal interface for checking reconciler status (avoids circular import). */
export interface ReconcilerStatus {
  isRunning: boolean;
}

export async function trackAccessedFile(
  toolName: string,
  toolInput: any,
  client: YaamEngineClient,
  projectRoot: string,
  reconciler?: ReconcilerStatus
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
      match: { label: "Workspace", status: "active" }
    };
    const active = await client.query(dsl);
    if (active.length > 0) {
      wsName = active[0].id;
    }
  } catch {
    return;
  }

  if (!wsName) return;

  if (reconciler) {
    let waitMs = 0;
    while (reconciler.isRunning && waitMs < 30000) {
      await new Promise(r => setTimeout(r, 200));
      waitMs += 200;
    }
  }

  try {
    if (fs.existsSync(resolvedPath)) {
      const entities = await client.query({
        match: { label: "Entity" },
        where: { edge_to: { id: relPath, relationship: "DECLARED_IN" } }
      });
      const timestamp = Math.floor(Date.now() / 1000);
      for (const entity of entities) {
        await client.linkNodes({
          from_id: wsName,
          to_id: entity.id,
          relationship: "MAPPED_TO",
          properties: { created_at: timestamp, is_stale: false }
        });
      }
    }
  } catch (e) {}
}
