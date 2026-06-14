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
    ignored_dirs = {
        ".git", ".venv", "__pycache__", ".pytest_cache", ".claude", ".gemini", 
        "node_modules", "llm_logs", "xray_data", "reports", "docs"
    }
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

def process_file_entities(conn, path, entities, ts, func_lookup, class_lookup):
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
                candidates = class_lookup.get(base_class, [])
                if candidates:
                    super_id = candidates[0]
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
                candidates = func_lookup.get(call_target, [])
                if candidates:
                    target_id = candidates[0]
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
                    candidates = func_lookup.get(call_target, [])
                    if candidates:
                        target_id = candidates[0]
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

    # Build files_to_reconcile list
    if full:
        disk_files = set(get_all_files())
        files_to_reconcile = [p for p in disk_files if p.endswith(".py")]
    else:
        status_map = get_git_status()
        files_to_reconcile = [item["path"] for item in status_map if item["status"] != "D" and item["path"].endswith(".py")]

    # Load all active entities from DB to populate the lookup maps
    existing_funcs = []
    existing_classes = []
    try:
        res = conn.execute("MATCH (f:Entity {type: 'Function', status: 'active'}) RETURN f.id")
        existing_funcs = [row[0] for row in res.get_all()]
        res = conn.execute("MATCH (c:Entity {type: 'Class', status: 'active'}) RETURN c.id")
        existing_classes = [row[0] for row in res.get_all()]
    except Exception:
        pass

    reconcile_prefixes = tuple(f"{p}::" for p in files_to_reconcile)
    
    func_lookup = {}
    for fid in existing_funcs:
        if fid.startswith(reconcile_prefixes):
            continue
        short_name = fid.split("::")[-1]
        func_lookup.setdefault(short_name, []).append(fid)
        
    class_lookup = {}
    for cid in existing_classes:
        if cid.startswith(reconcile_prefixes):
            continue
        short_name = cid.split("::")[-1]
        class_lookup.setdefault(short_name, []).append(cid)

    # Pre-parse files to populate lookups
    parsed_cache = {}
    print(f"Pre-parsing {len(files_to_reconcile)} python files...", flush=True)
    for path in files_to_reconcile:
        try:
            entities = extract_entities(path)
            parsed_cache[path] = entities
            
            # Update lookups
            for func_name in entities.get("top_level_functions", {}):
                fid = f"{path}::{func_name}"
                func_lookup.setdefault(func_name, []).append(fid)
            for class_name, class_info in entities.get("classes", {}).items():
                cid = f"{path}::{class_name}"
                class_lookup.setdefault(class_name, []).append(cid)
                for method_name in class_info.get("methods", {}):
                    mid = f"{path}::{class_name}::{method_name}"
                    func_lookup.setdefault(method_name, []).append(mid)
        except Exception as e:
            import sys
            print(f"Error parsing functions and classes in {path}: {e}", file=sys.stderr)

    print(f"Pre-parsing complete. Registered {len(func_lookup)} functions and {len(class_lookup)} classes in memory.", flush=True)

    if full:
        # 1. Process all files on disk
        print(f"Processing {len(disk_files)} files on disk...", flush=True)
        for idx, path in enumerate(disk_files):
            if idx % 10 == 0:
                print(f"  Processed {idx}/{len(disk_files)} files...", flush=True)
            conn.execute(
                "MERGE (e:Entity {id: $path_val}) SET e.type = 'File', e.status = 'active', e.last_modified = $ts_val",
                {"path_val": path, "ts_val": ts}
            )
            if path.endswith(".py") and path in parsed_cache:
                try:
                    process_file_entities(conn, path, parsed_cache[path], ts, func_lookup, class_lookup)
                except Exception as e:
                    import sys
                    print(f"Error processing entities in {path}: {e}", file=sys.stderr)
        print("Completed processing all files on disk.", flush=True)
        
        # 2. Soft-delete files in DB that are no longer on disk
        db_files = []
        try:
            res = conn.execute("MATCH (e:Entity {type: 'File'}) RETURN e.id")
            db_files = [row[0] for row in res.get_all()]
        except Exception:
            pass
            
        stale_files = [f for f in db_files if f not in disk_files]
        if stale_files:
            print(f"Soft-deleting {len(stale_files)} stale files...", flush=True)
            # Find all active functions/classes that belong to stale files
            stale_files_set = set(stale_files)
            funcs_to_delete = [fid for fid in existing_funcs if fid.split("::")[0] in stale_files_set]
            classes_to_delete = [cid for cid in existing_classes if cid.split("::")[0] in stale_files_set]
            all_to_delete = funcs_to_delete + classes_to_delete
            
            chunk_size = 500
            if all_to_delete:
                print(f"Soft-deleting {len(all_to_delete)} sub-entities in stale files...", flush=True)
                for i in range(0, len(all_to_delete), chunk_size):
                    chunk = all_to_delete[i:i+chunk_size]
                    conn.execute(
                        "MATCH (e:Entity) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val",
                        {"chunk": chunk, "ts_val": ts}
                    )
            
            for i in range(0, len(stale_files), chunk_size):
                chunk = stale_files[i:i+chunk_size]
                conn.execute(
                    "MATCH (e:Entity {type: 'File'}) WHERE e.id IN $chunk SET e.status = 'deleted', e.last_modified = $ts_val",
                    {"chunk": chunk, "ts_val": ts}
                )
            
            # Check for workspace mappings to stale entities
            mapped_ids = set()
            try:
                res = conn.execute("MATCH (w:Workspace)-[:MAPPED_TO]->(e:Entity) RETURN e.id")
                mapped_ids = {row[0] for row in res.get_all()}
            except Exception:
                pass
                
            for db_file in stale_files:
                prefix = f"{db_file}::"
                has_mappings = db_file in mapped_ids or any(mid.startswith(prefix) for mid in mapped_ids)
                if has_mappings:
                    invalidation_query = """
                    MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity)
                    WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix
                    SET r.is_stale = true, r.invalidated_at = $ts_val
                    """
                    conn.execute(invalidation_query, {"path_val": db_file, "path_prefix": prefix, "ts_val": ts})
            print("Soft-deletion complete.", flush=True)

    else:
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

            if entity_status == "active" and path.endswith(".py") and path in parsed_cache:
                try:
                    process_file_entities(conn, path, parsed_cache[path], ts, func_lookup, class_lookup)
                except Exception as e:
                    import sys
                    print(f"Error processing entities in {path}: {e}", file=sys.stderr)
            
            if entity_status == "deleted":
                invalidation_query = """
                MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity)
                WHERE e.id = $path_val OR e.id STARTS WITH $path_prefix
                SET r.is_stale = true, r.invalidated_at = $ts_val
                """
                conn.execute(invalidation_query, {"path_val": path, "path_prefix": f"{path}::", "ts_val": ts})

