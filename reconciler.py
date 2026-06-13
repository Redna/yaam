import os
import subprocess
import ladybug as lb
import time
from db import get_connection

def get_git_status():
    """Runs git status --porcelain and returns the list of modified/deleted files."""
    try:
        result = subprocess.run(["git", "status", "--porcelain"], capture_output=True, text=True, check=True)
        lines = [line for line in result.stdout.splitlines() if line.strip()]
        if not lines:
            return []
        
        status_map = []
        for line in lines:
            status = line[:2].strip()
            filepath = line[3:].strip()
            status_map.append({"status": status, "path": filepath})
        return status_map
    except subprocess.CalledProcessError:
        return []

from parser import extract_entities

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

        # 2. Extract and link Classes and Functions for Python files
        if entity_status == "active" and path.endswith(".py"):
            try:
                entities = extract_entities(path)
                
                # Create and link top-level functions
                for func_name in entities["top_level_functions"]:
                    func_id = f"{path}::{func_name}"
                    conn.execute(
                        "MERGE (f:Entity {id: $fid}) SET f.type = 'Function', f.status = 'active', f.last_modified = $ts_val",
                        {"fid": func_id, "ts_val": ts}
                    )
                    conn.execute(
                        "MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                        {"pid": path, "fid": func_id}
                    )
                
                # Create and link classes and their methods
                for class_name, methods in entities["classes"].items():
                    class_id = f"{path}::{class_name}"
                    # Create Class node
                    conn.execute(
                        "MERGE (c:Entity {id: $cid}) SET c.type = 'Class', c.status = 'active', c.last_modified = $ts_val",
                        {"cid": class_id, "ts_val": ts}
                    )
                    # Link Class to File
                    conn.execute(
                        "MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                        {"pid": path, "cid": class_id}
                    )
                    
                    # Create Class Methods
                    for method_name in methods:
                        method_id = f"{path}::{class_name}::{method_name}"
                        conn.execute(
                            "MERGE (m:Entity {id: $mid}) SET m.type = 'Function', m.status = 'active', m.last_modified = $ts_val",
                            {"mid": method_id, "ts_val": ts}
                        )
                        # Link Method to Class
                        conn.execute(
                            "MATCH (cls:Entity {id: $cid}), (method:Entity {id: $mid}) MERGE (method)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(cls)",
                            {"cid": class_id, "mid": method_id}
                        )
            except Exception as e:
                import sys
                print(f"Error parsing functions and classes in {path}: {e}", file=sys.stderr)
        
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
