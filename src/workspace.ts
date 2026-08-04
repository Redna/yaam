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

  return `Note added to workspace '${workspace}'.`;
}

/** Minimal interface for checking reconciler status (avoids circular import). */
export interface ReconcilerStatus {
  isRunning: boolean;
}

/**
 * Track a file accessed by a pi tool to the active workspace.
 * Uses pi's actual tool names: read, write, edit.
 *
 * This function is designed to be called fire-and-forget (not awaited by the
 * caller). It queries existing graph entities for the file and links them to
 * the active workspace via MAPPED_TO edges. It does NOT reconcile the file —
 * that is handled by `Reconciler.runSync()` via `scheduleIncremental()`, which
 * avoids a duplicate (and expensive) tree-sitter + LSP + embedding round-trip.
 */
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

  // Wait for any ongoing reconciliation to finish so entities are current.
  // This is non-blocking to the agent since trackAccessedFile is fire-and-forget.
  // Timeout after 30s to avoid infinite wait if reconciliation is stuck.
  if (reconciler) {
    let waitMs = 0;
    while (reconciler.isRunning && waitMs < 30000) {
      await new Promise(r => setTimeout(r, 200));
      waitMs += 200;
    }
  }

  // Query existing entities declared in this file and link them to the workspace.
  // We no longer call client.reconcile() here — that was duplicating the
  // reconciliation already performed by Reconciler.runSync() via
  // scheduleIncremental(). Entity IDs are deterministic (file_path:name), so
  // MAPPED_TO edges remain valid even after re-reconciliation recreates them.
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
  } catch (e) {
    // Error mapping workspace to file
  }
}