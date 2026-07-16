//! Document adapter for non-code files (markdown, etc.).
//!
//! Parses markdown files into `Section` entities based on headings, and
//! extracts inline code references and file path references for `REFERENCES`
//! edge creation. No LSP is used — the graph itself resolves references.

use std::collections::HashSet;
use std::path::Path;

/// A parsed section from a markdown document.
#[derive(Debug, Clone)]
pub struct ParsedSection {
    pub id: String,
    pub name: String,
    pub content: String,
    pub level: u8,
    pub start_line: usize,
    pub end_line: usize,
    pub inline_code_refs: Vec<String>,
    pub file_path_refs: Vec<String>,
}

/// Trait for non-code document parsers.
pub trait DocumentAdapter: Send + Sync {
    /// Return the file extensions this adapter handles (without leading dot).
    fn extensions(&self) -> &[&str];

    /// Parse a document file and extract sections.
    fn parse_document(&self, file_path: &Path, content: &str) -> Vec<ParsedSection>;
}

/// Factory: returns the appropriate document adapter for a file.
pub fn get_document_adapter(file_path: &Path) -> Option<Box<dyn DocumentAdapter>> {
    let ext = file_path.extension().and_then(|e| e.to_str())?;
    match ext {
        "md" => Some(Box::new(MarkdownAdapter)),
        _ => None,
    }
}

// ─── Markdown Adapter ───────────────────────────────────────────────────────

pub struct MarkdownAdapter;

/// A heading detected during line-by-line parsing.
struct Heading {
    name: String,
    level: u8,
    line: usize, // 0-indexed line number of the heading
}

/// Regex-like pattern for inline code: `identifier`
/// We use a simple state machine instead of a regex crate to avoid dependencies.
fn extract_inline_code(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut in_code = false;
    let mut current = String::new();

    for ch in text.chars() {
        if ch == '`' {
            if in_code {
                // End of inline code
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() && !refs.contains(&trimmed) {
                    refs.push(trimmed);
                }
                current.clear();
                in_code = false;
            } else {
                // Start of inline code
                in_code = true;
            }
        } else if in_code {
            current.push(ch);
        }
    }

    refs
}

/// Extract file path references from text.
/// Matches patterns like `src/reconciler.rs`, `lib/utils.py`, `README.md`.
fn extract_file_paths(text: &str) -> Vec<String> {
    let extensions = [
        "rs", "ts", "tsx", "js", "jsx", "py", "md", "go", "java", "rb", "c", "cpp", "h",
    ];
    let mut refs = Vec::new();
    let mut seen = HashSet::new();

    // Simple word-by-word scan
    for word in text.split_whitespace() {
        // Strip backticks and surrounding non-alphanumeric characters (not / and .)
        let cleaned: String = word
            .trim_matches(|c: char| c == '`')
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.')
            .trim_end_matches('.')  // Strip trailing period (e.g., end of sentence)
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.')  // Strip again after period removal
            .to_string();

        if cleaned.contains('/') || cleaned.contains('.') {
            for ext in &extensions {
                let suffix = format!(".{}", ext);
                if cleaned.ends_with(&suffix) && cleaned.len() > suffix.len() {
                    // Make sure it looks like a path (has at least one path separator or starts with a letter)
                    if cleaned.contains('/') || cleaned.chars().next().map_or(false, |c| c.is_alphabetic()) {
                        if seen.insert(cleaned.clone()) {
                            refs.push(cleaned);
                        }
                    }
                    break;
                }
            }
        }
    }

    refs
}

/// Strip YAML frontmatter (---\n...\n---) from the start of the file.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if trimmed.starts_with("---\n") || trimmed.starts_with("---\r\n") {
        // Find the closing ---
        let after_first_delim = &trimmed[3..];
        if let Some(rest_pos) = after_first_delim.find("\n---") {
            let after_close = &after_first_delim[rest_pos + 4..];
            return after_close.trim_start();
        }
    }
    content
}

