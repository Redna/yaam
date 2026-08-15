use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tree_sitter::{Parser, Query, QueryCursor};

use crate::language_adapter::{get_adapter, LanguageAdapter};
use crate::types::{
    Event, EventPayload, EventType, LinkNodesPayload, UpsertNodePayload, EVENT_VERSION,
    DeleteNodePayload,
};

pub struct ParsedDeclaration {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    pub line: usize,
    pub source_text: String,
}

pub struct ParsedReference {
    pub ref_type: String, // "CALLS" or "IMPORTS"
    pub name: String,
    pub line: usize, // 0-indexed for LSP
    pub col: usize,  // 0-indexed for LSP
    pub enclosing_function_id: Option<String>, // ID of the function containing this reference
}

/// A reference extracted by tree-sitter that needs async LSP resolution (Spec #2).
///
/// When `reconcile_file` is called with `lsp: None`, references are collected
/// into this struct instead of being resolved inline. The caller queues them
/// for background LSP resolution via a tokio channel.
#[derive(Debug, Clone)]
pub struct PendingReference {
    /// Relative file path (e.g. "src/index.ts")
    pub source_file: String,
    /// Absolute file:// URI for LSP (e.g. "file:///home/user/project/src/index.ts")
    pub source_file_uri: String,
    /// ID of the enclosing function (or file ID for top-level references)
    pub source_id: String,
    /// Name of the referenced identifier
    pub ref_name: String,
    /// "CALLS" or "IMPORTS"
    pub ref_type: String,
    /// 0-indexed line for LSP
    pub line: u32,
    /// 0-indexed column for LSP
    pub col: u32,
    /// Full file content for LSP notify_open
    pub content: std::sync::Arc<String>,
    /// Language ID for LSP (e.g. "typescript", "python")
    pub language_id: String,
}

/// Parse a source file using the given language adapter.
///
/// Extracts declarations (functions, classes) and references (calls, imports)
/// via tree-sitter queries.  The adapter provides the grammar, query source,
/// and enclosing-function lookup specific to the language.
pub fn parse_file(
    file_path: &Path,
    content: &str,
    adapter: &dyn LanguageAdapter,
) -> (Vec<ParsedDeclaration>, Vec<ParsedReference>) {
    let mut parser = Parser::new();
    let language = adapter.language();
    parser
        .set_language(&language)
        .expect("Error loading language grammar");

    let tree = parser.parse(content, None).unwrap();
    let source_code = content.as_bytes();

    let query_source = adapter.query_source();
    let query = Query::new(&language, query_source).unwrap();
    let mut query_cursor = QueryCursor::new();
    let matches = query_cursor.matches(&query, tree.root_node(), source_code);

    let mut declarations = Vec::new();
    let mut references = Vec::new();

    for m in matches {
        for capture in m.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            let node = capture.node;

            let name = node
                .utf8_text(source_code)
                .unwrap_or("")
                .to_string();

            if capture_name.starts_with("call") {
                let enclosing = adapter.find_enclosing_function(node, source_code, file_path);
                references.push(ParsedReference {
                    ref_type: "CALLS".to_string(),
                    name,
                    line: node.start_position().row,
                    col: node.start_position().column,
                    enclosing_function_id: enclosing,
                });
            } else if capture_name.starts_with("import") {
                // Imports are at the top level; enclosing function is None
                references.push(ParsedReference {
                    ref_type: "IMPORTS".to_string(),
                    name,
                    line: node.start_position().row,
                    col: node.start_position().column,
                    enclosing_function_id: None,
                });
            } else {
                let entity_type = if capture_name.starts_with("class") {
                    "Class"
                } else {
                    // function.name, method.name, variable.name → all Function
                    "Function"
                };

                let id = format!("{}:{}", file_path.display(), name);

                // Extract full source text from the enclosing declaration node.
                let source_text = adapter
                    .declaration_node(node)
                    .and_then(|decl_node| decl_node.utf8_text(source_code).ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default();

                declarations.push(ParsedDeclaration {
                    id,
                    entity_type: entity_type.to_string(),
                    name,
                    line: node.start_position().row + 1,
                    source_text,
                });
            }
        }
    }

    (declarations, references)
}

fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn uri_to_path(uri: &str) -> String {
    if let Some(stripped) = uri.strip_prefix("file://") {
        stripped.to_string()
    } else {
        uri.to_string()
    }
}

