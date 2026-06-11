import math
import time
from db import get_connection

def evaluate_scratchpad_decay(notes: list[dict], decay_constant: float = 0.05) -> list[dict]:
    """Filters and ranks scratchpad memories based on exponential time-decay."""
    current_time_hours = time.time() / 3600
    active_payload = []
    for note in notes:
        # Expecting created_at as a timestamp in seconds
        created_hours = note["created_at"] / 3600
        delta_t = current_time_hours - created_hours
        weight = math.exp(-decay_constant * delta_t)
        if weight >= 0.1:
            note["recency_weight"] = round(weight, 2)
            active_payload.append(note)
    return sorted(active_payload, key=lambda x: x["recency_weight"], reverse=True)

def get_active_memory(workspace_name: str):
    """Fetches and filters active scratchpad notes for a given workspace."""
    conn = get_connection()
    query = f"""
    MATCH (w:Workspace {{workspace_name: '{workspace_name}'}})-[:HAS_SCRATCHPAD]->(s:Scratchpad)
    RETURN s.id, s.content, s.created_at
    """
    results = conn.execute(query)
    notes = []
    while results.has_next():
        row = results.get_next()
        notes.append({
            "id": row[0],
            "content": row[1],
            "created_at": row[2]
        })
    
    return evaluate_scratchpad_decay(notes)

def search_entities(text_query: str):
    """Placeholder for Hybrid Retrieval: FTS + 2-Hop Path Expansion."""
    # Note: Kuzu FTS requires index setup. For this blueprint, we'll use a basic MATCH.
    conn = get_connection()
    # Simple contains match as a fallback for FTS
    query = f"""
    MATCH (e:Entity)
    WHERE e.id CONTAINS '{text_query}' OR e.metadata CONTAINS '{text_query}'
    RETURN e.id, e.type, e.status
    LIMIT 20
    """
    results = conn.execute(query)
    # ... logic for 2-hop expansion would go here
    return results.get_all_rows()
