//! BM25 keyword search module.
//!
//! Provides a Unicode-aware tokenizer and a BM25 inverted index for
//! keyword-based document retrieval across memory graph fields:
//! entity `name`, scratchpad `content`, workspace `description`,
//! and entity `metadata.docComment`.

use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

// ─── BM25 Parameters ────────────────────────────────────────────────────────

/// BM25 term-frequency saturation parameter.
const K1: f32 = 1.2;
/// BM25 document-length normalization parameter.
const B: f32 = 0.75;

// ─── Tokenizer ──────────────────────────────────────────────────────────────

/// Split a camelCase or PascalCase word into lowercase sub-tokens.
///
/// Boundary detection rules:
///   - Uppercase letter preceded by a lowercase letter:  `validateToken` → `validate`, `Token`
///   - Uppercase letter followed by a lowercase letter when preceded by uppercase:
///     `parseHTMLDocument` → `parse`, `HTML`, `Document`
fn split_camel_case(word: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = word.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if c.is_uppercase() && !current.is_empty() {
            // Case 1: transition from lowercase → uppercase (e.g. validate|Token)
            let prev = chars[i - 1];
            if prev.is_lowercase() || prev.is_ascii_digit() {
                tokens.push(current.clone());
                current.clear();
            }
            // Case 2: run of uppercase followed by a lowercase (e.g. HTM|L|Document →
            //         we split *before* the last uppercase so "HTML" stays together
            //         until the lowercase forces a split)
            else if c.is_uppercase()
                && i + 1 < chars.len()
                && chars[i + 1].is_lowercase()
                && prev.is_uppercase()
            {
                tokens.push(current.clone());
                current.clear();
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    tokens.into_iter().map(|t| t.to_lowercase()).collect()
}

/// Tokenize text into lowercase, searchable tokens.
///
/// Processing pipeline:
/// 1. Unicode grapheme-aware whitespace split.
/// 2. Strip non-alphanumeric characters from each word boundary.
/// 3. Split on underscores (snake_case).
/// 4. Split on camelCase / PascalCase boundaries.
/// 5. Lowercase everything.
///
/// # Examples
/// ```
/// use yaam_engine::search::tokenize;
/// assert_eq!(tokenize("validateToken"), vec!["validate", "token"]);
/// assert_eq!(tokenize("get_user_by_id"), vec!["get", "user", "by", "id"]);
/// ```
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();

    // Split on Unicode word boundaries first, then process each segment.
    for segment in text.split_whitespace() {
        // For each whitespace-delimited segment, split on underscores.
        for part in segment.split('_') {
            // Strip leading/trailing non-alphanumeric characters (punctuation, etc.)
            let trimmed: String = part
                .graphemes(true)
                .skip_while(|g| !g.chars().next().map_or(false, |c| c.is_alphanumeric()))
                .collect::<String>();
            let trimmed: String = trimmed
                .graphemes(true)
                .rev()
                .skip_while(|g| !g.chars().next().map_or(false, |c| c.is_alphanumeric()))
                .collect::<Vec<&str>>()
                .into_iter()
                .rev()
                .collect();

            if trimmed.is_empty() {
                continue;
            }

            // Split camelCase / PascalCase.
            let sub_tokens = split_camel_case(&trimmed);
            for t in sub_tokens {
                if !t.is_empty() {
                    tokens.push(t);
                }
            }
        }
    }

    tokens
}

// ─── BM25 Inverted Index ────────────────────────────────────────────────────

/// A BM25-scored inverted index for keyword search over documents.
///
/// Documents are identified by string IDs and indexed by their tokenized text.
/// The index supports incremental add/remove and ranked retrieval.
#[derive(Debug, Clone)]
pub struct BM25Index {
    /// token → Vec<(doc_id, term_frequency)>
    pub inverted_index: HashMap<String, Vec<(String, f32)>>,
    /// doc_id → document length (number of tokens)
    pub doc_lengths: HashMap<String, f32>,
    /// Average document length across all indexed documents.
    pub avg_doc_length: f32,
    /// Total number of indexed documents.
    pub doc_count: usize,
}

impl BM25Index {
    /// Create a new, empty BM25 index.
    pub fn new() -> Self {
        Self {
            inverted_index: HashMap::new(),
            doc_lengths: HashMap::new(),
            avg_doc_length: 0.0,
            doc_count: 0,
        }
    }

    /// Recompute `avg_doc_length` from current `doc_lengths`.
    fn recompute_avg(&mut self) {
        if self.doc_count == 0 {
            self.avg_doc_length = 0.0;
        } else {
            let total: f32 = self.doc_lengths.values().sum();
            self.avg_doc_length = total / self.doc_count as f32;
        }
    }

    /// Add (or replace) a document in the index.
    ///
    /// The text is tokenized and each token's frequency is recorded in the
    /// inverted index. If a document with the same `doc_id` already exists,
    /// it is removed first.
    pub fn add_document(&mut self, doc_id: &str, text: &str) {
        // Remove previous version if present.
        if self.doc_lengths.contains_key(doc_id) {
            self.remove_document(doc_id);
        }

        let tokens = tokenize(text);
        let doc_len = tokens.len() as f32;

        // Count term frequencies.
        let mut tf_map: HashMap<String, f32> = HashMap::new();
        for token in &tokens {
            *tf_map.entry(token.clone()).or_insert(0.0) += 1.0;
        }

        // Insert into inverted index.
        for (token, freq) in tf_map {
            self.inverted_index
                .entry(token)
                .or_insert_with(Vec::new)
                .push((doc_id.to_string(), freq));
        }

        self.doc_lengths.insert(doc_id.to_string(), doc_len);
        self.doc_count += 1;
        self.recompute_avg();
    }

    /// Remove a document from the index.
    ///
    /// Removes all postings for `doc_id` from every token's posting list
    /// and updates corpus statistics. No-op if the document is not indexed.
    pub fn remove_document(&mut self, doc_id: &str) {
        if self.doc_lengths.remove(doc_id).is_none() {
            return;
        }
        self.doc_count -= 1;

        // Remove doc_id from every posting list; drop empty lists.
        let mut empty_tokens = Vec::new();
        for (token, postings) in self.inverted_index.iter_mut() {
            postings.retain(|(id, _)| id != doc_id);
            if postings.is_empty() {
                empty_tokens.push(token.clone());
            }
        }
        for token in empty_tokens {
            self.inverted_index.remove(&token);
        }

        self.recompute_avg();
    }

    /// Search the index and return the top-k documents ranked by BM25 score.
    ///
    /// The query is tokenized identically to documents. For each query token
    /// the BM25 score contribution is accumulated per document. Results are
    /// returned as `(doc_id, score)` pairs sorted descending by score.
    ///
    /// # BM25 Formula
    ///
    /// ```text
    /// score(D, Q) = Σ IDF(qi) · ( tf(qi, D) · (k1 + 1) )
    ///                           / ( tf(qi, D) + k1 · (1 - b + b · |D| / avgdl) )
    /// ```
    ///
    /// where IDF uses the Robertson–Spärck Jones formula:
    ///
    /// ```text
    /// IDF(qi) = ln( (N - n(qi) + 0.5) / (n(qi) + 0.5) + 1 )
    /// ```
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(String, f32)> {
        if self.doc_count == 0 {
            return Vec::new();
        }

        let query_tokens = tokenize(query);
        let mut scores: HashMap<String, f32> = HashMap::new();

        let n = self.doc_count as f32;

        for token in &query_tokens {
            let postings = match self.inverted_index.get(token) {
                Some(p) => p,
                None => continue,
            };

            let df = postings.len() as f32;
            // Robertson–Spärck Jones IDF with +1 to avoid negative values.
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

            for (doc_id, tf) in postings {
                let doc_len = self.doc_lengths.get(doc_id).copied().unwrap_or(0.0);
                let numerator = tf * (K1 + 1.0);
                let denominator = tf + K1 * (1.0 - B + B * doc_len / self.avg_doc_length);
                let score = idf * numerator / denominator;

                *scores.entry(doc_id.clone()).or_insert(0.0) += score;
            }
        }

        // Sort by score descending, then by doc_id ascending for stability.
        let mut results: Vec<(String, f32)> = scores.into_iter().collect();
        results.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        results.truncate(top_k);
        results
    }
}

impl Default for BM25Index {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tokenizer Tests ─────────────────────────────────────────────────

    #[test]
    fn test_camel_case_splitting() {
        assert_eq!(tokenize("validateToken"), vec!["validate", "token"]);
        assert_eq!(tokenize("getElementById"), vec!["get", "element", "by", "id"]);
    }

    #[test]
    fn test_pascal_case_splitting() {
        assert_eq!(tokenize("MyComponent"), vec!["my", "component"]);
    }

    #[test]
    fn test_uppercase_runs() {
        // "parseHTMLDocument" → ["parse", "html", "document"]
        assert_eq!(
            tokenize("parseHTMLDocument"),
            vec!["parse", "html", "document"]
        );
    }

    #[test]
    fn test_snake_case_splitting() {
        assert_eq!(
            tokenize("get_user_by_id"),
            vec!["get", "user", "by", "id"]
        );
    }

    #[test]
    fn test_mixed_case() {
        // camelCase inside snake_case
        assert_eq!(
            tokenize("get_userById"),
            vec!["get", "user", "by", "id"]
        );
    }

    #[test]
    fn test_whitespace_splitting() {
        assert_eq!(
            tokenize("hello world  foo"),
            vec!["hello", "world", "foo"]
        );
    }

    #[test]
    fn test_lowercasing() {
        assert_eq!(tokenize("HELLO"), vec!["hello"]);
        assert_eq!(tokenize("Hello"), vec!["hello"]);
    }

    #[test]
    fn test_punctuation_stripping() {
        assert_eq!(tokenize("hello, world!"), vec!["hello", "world"]);
        assert_eq!(tokenize("(foo)"), vec!["foo"]);
    }

    #[test]
    fn test_empty_input() {
        let result: Vec<String> = tokenize("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_unicode_tokens() {
        // Basic Unicode letters should survive.
        let tokens = tokenize("café résumé naïve");
        assert_eq!(tokens, vec!["café", "résumé", "naïve"]);
    }

    #[test]
    fn test_unicode_cjk() {
        // CJK characters: no case-splitting applies, kept as single tokens per
        // whitespace-delimited segment.
        let tokens = tokenize("你好 世界");
        assert_eq!(tokens, vec!["你好", "世界"]);
    }

    #[test]
    fn test_digits_in_tokens() {
        assert_eq!(tokenize("http2Server"), vec!["http2", "server"]);
        assert_eq!(tokenize("v2_beta"), vec!["v2", "beta"]);
    }

    // ── BM25 Index Tests ────────────────────────────────────────────────

    #[test]
    fn test_index_and_search_basic() {
        let mut index = BM25Index::new();
        index.add_document("doc1", "the quick brown fox jumps over the lazy dog");
        index.add_document("doc2", "the quick brown fox");
        index.add_document("doc3", "the lazy dog sleeps all day");

        let results = index.search("lazy dog", 10);

        // doc3 mentions "lazy" and "dog" and is short → highest score.
        // doc1 also has both terms but is longer.
        // doc2 has neither → absent.
        assert!(results.len() >= 2);
        assert_eq!(results[0].0, "doc3", "doc3 should rank first for 'lazy dog'");
        assert_eq!(results[1].0, "doc1", "doc1 should rank second");

        // doc2 should NOT appear (doesn't contain "lazy" or "dog").
        assert!(
            !results.iter().any(|(id, _)| id == "doc2"),
            "doc2 should not appear in results for 'lazy dog'"
        );
    }

    #[test]
    fn test_search_ranking_order() {
        let mut index = BM25Index::new();
        // doc_a: highly relevant to "rust programming"
        index.add_document(
            "doc_a",
            "rust programming language rust systems programming rust",
        );
        // doc_b: somewhat relevant
        index.add_document("doc_b", "programming in rust is fun");
        // doc_c: irrelevant
        index.add_document("doc_c", "the weather is sunny today");

        let results = index.search("rust programming", 10);

        assert!(results.len() >= 2);
        // doc_a has higher term frequency for both query terms.
        assert_eq!(results[0].0, "doc_a");
        assert_eq!(results[1].0, "doc_b");
        // doc_c should not appear.
        assert!(!results.iter().any(|(id, _)| id == "doc_c"));
    }

    #[test]
    fn test_remove_document() {
        let mut index = BM25Index::new();
        index.add_document("doc1", "alpha beta gamma");
        index.add_document("doc2", "alpha delta epsilon");
        index.add_document("doc3", "beta gamma delta");

        // Verify doc1 appears before removal.
        let results = index.search("alpha", 10);
        assert!(results.iter().any(|(id, _)| id == "doc1"));

        // Remove and verify absence.
        index.remove_document("doc1");

        let results = index.search("alpha", 10);
        assert!(
            !results.iter().any(|(id, _)| id == "doc1"),
            "doc1 should no longer appear after removal"
        );
        // doc2 should still be found.
        assert!(results.iter().any(|(id, _)| id == "doc2"));

        // Corpus stats should be updated.
        assert_eq!(index.doc_count, 2);
        assert!(!index.doc_lengths.contains_key("doc1"));
    }

    #[test]
    fn test_remove_nonexistent_document_is_noop() {
        let mut index = BM25Index::new();
        index.add_document("doc1", "hello world");
        index.remove_document("nonexistent");
        assert_eq!(index.doc_count, 1);
    }

    #[test]
    fn test_replace_document() {
        let mut index = BM25Index::new();
        index.add_document("doc1", "old content about cats");
        index.add_document("doc1", "new content about dogs");

        let results = index.search("cats", 10);
        assert!(results.is_empty(), "old content should be gone");

        let results = index.search("dogs", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "doc1");
        assert_eq!(index.doc_count, 1);
    }

    #[test]
    fn test_empty_index_search() {
        let index = BM25Index::new();
        let results = index.search("anything", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_top_k_limits_results() {
        let mut index = BM25Index::new();
        for i in 0..20 {
            index.add_document(&format!("doc{}", i), &format!("common term number {}", i));
        }

        let results = index.search("common", 5);
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_camel_case_query_matches_document() {
        let mut index = BM25Index::new();
        index.add_document("fn1", "validateUserToken checks the auth token");
        index.add_document("fn2", "parseConfigFile reads config from disk");

        // Query with camelCase should be tokenized the same way.
        let results = index.search("validateToken", 10);
        assert!(results.iter().any(|(id, _)| id == "fn1"));
    }
}
