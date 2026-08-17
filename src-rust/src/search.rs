//! BM25 keyword search module.
//!
//! Provides a Unicode-aware tokenizer with lightweight stemming, a BM25
//! inverted index for keyword-based document retrieval, and a multi-field
//! BM25 index that applies per-field weights (name, content, docComment).
//!
//! Indexed fields include entity `name`, scratchpad `content`, workspace
//! `description`, and entity `metadata.docComment`.

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

/// Tokenize text into lowercase, stemmed, searchable tokens.
///
/// Processing pipeline:
/// 1. Unicode grapheme-aware whitespace split.
/// 2. Strip non-alphanumeric characters from each word boundary.
/// 3. Split on underscores (snake_case).
/// 4. Split on camelCase / PascalCase boundaries.
/// 5. Lowercase everything.
/// 6. Apply lightweight suffix-stripping stemmer (see [`stem`]).
///
/// # Examples
/// ```
/// use yaam_engine::search::tokenize;
/// assert_eq!(tokenize("validateToken"), vec!["validat", "token"]);
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
                    // Apply stemming to normalize morphological variants
                    // (e.g. validate/validation/validator → validat).
                    let stemmed = stem(&t);
                    if !stemmed.is_empty() {
                        tokens.push(stemmed);
                    }
                }
            }
        }
    }

    tokens
}

// ─── Stemmer ────────────────────────────────────────────────────────────────

