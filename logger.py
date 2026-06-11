import json
import time

HISTORY_FILE = "history.jsonl"

def log_event(tool_name, args):
    """Logs an event to the append-only history ledger."""
    event = {
        "timestamp": int(time.time()),
        "tool": tool_name,
        "args": args
    }
    with open(HISTORY_FILE, "a", encoding="utf-8") as f:
        f.write(json.dumps(event) + "\n")

def get_history():
    """Returns the history of events."""
    if not os.path.exists(HISTORY_FILE):
        return []
    with open(HISTORY_FILE, "r", encoding="utf-8") as f:
        return [json.loads(line) for line in f]