/// Detect atx headings (lines starting with #) and setext headings (text + ===/---).
fn find_headings(content: &str) -> Vec<Heading> {
    let lines: Vec<&str> = content.lines().collect();
    let mut headings = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim_start();

        // ATX heading: 1-6 '#' characters followed by space or end of line
        if line.starts_with('#') {
            let level = line.chars().take_while(|&c| c == '#').count() as u8;
            if level >= 1 && level <= 6 {
                let rest = &line[level as usize..];
                // Must be followed by space or end of line
                if rest.is_empty() || rest.starts_with(' ') {
                    let name = rest.trim().to_string();
                    if !name.is_empty() {
                        headings.push(Heading {
                            name,
                            level,
                            line: i,
                        });
                    }
                }
            }
        }

        // Setext heading: next line is === (h1) or --- (h2)
        if i + 1 < lines.len() && !line.is_empty() {
            let next = lines[i + 1].trim();
            if !next.is_empty()
                && next.chars().all(|c| c == '=')
                && next.len() >= 1
            {
                headings.push(Heading {
                    name: line.trim().to_string(),
                    level: 1,
                    line: i,
                });
            } else if !next.is_empty()
                && next.chars().all(|c| c == '-')
                && next.len() >= 1
                && !line.starts_with('#')
            {
                // Make sure this isn't a thematic break (---) or frontmatter
                headings.push(Heading {
                    name: line.trim().to_string(),
                    level: 2,
                    line: i,
                });
            }
        }

        i += 1;
    }

    headings
}

/// Find the end line for a heading — the line before the next heading at the
/// same or higher level, or the end of the file.
fn find_section_end(headings: &[Heading], current_idx: usize, total_lines: usize) -> usize {
    let current = &headings[current_idx];
    for (idx, h) in headings.iter().enumerate().skip(current_idx + 1) {
        if h.level <= current.level {
            return h.line.saturating_sub(1);
        }
    }
    total_lines.saturating_sub(1)
}

impl DocumentAdapter for MarkdownAdapter {
    fn extensions(&self) -> &[&str] {
        &["md"]
    }

