//! ANN (Approximate Nearest Neighbor) index for dense vector search (Spec #1).
//!
//! Provides an indexed search over embedding vectors, replacing the O(n) linear
//! scan over all graph nodes in `handle_search()`. The index handles multi-chunk
//! embeddings by flattening them with composite keys and tracking the mapping
//! back to node IDs.
//!
//! ## Current Implementation: Flat Index
//!
//! The current implementation uses a flat (brute-force) index that computes
//! cosine similarity against all stored vectors. This is exact (not approximate)
//! and sufficient for datasets up to ~5,000 vectors where latency is < 5ms.
//!
//! The interface is designed to be upgradeable to HNSW (Hierarchical Navigable
//! Small World) when scale demands it. The upgrade path is:
//! - Replace `FlatIndex` with an HNSW graph internally
//! - Keep the same `add`, `remove`, `search` API
//! - HNSW parameters: M=16, ef_construction=200, ef_search=max(50, top_k×5)
//!
//! ## Multi-Chunk Embedding Handling
//!
//! Each `MemoryNode` stores `embedding: Option<Vec<Vec<f32>>>` — a list of chunk
//! vectors. The ANN index flattens these with composite keys:
//!
//! ```text
//! ann_key = "{node_id}#{chunk_index}"
//! ```
//!
//! A secondary map `key_to_node` resolves results back to nodes. After retrieving
//! top-k ANN hits, the caller groups by `node_id` and takes the max similarity
//! per node, preserving the current multi-chunk semantics.

use std::collections::HashMap;

/// A flattened entry in the ANN index.
#[derive(Clone)]
struct FlatEntry {
    /// Composite key: "{node_id}#{chunk_index}"
    key: String,
    /// The embedding vector (384-dim for gte-small)
    vector: Vec<f32>,
}

/// ANN index for dense vector search (Spec #1).
///
/// Stores embedding vectors with composite keys for multi-chunk handling.
/// Provides add/remove/search operations that are used by the search handler
/// to replace the linear scan over all graph nodes.
pub struct AnnIndex {
    /// All stored vectors with their composite keys.
    entries: Vec<FlatEntry>,

    /// Reverse map: composite key → (node_id, chunk_index)
    key_to_node: HashMap<String, (String, usize)>,

    /// Forward map: node_id → list of composite keys (for removal on upsert/delete)
    node_to_keys: HashMap<String, Vec<String>>,

    /// Embedding dimension (384 for gte-small)
    dim: usize,
}

impl AnnIndex {
    /// Create a new, empty ANN index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            key_to_node: HashMap::new(),
            node_to_keys: HashMap::new(),
            dim: 0,
        }
    }

    /// Add (or replace) all chunk vectors for a node.
    ///
    /// Removes any existing entries for `node_id` first, then inserts the new
    /// chunk vectors with composite keys `"{node_id}#{chunk_index}"`.
    pub fn add(&mut self, node_id: &str, embeddings: &[Vec<f32>]) {
        // Remove existing entries for this node
        self.remove(node_id);

        if embeddings.is_empty() {
            return;
        }

        // Detect dimension from first embedding
        if self.dim == 0 && !embeddings[0].is_empty() {
            self.dim = embeddings[0].len();
        }

        let mut keys = Vec::with_capacity(embeddings.len());
        for (chunk_idx, vector) in embeddings.iter().enumerate() {
            let key = format!("{}#{}", node_id, chunk_idx);
            self.key_to_node
                .insert(key.clone(), (node_id.to_string(), chunk_idx));
            self.entries.push(FlatEntry {
                key: key.clone(),
                vector: vector.clone(),
            });
            keys.push(key);
        }
        self.node_to_keys.insert(node_id.to_string(), keys);
    }

    /// Remove all chunk vectors for a node from the index.
    pub fn remove(&mut self, node_id: &str) {
        if let Some(keys) = self.node_to_keys.remove(node_id) {
            for key in &keys {
                self.key_to_node.remove(key);
            }
            // Remove from entries (retain only entries whose key is not in the removed set)
            let key_set: std::collections::HashSet<&str> =
                keys.iter().map(|s| s.as_str()).collect();
            self.entries.retain(|e| !key_set.contains(e.key.as_str()));
        }
    }

    /// Search for the top-k nearest vectors by cosine similarity.
    ///
    /// Returns `(composite_key, similarity)` pairs sorted by similarity descending.
    /// The caller should group by `node_id` (via `key_to_node`) and take the max
    /// similarity per node to preserve multi-chunk semantics.
    ///
    /// For L2-normalized vectors (which gte-small produces), cosine similarity
    /// equals the dot product.
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<(String, f32)> {
        if self.entries.is_empty() || query.is_empty() {
            return Vec::new();
        }

        // Compute cosine similarity (dot product for normalized vectors) for all entries
        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .map(|e| {
                let sim = cosine_similarity(query, &e.vector);
                (e.key.clone(), sim)
            })
            .collect();

        // Sort by similarity descending
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        scored.truncate(top_k);
        scored
    }

    /// Resolve a composite key back to its (node_id, chunk_index).
    pub fn resolve_key(&self, key: &str) -> Option<&(String, usize)> {
        self.key_to_node.get(key)
    }

    /// Number of nodes in the index.
    pub fn node_count(&self) -> usize {
        self.node_to_keys.len()
    }

    /// Total number of vectors (chunks) in the index.
    pub fn vector_count(&self) -> usize {
        self.entries.len()
    }

    /// Embedding dimension.
    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Rebuild the index from a set of (node_id, embeddings) pairs.
    /// Used at daemon startup to populate the index from the graph.
    pub fn rebuild_from(&mut self, nodes: impl Iterator<Item = (String, Vec<Vec<f32>>)>) {
        self.entries.clear();
        self.key_to_node.clear();
        self.node_to_keys.clear();
        self.dim = 0;

        for (node_id, embeddings) in nodes {
            self.add(&node_id, &embeddings);
        }
    }
}