pub fn reconcile_file(
    file_path: &Path,
    content: Option<&str>,
    lsp: Option<&mut dyn crate::lsp_adapter::LspAdapter>,
    engine: &crate::graph::MemoryEngine,
) -> (Vec<Event>, Vec<PendingReference>) {
    let mut events = Vec::new();
    let timestamp = get_timestamp();
    let file_id = format!("{}", file_path.display());

    // 1. Delete existing declarations for this file from the engine graph
    let inbound_edges = engine.get_reverse_edges(&file_id);
    for edge in inbound_edges {
        if edge.relationship == "DECLARED_IN" {
            events.push(Event {
                version: EVENT_VERSION,
                timestamp,
                event_type: EventType::DeleteNode,
                payload: EventPayload::DeleteNode(DeleteNodePayload {
                    id: edge.from_id.clone(),
                }),
            });
        }
    }

    // 2. If content is empty or missing, delete the file node as well and we are done.
    if content.is_none() || content.unwrap().is_empty() {
        events.push(Event {
            version: EVENT_VERSION,
            timestamp,
            event_type: EventType::DeleteNode,
            payload: EventPayload::DeleteNode(DeleteNodePayload {
                id: file_id.clone(),
            }),
        });
        return (events, Vec::new());
    }

    let content_str = content.unwrap();

    // 3. Upsert the file node
    let mut file_props = HashMap::new();
    file_props.insert(
        "entity_type".to_string(),
        serde_json::Value::String("File".to_string()),
    );
    file_props.insert(
        "name".to_string(),
        serde_json::Value::String(
            file_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ),
    );
    file_props.insert(
        "status".to_string(),
        serde_json::Value::String("active".to_string()),
    );
    file_props.insert(
        "last_modified".to_string(),
        serde_json::Value::Number(serde_json::Number::from(timestamp)),
    );

    events.push(Event {
        version: EVENT_VERSION,
        timestamp,
        event_type: EventType::UpsertNode,
        payload: EventPayload::UpsertNode(UpsertNodePayload {
            id: file_id.clone(),
            label: "Entity".to_string(),
            properties: file_props,
        }),
    });

    // 4. Resolve the language adapter for this file.
    //    If the file type is not supported as code, try a document adapter.
    //    If neither is found, we stop here — the file node is upserted but no
    //    declarations or references are extracted.
    let adapter = match get_adapter(file_path) {
        Some(a) => a,
        None => {
            // Try document adapter (markdown, etc.)
            if let Some(doc_adapter) = crate::document_adapter::get_document_adapter(file_path) {
                return {
                    let (evs, refs) = reconcile_document(file_path, content_str, doc_adapter.as_ref(), engine);
                    (evs, refs)
                };
            }
            return (events, Vec::new());
        }
    };

    // 5. Parse content
    let (declarations, references) = parse_file(file_path, content_str, adapter.as_ref());

    // 6. Upsert new declarations
    for decl in declarations {
        let mut entity_props = HashMap::new();
        entity_props.insert(
            "entity_type".to_string(),
            serde_json::Value::String(decl.entity_type.clone()),
        );
        entity_props.insert(
            "name".to_string(),
            serde_json::Value::String(decl.name.clone()),
        );
        entity_props.insert(
            "line".to_string(),
            serde_json::Value::Number(serde_json::Number::from(decl.line)),
        );
        entity_props.insert(
            "status".to_string(),
            serde_json::Value::String("active".to_string()),
        );
        entity_props.insert(
            "last_modified".to_string(),
            serde_json::Value::Number(serde_json::Number::from(timestamp)),
        );
        entity_props.insert(
            "content".to_string(),
            serde_json::Value::String(decl.source_text.clone()),
        );
        // Store line number in metadata so it's accessible from MemoryNode
        // without needing to keep raw properties around.
        let metadata = serde_json::json!({"line": decl.line}).to_string();
        entity_props.insert(
            "metadata".to_string(),
            serde_json::Value::String(metadata),
        );

        events.push(Event {
            version: EVENT_VERSION,
            timestamp,
            event_type: EventType::UpsertNode,
            payload: EventPayload::UpsertNode(UpsertNodePayload {
                id: decl.id.clone(),
                label: "Entity".to_string(),
                properties: entity_props,
            }),
        });

        events.push(Event {
            version: EVENT_VERSION,
            timestamp,
            event_type: EventType::LinkNodes,
            payload: EventPayload::LinkNodes(LinkNodesPayload {
                from_id: decl.id.clone(),
                to_id: file_id.clone(),
                relationship: "DECLARED_IN".to_string(),
                properties: HashMap::new(),
            }),
        });
    }

    // 7. Resolve references via LSP or collect for background resolution (Spec #2)
    let abs_path = std::env::current_dir()
        .unwrap_or_default()
        .join(file_path);
    let file_uri = format!("file://{}", abs_path.display());

    if let Some(lsp_client) = lsp {
        // Inline LSP resolution (backward-compatible path)
        let _ = lsp_client.notify_open(&file_uri, content_str, adapter.language_id());

        for rf in references {
            if let Ok(locations) =
                lsp_client.get_definition(&file_uri, rf.line as u32, rf.col as u32)
            {
                if let Some(loc) = locations.first() {
                    let absolute_path = uri_to_path(&loc.uri);
                    let target_file_path = std::path::Path::new(&absolute_path)
                        .strip_prefix(std::env::current_dir().unwrap_or_default())
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or(absolute_path);
                    let target_id = format!("{}:{}", target_file_path, rf.name);

                    let source_id = rf
                        .enclosing_function_id
                        .unwrap_or_else(|| file_id.clone());

                    events.push(Event {
                        version: EVENT_VERSION,
                        timestamp,
                        event_type: EventType::LinkNodes,
                        payload: EventPayload::LinkNodes(LinkNodesPayload {
                            from_id: source_id,
                            to_id: target_id,
                            relationship: rf.ref_type,
                            properties: HashMap::new(),
                        }),
                    });
                }
            }
        }
        (events, Vec::new())
    } else {
        // No LSP provided — collect references for background resolution (Spec #2)
        let content_arc = std::sync::Arc::new(content_str.to_string());
        let pending: Vec<PendingReference> = references
            .iter()
            .map(|rf| {
                let source_id = rf
                    .enclosing_function_id
                    .clone()
                    .unwrap_or_else(|| file_id.clone());
                PendingReference {
                    source_file: file_id.clone(),
                    source_file_uri: file_uri.clone(),
                    source_id,
                    ref_name: rf.name.clone(),
                    ref_type: rf.ref_type.clone(),
                    line: rf.line as u32,
                    col: rf.col as u32,
                    content: std::sync::Arc::clone(&content_arc),
                    language_id: adapter.language_id().to_string(),
                }
            })
            .collect();
        (events, pending)
    }
}

