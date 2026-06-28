/**
 * graph_explore — read-only Cypher query with write protection.
 */

import * as path from 'path';
import * as fs from 'fs';

const FORBIDDEN_KEYWORDS = ["CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DROP", "ALTER"];

export function validateReadQuery(query: string): string {
  const cleanQuery = query.trim();

  const hasForbidden = FORBIDDEN_KEYWORDS.some(k =>
    new RegExp(`\\b${k}\\b`, "i").test(cleanQuery)
  );
  if (hasForbidden) {
    throw new Error("Write operations forbidden via this tool. Use workspace mutation tools to alter memory.");
  }

  let constrained = cleanQuery;
  if (!constrained.toUpperCase().includes("LIMIT")) {
    constrained = constrained.replace(/(RETURN\s+.+)/i, "$1 LIMIT 500");
    if (!constrained.toUpperCase().includes("LIMIT 500")) {
      constrained += " LIMIT 500";
    }
  }

  return constrained;
}

export interface ExploreResult {
  text: string;
  spooledTo?: string;
}

export async function exploreGraph(query: string, conn: any, baseDir: string): Promise<ExploreResult> {
  const constrained = validateReadQuery(query);

  const queryResult = await conn.query(constrained);
  const rows = await queryResult.getAll();

  if (rows.length === 0) {
    return { text: "Query completed successfully. Zero rows returned." };
  }

  if (rows.length > 20) {
    const tmpDir = path.join(baseDir, '.chunks', 'memory_dumps');
    if (!fs.existsSync(tmpDir)) {
      fs.mkdirSync(tmpDir, { recursive: true });
    }
    const outputFile = path.join(tmpDir, 'query_out.txt');
    const fileContent = `Source Query: ${query}\n${"=".repeat(40)}\n` +
      rows.map((r: any) => JSON.stringify(r)).join("\n");
    fs.writeFileSync(outputFile, fileContent, "utf-8");
    return {
      text: `SUCCESS: Query returned ${rows.length} rows. Results spooled to: '${outputFile}'.`,
      spooledTo: outputFile,
    };
  }

  const lines = rows.map((r: any) => `- ${JSON.stringify(r)}`).join("\n");
  return { text: `Results:\n${lines}` };
}