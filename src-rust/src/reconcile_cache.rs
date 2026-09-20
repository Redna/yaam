//! Content-addressed reconcile cache (Leg 1).
//!
//! Stores, per source file, a compact **current state** entry keyed by content
//! hash: the declaration events the reconciler emitted, the LSP-resolved
//! reference edges, and the content hashes of the files its references resolved
//! into (`deps`). A valid entry lets `handle_reconcile` replay the graph with
//! **no tree-sitter parse, no reference queueing and no LSP round-trip**.
//!
//! Everything here is regenerable: deleting the cache file loses nothing but
//! work, and every IO path is best-effort (a cache write must never fail a
//! reconcile).
//!
//! Switches:
//! - `YAAM_RECONCILE_CACHE=off` — skip reads (bypass); writes still rebuild.
//! - `YAAM_RECONCILE_CACHE_DIR` — override the `.yaam` location (tests).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{
    DeleteNodePayload, Event, EventPayload, EventType, LinkNodesPayload, EVENT_VERSION,
};

/// On-disk format version. Bump for any incompatible change.
pub const CACHE_VERSION: u32 = 1;
/// File name written under the cache directory.
pub const CACHE_FILE: &str = "reconcile-cache.json";

/// A reference edge that the LSP resolved. This is the cache-side twin of the
/// `LinkNodes` event `resolve_reference_sync` emits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedRef {
    pub ref_name: String,
    pub ref_type: String,
    pub from_id: String,
    pub to_id: String,
    pub line: u32,
    pub col: u32,
}

/// One source file's cached state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// `sha256:<hex>` of the file content this entry was built from.
    pub hash: String,
    /// Resolved reference target path -> that file's content hash at cache time.
    #[serde(default)]
    pub deps: HashMap<String, String>,
    /// The reconciler's declaration events (post-embedding), excluding the
    /// per-reconcile `DeleteNode` events (those are recomputed from the graph).
    #[serde(default)]
    pub decls: Vec<Event>,
    /// LSP-resolved reference edges.
    #[serde(default)]
    pub refs: Vec<CachedRef>,
    /// References queued but not yet resolved. An entry is only usable once
    /// this reaches zero — otherwise its resolved edges are incomplete.
    #[serde(default)]
    pub refs_pending: u32,
}

/// Top-level cache document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileCache {
    pub version: u32,
    pub parser: String,
    #[serde(default)]
    pub files: HashMap<String, CacheEntry>,
}

impl Default for ReconcileCache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            parser: parser_tag(),
            files: HashMap::new(),
        }
    }
}

/// Identifies the parser/resolver configuration that produced the entries.
/// A change invalidates every entry (checked on load and on lookup).
pub fn parser_tag() -> String {
    format!(
        "yaam-engine@{};lsp={}",
        env!("CARGO_PKG_VERSION"),
        crate::language_adapter::lsp_resolver_enabled()
    )
}

/// Stable content hash. `sha2` is already a dependency; `DefaultHasher` is
/// deliberately avoided (it is not stable across builds).
pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest.iter() {
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

/// Normalise a request path into the cache key space (relative to `root`).
pub fn rel_key(file_path: &str, root: &Path) -> String {
    let p = Path::new(file_path);
    if p.is_absolute() {
        p.strip_prefix(root)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| file_path.to_string())
    } else {
        file_path.to_string()
    }
}

/// Pure tag check: version + parser must match the running engine.
pub fn tags_match(cache: &ReconcileCache, parser: &str) -> bool {
    cache.version == CACHE_VERSION && cache.parser == parser
}

/// Pure validity check.
///
/// An entry is usable iff it is complete (no references still pending), its
/// stored content hash matches the file's, and every dependency still hashes to
/// its stored value. `dep_hash` returns `None` for a missing file, which
/// invalidates the entry (a pruned/deleted dep invalidates its dependents).
pub fn entry_is_valid(
    entry: &CacheEntry,
    current_hash: &str,
    dep_hash: &dyn Fn(&str) -> Option<String>,
) -> bool {
    if entry.refs_pending != 0 {
        return false;
    }
    if entry.hash != current_hash {
        return false;
    }
    for (path, stored) in &entry.deps {
        match dep_hash(path) {
            Some(current) if current == *stored => {}
            _ => return false,
        }
    }
    true
}