/// Check if a character is a consonant (not a, e, i, o, u).
fn is_consonant(c: char) -> bool {
    !matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Collapse a doubled final consonant (e.g. `embedd` → `embed`, `runn` → `run`).
///
/// After stripping suffixes like `ing`, `ed`, or `er`, the remaining stem
/// may end in a doubled consonant that was part of the original word's
/// spelling convention (e.g. `embedded` → strip `ed` → `embedd` → `embed`).
fn dedouble(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    if n >= 2 && chars[n - 1] == chars[n - 2] && is_consonant(chars[n - 1]) {
        chars[..n - 1].iter().collect()
    } else {
        s.to_string()
    }
}

/// Apply a lightweight suffix-stripping stemmer to improve recall.
///
/// Reduces morphological variants to a common stem so that e.g.
/// `validate`, `validation`, `validator`, `validated`, `validating`
/// all map to `validat`.
///
/// Rules are applied in sequential steps, at most one rule per step.
/// Each step operates on the result of the previous step:
///
/// 1. **Plurals:** `ies`→`y`, `es`→``, `s`→``
/// 2. **Tense:** `ied`→`y`, `ing`→``, `ed`→``
/// 3. **Derivational:** `ization`→`ize`, `ation`→`ate`, `tion`→`t`, `sion`→`s`, `ment`→``, `ness`→``
/// 4. **Agent:** `er`→``, `or`→``
/// 5. **Final e:** `e`→``
///
/// After stripping `ing`, `ed`, or `er`, doubled final consonants are
/// collapsed via [`dedouble`].
///
/// Only applies to ASCII tokens of length >= 4. Shorter tokens and
/// non-ASCII (CJK, etc.) tokens are returned unchanged.
///
/// # Design rationale
///
/// This is intentionally simpler than the full Porter stemmer. It targets
/// the most common morphological variations in code identifiers and
/// documentation: plurals, verb tenses, nominalizations, and agent nouns.
/// The goal is improved recall for BM25 keyword search — exact stem
/// equality is not required for semantic search, which handles synonyms
/// and abbreviations via embeddings.
fn stem(word: &str) -> String {
    // Only stem ASCII words of sufficient length.
    if word.len() < 4 || !word.is_ascii() {
        return word.to_string();
    }

    let s = word.to_string();

    // ── Step 1: Plurals ──
    let s = if let Some(stem) = strip_suffix(&s, "ies", 2) {
        format!("{}y", stem)
    } else if let Some(stem) = strip_suffix(&s, "es", 3) {
        stem
    } else if s.ends_with('s')
        && !s.ends_with("ss")
        && !s.ends_with("us")
        && !s.ends_with("is")
    {
        if let Some(stem) = strip_suffix(&s, "s", 3) {
            stem
        } else {
            s.clone()
        }
    } else {
        s.clone()
    };

    // ── Step 2: Tense ──
    // min_stem=4 protects base words like "embed" (emb+ed), "ring" (r+ing)
    let s = if let Some(stem) = strip_suffix(&s, "ied", 2) {
        format!("{}y", stem)
    } else if let Some(stem) = strip_suffix(&s, "ing", 4) {
        dedouble(&stem)
    } else if let Some(stem) = strip_suffix(&s, "ed", 4) {
        dedouble(&stem)
    } else {
        s.clone()
    };

    // ── Step 3: Derivational ──
    let s = if let Some(stem) = strip_suffix(&s, "ization", 3) {
        format!("{}ize", stem)
    } else if let Some(stem) = strip_suffix(&s, "ation", 3) {
        format!("{}ate", stem)
    } else if let Some(stem) = strip_suffix(&s, "tion", 3) {
        format!("{}t", stem)
    } else if let Some(stem) = strip_suffix(&s, "sion", 3) {
        format!("{}s", stem)
    } else if let Some(stem) = strip_suffix(&s, "ment", 4) {
        stem
    } else if let Some(stem) = strip_suffix(&s, "ness", 3) {
        stem
    } else {
        s.clone()
    };

    // ── Step 4: Agent suffixes ──
    // min_stem=4 protects common words like "user", "over", "water", "error"
    let s = if let Some(stem) = strip_suffix(&s, "er", 4) {
        dedouble(&stem)
    } else if let Some(stem) = strip_suffix(&s, "or", 4) {
        stem
    } else {
        s.clone()
    };

    // ── Step 5: Final e ──
    let s = if let Some(stem) = strip_suffix(&s, "e", 3) {
        stem
    } else {
        s.clone()
    };

    s
}

/// Strip `suffix` from `word` if the remaining stem is at least `min_stem` chars.
/// Returns `Some(stem)` on success, `None` otherwise.
fn strip_suffix(word: &str, suffix: &str, min_stem: usize) -> Option<String> {
    if word.ends_with(suffix) {
        let stem = &word[..word.len() - suffix.len()];
        if stem.len() >= min_stem {
            return Some(stem.to_string());
        }
    }
    None
}

// ─── Multi-Field BM25 Index ─────────────────────────────────────────────────

/// Default field weights for the multi-field BM25 index.
///
/// Matches on entity names are weighted most heavily, followed by
/// docComments, then full source/content text.
pub const NAME_WEIGHT: f32 = 3.0;
pub const DOC_WEIGHT: f32 = 2.0;
pub const CONTENT_WEIGHT: f32 = 1.0;

/// A multi-field BM25 index that applies per-field weights.
///
/// Wraps multiple [`BM25Index`] instances — one per field (name, content,
/// doc) — and combines their scores using configurable weights. This
/// ensures that a match on a function's **name** ranks higher than a match
/// in its body, even when the body has higher term frequency.
///
/// # Field Weights
///
/// | Field | Weight | Rationale |
/// |-------|--------|-----------|
/// | `name` | 3.0 | Entity names are the strongest relevance signal |
/// | `doc` | 2.0 | DocComments are curated summaries |
/// | `content` | 1.0 | Full source/body text — high recall, lower precision |
#[derive(Debug, Clone)]
pub struct BM25FieldIndex {
    name_index: BM25Index,
    content_index: BM25Index,
    doc_index: BM25Index,
}

impl BM25FieldIndex {
    /// Create a new, empty multi-field BM25 index with default weights.
    pub fn new() -> Self {
        Self {
            name_index: BM25Index::new(),
            content_index: BM25Index::new(),
            doc_index: BM25Index::new(),
        }
    }

    /// Add (or replace) a document across all field indexes.
    ///
    /// `fields` maps field names (`"name"`, `"content"`, `"doc"`) to their
    /// text. Missing fields are skipped — only present fields are indexed.
    pub fn add_document(&mut self, doc_id: &str, fields: &HashMap<String, String>) {
        // Remove previous version from all sub-indexes first.
        self.remove_document(doc_id);

        if let Some(text) = fields.get("name") {
            if !text.is_empty() {
                self.name_index.add_document(doc_id, text);
            }
        }
        if let Some(text) = fields.get("content") {
            if !text.is_empty() {
                self.content_index.add_document(doc_id, text);
            }
        }
        if let Some(text) = fields.get("doc") {
            if !text.is_empty() {
                self.doc_index.add_document(doc_id, text);
            }
        }
    }

    /// Remove a document from all field indexes.
    pub fn remove_document(&mut self, doc_id: &str) {
        self.name_index.remove_document(doc_id);
        self.content_index.remove_document(doc_id);
        self.doc_index.remove_document(doc_id);
    }

    /// Search all fields and return the top-k documents ranked by
    /// weighted combined BM25 score.
    ///
    /// The query is tokenized identically to documents. Each field index
    /// is queried independently, and per-field scores are combined as:
    ///
    /// ```text
    /// score(D) = NAME_WEIGHT * bm25_name(D)
    ///          + CONTENT_WEIGHT * bm25_content(D)
    ///          + DOC_WEIGHT * bm25_doc(D)
    /// ```
    ///
    /// Results are returned as `(doc_id, score)` pairs sorted descending.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(String, f32)> {
        let mut combined: HashMap<String, f32> = HashMap::new();

        // Query each field index with a larger pool to ensure good coverage
        // across fields, then combine with weights.
        let pool = top_k.saturating_mul(3).max(20);

        for (id, score) in self.name_index.search(query, pool) {
            *combined.entry(id).or_insert(0.0) += NAME_WEIGHT * score;
        }
        for (id, score) in self.content_index.search(query, pool) {
            *combined.entry(id).or_insert(0.0) += CONTENT_WEIGHT * score;
        }
        for (id, score) in self.doc_index.search(query, pool) {
            *combined.entry(id).or_insert(0.0) += DOC_WEIGHT * score;
        }

        let mut ranked: Vec<(String, f32)> = combined.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked.truncate(top_k);
        ranked
    }
}

