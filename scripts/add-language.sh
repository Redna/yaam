#!/usr/bin/env bash
#
# YAAM Language Adapter Scaffolding Utility
# ===========================================
#
# Generates all boilerplate code needed to add a new language to YAAM.
#
# Usage:
#   ./scripts/add-language.sh [--apply] <name> <extensions> <tree-sitter-crate> <crate-version> <lsp-command> [lsp-args...]
#
# Arguments:
#   name              Human-readable language name (e.g. "Rust", "Go", "Java")
#   extensions        Comma-separated file extensions without dots (e.g. "rs" or "go")
#   tree-sitter-crate crates.io name of the tree-sitter grammar (e.g. "tree-sitter-rust")
#   crate-version     crates.io version (e.g. "0.21")
#   lsp-command       LSP server executable (e.g. "rust-analyzer") or "none"
#   lsp-args          Optional arguments for the LSP server (e.g. "--stdio")
#
# Examples:
#   ./scripts/add-language.sh Rust rs tree-sitter-rust 0.21 rust-analyzer
#   ./scripts/add-language.sh Go go tree-sitter-go 0.21 gopls serve
#   ./scripts/add-language.sh Ruby rb tree-sitter-ruby 0.21 solargraph --stdio
#
set -euo pipefail

# ─── Parse Arguments ────────────────────────────────────────────────────────

if [[ "${1:-}" == "--apply" ]]; then
  APPLY=true
  shift
else
  APPLY=false
fi

