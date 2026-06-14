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

def resolve_module_to_path(module_name):
    """Resolves an imported module name to a relative file path in the workspace."""
    parts = module_name.split('.')
    path_candidate = os.path.join(*parts) + ".py"
    if os.path.exists(path_candidate):
        return path_candidate
    dir_candidate = os.path.join(*parts)
    init_candidate = os.path.join(dir_candidate, "__init__.py")
    if os.path.exists(init_candidate):
        return init_candidate
    return None

def cleanup_stale_entities(conn, path, parsed_ids, ts):
    """Marks functions and classes no longer declared in a modified file as deleted."""
    prefix = f"{path}::"
    db_entities = []
    try:
        res = conn.execute(
            "MATCH (e:Entity) WHERE e.id STARTS WITH $prefix AND e.type IN ['Function', 'Class'] RETURN e.id",
            {"prefix": prefix}
        )
        db_entities = [row[0] for row in res.get_all()]
    except Exception:
        pass
        
    for ent_id in db_entities:
        if ent_id not in parsed_ids:
            conn.execute(
                "MATCH (e:Entity {id: $eid}) SET e.status = 'deleted', e.last_modified = $ts_val",
                {"eid": ent_id, "ts_val": ts}
            )

from parser import extract_entities

def process_file_entities(conn, path, entities, ts):
    """Creates, links, and updates file, class, method, and call graph entities."""
    parsed_ids = set()
    
    # 1. Handle File Imports
    for imp_module in entities.get("imports", []):
        target_path = resolve_module_to_path(imp_module)
        if target_path and target_path != path:
            conn.execute(
                "MERGE (target:Entity {id: $target_id}) SET target.type = 'File'",
                {"target_id": target_path}
            )
            conn.execute(
                "MATCH (src:Entity {id: $src_id}), (dst:Entity {id: $dst_id}) MERGE (src)-[:LINKED_TO {relationship_type: 'IMPORTS'}]->(dst)",
                {"src_id": path, "dst_id": target_path}
            )
            
    # 2. Create top-level functions
    for func_name, func_info in entities.get("top_level_functions", {}).items():
        func_id = f"{path}::{func_name}"
        parsed_ids.add(func_id)
        metadata = json.dumps({"line": func_info["line"]})
        conn.execute(
            "MERGE (f:Entity {id: $fid}) SET f.type = 'Function', f.status = 'active', f.last_modified = $ts_val, f.metadata = $meta_val",
            {"fid": func_id, "ts_val": ts, "meta_val": metadata}
        )
        conn.execute(
            "MATCH (file:Entity {id: $pid}), (func:Entity {id: $fid}) MERGE (func)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
            {"pid": path, "fid": func_id}
        )
        
    # 3. Create classes and their methods
    for class_name, class_info in entities.get("classes", {}).items():
        class_id = f"{path}::{class_name}"
        parsed_ids.add(class_id)
        class_meta = json.dumps({"line": class_info["line"]})
        conn.execute(
            "MERGE (c:Entity {id: $cid}) SET c.type = 'Class', c.status = 'active', c.last_modified = $ts_val, c.metadata = $meta_val",
            {"cid": class_id, "ts_val": ts, "meta_val": class_meta}
        )
        conn.execute(
            "MATCH (file:Entity {id: $pid}), (cls:Entity {id: $cid}) MERGE (cls)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(file)",
            {"pid": path, "cid": class_id}
        )
        
        # Class Inheritance
        for base_class in class_info.get("superclasses", []):
            super_id = None
            if base_class in entities["classes"]:
                super_id = f"{path}::{base_class}"
            else:
                try:
                    res = conn.execute(
                        "MATCH (c:Entity {type: 'Class'}) WHERE c.id ENDS WITH $suffix RETURN c.id",
                        {"suffix": f"::{base_class}"}
                    )
                    if res.has_next():
                        super_id = res.get_next()[0]
                except Exception:
                    pass
            if super_id:
                conn.execute(
                    "MATCH (sub:Entity {id: $sub_id}), (sup:Entity {id: $sup_id}) MERGE (sub)-[:LINKED_TO {relationship_type: 'INHERITS_FROM'}]->(sup)",
                    {"sub_id": class_id, "sup_id": super_id}
                )
        
        # Class Methods
        for method_name, method_info in class_info.get("methods", {}).items():
            method_id = f"{path}::{class_name}::{method_name}"
            parsed_ids.add(method_id)
            method_meta = json.dumps({"line": method_info["line"]})
            conn.execute(
                "MERGE (m:Entity {id: $mid}) SET m.type = 'Function', m.status = 'active', m.last_modified = $ts_val, m.metadata = $meta_val",
                {"mid": method_id, "ts_val": ts, "meta_val": method_meta}
            )
            conn.execute(
                "MATCH (cls:Entity {id: $cid}), (method:Entity {id: $mid}) MERGE (method)-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(cls)",
                {"cid": class_id, "mid": method_id}
            )

    # 4. Method and Function calls (Call Graph resolution)
    for func_name, func_info in entities.get("top_level_functions", {}).items():
        func_id = f"{path}::{func_name}"
        for call_target in func_info.get("calls", []):
            target_id = None
            if call_target in entities["top_level_functions"]:
                target_id = f"{path}::{call_target}"
            else:
                try:
                    res = conn.execute(
                        "MATCH (f:Entity {type: 'Function'}) WHERE f.id ENDS WITH $suffix RETURN f.id",
                        {"suffix": f"::{call_target}"}
                    )
                    if res.has_next():
                        target_id = res.get_next()[0]
                except Exception:
                    pass
            if target_id:
                conn.execute(
                    "MERGE (target:Entity {id: $target_id}) SET target.type = 'Function'",
                    {"target_id": target_id}
                )
                conn.execute(
                    "MATCH (caller:Entity {id: $caller_id}), (callee:Entity {id: $callee_id}) MERGE (caller)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee)",
                    {"caller_id": func_id, "callee_id": target_id}
                )
                
    for class_name, class_info in entities.get("classes", {}).items():
        for method_name, method_info in class_info.get("methods", {}).items():
            method_id = f"{path}::{class_name}::{method_name}"
            for call_target in method_info.get("calls", []):
                target_id = None
                if call_target.startswith("self."):
                    sibling_name = call_target.split('.', 1)[1]
                    if sibling_name in class_info["methods"]:
                        target_id = f"{path}::{class_name}::{sibling_name}"
                elif call_target in entities["top_level_functions"]:
                    target_id = f"{path}::{call_target}"
                else:
                    try:
                        res = conn.execute(
                            "MATCH (f:Entity {type: 'Function'}) WHERE f.id ENDS WITH $suffix RETURN f.id",
                            {"suffix": f"::{call_target}"}
                        )
                        if res.has_next():
                            target_id = res.get_next()[0]
                    except Exception:
                        pass
                if target_id:
                    conn.execute(
                        "MERGE (target:Entity {id: $target_id}) SET target.type = 'Function'",
                        {"target_id": target_id}
                    )
                    conn.execute(
                        "MATCH (caller:Entity {id: $caller_id}), (callee:Entity {id: $callee_id}) MERGE (caller)-[:LINKED_TO {relationship_type: 'CALLS'}]->(callee)",
                        {"caller_id": method_id, "callee_id": target_id}
                    )

    # 5. Run GC for stale entities in this file
    cleanup_stale_entities(conn, path, parsed_ids, ts)

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
                    process_file_entities(conn, path, entities, ts)
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
                    process_file_entities(conn, path, entities, ts)
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
    if not sys.stdin.isatty():
        print(json.dumps({"decision": "allow"}))
