import os
import re
import json
import time
import sys
import logging
from contextlib import redirect_stdout
from fastmcp import FastMCP
import ladybug as lb
from db import get_connection
from logger import log_event

# Configure logging to stderr to prevent interference with JSON-RPC over stdout
logging.basicConfig(level=logging.ERROR, stream=sys.stderr)
logger = logging.getLogger("yaam_memory")

# Initialize Prefect FastMCP
mcp = FastMCP("yaam_memory")

@mcp.tool()
def graph_explore(query: str) -> str:
    """
    Executes a read-only Cypher query to explore code relationships (Layer 0) 
    and agent memories (Layer 1).
    """
    # Force any stray stdout from libraries to stderr
    with redirect_stdout(sys.stderr):
        try:
            from reconciler import reconcile
            reconcile()
        except Exception as e:
            logger.error(f"Passive reconciliation failed: {e}")

        clean_query = query.strip()
        
        # Guardrail 1: Enforce Read-Only Mutation Blocks (using whole word matching)
        forbidden = ["CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DROP", "ALTER"]
        if any(re.search(rf"\b{k}\b", clean_query, re.IGNORECASE) for k in forbidden):
            return "ERROR: Write operations forbidden via this tool. Use workspace mutation tools to alter memory."
        
        # Guardrail 2: Automatic Window Size Constraints
        if "LIMIT" not in clean_query.upper():
            # Inject LIMIT 500 into the return clause
            clean_query = re.sub(r'(RETURN\s+.+)', r'\1 LIMIT 500', clean_query, flags=re.IGNORECASE)
            # Fallback if regex failed to find RETURN
            if "LIMIT 500" not in clean_query.upper():
                 clean_query += " LIMIT 500"

        try:
            # get_connection() now creates a fresh LB Database object that will be 
            # garbage collected (closing the lock) when the function scope ends.
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
                        f"Results spooled to: '{output_file}'.")
            
            return "Results:\n" + "\n".join(f"- {str(r)}" for r in rows)
        except Exception as e:
            return f"Database Syntax Error: {str(e)}"

@mcp.tool()
def workspace_initialize(name: str, description: str) -> str:
    """Initializes a new workspace for task tracking."""
    with redirect_stdout(sys.stderr):
        try:
            from reconciler import reconcile
            reconcile()
        except Exception as e:
            logger.error(f"Passive reconciliation failed: {e}")

        log_event("workspace_initialize", {"name": name, "description": description})
        try:
            conn = get_connection()
            # Note: Using 'description_val' instead of 'desc' as 'desc' is a Cypher reserved word
            query = "CREATE (:Workspace {workspace_name: $name, description: $description_val, status: 'active'})"
            conn.execute(query, {"name": name, "description_val": description})
            return f"Workspace '{name}' initialized successfully."
        except Exception as e:
            return f"Error: {str(e)}"

@mcp.tool()
def workspace_append_note(workspace_name: str, content: str) -> str:
    """Appends a new note to the scratchpad of a workspace."""
    with redirect_stdout(sys.stderr):
        try:
            from reconciler import reconcile
            reconcile()
        except Exception as e:
            logger.error(f"Passive reconciliation failed: {e}")

        note_id = f"note_{int(time.time() * 1000)}"
        ts = int(time.time())
        log_event("workspace_append_note", {"workspace_name": workspace_name, "content": content})
        try:
            conn = get_connection()
            # Create Scratchpad node using parameters to safely handle quotes
            conn.execute(
                "CREATE (:Scratchpad {id: $id, content: $content, created_at: $ts})",
                {"id": note_id, "content": content, "ts": ts}
            )
            # Link to Workspace
            conn.execute(
                "MATCH (w:Workspace {workspace_name: $ws_name}), (s:Scratchpad {id: $n_id}) CREATE (w)-[:HAS_SCRATCHPAD]->(s)",
                {"ws_name": workspace_name, "n_id": note_id}
            )
            return f"Note added to workspace '{workspace_name}'."
        except Exception as e:
            return f"Error: {str(e)}"

@mcp.resource("skills://yaam-memory-manager")
def get_skill_blueprint() -> str:
    """Provides the cognitive blueprint / guide for YAAM memory manager."""
    try:
        skill_path = "/home/anima/yaam/.agents/skills/yaam-memory-manager/SKILL.md"
        if os.path.exists(skill_path):
            with open(skill_path, "r", encoding="utf-8") as f:
                return f.read()
        return "Skill blueprint file not found."
    except Exception as e:
        return f"Error reading skill blueprint: {str(e)}"

@mcp.prompt()
def use_yaam_memory() -> str:
    """Instructions and guidance on how to use YAAM memory engine in this workspace."""
    try:
        skill_path = "/home/anima/yaam/.agents/skills/yaam-memory-manager/SKILL.md"
        if os.path.exists(skill_path):
            with open(skill_path, "r", encoding="utf-8") as f:
                content = f.read()
            return f"You are equipped with YAAM (Yet Another Agent Memory). Please follow these instructions to manage task context and memory:\n\n{content}"
    except Exception:
        pass
    return "Please initialize the task using workspace_initialize and record insights with workspace_append_note."

if __name__ == "__main__":
    # Disable the banner which breaks the MCP protocol on stdout
    mcp.run(show_banner=False)