if [[ $# -lt 5 ]]; then
  echo "Usage: $0 [--apply] <name> <extensions> <tree-sitter-crate> <crate-version> <lsp-command> [lsp-args...]"
  echo ""
  echo "Examples:"
  echo "  $0 Rust rs tree-sitter-rust 0.21 rust-analyzer"
  echo "  $0 Go go tree-sitter-go 0.21 gopls serve"
  echo "  $0 Ruby rb tree-sitter-ruby 0.21 solargraph --stdio"
  exit 1
fi

NAME="$1"
EXTENSIONS="$2"
TS_CRATE="$3"
TS_VERSION="$4"
LSP_CMD="$5"
shift 5
LSP_ARGS=("$@")

# ─── Derive Identifiers ─────────────────────────────────────────────────────

STRUCT_NAME="${NAME}"
LANG_ID=$(echo "$NAME" | tr '[:upper:]' '[:lower:]')
# tree-sitter-rust → tree_sitter_rust::language()
TS_CRATE_SNAKE=$(echo "$TS_CRATE" | tr '-' '_')
TS_FN="${TS_CRATE_SNAKE}::language()"

# Convert extensions to Rust match arms
IFS=',' read -ra EXT_ARRAY <<< "$EXTENSIONS"
MATCH_ARMS=""
for ext in "${EXT_ARRAY[@]}"; do
  if [[ -n "$MATCH_ARMS" ]]; then
    MATCH_ARMS="${MATCH_ARMS} | \"${ext}\""
  else
    MATCH_ARMS="\"${ext}\""
  fi
done

# Build extensions vec![] for registry
EXT_VEC="vec!["
for ext in "${EXT_ARRAY[@]}"; do
  EXT_VEC="${EXT_VEC}\"${ext}\".to_string(), "
done
EXT_VEC="${EXT_VEC}]"

# ─── Generate LSP block ─────────────────────────────────────────────────────

if [[ "$LSP_CMD" == "none" || "$LSP_CMD" == "NONE" ]]; then
  LSP_BLOCK='    fn lsp_command(&self) -> Option<LspCommand> {
        None
    }'
  LSP_INFO="lsp_command: None,"
  LSP_DISPLAY="none"
else
  if [[ ${#LSP_ARGS[@]} -eq 0 ]]; then
    LSP_DISPLAY="$LSP_CMD"
    ARGS_VEC="vec![]"
  else
    LSP_DISPLAY="$LSP_CMD ${LSP_ARGS[*]}"
    ARGS_VEC="vec!["
    for arg in "${LSP_ARGS[@]}"; do
      ARGS_VEC="${ARGS_VEC}\"${arg}\".to_string(), "
    done
    ARGS_VEC="${ARGS_VEC}]"
  fi
  LSP_BLOCK="    fn lsp_command(&self) -> Option<LspCommand> {
        Some(LspCommand {
            command: \"${LSP_CMD}\".to_string(),
            args: ${ARGS_VEC},
        })
    }"
  LSP_INFO="lsp_command: ${STRUCT_NAME}Adapter.lsp_command(),"
fi

# ─── Generate Adapter Code ──────────────────────────────────────────────────

generate_adapter() {
  cat <<ADAPTER_EOF

// ─── ${NAME} Adapter ──────────────────────────────────────────────────────────

pub struct ${STRUCT_NAME}Adapter;

impl LanguageAdapter for ${STRUCT_NAME}Adapter {
    fn language(&self) -> Language {
        ${TS_FN}
    }

    fn query_source(&self) -> &'static str {
        r#"
        // TODO: Replace with ${NAME}-specific tree-sitter queries.
        // Capture name conventions (checked via prefix in parse_file):
        //   @class.name     -> Class declaration
        //   @function.name  -> Function declaration
        //   @method.name    -> Function declaration (method)
        //   @variable.name  -> Function declaration (variable holding function)
        //   @call.name      -> CALLS reference
        //   @import.name    -> IMPORTS reference
        //
        // Use tree-sitter playground to find node types:
        //   https://tree-sitter.github.io/tree-sitter/playground
        "#
    }

    fn language_id(&self) -> &'static str {
        "${LANG_ID}"
    }

    fn find_enclosing_function(
        &self,
        node: Node,
        source_code: &[u8],
        file_path: &Path,
    ) -> Option<String> {
        // TODO: Replace with ${NAME}-specific enclosing function/class node kinds.
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

${LSP_BLOCK}
}
ADAPTER_EOF
}

# ─── Generate Registry Entry ────────────────────────────────────────────────

REGISTRY_ENTRY="        LanguageInfo {
            name: \"${NAME}\".to_string(),
            extensions: ${EXT_VEC},
            language_id: \"${LANG_ID}\".to_string(),
            ${LSP_INFO}
        },"

# ─── Generate Match Arm ─────────────────────────────────────────────────────

MATCH_ARM="        ${MATCH_ARMS} => Some(Box::new(${STRUCT_NAME}Adapter)),"

# ─── Generate Cargo.toml Line ───────────────────────────────────────────────

CARGO_LINE="${TS_CRATE} = \"${TS_VERSION}\""

# ─── Output ─────────────────────────────────────────────────────────────────

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ADAPTER_FILE="${ROOT}/src-rust/src/language_adapter.rs"
CARGO_FILE="${ROOT}/src-rust/Cargo.toml"

echo "╔══════════════════════════════════════════════════════════════════════════╗"
echo "║  YAAM Language Scaffolding: ${NAME}                                        ║"
echo "╚══════════════════════════════════════════════════════════════════════════╝"
echo ""

if $APPLY; then
  # ─── Automatic mode ──────────────────────────────────────────────────────

  echo "► Applying changes automatically..."
  echo ""

  # 1. Add to Cargo.toml
  if grep -q "${TS_CRATE}" "$CARGO_FILE"; then
    echo "  ✓ Cargo.toml: ${TS_CRATE} already present"
  else
    sed -i "/^tree-sitter /a ${CARGO_LINE}" "$CARGO_FILE"
    echo "  ✓ Cargo.toml: Added ${CARGO_LINE}"
  fi

  # 2. Add adapter struct + impl
  if grep -q "${STRUCT_NAME}Adapter" "$ADAPTER_FILE"; then
    echo "  ✓ language_adapter.rs: ${STRUCT_NAME}Adapter already present"
  else
    generate_adapter >> "$ADAPTER_FILE"
    echo "  ✓ language_adapter.rs: Added ${STRUCT_NAME}Adapter struct + impl"
  fi

  # 3. Add match arm to get_adapter()
  if grep -A20 "pub fn get_adapter" "$ADAPTER_FILE" | grep -q "${STRUCT_NAME}Adapter"; then
    echo "  ✓ get_adapter(): ${STRUCT_NAME}Adapter already registered"
  else
    sed -i "/^        _ => None,/i\\${MATCH_ARM}" "$ADAPTER_FILE"
    echo "  ✓ get_adapter(): Added match arm for ${MATCH_ARMS}"
  fi

  # 4. Add registry entry to list_languages()
  if grep -A30 "pub fn list_languages" "$ADAPTER_FILE" | grep -q "\"${LANG_ID}\""; then
    echo "  ✓ list_languages(): ${LANG_ID} already registered"
  else
    sed -i "/^    ]$/i\\${REGISTRY_ENTRY}" "$ADAPTER_FILE"
    echo "  ✓ list_languages(): Added registry entry"
  fi

  echo ""
  echo "► Next steps:"
  echo "  1. Edit the tree-sitter query in ${STRUCT_NAME}Adapter::query_source()"
  echo "     Use https://tree-sitter.github.io/tree-sitter/playground to find node types."
  echo "  2. Edit ${STRUCT_NAME}Adapter::find_enclosing_function() if ${NAME}"
  echo "     uses different AST node kinds for functions/classes."
  echo "  3. Run: cd src-rust && cargo build --release"
  if [[ "$LSP_CMD" != "none" && "$LSP_CMD" != "NONE" ]]; then
    echo "  4. Install the LSP server: ${LSP_DISPLAY}"
  fi
  echo ""

else
  # ─── Manual mode: print code with instructions ───────────────────────────

  echo "═══════════════════════════════════════════════════════════════════════════"
  echo "  STEP 1: Add to src-rust/Cargo.toml [dependencies]"
  echo "═══════════════════════════════════════════════════════════════════════════"
  echo ""
  echo "  ${CARGO_LINE}"
  echo ""

  echo "═══════════════════════════════════════════════════════════════════════════"
  echo "  STEP 2: Add adapter to src-rust/src/language_adapter.rs"
  echo "  (paste at the end of the file)"
  echo "═══════════════════════════════════════════════════════════════════════════"
  echo ""
  generate_adapter
  echo ""

  echo "═══════════════════════════════════════════════════════════════════════════"
  echo "  STEP 3: Register in get_adapter() match block"
  echo "  (add before the _ => None line)"
  echo "═══════════════════════════════════════════════════════════════════════════"
  echo ""
  echo "${MATCH_ARM}"
  echo ""

  echo "═══════════════════════════════════════════════════════════════════════════"
  echo "  STEP 4: Add to list_languages() registry"
  echo "  (add before the closing bracket of the vec![])"
  echo "═══════════════════════════════════════════════════════════════════════════"
  echo ""
  echo "${REGISTRY_ENTRY}"
  echo ""

  echo "═══════════════════════════════════════════════════════════════════════════"
  echo "  STEP 5: Customize the generated code"
  echo "═══════════════════════════════════════════════════════════════════════════"
  echo ""
  echo "  The generated query_source() is a template with TODO comments."
  echo "  Replace it with ${NAME}-specific tree-sitter queries."
  echo ""
  echo "  Tools to find node types:"
  echo "    Web:   https://tree-sitter.github.io/tree-sitter/playground"
  echo ""
  echo "  Capture name conventions (checked via prefix):"
  echo "    @class.name     -> Class declaration"
  echo "    @function.name  -> Function declaration"
  echo "    @method.name    -> Function (method)"
  echo "    @variable.name  -> Function (variable holding function)"
  echo "    @call.name      -> CALLS reference"
  echo "    @import.name    -> IMPORTS reference"
  echo ""

  echo "═══════════════════════════════════════════════════════════════════════════"
  echo "  STEP 6: Build and install LSP"
  echo "═══════════════════════════════════════════════════════════════════════════"
  echo ""
  echo "  cd src-rust && cargo build --release"
  if [[ "$LSP_CMD" != "none" && "$LSP_CMD" != "NONE" ]]; then
    echo ""
    echo "  Install the LSP server: ${LSP_DISPLAY}"
  fi
  echo ""
  echo "  Or re-run with --apply to auto-insert the code:"
  echo "    $0 --apply $NAME $EXTENSIONS $TS_CRATE $TS_VERSION $LSP_CMD ${LSP_ARGS[*]}"
  echo ""
fi