/// Drop entries for files that no longer exist. Returns the number pruned.
pub fn prune_missing(cache: &mut ReconcileCache, exists: &dyn Fn(&str) -> bool) -> usize {
    let before = cache.files.len();
    cache.files.retain(|rel, _| exists(rel));
    before - cache.files.len()
}

/// Outcome of a background reference resolution, used to update the cache.
pub enum Resolution {
    /// LSP is disabled/unavailable — leave the entry incomplete so it is
    /// re-attempted when the resolver is available.
    LspUnavailable,
    /// Resolved to nothing (no definition). Counts as resolved (no edge).
    Unresolved,
    /// Resolved to a target. `dep_path` is the target's file, when it is a
    /// different file (the dependency that must stay unchanged for this answer
    /// to stay valid).
    Resolved {
        to_id: String,
        dep_path: Option<String>,
    },
}

/// Build the events a cache hit replays: freshly computed declaration deletes,
/// then the cached declaration events (timestamps refreshed), then the cached
/// resolved reference edges.
pub fn replay_events(
    file_path: &Path,
    decls: &[Event],
    refs: &[CachedRef],
    engine: &crate::graph::MemoryEngine,
) -> Vec<Event> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let file_id = file_path.display().to_string();
    let mut events = Vec::with_capacity(decls.len() + refs.len() + 4);

    // Same deletion pass `reconcile_file` performs: drop this file's previous
    // declarations so a stale graph cannot keep ghosts alive.
    for edge in engine.get_reverse_edges(&file_id) {
        if edge.relationship == "DECLARED_IN" {
            events.push(Event {
                version: EVENT_VERSION,
                timestamp: now,
                event_type: EventType::DeleteNode,
                payload: EventPayload::DeleteNode(DeleteNodePayload {
                    id: edge.from_id.clone(),
                }),
            });
        }
    }

    for decl in decls {
        let mut event = decl.clone();
        event.timestamp = now;
        if let EventPayload::UpsertNode(ref mut payload) = event.payload {
            payload
                .properties
                .insert("last_modified".to_string(), serde_json::json!(now));
        }
        events.push(event);
    }

    for r in refs {
        events.push(Event {
            version: EVENT_VERSION,
            timestamp: now,
            event_type: EventType::LinkNodes,
            payload: EventPayload::LinkNodes(LinkNodesPayload {
                from_id: r.from_id.clone(),
                to_id: r.to_id.clone(),
                relationship: r.ref_type.clone(),
                properties: HashMap::new(),
            }),
        });
    }

    events
}

/// In-memory handle around the on-disk cache, held behind a `Mutex` on
/// `AppState`. All IO is wrapped: failures log and are otherwise ignored.
pub struct CacheHandle {
    path: PathBuf,
    root: PathBuf,
    parser: String,
    read_enabled: bool,
    cache: ReconcileCache,
    dirty: bool,
}