// ─── Document Reconciliation (markdown, etc.) ─────────────────────────────────

/// Reconcile a non-code document file (e.g., markdown).
///
/// Parses the document into `Section` entities, creates `DECLARED_IN` edges
/// to the file node, and resolves `REFERENCES` edges to existing code entities
/// via inline-code and file-path name matching against the graph.
fn reconcile_document(
    file_path: &Path,
    content: &str,
    doc_adapter: &dyn crate::document_adapter::DocumentAdapter,
    engine: &crate::graph::MemoryEngine,
) -> (Vec<Event>, Vec<PendingReference>) {
    let mut events = Vec::new();
    let timestamp = get_timestamp();
    let file_id = format!("{}", file_path.display());

    // Parse the document into sections
    let sections = doc_adapter.parse_document(file_path, content);

    // Upsert Section entities and DECLARED_IN edges
    for section in &sections {
        let mut entity_props = HashMap::new();
        entity_props.insert(
            "entity_type".to_string(),
            serde_json::Value::String("Section".to_string()),
        );
        entity_props.insert(
            "name".to_string(),
            serde_json::Value::String(section.name.clone()),
        );
        entity_props.insert(
            "content".to_string(),
            serde_json::Value::String(section.content.clone()),
        );
        entity_props.insert(
            "status".to_string(),
            serde_json::Value::String("active".to_string()),
        );
        entity_props.insert(
            "last_modified".to_string(),
            serde_json::Value::Number(serde_json::Number::from(timestamp)),
        );
        // Store metadata: heading level and line range
        let metadata = serde_json::json!({
            "level": section.level,
            "start_line": section.start_line,
            "end_line": section.end_line,
        });
        entity_props.insert(
            "metadata".to_string(),
            serde_json::Value::String(metadata.to_string()),
        );

        events.push(Event {
            version: EVENT_VERSION,
            timestamp,
            event_type: EventType::UpsertNode,
            payload: EventPayload::UpsertNode(UpsertNodePayload {
                id: section.id.clone(),
                label: "Entity".to_string(),
                properties: entity_props,
            }),
        });

        events.push(Event {
            version: EVENT_VERSION,
            timestamp,
            event_type: EventType::LinkNodes,
            payload: EventPayload::LinkNodes(LinkNodesPayload {
                from_id: section.id.clone(),
                to_id: file_id.clone(),
                relationship: "DECLARED_IN".to_string(),
                properties: HashMap::new(),
            }),
        });
    }

    // Resolve REFERENCES edges via graph name-matching
    let ref_events = resolve_document_references(&sections, engine);
    events.extend(ref_events);

    (events, Vec::new())
}

