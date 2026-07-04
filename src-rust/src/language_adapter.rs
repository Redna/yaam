//! Language adapter trait — strategy pattern for multi-language tree-sitter parsing.
//!
//! Each adapter encapsulates the tree-sitter grammar, query source, LSP language
//! identifier, and AST-walking logic specific to a programming language.
//! The reconciler delegates to the adapter so that adding a new language only
//! requires implementing this trait and registering it in [`get_adapter`].

use std::path::Path;
use tree_sitter::{Language, Node};

/// Configuration for starting a language server process.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LspCommand {
    /// Executable name (e.g. `"npx"`, `"pylsp"`).
    pub command: String,
    /// Command-line arguments.
    pub args: Vec<String>,
}

/// Trait that abstracts language-specific parsing behaviour.
///
/// Implementations provide:
/// - The tree-sitter `Language` (grammar) to use for parsing.
/// - The tree-sitter query source that captures declarations, calls, and imports.
/// - The LSP `languageId` string used in `textDocument/didOpen` notifications.
/// - A function that walks the AST to find the enclosing function/class for a
///   given node (used to attribute CALLS edges to the right declaring entity).
/// - The LSP server command to start (lazily, on first use) for cross-file
///   resolution.  Return `None` if no LSP server is known for this language.
pub trait LanguageAdapter: Send + Sync {
    /// Return the tree-sitter `Language` for this adapter.
    fn language(&self) -> Language;

    /// Return the tree-sitter query source string.
    ///
    /// Capture names must follow these conventions (checked via prefix):
    /// - `class.name`  → declaration with entity_type "Class"
    /// - `function.name`, `method.name`, `variable.name` → declaration, entity_type "Function"
    /// - `call.name`    → reference with ref_type "CALLS"
    /// - `import.name`  → reference with ref_type "IMPORTS"
    fn query_source(&self) -> &'static str;

    /// Return the LSP `languageId` for `textDocument/didOpen`.
    fn language_id(&self) -> &'static str;

    /// Walk up the AST from `node` to find the enclosing function or class
    /// declaration.  Returns the entity ID (`"file_path:name"`) if found.
    fn find_enclosing_function(
        &self,
        node: Node,
        source_code: &[u8],
        file_path: &Path,
    ) -> Option<String>;

    /// Returns the LSP server command for this language, if one is available.
    ///
    /// The caller is responsible for actually starting the process.  Returning
    /// `None` means cross-file resolution is not supported for this language
    /// (declarations will still be extracted via tree-sitter).
    fn lsp_command(&self) -> Option<LspCommand>;
}

// ─── Language Registry ─────────────────────────────────────────────────────

/// Metadata about a registered language, used for introspection via RPC
/// (`languages.list`) and by the scaffolding utility.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LanguageInfo {
    /// Human-readable language name.
    pub name: String,
    /// File extensions handled by this adapter (without leading dot).
    pub extensions: Vec<String>,
    /// LSP `languageId` string.
    pub language_id: String,
    /// LSP server command, if one is configured.
    pub lsp_command: Option<LspCommand>,
}

/// Returns metadata for every registered language.
///
/// This is the single source of truth for the language registry.
/// When you add a new adapter, also add an entry here.
pub fn list_languages() -> Vec<LanguageInfo> {
    vec![
        LanguageInfo {
            name: "TypeScript".to_string(),
            extensions: vec!["ts".to_string(), "tsx".to_string(), "js".to_string(), "jsx".to_string()],
            language_id: "typescript".to_string(),
            lsp_command: TypeScriptAdapter.lsp_command(),
        },
        LanguageInfo {
            name: "Python".to_string(),
            extensions: vec!["py".to_string()],
            language_id: "python".to_string(),
            lsp_command: PythonAdapter.lsp_command(),
        },
        LanguageInfo {
            name: "Rust".to_string(),
            extensions: vec!["rs".to_string()],
            language_id: "rust".to_string(),
            lsp_command: RustAdapter.lsp_command(),
        },
    ]
}

/// Factory: returns the appropriate adapter for a file based on its extension,
/// or `None` if the language is not supported.
pub fn get_adapter(file_path: &Path) -> Option<Box<dyn LanguageAdapter>> {
    let ext = file_path.extension().and_then(|e| e.to_str())?;
    match ext {
        "ts" | "tsx" | "js" | "jsx" => Some(Box::new(TypeScriptAdapter)),
        "py" => Some(Box::new(PythonAdapter)),
        "rs" => Some(Box::new(RustAdapter)),
        _ => None,
    }
}