impl CacheHandle {
    /// Open (or start) the cache for `root`.
    pub fn open(root: PathBuf) -> Self {
        let read_enabled = std::env::var("YAAM_RECONCILE_CACHE")
            .map(|v| !v.eq_ignore_ascii_case("off"))
            .unwrap_or(true);
        let parser = parser_tag();
        let dir = match std::env::var("YAAM_RECONCILE_CACHE_DIR") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => root.join(".yaam"),
        };
        let path = dir.join(CACHE_FILE);
        let cache = load(&path, &parser);
        Self {
            path,
            root,
            parser,
            read_enabled,
            cache,
            dirty: false,
        }
    }

    pub fn read_enabled(&self) -> bool {
        self.read_enabled
    }

    pub fn len(&self) -> usize {
        self.cache.files.len()
    }

    /// Return a usable entry for `rel` with `content`, or `None` on any miss.
    pub fn lookup(&self, rel: &str, content: &str) -> Option<CacheEntry> {
        if !self.read_enabled {
            return None;
        }
        if !tags_match(&self.cache, &self.parser) {
            return None;
        }
        let entry = self.cache.files.get(rel)?.clone();
        let hash = content_hash(content);
        let root = self.root.clone();
        let dep_hash = move |p: &str| -> Option<String> {
            let abs = if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                root.join(p)
            };
            std::fs::read_to_string(abs).ok().map(|c| content_hash(&c))
        };
        if entry_is_valid(&entry, &hash, &dep_hash) {
            Some(entry)
        } else {
            None
        }
    }

    /// Replace an entry with fresh declarations. Resolved edges are recorded
    /// later, as the background worker produces them.
    pub fn put_decls(&mut self, rel: &str, content: &str, decls: Vec<Event>, refs_pending: u32) {
        self.cache.files.insert(
            rel.to_string(),
            CacheEntry {
                hash: content_hash(content),
                deps: HashMap::new(),
                decls,
                refs: Vec::new(),
                refs_pending,
            },
        );
        self.dirty = true;
    }

    /// Record one background resolution: decrement the pending counter, add the
    /// edge when there is one, and remember the dependency hash.
    pub fn record_resolution(
        &mut self,
        rel: &str,
        edge: Option<CachedRef>,
        dep: Option<(String, String)>,
    ) {
        let Some(entry) = self.cache.files.get_mut(rel) else {
            return;
        };
        if entry.refs_pending > 0 {
            entry.refs_pending -= 1;
        }
        if let Some(edge) = edge {
            entry.refs.push(edge);
        }
        if let Some((path, hash)) = dep {
            entry.deps.insert(path, hash);
        }
        self.dirty = true;
    }

    /// Best-effort atomic flush. Never panics, never returns an error.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let root = self.root.clone();
        let pruned = prune_missing(&mut self.cache, &|rel: &str| {
            let abs = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                root.join(rel)
            };
            abs.exists()
        });
        if pruned > 0 {
            eprintln!("[yaam] reconcile-cache: pruned {} missing file(s)", pruned);
        }

        if let Some(parent) = self.path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "[yaam] reconcile-cache: cannot create {} — {}",
                    parent.display(),
                    e
                );
                return;
            }
        }
        let json = match serde_json::to_string(&self.cache) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[yaam] reconcile-cache: serialize failed — {}", e);
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json) {
            eprintln!("[yaam] reconcile-cache: write failed — {}", e);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            eprintln!("[yaam] reconcile-cache: rename failed — {}", e);
            return;
        }
        self.dirty = false;
    }
}