impl Default for BM25FieldIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ─── BM25 Inverted Index ────────────────────────────────────────────────────

/// A BM25-scored inverted index for keyword search over documents.
///
/// Documents are identified by string IDs and indexed by their tokenized text.
/// The index supports incremental add/remove and ranked retrieval.
#[derive(Debug, Clone)]
pub struct BM25Index {
    /// token → Vec<(doc_id, term_frequency)>
    pub inverted_index: HashMap<String, Vec<(u32, f32)>>,
    pub id_map: HashMap<String, u32>,
    pub rev_id_map: HashMap<u32, String>,
    pub next_id: u32,
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
            id_map: HashMap::new(),
            rev_id_map: HashMap::new(),
            next_id: 1,
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

        let internal_id = *self.id_map.entry(doc_id.to_string()).or_insert_with(|| {
            let id = self.next_id;
            self.next_id += 1;
            self.rev_id_map.insert(id, doc_id.to_string());
            id
        });

        // Insert into inverted index.
        for (token, freq) in tf_map {
            self.inverted_index
                .entry(token)
                .or_insert_with(Vec::new)
                .push((internal_id, freq));
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

        let internal_id = match self.id_map.get(doc_id) {
            Some(&id) => id,
            None => return,
        };

        // Remove doc_id from every posting list; drop empty lists.
        let mut empty_tokens = Vec::new();
        for (token, postings) in self.inverted_index.iter_mut() {
            postings.retain(|(id, _)| *id != internal_id);
            if postings.is_empty() {
                empty_tokens.push(token.clone());
            }
            postings.shrink_to_fit(); // Prevent capacity leaks!
        }
        
        self.id_map.remove(doc_id);
        self.rev_id_map.remove(&internal_id);
        for token in empty_tokens {
            self.inverted_index.remove(&token);
        }

        self.recompute_avg();
    }

    pub fn search(&self, query: &str, top_k: usize) -> Vec<(String, f32)> {
        if self.doc_count == 0 {
            return Vec::new();
        }

        let query_tokens = tokenize(query);
        let n = self.doc_count as f32;
        let mut scores: HashMap<u32, f32> = HashMap::new();

        for token in query_tokens {
            let postings = match self.inverted_index.get(&token) {
                Some(p) => p,
                None => continue,
            };

            let df = postings.len() as f32;
            // Robertson–Spärck Jones IDF with +1 to avoid negative values.
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

            for &(internal_id, tf) in postings {
                let doc_id = self.rev_id_map.get(&internal_id).unwrap();
                let doc_len = self.doc_lengths.get(doc_id).copied().unwrap_or(0.0);
                let numerator = tf * (K1 + 1.0);
                let denominator = tf + K1 * (1.0 - B + B * doc_len / self.avg_doc_length);
                let score = idf * numerator / denominator;

                *scores.entry(internal_id).or_insert(0.0) += score;
            }
        }

        let mut results: Vec<(String, f32)> = Vec::new();
        for (internal_id, score) in scores {
            if let Some(doc_id) = self.rev_id_map.get(&internal_id) {
                results.push((doc_id.clone(), score));
            }
        }
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
        // "validate" is stemmed to "validat" (strip final 'e')
        assert_eq!(tokenize("validateToken"), vec!["validat", "token"]);
        assert_eq!(tokenize("getElementById"), vec!["get", "element", "by", "id"]);
    }

