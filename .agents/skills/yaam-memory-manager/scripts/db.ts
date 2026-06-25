import ladybug from '@ladybugdb/core';
import * as path from 'path';

const DB_PATH = path.join(process.cwd(), 'memory.lbug');

export function getConn() {
  const db = new ladybug.Database(DB_PATH);
  const conn = new ladybug.Connection(db);
  return { db, conn };
}

export async function setupDatabase() {
  const { db, conn } = getConn();
  try {
    const safeExecute = async (query: string) => {
      try {
        await conn.query(query);
      } catch (e: any) {
        if (String(e).toLowerCase().includes("already exists")) {
          // Safe to ignore
        } else {
          console.error(`Error executing query: ${query}\n`, e);
        }
      }
    };

    // Layer 0: The Structural File System / AST Topology
    await safeExecute("CREATE NODE TABLE Entity(id STRING, type STRING, status STRING, last_modified INT64, metadata STRING, PRIMARY KEY (id))");
    await safeExecute("CREATE REL TABLE LINKED_TO(FROM Entity TO Entity, relationship_type STRING)");

    // Layer 1: The Agent Operational Context & Memory
    await safeExecute("CREATE NODE TABLE Workspace(workspace_name STRING, description STRING, status STRING, closed_at INT64, PRIMARY KEY (workspace_name))");
    await safeExecute("CREATE NODE TABLE Scratchpad(id STRING, content STRING, created_at INT64, PRIMARY KEY (id))");

    // Cross-Layer Mappings (Polymorphic Target Support)
    await safeExecute("CREATE REL TABLE MAPPED_TO(FROM Workspace TO Entity, created_at INT64, invalidated_at INT64, is_stale BOOLEAN)");
    await safeExecute("CREATE REL TABLE HAS_SCRATCHPAD(FROM Workspace TO Scratchpad)");
  } finally {
    await conn.close();
    await db.close();
  }
}
