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
        
        # 1. Handle the File Entity
        query = f"""
        MERGE (e:Entity {{id: '{path}'}})
        SET e.type = 'File', e.status = '{entity_status}', e.last_modified = {ts}
        """
        conn.execute(query)

        # 2. Extract and link Functions for Python files
        if entity_status == "active" and path.endswith(".py"):
            functions = extract_functions(path)
            for func_name in functions:
                func_id = f"{path}::{func_name}"
                # Create function entity
                conn.execute(f"MERGE (f:Entity {{id: '{func_id}'}}) SET f.type = 'Function', f.status = 'active', f.last_modified = {ts}")
                # Link function to file
                conn.execute(f"MATCH (file:Entity {{id: '{path}'}}), (func:Entity {{id: '{func_id}'}}) MERGE (func)-[:LINKED_TO {{relationship_type: 'DECLARED_IN'}}]->(file)")
        
        # Soft invalidation for deleted files (Layer 1 Mappings)
        if entity_status == "deleted":
            invalidation_query = f"""
            MATCH (w:Workspace)-[r:MAPPED_TO]->(e:Entity)
            WHERE e.id = '{path}' OR e.id STARTS WITH '{path}::'
            SET r.is_stale = true, r.invalidated_at = {ts}
            """
            conn.execute(invalidation_query)

    # Suppress output if running in a hook context or just make it silent by default
    # print(f"Reconciliation complete. Processed {len(status_map)} items.")

if __name__ == "__main__":
    reconcile()
    # Output valid hook decision for Gemini CLI
    import json
    import sys
    if not sys.stdin.isatty():
        print(json.dumps({"decision": "allow"}))
