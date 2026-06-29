//! Append-only JSONL event persistence.
//!
//! Events are stored one-per-line in `events.jsonl`. Writes use OS-level
//! exclusive file locking (`fs2`) to prevent concurrent corruption.
//! Replay reads the file top-to-bottom, skipping malformed lines.

use crate::types::{
    DeleteEdgesPayload, DeleteNodePayload, Event, EventPayload, EventType, LinkNodesPayload,
    UpsertNodePayload, EVENT_VERSION,
};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Constants ──────────────────────────────────────────────────────────────

const EVENTS_FILE: &str = "events.jsonl";

// ─── Storage Handle ─────────────────────────────────────────────────────────

/// Handle for the JSONL event store rooted at a given directory.
#[derive(Debug, Clone)]
pub struct EventStore {
    /// Path to the `events.jsonl` file.
    path: PathBuf,
}

impl EventStore {
    /// Create a new `EventStore` that persists events inside `dir`.
    ///
    /// The directory is created if it does not already exist.
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join(EVENTS_FILE),
        })
    }

    /// Return the path to the underlying `events.jsonl` file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ── Append ──────────────────────────────────────────────────────────

    /// Serialize `event` as a single JSON line and append it to the store.
    ///
    /// An exclusive (`LOCK_EX`) file lock is held for the duration of the
    /// write and is released before this function returns.
    pub fn append(&self, event: &Event) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        // Acquire exclusive lock — blocks until available.
        file.lock_exclusive()?;

        let result = (|| -> std::io::Result<()> {
            let mut line = serde_json::to_string(event)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
            file.flush()?;
            Ok(())
        })();

        // Always unlock, even on write failure.
        file.unlock()?;
        result
    }

    // ── Replay ──────────────────────────────────────────────────────────

    /// Read every event from the store in insertion order.
    ///
    /// Malformed lines (e.g. from a crash mid-write) are skipped with a
    /// warning printed to stderr.  Returns all successfully parsed events.
    pub fn replay(&self) -> std::io::Result<Vec<Event>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = OpenOptions::new().read(true).open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for (line_no, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(event) => events.push(event),
                Err(err) => {
                    eprintln!(
                        "WARNING: skipping malformed event at {}:{}: {}",
                        self.path.display(),
                        line_no + 1,
                        err,
                    );
                }
            }
        }

        Ok(events)
    }
}

// ─── Event Construction Helpers ─────────────────────────────────────────────

/// Current Unix-epoch timestamp in seconds.
fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

/// Create an `UPSERT_NODE` event.
pub fn upsert_node_event(
    id: impl Into<String>,
    label: impl Into<String>,
    properties: HashMap<String, serde_json::Value>,
) -> Event {
    Event {
        version: EVENT_VERSION,
        timestamp: now_ts(),
        event_type: EventType::UpsertNode,
        payload: EventPayload::UpsertNode(UpsertNodePayload {
            id: id.into(),
            label: label.into(),
            properties,
        }),
    }
}

/// Create a `LINK_NODES` event.
pub fn link_nodes_event(
    from_id: impl Into<String>,
    to_id: impl Into<String>,
    relationship: impl Into<String>,
    properties: HashMap<String, serde_json::Value>,
) -> Event {
    Event {
        version: EVENT_VERSION,
        timestamp: now_ts(),
        event_type: EventType::LinkNodes,
        payload: EventPayload::LinkNodes(LinkNodesPayload {
            from_id: from_id.into(),
            to_id: to_id.into(),
            relationship: relationship.into(),
            properties,
        }),
    }
}

/// Create a `DELETE_NODE` event.
pub fn delete_node_event(id: impl Into<String>) -> Event {
    Event {
        version: EVENT_VERSION,
        timestamp: now_ts(),
        event_type: EventType::DeleteNode,
        payload: EventPayload::DeleteNode(DeleteNodePayload { id: id.into() }),
    }
}

/// Create a `DELETE_EDGES` event.
pub fn delete_edges_event(
    from_id: impl Into<String>,
    direction: impl Into<String>,
) -> Event {
    Event {
        version: EVENT_VERSION,
        timestamp: now_ts(),
        event_type: EventType::DeleteEdges,
        payload: EventPayload::DeleteEdges(DeleteEdgesPayload {
            from_id: from_id.into(),
            direction: direction.into(),
        }),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    /// Helper: build a store in a fresh temp directory.
    fn temp_store() -> (TempDir, EventStore) {
        let dir = TempDir::new().expect("failed to create temp dir");
        let store = EventStore::new(dir.path()).expect("failed to create EventStore");
        (dir, store)
    }

    #[test]
    fn roundtrip_append_and_replay() {
        let (_dir, store) = temp_store();

        let e1 = upsert_node_event("n1", "Entity", HashMap::new());
        let e2 = link_nodes_event("n1", "n2", "CALLS", HashMap::new());
        let e3 = delete_node_event("n1");
        let e4 = delete_edges_event("n1", "outbound");

        store.append(&e1).unwrap();
        store.append(&e2).unwrap();
        store.append(&e3).unwrap();
        store.append(&e4).unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events.len(), 4);

        // Verify event types round-tripped correctly.
        assert_eq!(events[0].event_type, EventType::UpsertNode);
        assert_eq!(events[1].event_type, EventType::LinkNodes);
        assert_eq!(events[2].event_type, EventType::DeleteNode);
        assert_eq!(events[3].event_type, EventType::DeleteEdges);

        // Verify version is preserved.
        for ev in &events {
            assert_eq!(ev.version, EVENT_VERSION);
        }
    }

    #[test]
    fn corrupt_line_recovery() {
        let (_dir, store) = temp_store();

        // Write a valid event.
        let e1 = upsert_node_event("good1", "Entity", HashMap::new());
        store.append(&e1).unwrap();

        // Manually inject a corrupt line.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(store.path())
                .unwrap();
            writeln!(f, "{{not valid json!!!").unwrap();
        }

        // Write another valid event.
        let e2 = delete_node_event("good2");
        store.append(&e2).unwrap();

        let events = store.replay().unwrap();
        assert_eq!(
            events.len(),
            2,
            "should skip the corrupt line and return 2 valid events"
        );
        assert_eq!(events[0].event_type, EventType::UpsertNode);
        assert_eq!(events[1].event_type, EventType::DeleteNode);
    }

    #[test]
    fn replay_empty_store() {
        let (_dir, store) = temp_store();
        let events = store.replay().unwrap();
        assert!(events.is_empty(), "empty store should replay zero events");
    }

    #[test]
    fn file_lock_released_after_append() {
        let (_dir, store) = temp_store();

        let e = upsert_node_event("n1", "Entity", HashMap::new());
        store.append(&e).unwrap();

        // If the lock were still held, this second append would deadlock or
        // error.  Succeeding here proves the lock was released.
        let e2 = upsert_node_event("n2", "Entity", HashMap::new());
        store.append(&e2).unwrap();

        // Also verify we can acquire an exclusive lock from a separate handle
        // (further proof the previous lock was dropped).
        let f = OpenOptions::new()
            .read(true)
            .open(store.path())
            .unwrap();
        f.lock_exclusive().expect("lock should be available");
        f.unlock().unwrap();

        let events = store.replay().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn event_helpers_set_timestamps() {
        let before = now_ts();
        let e = upsert_node_event("x", "Entity", HashMap::new());
        let after = now_ts();

        assert!(
            e.timestamp >= before && e.timestamp <= after,
            "timestamp should be within the test window"
        );
    }
}
