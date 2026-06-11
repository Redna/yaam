import ladybug as lb
import os

DB_PATH = "./memory.lbug"

def get_connection():
    db = lb.Database(DB_PATH)
    return lb.Connection(db)

def setup_database():
    """Initializes the LadybugDB schema for YAAM."""
    conn = get_connection()
    
    # Helper to execute and ignore 'Table already exists' errors
    def safe_execute(query):
        try:
            conn.execute(query)
        except Exception as e:
            if "already exists" in str(e).lower():
                pass
            else:
                print(f"Error executing query: {query}\n{e}")

    # Layer 0: The Structural File System / AST Topology
    safe_execute("CREATE NODE TABLE Entity(id STRING, type STRING, status STRING, last_modified INT64, metadata STRING, PRIMARY KEY (id))")
    safe_execute("CREATE REL TABLE LINKED_TO(FROM Entity TO Entity, relationship_type STRING)")

    # Layer 1: The Agent Operational Context & Memory
    safe_execute("CREATE NODE TABLE Workspace(workspace_name STRING, description STRING, status STRING, closed_at INT64, PRIMARY KEY (workspace_name))")
    safe_execute("CREATE NODE TABLE Scratchpad(id STRING, content STRING, created_at INT64, PRIMARY KEY (id))")

    # Cross-Layer Mappings (Polymorphic Target Support)
    safe_execute("CREATE REL TABLE MAPPED_TO(FROM Workspace TO Entity, created_at INT64, invalidated_at INT64, is_stale BOOLEAN)")
    safe_execute("CREATE REL TABLE HAS_SCRATCHPAD(FROM Workspace TO Scratchpad)")
    
    print("Database schema setup complete.")

if __name__ == "__main__":
    setup_database()
