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

    const rows = await client.query(dsl);

    if (rows.length === 0) {
      return { text: "Query completed successfully. Zero rows returned." };
    }

    const sanitizeRow = (r: any) => {
      if (!r || typeof r !== 'object') return r;
      const copy = { ...r };
      if (typeof copy.content === 'string' && copy.content.length > 200) {
        copy.content = copy.content.substring(0, 200) + '... [TRUNCATED]';
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

    return {
      text: `SUCCESS. Results:\n${rows.map((r: any) => JSON.stringify(sanitizeRow(r))).join("\n")}`,
    };
  } catch (err: any) {
    return { text: `Error executing query: ${err.message}` };
  }
}