/// Resolve `REFERENCES` edges from document sections to existing code entities.
///
/// Uses inline-code matching (backtick-quoted identifiers) as the primary
/// mechanism and file-path matching as a secondary mechanism. No LSP is used —
/// the graph itself is the resolver.
fn resolve_document_references(
    sections: &[crate::document_adapter::ParsedSection],
    engine: &crate::graph::MemoryEngine,
) -> Vec<Event> {
    let mut events = Vec::new();
    let timestamp = get_timestamp();

    // Build a name → node_ids index from existing Function and Class entities.
    // This is O(n) once per file reconciliation, not per reference.
    let mut name_index: HashMap<String, Vec<String>> = HashMap::new();
    for node in engine.all_nodes() {
        if let crate::types::NodeLabel::Entity { entity_type, .. } = &node.label {
            if entity_type == "Function" || entity_type == "Class" {
                name_index
                    .entry(node.name.clone())
                    .or_default()
                    .push(node.id.clone());
            }
        }
    }

    // Also index File nodes by their ID for file-path matching
    let file_index: std::collections::HashSet<String> = engine
        .all_nodes()
        .iter()
        .filter(|n| matches!(&n.label, crate::types::NodeLabel::Entity { entity_type, .. } if entity_type == "File"))
        .map(|n| n.id.clone())
        .collect();

    let mut seen_edges: std::collections::HashSet<(String, String)> = HashSet::new();

    for section in sections {
        // 1. Inline code matching (primary, high-precision)
        for ref_name in &section.inline_code_refs {
            if let Some(targets) = name_index.get(ref_name) {
                for target_id in targets {
                    let edge_key = (section.id.clone(), target_id.clone());
                    if seen_edges.contains(&edge_key) {
                        continue;
                    }
                    seen_edges.insert(edge_key.clone());

                    let mut props = HashMap::new();
                    props.insert(
                        "match_type".to_string(),
                        serde_json::json!("inline_code"),
                    );
                    props.insert(
                        "matched_text".to_string(),
                        serde_json::json!(ref_name),
                    );

                    events.push(Event {
                        version: EVENT_VERSION,
                        timestamp,
                        event_type: EventType::LinkNodes,
                        payload: EventPayload::LinkNodes(LinkNodesPayload {
                            from_id: section.id.clone(),
                            to_id: target_id.clone(),
                            relationship: "REFERENCES".to_string(),
                            properties: props,
                        }),
                    });
                }
            }
        }

        // 2. File path matching (secondary)
        for path_ref in &section.file_path_refs {
            // Try exact match against file IDs
            if file_index.contains(path_ref) {
                let edge_key = (section.id.clone(), path_ref.clone());
                if seen_edges.contains(&edge_key) {
                    continue;
                }
                seen_edges.insert(edge_key);

                let mut props = HashMap::new();
                props.insert(
                    "match_type".to_string(),
                    serde_json::json!("file_path"),
                );
                props.insert(
                    "matched_text".to_string(),
                    serde_json::json!(path_ref),
                );

                events.push(Event {
                    version: EVENT_VERSION,
                    timestamp,
                    event_type: EventType::LinkNodes,
                    payload: EventPayload::LinkNodes(LinkNodesPayload {
                        from_id: section.id.clone(),
                        to_id: path_ref.clone(),
                        relationship: "REFERENCES".to_string(),
                        properties: props,
                    }),
                });
            }
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_adapter::{RustAdapter, TypeScriptAdapter, PythonAdapter};

    // ── 7.1 Source Text Extraction ───────────────────────────────────

    #[test]
    fn test_source_text_rust_function() {
        let adapter = RustAdapter;
        let content = r#"
pub fn embed_chunked(
    &self,
    text: &str,
    max_tokens: usize,
) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
    let total_tokens = self.count_tokens(text);
    if total_tokens <= max_tokens {
        return Ok(vec![self.embed(text)?]);
    }
    Ok(vec![])
}
"#;
        let (declarations, _) = parse_file(
            std::path::Path::new("test.rs"),
            content,
            &adapter,
        );
        let func = declarations.iter().find(|d| d.name == "embed_chunked").unwrap();
        assert_eq!(func.entity_type, "Function");
        assert!(func.source_text.contains("pub fn embed_chunked"));
        assert!(func.source_text.contains("max_tokens"));
        assert!(func.source_text.contains("count_tokens"));
    }

    #[test]
    fn test_source_text_rust_struct() {
        let adapter = RustAdapter;
        let content = r#"
pub struct EmbeddingModel {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}
"#;
        let (declarations, _) = parse_file(
            std::path::Path::new("test.rs"),
            content,
            &adapter,
        );
        let class = declarations.iter().find(|d| d.name == "EmbeddingModel").unwrap();
        assert_eq!(class.entity_type, "Class");
        assert!(class.source_text.contains("pub struct EmbeddingModel"));
        assert!(class.source_text.contains("session"));
        assert!(class.source_text.contains("tokenizer"));
    }

    #[test]
    fn test_source_text_typescript_arrow_function() {
        let adapter = TypeScriptAdapter;
        let content = r#"
const foo = (x: number): number => {
    return x + 1;
};
"#;
        let (declarations, _) = parse_file(
            std::path::Path::new("test.ts"),
            content,
            &adapter,
        );
        let func = declarations.iter().find(|d| d.name == "foo").unwrap();
        assert_eq!(func.entity_type, "Function");
        // The parent is variable_declarator, which includes the arrow function body
        // but not the `const` keyword.
        assert!(func.source_text.contains("foo"));
        assert!(func.source_text.contains("=>"));
        assert!(func.source_text.contains("return x + 1"));
    }

    #[test]
    fn test_source_text_typescript_class() {
        let adapter = TypeScriptAdapter;
        let content = r#"
class MyClass {
    method() {
        return 42;
    }
}
"#;
        let (declarations, _) = parse_file(
            std::path::Path::new("test.ts"),
            content,
            &adapter,
        );
        let class = declarations.iter().find(|d| d.name == "MyClass").unwrap();
        assert_eq!(class.entity_type, "Class");
        assert!(class.source_text.contains("class MyClass"));
        assert!(class.source_text.contains("method"));
    }

    #[test]
    fn test_source_text_python_function() {
        let adapter = PythonAdapter;
        let content = r#"
def compute_score(a, b):
    result = a + b
    return result
"#;
        let (declarations, _) = parse_file(
            std::path::Path::new("test.py"),
            content,
            &adapter,
        );
        let func = declarations.iter().find(|d| d.name == "compute_score").unwrap();
        assert_eq!(func.entity_type, "Function");
        assert!(func.source_text.contains("def compute_score"));
        assert!(func.source_text.contains("result = a + b"));
        assert!(func.source_text.contains("return result"));
    }

    #[test]
    fn test_source_text_python_class() {
        let adapter = PythonAdapter;
        let content = r#"
class Parser:
    def parse(self):
        pass
"#;
        let (declarations, _) = parse_file(
            std::path::Path::new("test.py"),
            content,
            &adapter,
        );
        let class = declarations.iter().find(|d| d.name == "Parser").unwrap();
        assert_eq!(class.entity_type, "Class");
        assert!(class.source_text.contains("class Parser"));
        assert!(class.source_text.contains("def parse"));
    }

    #[test]
    fn test_source_text_empty_function_body() {
        // A trait method with no body (just a signature in a trait)
        let adapter = RustAdapter;
        let content = "trait Foo { fn bar(&self); }\n";
        let (declarations, _) = parse_file(
            std::path::Path::new("test.rs"),
            content,
            &adapter,
        );
        // If bar is captured, its source_text should contain at least the signature
        if let Some(func) = declarations.iter().find(|d| d.name == "bar") {
            assert!(func.source_text.contains("fn bar"));
        }
    }

    #[test]
    fn test_source_text_not_empty_for_normal_function() {
        let adapter = RustAdapter;
        let content = "fn simple() { let x = 1; }\n";
        let (declarations, _) = parse_file(
            std::path::Path::new("test.rs"),
            content,
            &adapter,
        );
        let func = declarations.iter().find(|d| d.name == "simple").unwrap();
        assert!(!func.source_text.is_empty());
        assert!(func.source_text.contains("fn simple"));
        assert!(func.source_text.contains("let x = 1"));
    }
}