    fn parse_document(&self, file_path: &Path, content: &str) -> Vec<ParsedSection> {
        let content = strip_frontmatter(content);
        let lines: Vec<&str> = content.lines().collect();
        let headings = find_headings(content);

        if headings.is_empty() {
            return Vec::new();
        }

        let file_id = format!("{}", file_path.display());
        let mut used_names: HashSet<String> = HashSet::new();
        let mut sections = Vec::new();

        for (idx, heading) in headings.iter().enumerate() {
            let end_line = find_section_end(&headings, idx, lines.len());

            // Extract content between this heading and the next
            let start = heading.line + 1; // skip the heading line itself
            let content_text = if start <= end_line && start < lines.len() {
                lines[start..=end_line.min(lines.len() - 1)].join("\n")
            } else {
                String::new()
            };

            // Generate unique ID with collision suffixing
            let base_id = format!("{}:{}", file_id, heading.name);
            let id = if used_names.contains(&base_id) {
                let mut suffix = 2;
                loop {
                    let candidate = format!("{}:{}", base_id, suffix);
                    if !used_names.contains(&candidate) {
                        used_names.insert(candidate.clone());
                        break candidate;
                    }
                    suffix += 1;
                }
            } else {
                used_names.insert(base_id.clone());
                base_id
            };

            // Extract inline code references from the content (not the heading)
            let inline_code_refs = extract_inline_code(&content_text);

            // Extract file path references from the content
            let file_path_refs = extract_file_paths(&content_text);

            sections.push(ParsedSection {
                id,
                name: heading.name.clone(),
                content: content_text,
                level: heading.level,
                start_line: heading.line + 1, // 1-indexed
                end_line: end_line + 1,       // 1-indexed
                inline_code_refs,
                file_path_refs,
            });
        }

        sections
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_find_headings_atx() {
        let content = "# Title\n\nText.\n\n## Sub\n\nMore.\n";
        let headings = find_headings(content);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].name, "Title");
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[0].line, 0);
        assert_eq!(headings[1].name, "Sub");
        assert_eq!(headings[1].level, 2);
        assert_eq!(headings[1].line, 4);
    }

    #[test]
    fn test_find_headings_setext() {
        let content = "Title\n=====\n\nText.\n\nSub\n-----\n\nMore.\n";
        let headings = find_headings(content);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].name, "Title");
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[1].name, "Sub");
        assert_eq!(headings[1].level, 2);
    }

    #[test]
    fn test_find_headings_none() {
        let content = "Just some text.\nNo headings here.\n";
        let headings = find_headings(content);
        assert!(headings.is_empty());
    }

    #[test]
    fn test_extract_inline_code() {
        let text = "Content about `reconcile_file` and `parse_file`.";
        let refs = extract_inline_code(text);
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&"reconcile_file".to_string()));
        assert!(refs.contains(&"parse_file".to_string()));
    }

    #[test]
    fn test_extract_inline_code_dedup() {
        let text = "`reconcile_file` is used. `reconcile_file` again.";
        let refs = extract_inline_code(text);
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn test_extract_file_paths() {
        let text = "See src/reconciler.rs and lib/utils.py for details.";
        let refs = extract_file_paths(text);
        assert!(refs.contains(&"src/reconciler.rs".to_string()));
        assert!(refs.contains(&"lib/utils.py".to_string()));
    }

    #[test]
    fn test_strip_frontmatter() {
        let content = "---\ntitle: Test\n---\n\n# Heading\n\nText.\n";
        let stripped = strip_frontmatter(content);
        assert!(stripped.starts_with("# Heading"));
    }

    #[test]
    fn test_strip_frontmatter_none() {
        let content = "# Heading\n\nText.\n";
        let stripped = strip_frontmatter(content);
        assert_eq!(stripped, content);
    }

    #[test]
    fn test_parse_document_basic() {
        let adapter = MarkdownAdapter;
        let path = Path::new("README.md");
        let content = "# Title\n\nIntro text.\n\n## Section A\n\nContent about `reconcile_file`.\n\nSee `src/reconciler.rs`.\n\n## Section B\n\nOther content.\n";

        let sections = adapter.parse_document(path, content);
        assert_eq!(sections.len(), 3);

        assert_eq!(sections[0].name, "Title");
        assert_eq!(sections[0].level, 1);
        assert!(sections[0].content.contains("Intro text."));
        assert!(sections[0].content.contains("Section A")); // nested content included

        assert_eq!(sections[1].name, "Section A");
        assert_eq!(sections[1].level, 2);
        assert!(sections[1].content.contains("reconcile_file"));
        assert!(sections[1].inline_code_refs.contains(&"reconcile_file".to_string()));
        assert!(sections[1].file_path_refs.contains(&"src/reconciler.rs".to_string()));

        assert_eq!(sections[2].name, "Section B");
        assert_eq!(sections[2].level, 2);
    }

    #[test]
    fn test_parse_document_duplicate_headings() {
        let adapter = MarkdownAdapter;
        let path = Path::new("doc.md");
        let content = "# Architecture\n\nText 1.\n\n# Architecture\n\nText 2.\n";

        let sections = adapter.parse_document(path, content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].id, "doc.md:Architecture");
        assert_eq!(sections[1].id, "doc.md:Architecture:2");
    }

    #[test]
    fn test_parse_document_no_headings() {
        let adapter = MarkdownAdapter;
        let path = Path::new("changelog.md");
        let content = "Just some text without any headings.\n";

        let sections = adapter.parse_document(path, content);
        assert!(sections.is_empty());
    }

    #[test]
    fn test_parse_document_empty() {
        let adapter = MarkdownAdapter;
        let path = Path::new("empty.md");
        let sections = adapter.parse_document(path, "");
        assert!(sections.is_empty());
    }

    #[test]
    fn test_parse_document_frontmatter() {
        let adapter = MarkdownAdapter;
        let path = Path::new("spec.md");
        let content = "---\ntitle: Test Spec\nstatus: draft\n---\n\n# Overview\n\nThe overview.\n";

        let sections = adapter.parse_document(path, content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "Overview");
        assert!(!sections[0].content.contains("title: Test Spec"));
    }

    #[test]
    fn test_nested_sections_content_scope() {
        let adapter = MarkdownAdapter;
        let path = Path::new("test.md");
        let content = "# Parent\n\nParent intro.\n\n## Child\n\nChild content.\n\n# Sibling\n\nSibling content.\n";

        let sections = adapter.parse_document(path, content);
        assert_eq!(sections.len(), 3);

        // Parent's content should include Child's content
        assert!(sections[0].content.contains("Parent intro."));
        assert!(sections[0].content.contains("Child content."));

        // Child's content is just its own
        assert!(sections[1].content.contains("Child content."));
        assert!(!sections[1].content.contains("Parent intro."));

        // Sibling is a new top-level section
        assert_eq!(sections[2].name, "Sibling");
        assert!(sections[2].content.contains("Sibling content."));
        assert!(!sections[2].content.contains("Child content."));
    }

    #[test]
    fn test_get_document_adapter() {
        assert!(get_document_adapter(Path::new("README.md")).is_some());
        assert!(get_document_adapter(Path::new("spec.txt")).is_none());
        assert!(get_document_adapter(Path::new("code.rs")).is_none());
    }
}