/// Load the cache, treating a version/parser mismatch or an unreadable file as
/// "empty" (a mismatch invalidates everything).
fn load(path: &Path, parser: &str) -> ReconcileCache {
    let empty = || ReconcileCache {
        version: CACHE_VERSION,
        parser: parser.to_string(),
        files: HashMap::new(),
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return empty(),
    };
    match serde_json::from_str::<ReconcileCache>(&raw) {
        Ok(c) if c.version == CACHE_VERSION && c.parser == parser => c,
        Ok(c) => {
            eprintln!(
                "[yaam] reconcile-cache: version/parser mismatch ({} / {}) at {} — invalidated",
                c.version,
                c.parser,
                path.display()
            );
            empty()
        }
        Err(e) => {
            eprintln!(
                "[yaam] reconcile-cache: unreadable {} ({}) — starting empty",
                path.display(),
                e
            );
            empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str) -> Event {
        Event {
            version: EVENT_VERSION,
            timestamp: 1,
            event_type: EventType::UpsertNode,
            payload: EventPayload::UpsertNode(crate::types::UpsertNodePayload {
                id: id.to_string(),
                label: "Entity".to_string(),
                properties: HashMap::new(),
            }),
        }
    }

    fn entry(content: &str, deps: &[(&str, &str)], pending: u32) -> CacheEntry {
        CacheEntry {
            hash: content_hash(content),
            deps: deps
                .iter()
                .map(|(p, c)| (p.to_string(), content_hash(c)))
                .collect(),
            decls: vec![event("a.ts:helper")],
            refs: Vec::new(),
            refs_pending: pending,
        }
    }

    fn deps_from(map: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        move |p: &str| map.get(p).cloned()
    }

    #[test]
    fn valid_entry_hits() {
        let content = "export function helper() {}";
        let mut deps = HashMap::new();
        deps.insert("a.ts".to_string(), content_hash("export function helper() {}"));
        let e = entry(content, &[("a.ts", "export function helper() {}")], 0);
        assert!(entry_is_valid(&e, &content_hash(content), &deps_from(&deps)));
    }

    #[test]
    fn content_change_misses() {
        let e = entry("export function helper() {}", &[], 0);
        assert!(!entry_is_valid(
            &e,
            &content_hash("export function helper(x) {}"),
            &|_| None
        ));
    }

    #[test]
    fn dep_hash_change_misses() {
        let e = entry("import { helper } from './a'", &[("a.ts", "v1")], 0);
        let mut deps = HashMap::new();
        deps.insert("a.ts".to_string(), content_hash("v2"));
        assert!(!entry_is_valid(
            &e,
            &content_hash("import { helper } from './a'"),
            &deps_from(&deps)
        ));
    }

    #[test]
    fn missing_dep_misses() {
        let e = entry("import { helper } from './a'", &[("a.ts", "v1")], 0);
        assert!(!entry_is_valid(
            &e,
            &content_hash("import { helper } from './a'"),
            &|_| None
        ));
    }

    #[test]
    fn pending_refs_miss() {
        let e = entry("const x = helper()", &[], 2);
        assert!(!entry_is_valid(
            &e,
            &content_hash("const x = helper()"),
            &|_| None
        ));
    }

    #[test]
    fn version_or_parser_change_misses() {
        let good = ReconcileCache {
            version: CACHE_VERSION,
            parser: "yaam-engine@0.1.0;lsp=true".to_string(),
            files: HashMap::new(),
        };
        assert!(tags_match(&good, "yaam-engine@0.1.0;lsp=true"));

        let mut bad_version = good.clone();
        bad_version.version = CACHE_VERSION + 1;
        assert!(!tags_match(&bad_version, "yaam-engine@0.1.0;lsp=true"));

        let mut bad_parser = good;
        bad_parser.parser = "tsserver@4.0.0".to_string();
        assert!(!tags_match(&bad_parser, "yaam-engine@0.1.0;lsp=true"));
        assert!(!tags_match(&bad_parser, "yaam-engine@0.1.0;lsp=false"));
    }

    #[test]
    fn prune_drops_deleted_file() {
        let mut cache = ReconcileCache {
            version: CACHE_VERSION,
            parser: parser_tag(),
            files: HashMap::new(),
        };
        cache
            .files
            .insert("a.ts".to_string(), entry("a", &[], 0));
        cache
            .files
            .insert("b.ts".to_string(), entry("b", &[], 0));
        let pruned = prune_missing(&mut cache, &|rel: &str| rel == "a.ts");
        assert_eq!(pruned, 1);
        assert!(cache.files.contains_key("a.ts"));
        assert!(!cache.files.contains_key("b.ts"));
    }

    #[test]
    fn hash_is_stable_and_prefixed() {
        let h1 = content_hash("hello");
        let h2 = content_hash("hello");
        assert_eq!(h1, h2);
        assert_ne!(h1, content_hash("hello "));
        assert!(h1.starts_with("sha256:"));
        assert_eq!(h1.len(), 7 + 64);
    }
}