impl Default for AnnIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Cosine similarity for L2-normalized vectors (equivalent to dot product).
///
/// Since gte-small embeddings are L2-normalized at embedding time
/// (`embedding.rs` → `embed()`), cosine similarity = dot product.
/// We use the dot product directly for efficiency.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| x * y)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_index_search() {
        let index = AnnIndex::new();
        let results = index.search(&[1.0, 0.0, 0.0], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_add_and_search_single_chunk() {
        let mut index = AnnIndex::new();
        index.add("node_a", &[vec![1.0, 0.0, 0.0]]);
        index.add("node_b", &[vec![0.0, 1.0, 0.0]]);
        index.add("node_c", &[vec![0.9, 0.1, 0.0]]);

        // Query closest to node_a
        let results = index.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "node_a#0");
        assert!(results[0].1 > 0.99); // Near-exact match
        // node_c should be second (cosine sim ~0.99 with [1,0,0])
        assert_eq!(results[1].0, "node_c#0");
    }

    #[test]
    fn test_add_and_search_multi_chunk() {
        let mut index = AnnIndex::new();
        // Node with 3 chunks
        index.add(
            "doc_md:Section",
            &[
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ],
        );
        index.add("simple_fn", &[vec![0.9, 0.1, 0.0]]);

        // Query closest to chunk 0 of the multi-chunk node
        let results = index.search(&[1.0, 0.0, 0.0], 5);
        assert_eq!(results.len(), 4); // 3 chunks + 1 simple_fn

        // Top result should be chunk 0 of the multi-chunk node
        assert_eq!(results[0].0, "doc_md:Section#0");

        // Verify key resolution
        let (node_id, chunk_idx) = index.resolve_key("doc_md:Section#1").unwrap();
        assert_eq!(node_id, "doc_md:Section");
        assert_eq!(*chunk_idx, 1);
    }

    #[test]
    fn test_remove_node() {
        let mut index = AnnIndex::new();
        index.add("node_a", &[vec![1.0, 0.0, 0.0]]);
        index.add("node_b", &[vec![0.0, 1.0, 0.0]]);
        assert_eq!(index.node_count(), 2);
        assert_eq!(index.vector_count(), 2);

        index.remove("node_a");
        assert_eq!(index.node_count(), 1);
        assert_eq!(index.vector_count(), 1);

        // Search should not return node_a
        let results = index.search(&[1.0, 0.0, 0.0], 10);
        assert!(results.iter().all(|(k, _)| !k.starts_with("node_a")));
    }

    #[test]
    fn test_remove_multi_chunk_node() {
        let mut index = AnnIndex::new();
        index.add("multi", &[vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]]);
        assert_eq!(index.vector_count(), 3);

        index.remove("multi");
        assert_eq!(index.vector_count(), 0);
        assert!(index.resolve_key("multi#0").is_none());
        assert!(index.resolve_key("multi#1").is_none());
        assert!(index.resolve_key("multi#2").is_none());
    }

    #[test]
    fn test_replace_on_re_add() {
        let mut index = AnnIndex::new();
        index.add("node_a", &[vec![1.0, 0.0, 0.0]]);
        assert_eq!(index.vector_count(), 1);

        // Re-add with different vectors (e.g., content changed → different chunks)
        index.add("node_a", &[vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]]);
        assert_eq!(index.vector_count(), 2); // Old vector replaced, 2 new ones

        // Old vector should not be found
        let results = index.search(&[1.0, 0.0, 0.0], 10);
        // node_a#0 should now be [0,1,0] (cosine sim 0 with [1,0,0])
        assert_eq!(results[0].0, "node_a#0");
        assert!(results[0].1.abs() < 0.01); // Near-zero similarity
    }

    #[test]
    fn test_remove_nonexistent_is_noop() {
        let mut index = AnnIndex::new();
        index.add("node_a", &[vec![1.0, 0.0]]);
        index.remove("nonexistent");
        assert_eq!(index.node_count(), 1);
        assert_eq!(index.vector_count(), 1);
    }

    #[test]
    fn test_add_empty_embeddings() {
        let mut index = AnnIndex::new();
        index.add("node_a", &[]);
        assert_eq!(index.node_count(), 0);
        assert_eq!(index.vector_count(), 0);
    }

    #[test]
    fn test_resolve_key() {
        let mut index = AnnIndex::new();
        index.add("src/main.rs::foo", &[vec![1.0, 0.0], vec![0.0, 1.0]]);

        let (node_id, chunk_idx) = index.resolve_key("src/main.rs::foo#0").unwrap();
        assert_eq!(node_id, "src/main.rs::foo");
        assert_eq!(*chunk_idx, 0);

        let (node_id, chunk_idx) = index.resolve_key("src/main.rs::foo#1").unwrap();
        assert_eq!(node_id, "src/main.rs::foo");
        assert_eq!(*chunk_idx, 1);

        assert!(index.resolve_key("nonexistent#0").is_none());
    }

    #[test]
    fn test_rebuild_from() {
        let mut index = AnnIndex::new();
        index.add("temp", &[vec![1.0, 0.0]]);

        // Rebuild from a new set of nodes
        index.rebuild_from(
            vec![
                ("node_a".to_string(), vec![vec![1.0, 0.0, 0.0]]),
                ("node_b".to_string(), vec![vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]]),
            ]
            .into_iter(),
        );

        assert_eq!(index.node_count(), 2);
        assert_eq!(index.vector_count(), 3);
        assert!(index.resolve_key("temp#0").is_none()); // Old entry gone
        assert!(index.resolve_key("node_a#0").is_some());
        assert!(index.resolve_key("node_b#1").is_some());
    }

    #[test]
    fn test_search_returns_composite_keys() {
        let mut index = AnnIndex::new();
        index.add("a", &[vec![1.0, 0.0]]);
        index.add("b", &[vec![0.9, 0.1]]);

        let results = index.search(&[1.0, 0.0], 2);
        // All results should have composite keys
        assert!(results[0].0.contains('#'));
        assert!(results[1].0.contains('#'));
    }

    #[test]
    fn test_dimension_detection() {
        let mut index = AnnIndex::new();
        assert_eq!(index.dimension(), 0);

        index.add("node", &[vec![1.0; 384]]);
        assert_eq!(index.dimension(), 384);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_large_scale_performance() {
        // Simulate 5000 nodes with 384-dim vectors (the spec's validation target)
        let mut index = AnnIndex::new();
        for i in 0..5000 {
            // Generate pseudo-random 384-dim vector
            let vector: Vec<f32> = (0..384)
                .map(|j| {
                    let val = ((i * 384 + j) as f32).sin();
                    val
                })
                .collect();
            // Normalize
            let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
            let normalized: Vec<f32> = vector.iter().map(|v| v / norm).collect();
            index.add(&format!("node_{}", i), &[normalized]);
        }

        assert_eq!(index.node_count(), 5000);
        assert_eq!(index.vector_count(), 5000);

        // Create a query vector
        let query: Vec<f32> = (0..384)
            .map(|j| (j as f32 * 0.1).sin())
            .collect();
        let norm: f32 = query.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        let query: Vec<f32> = query.iter().map(|v| v / norm).collect();

        let start = std::time::Instant::now();
        let results = index.search(&query, 50);
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 50);
        // Spec validation: < 5ms at 5000 nodes
        // Note: This may vary by hardware. The flat index is O(n*d) = 5000*384 = 1.92M FMA ops.
        // On modern hardware with SIMD, this completes in ~1-3ms.
        // We log the actual time rather than asserting, since CI environments vary.
        eprintln!("[AnnIndex] 5000-vector search: {:?}", elapsed);
    }
}