import os
import subprocess
import ladybug as lb
import time
from db import get_connection

def get_git_status():
    """Runs git status --porcelain and returns the list of modified/deleted files."""
    try:
        result = subprocess.run(["git", "status", "--porcelain"], capture_output=True, text=True, check=True)
        lines = result.stdout.strip().split("\n")
        if not lines or lines == ['']:
            return []
        
        status_map = []
        for line in lines:
            status = line[:2].strip()
            filepath = line[3:].strip()
            status_map.append({"status": status, "path": filepath})
        return status_map
    except subprocess.CalledProcessError:
        return []

from parser import extract_functions

def reconcile():
    """Updates the Entity table based on filesystem changes."""
    status_map = get_git_status()
    if not status_map:
        return

    conn = get_connection()
    ts = int(time.time())

    for item in status_map:
        path = item["path"]
        git_status = item["status"]
        
        # Mapping git status to Entity status
        entity_status = "active"
        if git_status == "D":
            entity_status = "deleted"
        
        # 1. Handle the File Entity using parameters
        conn.execute(
            "MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = $status_val, e.last_modified = $ts_val",
            {"path_val": path, "status_val": entity_status, "ts_val": ts}
        )

        # 2. Extract and link Functions for Python files
        if entity_status == "active" and path.endswith(".py"):
            try:
                functions = extract_functions(path)
                for func_name in functions:
                    func_id = f"{path}::{func_name}"
                    # Create function entity
                    conn.execute(
                        "MERGE (f:Entity {id: $fid}) SET f.type = 'Function', f.status = 'active', f.last_modified = $ts_val",
                        {"fid": func_id, "ts_val": ts}
                    )
                    # Link function to file
                    conn.execute(
                        "MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                        {"pid": path, "fid": func_id}
                    )
            except Exception as e:
                import sys
                print(f"Error parsing functions in {path}: {e}", file=sys.stderr)
        
        # Soft invalidation for deleted files (Layer 1 Mappings)
        if entity_status == "deleted":
            # Identify and flag matching active workspaces
            invalidation_query = """
            MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity)
            WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix
            SET r.is_stale = true, r.invalidated_at = $ts_val
            """
            conn.execute(invalidation_query, {"path_val": path, "path_prefix": f"{path}::", "ts_val": ts})

if __name__ == "__main__":
    reconcile()
    # Output valid hook decision for Gemini CLI
    import json
    import sys
    if not sys.stdin.isatty():
        print(json.dumps({"decision": "allow"}))