    #[test]
    fn test_pascal_case_splitting() {
        assert_eq!(tokenize("MyComponent"), vec!["my", "component"]);
    }

    #[test]
    fn test_uppercase_runs() {
        // "parseHTMLDocument" → ["parse", "html", "document"] before stemming
        // After stemming: parse→pars, document→docu
        assert_eq!(
            tokenize("parseHTMLDocument"),
            vec!["pars", "html", "docu"]
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
        // "server" is stemmed to "serv" (strip "er" suffix)
        assert_eq!(tokenize("http2Server"), vec!["http2", "serv"]);
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

        // Query with camelCase should be tokenized + stemmed the same way.
        // "validateToken" → ["validat", "token"] — matches fn1's stemmed tokens.
        let results = index.search("validateToken", 10);
        assert!(results.iter().any(|(id, _)| id == "fn1"));
    }

    // ── Stemmer Tests ──────────────────────────────────────────────────

    #[test]
    fn test_stem_validate_family() {
        // All morphological variants of "validate" should share the same stem.
        let stem_validate = stem("validate");
        assert_eq!(stem("validation"), stem_validate);
        assert_eq!(stem("validator"), stem_validate);
        assert_eq!(stem("validated"), stem_validate);
        assert_eq!(stem("validating"), stem_validate);
        assert_eq!(stem("validators"), stem_validate);
        assert_eq!(stem_validate, "validat");
    }

    #[test]
    fn test_stem_search_family() {
        let stem_search = stem("search");
        assert_eq!(stem("searching"), stem_search);
        assert_eq!(stem("searched"), stem_search);
        assert_eq!(stem("searches"), stem_search);
        assert_eq!(stem_search, "search");
    }

    #[test]
    fn test_stem_embed_family() {
        let stem_embed = stem("embed");
        assert_eq!(stem("embedding"), stem_embed);
        assert_eq!(stem("embedded"), stem_embed);
        assert_eq!(stem("embedder"), stem_embed);
        assert_eq!(stem_embed, "embed");
    }

    #[test]
    fn test_stem_parse_family() {
        let stem_parse = stem("parse");
        assert_eq!(stem("parser"), stem_parse);
        assert_eq!(stem("parsing"), stem_parse);
        assert_eq!(stem("parsed"), stem_parse);
        assert_eq!(stem("parsers"), stem_parse);
        assert_eq!(stem_parse, "pars");
    }

    #[test]
    fn test_stem_tokenize_family() {
        let stem_tokenize = stem("tokenize");
        assert_eq!(stem("tokenizer"), stem_tokenize);
        assert_eq!(stem("tokenization"), stem_tokenize);
        assert_eq!(stem_tokenize, "tokeniz");
    }

    #[test]
    fn test_stem_create_family() {
        let stem_create = stem("create");
        assert_eq!(stem("created"), stem_create);
        assert_eq!(stem("creating"), stem_create);
        assert_eq!(stem("creation"), stem_create);
        assert_eq!(stem("creator"), stem_create);
        assert_eq!(stem("creates"), stem_create);
        assert_eq!(stem_create, "creat");
    }

    #[test]
    fn test_stem_normalize_family() {
        let stem_norm = stem("normalize");
        assert_eq!(stem("normalization"), stem_norm);
        assert_eq!(stem("normalizing"), stem_norm);
        assert_eq!(stem("normalized"), stem_norm);
        assert_eq!(stem_norm, "normaliz");
    }

    #[test]
    fn test_stem_plural_ies() {
        assert_eq!(stem("entities"), stem("entity"));
        assert_eq!(stem("queries"), stem("query"));
    }

    #[test]
    fn test_stem_plural_s() {
        assert_eq!(stem("tokens"), stem("token"));
        assert_eq!(stem("configs"), stem("config"));
    }

    #[test]
    fn test_stem_short_words_unchanged() {
        // Words < 4 chars should not be stemmed.
        assert_eq!(stem("id"), "id");
        assert_eq!(stem("get"), "get");
        assert_eq!(stem("set"), "set");
        assert_eq!(stem("run"), "run");
        assert_eq!(stem("the"), "the");
    }

    #[test]
    fn test_stem_protected_words() {
        // Words ending in 'ss', 'us', 'is' should not have 's' stripped.
        assert_eq!(stem("class"), "class");
        assert_eq!(stem("status"), "status"); // 'us' protects from 's' strip
        assert_eq!(stem("this"), "this"); // 'is' protects from 's' strip, < 4 anyway
    }

    #[test]
    fn test_stem_non_ascii_unchanged() {
        // Non-ASCII tokens should not be stemmed.
        assert_eq!(stem("café"), "café");
        assert_eq!(stem("你好"), "你好");
    }

    #[test]
    fn test_stem_ing_with_dedouble() {
        assert_eq!(stem("running"), "run");
        assert_eq!(stem("embedding"), "embed");
        assert_eq!(stem("stopping"), "stop");
    }

    #[test]
    fn test_stem_ing_without_dedouble() {
        // No doubled consonant — stem should just strip 'ing'.
        assert_eq!(stem("searching"), "search");
        assert_eq!(stem("parsing"), "pars");
    }

    #[test]
    fn test_stem_ing_too_short() {
        // 'ing' stripping requires stem >= 4 chars.
        assert_eq!(stem("ring"), "ring"); // stem would be 'r' (1 char)
        assert_eq!(stem("sing"), "sing"); // stem would be 's' (1 char)
        assert_eq!(stem("ping"), "ping"); // stem would be 'p' (1 char)
        assert_eq!(stem("string"), "string"); // stem 'str' (3 chars < 4)
        assert_eq!(stem("thing"), "thing"); // stem 'th' (2 chars < 4)
    }

    #[test]
    fn test_stem_measurement_family() {
        let stem_measure = stem("measure");
        assert_eq!(stem("measurement"), stem_measure);
        assert_eq!(stem("measuring"), stem_measure);
        assert_eq!(stem("measured"), stem_measure);
    }

    // ── BM25FieldIndex Tests ───────────────────────────────────────────

    #[test]
    fn test_field_index_name_outranks_content() {
        let mut index = BM25FieldIndex::new();
        let mut fields1 = HashMap::new();
        fields1.insert("name".to_string(), "search".to_string());
        fields1.insert("content".to_string(), "performs a linear scan across all nodes".to_string());
        index.add_document("fn1", &fields1);

        let mut fields2 = HashMap::new();
        fields2.insert("name".to_string(), "linearScan".to_string());
        fields2.insert("content".to_string(), "search search search search search".to_string());
        index.add_document("fn2", &fields2);

        // fn1 has "search" in its name (weight 3.0).
        // fn2 has "search" only in content (weight 1.0) with high TF.
        // Despite fn2 having 5 occurrences in content, fn1 should rank first
        // because the name match with weight 3.0 is a stronger signal.
        let results = index.search("search", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, "fn1", "name match should outrank content match");
    }

    #[test]
    fn test_field_index_multiple_fields_combine() {
        let mut index = BM25FieldIndex::new();
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "searchIndex".to_string());
        fields.insert("content".to_string(), "performs search operations".to_string());
        fields.insert("doc".to_string(), "Searches the index for matching terms".to_string());
        index.add_document("fn1", &fields);

        let mut fields2 = HashMap::new();
        fields2.insert("name".to_string(), "otherFunction".to_string());
        fields2.insert("content".to_string(), "search".to_string());
        index.add_document("fn2", &fields2);

        // fn1 has "search" in name + content + doc → higher combined score.
        let results = index.search("search", 10);
        assert_eq!(results[0].0, "fn1");
    }

    #[test]
    fn test_field_index_remove_document() {
        let mut index = BM25FieldIndex::new();
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "search".to_string());
        index.add_document("fn1", &fields);

        assert!(!index.search("search", 10).is_empty());

        index.remove_document("fn1");
        assert!(index.search("search", 10).is_empty());
    }

    #[test]
    fn test_field_index_empty_search() {
        let index = BM25FieldIndex::new();
        assert!(index.search("anything", 10).is_empty());
    }

    #[test]
    fn test_field_index_stemming_applies() {
        // BM25FieldIndex uses the same tokenizer (with stemming) as BM25Index.
        let mut index = BM25FieldIndex::new();
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "validateToken".to_string());
        index.add_document("fn1", &fields);

        // Query with "validated" should match "validate" via stemming.
        let results = index.search("validated", 10);
        assert!(results.iter().any(|(id, _)| id == "fn1"));
    }
}
