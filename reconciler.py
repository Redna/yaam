import os
import subprocess
import ladybug as lb
import time
import json
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

def get_all_files():
    """Recursively finds all files in the workspace, ignoring ignored directories."""
    ignored_dirs = {".git", ".venv", "__pycache__", ".pytest_cache", ".claude", ".gemini", "node_modules"}
    files = []
    for root, dirs, filenames in os.walk("."):
        # Modify dirs in-place to skip ignored directories
        dirs[:] = [d for d in dirs if d not in ignored_dirs]
        for filename in filenames:
            filepath = os.path.join(root, filename)
            # Normalize path (remove leading './')
            if filepath.startswith("./"):
                filepath = filepath[2:]
            files.append(filepath)
    return files

from parser import extract_entities

def reconcile(full=False):
    """Updates the Entity table based on filesystem changes or performs a full scan."""
    conn = get_connection()
    ts = int(time.time())

    if full:
        disk_files = set(get_all_files())
        # 1. Process all files on disk
        for path in disk_files:
            conn.execute(
                "MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = 'active', e.last_modified = $ts_val",
                {"path_val": path, "ts_val": ts}
            )
            if path.endswith(".py"):
                try:
                    entities = extract_entities(path)
                    
                    # Create and link top-level functions
                    for func_name, line_num in entities["top_level_functions"].items():
                        func_id = f"{path}::{func_name}"
                        metadata = json.dumps({"line": line_num})
                        conn.execute(
                            "MERGE (f:Entity {id: $fid}) SET f.type = 'Function', f.status = 'active', f.last_modified = $ts_val, f.metadata = $meta_val",
                            {"fid": func_id, "ts_val": ts, "meta_val": metadata}
                        )
                        conn.execute(
                            "MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                            {"pid": path, "fid": func_id}
                        )
                    
                    # Create and link classes and their methods
                    for class_name, class_info in entities["classes"].items():
                        class_id = f"{path}::{class_name}"
                        class_meta = json.dumps({"line": class_info["line"]})
                        conn.execute(
                            "MERGE (c:Entity {id: $cid}) SET c.type = 'Class', c.status = 'active', c.last_modified = $ts_val, c.metadata = $meta_val",
                            {"cid": class_id, "ts_val": ts, "meta_val": class_meta}
                        )
                        conn.execute(
                            "MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                            {"pid": path, "cid": class_id}
                        )
                        
                        for method_name, method_line in class_info["methods"].items():
                            method_id = f"{path}::{class_name}::{method_name}"
                            method_meta = json.dumps({"line": method_line})
                            conn.execute(
                                "MERGE (m:Entity {id: $mid}) SET m.type = 'Function', m.status = 'active', m.last_modified = $ts_val, m.metadata = $meta_val",
                                {"mid": method_id, "ts_val": ts, "meta_val": method_meta}
                            )
                            conn.execute(
                                "MATCH (cls:Entity {id: $cid}), (method:Entity {id: $mid}) MERGE (method)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(cls)",
                                {"cid": class_id, "mid": method_id}
                            )
                except Exception as e:
                    import sys
                    print(f"Error parsing functions and classes in {path}: {e}", file=sys.stderr)
        
        # 2. Soft-delete files in DB that are no longer on disk
        db_files = []
        try:
            res = conn.execute("MATCH (e:Entity {type: 'File'}) RETURN e.id")
            db_files = [row[0] for row in res.get_all()]
        except Exception:
            pass
            
        for db_file in db_files:
            if db_file not in disk_files:
                conn.execute(
                    "MATCH (e:Entity {id: $path_val}) SET e.status = 'deleted', e.last_modified = $ts_val",
                    {"path_val": db_file, "ts_val": ts}
                )
                invalidation_query = """
                MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity)
                WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix
                SET r.is_stale = true, r.invalidated_at = $ts_val
                """
                conn.execute(invalidation_query, {"path_val": db_file, "path_prefix": f"{db_file}::", "ts_val": ts})

    else:
        status_map = get_git_status()
        if not status_map:
            return

        for item in status_map:
            path = item["path"]
            git_status = item["status"]
            
            entity_status = "active"
            if git_status == "D":
                entity_status = "deleted"
            
            conn.execute(
                "MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = $status_val, e.last_modified = $ts_val",
                {"path_val": path, "status_val": entity_status, "ts_val": ts}
            )

            if entity_status == "active" and path.endswith(".py"):
                try:
                    entities = extract_entities(path)
                    
                    for func_name, line_num in entities["top_level_functions"].items():
                        func_id = f"{path}::{func_name}"
                        metadata = json.dumps({"line": line_num})
                        conn.execute(
                            "MERGE (f:Entity {id: $fid}) SET f.type = 'Function', f.status = 'active', f.last_modified = $ts_val, f.metadata = $meta_val",
                            {"fid": func_id, "ts_val": ts, "meta_val": metadata}
                        )
                        conn.execute(
                            "MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                            {"pid": path, "fid": func_id}
                        )
                    
                    for class_name, class_info in entities["classes"].items():
                        class_id = f"{path}::{class_name}"
                        class_meta = json.dumps({"line": class_info["line"]})
                        conn.execute(
                            "MERGE (c:Entity {id: $cid}) SET c.type = 'Class', c.status = 'active', c.last_modified = $ts_val, c.metadata = $meta_val",
                            {"cid": class_id, "ts_val": ts, "meta_val": class_meta}
                        )
                        conn.execute(
                            "MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
                            {"pid": path, "cid": class_id}
                        )
                        
                        for method_name, method_line in class_info["methods"].items():
                            method_id = f"{path}::{class_name}::{method_name}"
                            method_meta = json.dumps({"line": method_line})
                            conn.execute(
                                "MERGE (m:Entity {id: $mid}) SET m.type = 'Function', m.status = 'active', m.last_modified = $ts_val, m.metadata = $meta_val",
                                {"mid": method_id, "ts_val": ts, "meta_val": method_meta}
                            )
                            conn.execute(
                                "MATCH (cls:Entity {id: $cid}), (method:Entity {id: $mid}) MERGE (method)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(cls)",
                                {"cid": class_id, "mid": method_id}
                            )
                except Exception as e:
                    import sys
                    print(f"Error parsing functions and classes in {path}: {e}", file=sys.stderr)
            
            if entity_status == "deleted":
                invalidation_query = """
                MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity)
                WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix
                SET r.is_stale = true, r.invalidated_at = $ts_val
                """
                conn.execute(invalidation_query, {"path_val": path, "path_prefix": f"{path}::", "ts_val": ts})

if __name__ == "__main__":
    import sys
    full_sync = "--full" in sys.argv
    reconcile(full=full_sync)
    # Output valid hook decision for Gemini CLI
    import json
    if not sys.stdin.isatty():
        print(json.dumps({"decision": "allow"}))
