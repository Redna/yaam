import os
import re
import json
import time
from mcp.server.fastmcp import FastMCP
import ladybug as lb
from db import get_connection, DB_PATH
from logger import log_event

mcp = FastMCP("yaam_memory")

@mcp.tool()
def mcp__graph__explore(query: str) -> str:
    """Executes a read-only Cypher query to explore code relationships (Layer 0) and memories (Layer 1)."""
    clean_query = query.strip()
    
    # Guardrail 1: Enforce Read-Only Mutation Blocks
    forbidden = ["CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DROP", "ALTER"]
    if any(k in clean_query.upper() for k in forbidden):
        return "ERROR: Write operations forbidden via this tool. Use mcp__workspace tools to alter memory."
    
    # Guardrail 2: Automatic Window Size Constraints
    if "LIMIT" not in clean_query.upper():
        clean_query = re.sub(r'(RETURN\s+.+)', r'\1 LIMIT 500', clean_query, flags=re.IGNORECASE)
        if "LIMIT 500" not in clean_query.upper():
             clean_query += " LIMIT 500"

    try:
        conn = get_connection()
        results = conn.execute(clean_query)
        rows = results.get_all()
        
        if not rows:
            return "Query completed successfully. Zero rows returned."
        
        # Guardrail 3: Disk Spooling to Prevent Prompt Token Bloat
        if len(rows) > 20:
            tmp_dir = "./.chunks/memory_dumps"
            os.makedirs(tmp_dir, exist_ok=True)
            output_file = f"{tmp_dir}/query_out.txt"
            with open(output_file, "w", encoding="utf-8") as f:
                f.write(f"Source Query: {query}\n" + "="*40 + "\n")
                for r in rows:
                    f.write(f"{str(r)}\n")
            return (f"SUCCESS: Query returned {len(rows)} rows. "
                    f"To protect your context window, results have been spooled to disk at: '{output_file}'. "
                    f"Use your local file tools (e.g., grep, view_file, or bash/sed) to inspect the data.")
        
        return "Results:\n" + "\n".join(f"- {str(r)}" for r in rows)
    except Exception as e:
        return f"Database Syntax Error: {str(e)}"

@mcp.tool()
def mcp__workspace__initialize(name: str, description: str) -> str:
    """Initializes a new workspace for task tracking."""
    log_event("mcp__workspace__initialize", {"name": name, "description": description})
    try:
        conn = get_connection()
        query = f"CREATE (:Workspace {{workspace_name: '{name}', description: '{description}', status: 'active'}})"
        conn.execute(query)
        return f"Workspace '{name}' initialized successfully."
    except Exception as e:
        return f"Error: {str(e)}"

@mcp.tool()
def mcp__workspace__append_note(workspace_name: str, content: str) -> str:
    """Appends a new note to the scratchpad of a workspace."""
    note_id = f"note_{int(time.time() * 1000)}"
    ts = int(time.time())
    log_event("mcp__workspace__append_note", {"workspace_name": workspace_name, "content": content})
    try:
        conn = get_connection()
        # Create Scratchpad node
        conn.execute(f"CREATE (:Scratchpad {{id: '{note_id}', content: '{content}', created_at: {ts}}})")
        # Link to Workspace
        conn.execute(f"MATCH (w:Workspace {{workspace_name: '{workspace_name}'}}), (s:Scratchpad {{id: '{note_id}'}}) CREATE (w)-[:HAS_SCRATCHPAD]->(s)")
        return f"Note added to workspace '{workspace_name}'."
    except Exception as e:
        return f"Error: {str(e)}"

if __name__ == "__main__":
    mcp.run()
