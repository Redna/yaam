import sys
import time
from db import get_connection

def add_note(workspace_name, content):
    conn = get_connection()
    note_id = f"note_{int(time.time() * 1000)}"
    ts = int(time.time())
    try:
        # Create Scratchpad node
        conn.execute(f"CREATE (:Scratchpad {{id: '{note_id}', content: '{content}', created_at: {ts}}})")
        # Link to Workspace
        conn.execute(f"MATCH (w:Workspace {{workspace_name: '{workspace_name}'}}), (s:Scratchpad {{id: '{note_id}'}}) CREATE (w)-[:HAS_SCRATCHPAD]->(s)")
        print(f"Note added to workspace '{workspace_name}'.")
    except Exception as e:
        print(f"Error: {e}")

if __name__ == "__main__":
    if len(sys.argv) < 3:
        print("Usage: python3 add_note.py <workspace_name> <content>")
    else:
        add_note(sys.argv[1], sys.argv[2])
