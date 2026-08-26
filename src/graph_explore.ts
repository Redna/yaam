import * as fs from 'fs';
import * as path from 'path';
import { YaamEngineClient } from './engine-client.js';

export interface ExploreResult {
  text: string;
  spooledTo?: string;
}

export async function exploreGraph(
  queryOrDsl: any,
  client: YaamEngineClient,
  baseDir: string
): Promise<ExploreResult> {
  try {
    let dsl = queryOrDsl;
    if (typeof queryOrDsl === "string") {
      try {
        dsl = JSON.parse(queryOrDsl);
      } catch {
        return { text: "Error: The yaam_graph_explore tool now expects JSON Query DSL, not Cypher." };
      }
    }

    if (dsl && typeof dsl === 'object') {
      if (dsl.limit === undefined || typeof dsl.limit !== 'number') {
        dsl.limit = 25;
      } else if (dsl.limit > 50) {
        dsl.limit = 50;
      }
    }

    const rows = await client.query(dsl);

    if (rows.length === 0) {
      return { text: "Query completed successfully. Zero rows returned." };
    }

    let filesOmitted = false;

    const sanitizeRow = (r: any) => {
      if (!r || typeof r !== 'object') return r;
      const copy = { ...r };
      
      if (typeof copy.content === 'string') {
        const isFile = copy.entity_type === 'File' || copy.type === 'File';
        if (isFile) {
          delete copy.content;
          filesOmitted = true;
        } else {
          // For Functions, Classes, Types, Sections - 1000 chars preserves the signature + docstring + body
          if (copy.content.length > 1000) {
            copy.content = copy.content.substring(0, 1000) + '\n... [BODY TRUNCATED]';
          }
        }
      }
      
      if (copy.embedding) {
        delete copy.embedding;
      }
      return copy;
    };

    if (rows.length > 20) {
      const tmpDir = path.join(baseDir, '.chunks', 'memory_dumps');
      if (!fs.existsSync(tmpDir)) {
        fs.mkdirSync(tmpDir, { recursive: true });
      }
      const outputFile = path.join(tmpDir, 'query_out.txt');
      const fileContent = `Source DSL: ${JSON.stringify(dsl)}\n${"=".repeat(40)}\n` +
        rows.map((r: any) => JSON.stringify(sanitizeRow(r))).join("\n");
      fs.writeFileSync(outputFile, fileContent, "utf-8");
      return {
        text: `SUCCESS: Query returned ${rows.length} rows. Results spooled to: '${outputFile}'.`,
        spooledTo: outputFile,
      };
    }

    const warning = filesOmitted ? "(Note: File contents omitted to prevent token bloat. Traverse inbound 'DECLARED_IN' edges to list entities, or use the 'read' tool to view full source.)\n" : "";

    return {
      text: `SUCCESS. Results:\n${warning}${rows.map((r: any) => JSON.stringify(sanitizeRow(r))).join("\n")}`,
    };
  } catch (err: any) {
    return { text: `Error executing query: ${err.message}` };
  }
}