def track_accessed_file(conn, tool_name, tool_input):
    """Automatically maps the active workspace to the file and classes accessed/modified by the tool."""
    path = None
    if tool_name == "view_file" and "AbsolutePath" in tool_input:
        path = tool_input["AbsolutePath"]
    elif tool_name in ["replace_file_content", "multi_replace_file_content", "write_to_file"] and "TargetFile" in tool_input:
        path = tool_input["TargetFile"]
        
    if not path:
        return
        
    # Normalize path relative to the workspace root
    workspace_root = os.getcwd()
    rel_path = os.path.relpath(path, workspace_root)
    # Ensure it's not a path starting with '../' (outside workspace)
    if rel_path.startswith(".."):
        return
        
    # Get active workspace name
    ws_name = None
    try:
        res = conn.execute("MATCH (w:Workspace {status: 'active'}) RETURN w.workspace_name")
        if res.has_next():
            ws_name = res.get_next()[0]
    except Exception:
        pass
        
    if not ws_name:
        return
        
    # Create MAPPED_TO relationship from Workspace to the File entity
    ts = int(time.time())
    try:
        # Merge target File entity to be safe (if not already reconciled)
        conn.execute(
            "MERGE (e:Entity {id: $eid}) SET e.type = 'File'",
            {"eid": rel_path}
        )
        # Create MAPPED_TO relation
        conn.execute(
            "MATCH (w:Workspace {workspace_name: $ws}), (e:Entity {id: $eid}) "
            "MERGE (w)-[:MAPPED_TO {created_at: $ts_val, is_stale: false}]->(e)",
            {"ws": ws_name, "eid": rel_path, "ts_val": ts}
        )
        
        # Get all classes declared in this file and map them as well
        res_c = conn.execute(
            "MATCH (c:Entity {type: 'Class'})-[:LINKED_TO {relationship_type: 'DECLARED_IN'}]->(f:Entity {id: $fid}) RETURN c.id",
            {"fid": rel_path}
        )
        class_ids = [row[0] for row in res_c.get_all()]
        for cid in class_ids:
            conn.execute(
                "MATCH (w:Workspace {workspace_name: $ws}), (e:Entity {id: $eid}) "
                "MERGE (w)-[:MAPPED_TO {created_at: $ts_val, is_stale: false}]->(e)",
                {"ws": ws_name, "eid": cid, "ts_val": ts}
            )
    except Exception as e:
        import sys
        print(f"Error mapping workspace to file: {e}", file=sys.stderr)

if __name__ == "__main__":
    import sys
    full_sync = "--full" in sys.argv
    
    # Read tool use info from stdin if available
    tool_name = None
    tool_input = {}
    if not sys.stdin.isatty():
        try:
            payload_str = sys.stdin.read()
            if payload_str.strip():
                payload = json.loads(payload_str)
                tool_name = payload.get("tool_name")
                tool_input = payload.get("tool_input") or payload.get("args") or {}
        except Exception as e:
            print(f"Error reading hook payload: {e}", file=sys.stderr)

    reconcile(full=full_sync)
    
    # If we have a tool name and input, track the accessed file
    if tool_name:
        try:
            conn = get_connection()
            track_accessed_file(conn, tool_name, tool_input)
        except Exception as e:
            print(f"Error executing track_accessed_file: {e}", file=sys.stderr)
        
    # Output valid hook decision for Gemini CLI
    if not sys.stdin.isatty():
        print(json.dumps({"decision": "allow"}))