// ─── TypeScript Adapter ─────────────────────────────────────────────────────

pub struct TypeScriptAdapter;

impl LanguageAdapter for TypeScriptAdapter {
    fn language(&self) -> Language {
        tree_sitter_typescript::language_typescript()
    }

    fn query_source(&self) -> &'static str {
        r#"
        (class_declaration name: (type_identifier) @class.name)
        (function_declaration name: (identifier) @function.name)
        (method_definition name: (property_identifier) @method.name)
        (lexical_declaration (variable_declarator name: (identifier) @variable.name value: [(arrow_function) (function_expression)]))
        (call_expression function: (identifier) @call.name)
        (call_expression function: (member_expression property: (property_identifier) @call.name))
        (import_statement (import_clause (identifier) @import.name))
        (import_statement (import_clause (named_imports (import_specifier name: (identifier) @import.name))))
        "#
    }

    fn language_id(&self) -> &'static str {
        "typescript"
    }

    fn find_enclosing_function(
        &self,
        node: Node,
        source_code: &[u8],
        file_path: &Path,
    ) -> Option<String> {
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
                        if value_node.kind() == "arrow_function"
                            || value_node.kind() == "function_expression"
                        {
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

    fn lsp_command(&self) -> Option<LspCommand> {
        Some(LspCommand {
            command: "npx".to_string(),
            args: vec![
                "typescript-language-server".to_string(),
                "--stdio".to_string(),
            ],
        })
    }
}

// ─── Python Adapter ─────────────────────────────────────────────────────────

pub struct PythonAdapter;

impl LanguageAdapter for PythonAdapter {
    fn language(&self) -> Language {
        tree_sitter_python::language()
    }

    fn query_source(&self) -> &'static str {
        r#"
        (class_definition name: (identifier) @class.name)
        (function_definition name: (identifier) @function.name)
        (call function: (identifier) @call.name)
        (call function: (attribute attribute: (identifier) @call.name))
        (import_statement (dotted_name) @import.name)
        (import_from_statement (dotted_name) @import.name)
        "#
    }

    fn language_id(&self) -> &'static str {
        "python"
    }

    fn find_enclosing_function(
        &self,
        node: Node,
        source_code: &[u8],
        file_path: &Path,
    ) -> Option<String> {
        let mut current = node.parent();
        while let Some(parent) = current {
            match parent.kind() {
                "function_definition" | "class_definition" => {
                    if let Some(name_node) = parent.child_by_field_name("name") {
                        let name = name_node.utf8_text(source_code).ok()?.to_string();
                        return Some(format!("{}:{}", file_path.display(), name));
                    }
                }
                _ => {}
            }
            current = parent.parent();
        }
        None
    }

    fn lsp_command(&self) -> Option<LspCommand> {
        Some(LspCommand {
            command: "pylsp".to_string(),
            args: vec![],
        })
    }
}

// ─── Rust Adapter ───────────────────────────────────────────────────────────

pub struct RustAdapter;

impl LanguageAdapter for RustAdapter {
    fn language(&self) -> Language {
        tree_sitter_rust::language()
    }

    fn query_source(&self) -> &'static str {
        r#"
        (struct_item name: (type_identifier) @class.name)
        (enum_item name: (type_identifier) @class.name)
        (trait_item name: (type_identifier) @class.name)
        (function_item name: (identifier) @function.name)
        (call_expression function: (identifier) @call.name)
        (call_expression function: (field_expression field: (field_identifier) @call.name))
        (call_expression function: (scoped_identifier) @call.name)
        (use_declaration (scoped_identifier) @import.name)
        (use_declaration (identifier) @import.name)
        "#
    }

    fn language_id(&self) -> &'static str {
        "rust"
    }

    fn find_enclosing_function(
        &self,
        node: Node,
        source_code: &[u8],
        file_path: &Path,
    ) -> Option<String> {
        let mut current = node.parent();
        while let Some(parent) = current {
            if parent.kind() == "function_item" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    let name = name_node.utf8_text(source_code).ok()?.to_string();
                    return Some(format!("{}:{}", file_path.display(), name));
                }
            }
            current = parent.parent();
        }
        None
    }

    fn lsp_command(&self) -> Option<LspCommand> {
        Some(LspCommand {
            command: "rust-analyzer".to_string(),
            args: vec![],
        })
    }
}