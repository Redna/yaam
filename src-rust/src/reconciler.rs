use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tree_sitter::{Parser, Query, QueryCursor, Node};

use crate::types::{
    Event, EventPayload, EventType, LinkNodesPayload, UpsertNodePayload, EVENT_VERSION,
    DeleteNodePayload,
};

pub struct ParsedDeclaration {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    pub line: usize,
}

pub struct ParsedReference {
    pub ref_type: String, // "CALLS" or "IMPORTS"
    pub name: String,
    pub line: usize, // 0-indexed for LSP
    pub col: usize,  // 0-indexed for LSP
    pub enclosing_function_id: Option<String>, // ID of the function containing this reference
}

/// Walk up the tree-sitter AST from a given node to find the enclosing
/// function/class declaration. Returns the entity ID ("file_path:name") if found.
fn find_enclosing_function(node: Node, source_code: &[u8], file_path: &Path) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "function_declaration" | "class_declaration" | "method_definition" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    let name = name_node.utf8_text(source_code).ok()?.to_string();
                    return Some(format!("{}:{}", file_path.display(), name));
                }
            }
            "variable_declarator" => {
                // Check if this is a function variable (const foo = () => {})
                if let Some(value_node) = parent.child_by_field_name("value") {
                    if value_node.kind() == "arrow_function" || value_node.kind() == "function_expression" {
                        if let Some(name_node) = parent.child_by_field_name("name") {
                            let name = name_node.utf8_text(source_code).ok()?.to_string();
                            return Some(format!("{}:{}", file_path.display(), name));
                        }
                    }
                }
            }
            _ => {}
        }
        current = parent.parent();
    }
    None
}

pub fn parse_typescript(file_path: &Path, content: &str) -> (Vec<ParsedDeclaration>, Vec<ParsedReference>) {
    let mut parser = Parser::new();
    let language = tree_sitter_typescript::language_typescript();
    parser.set_language(&language).expect("Error loading TypeScript grammar");

    let tree = parser.parse(content, None).unwrap();
    let source_code = content.as_bytes();

    let query_source = r#"
        (class_declaration name: (type_identifier) @class.name)
        (function_declaration name: (identifier) @function.name)
        (method_definition name: (property_identifier) @method.name)
        (lexical_declaration (variable_declarator name: (identifier) @variable.name value: [(arrow_function) (function_expression)]))
        (call_expression function: (identifier) @call.name)
        (call_expression function: (member_expression property: (property_identifier) @call.name))
        (import_statement (import_clause (identifier) @import.name))
        (import_statement (import_clause (named_imports (import_specifier name: (identifier) @import.name))))
    "#;

    let query = Query::new(&language, query_source).unwrap();
    let mut query_cursor = QueryCursor::new();
    let matches = query_cursor.matches(&query, tree.root_node(), source_code);

    let mut declarations = Vec::new();
    let mut references = Vec::new();

    for m in matches {
        for capture in m.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            let node = capture.node;

            let name = node.utf8_text(source_code).unwrap_or("").to_string();
            
            if capture_name.starts_with("call") {
                let enclosing = find_enclosing_function(node, source_code, file_path);
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

                declarations.push(ParsedDeclaration {
                    id,
                    entity_type: entity_type.to_string(),
                    name,
                    line: node.start_position().row + 1,
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
) -> Vec<Event> {
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
        return events;
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

    // 4. Parse content
    let (declarations, references) = parse_typescript(file_path, content_str);

    // 5. Upsert new declarations
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

    // 6. Resolve references via LSP
    if let Some(mut lsp_client) = lsp {
        // Use absolute path for the file URI so the LSP can resolve it correctly.
        let abs_path = std::env::current_dir()
            .unwrap_or_default()
            .join(file_path);
        let file_uri = format!("file://{}", abs_path.display());

        // Notify the LSP about this file's content so it can resolve definitions
        // even if the on-disk content differs or hasn't been indexed yet.
        let _ = lsp_client.notify_open(&file_uri, content_str, "typescript");

        for rf in references {
            if let Ok(locations) = lsp_client.get_definition(&file_uri, rf.line as u32, rf.col as u32) {
                if let Some(loc) = locations.first() {
                    let absolute_path = uri_to_path(&loc.uri);
                    let target_file_path = std::path::Path::new(&absolute_path)
                        .strip_prefix(std::env::current_dir().unwrap_or_default())
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or(absolute_path);
                    let target_id = format!("{}:{}", target_file_path, rf.name);

                    // Use the enclosing function as the source of the edge.
                    // Fall back to the file node if the call is at the top level.
                    let source_id = rf.enclosing_function_id
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
    }

    events
}