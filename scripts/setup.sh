#!/usr/bin/env bash
#
# YAAM Setup & Bootstrap
# ========================
#
# One-command setup for a fresh clone of the YAAM repository.
# Reads .yaam/config.json to determine which languages to enable,
# then installs LSP servers, builds the Rust daemon, downloads model
# weights, and compiles the TypeScript extension.
#
# Usage:
#   ./scripts/setup.sh              Interactive (prompts for each step)
#   ./scripts/setup.sh --yes        Non-interactive (install everything)
#   ./scripts/setup.sh --no-lsp     Skip LSP server installation
#   ./scripts/setup.sh --no-build   Skip Rust + TS build (LSP only)
#
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CONFIG_FILE="${ROOT}/.yaam/config.json"

# ─── Colors ─────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}►${NC} $*"; }
ok()    { echo -e "${GREEN}✓${NC} $*"; }
warn()  { echo -e "${YELLOW}⚠${NC}  $*"; }
fail()  { echo -e "${RED}✗${NC} $*"; }

# ─── Parse Flags ─────────────────────────────────────────────────────────────
AUTO_YES=false
SKIP_LSP=false
SKIP_BUILD=false

for arg in "$@"; do
  case "$arg" in
    --yes|-y)     AUTO_YES=true ;;
    --no-lsp)     SKIP_LSP=true ;;
    --no-build)   SKIP_BUILD=true ;;
    --help|-h)
      echo "Usage: $0 [--yes] [--no-lsp] [--no-build]"
      echo ""
      echo "  --yes        Non-interactive: install everything without prompts"
      echo "  --no-lsp     Skip LSP server installation"
      echo "  --no-build   Skip Rust daemon + TS extension build"
      exit 0
      ;;
    *)
      fail "Unknown option: $arg"
      exit 1
      ;;
  esac
done

# ─── Helpers ─────────────────────────────────────────────────────────────────

prompt() {
  if $AUTO_YES; then return 0; fi
  echo -en "${BOLD}$* [Y/n]${NC} "
  read -r response
  case "$response" in
    [nN][oO]|[nN]) return 1 ;;
    *) return 0 ;;
  esac
}

command_exists() {
  command -v "$1" &>/dev/null
}

# ─── Banner ──────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║  YAAM Setup & Bootstrap                                         ║${NC}"
echo -e "${BOLD}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""

# ─── Check Prerequisites ────────────────────────────────────────────────────
info "Checking prerequisites..."

MISSING=()

if ! command_exists rustc; then
  MISSING+=("rustc (Install via https://rustup.rs)")
fi
if ! command_exists cargo; then
  MISSING+=("cargo (Install via https://rustup.rs)")
fi
if ! command_exists node; then
  MISSING+=("node (Install via https://nodejs.org)")
fi
if ! command_exists npm; then
  MISSING+=("npm (Comes with Node.js)")
fi

if [[ ${#MISSING[@]} -gt 0 ]]; then
  echo ""
  fail "Missing prerequisites:"
  for m in "${MISSING[@]}"; do
    echo "    - $m"
  done
  echo ""
  exit 1
fi

ok "All prerequisites found: rust $(rustc --version | head -1 | awk '{print $2}'), node $(node --version), npm $(npm --version)"

# ─── Load Config ────────────────────────────────────────────────────────────
if [[ ! -f "$CONFIG_FILE" ]]; then
  warn ".yaam/config.json not found — using defaults (all languages enabled)"
  CONFIG_LANGUAGES='{"typescript":{"enabled":true,"lsp":{"command":"typescript-language-server","install":"npm install -g typescript-language-server typescript","check":"typescript-language-server --version"}},"python":{"enabled":true,"lsp":{"command":"pylsp","install":"pip install python-lsp-server","check":"pylsp --version"}},"rust":{"enabled":true,"lsp":{"command":"rust-analyzer","install":"rustup component add rust-analyzer","check":"rust-analyzer --version"}}}'
else
  CONFIG_LANGUAGES=$(python3 -c "
import json
with open('$CONFIG_FILE') as f:
    cfg = json.load(f)
langs = {}
for name, info in cfg.get('languages', {}).items():
    if info.get('enabled', True):
        langs[name] = info
print(json.dumps(langs))
" 2>/dev/null || echo '{}')
fi

# ─── Step 1: Install npm dependencies ───────────────────────────────────────
echo ""
echo -e "${BOLD}── Step 1: Node dependencies ──────────────────────────────────${NC}"
if [[ -d "${ROOT}/node_modules" ]]; then
  ok "node_modules already exists (skipping npm install)"
else
  if prompt "Install npm dependencies?"; then
    info "Running npm install..."
    (cd "$ROOT" && npm install)
    ok "npm dependencies installed"
  else
    warn "Skipped — you'll need to run 'npm install' manually"
  fi
fi

# ─── Step 2: Build Rust daemon + model weights ──────────────────────────────
if ! $SKIP_BUILD; then
  echo ""
  echo -e "${BOLD}── Step 2: Rust daemon ─────────────────────────────────────────${NC}"

  BIN_PATH="${ROOT}/src-rust/target/release/yaam-engine"

  if [[ -f "$BIN_PATH" ]]; then
    ok "Release binary already exists (skipping cargo build)"
  else
    if prompt "Build Rust daemon (cargo build --release)?"; then
      info "Building Rust daemon..."
      (cd "${ROOT}/src-rust" && cargo build --release)
      ok "Rust daemon built"
    else
      warn "Skipped — you'll need to run 'cd src-rust && cargo build --release' manually"
    fi
  fi

  # Model weights
  echo ""
  echo -e "${BOLD}── Step 3: Model weights ──────────────────────────────────────${NC}"
  MODEL_DIR="${ROOT}/src-rust/model"
  if [[ -d "$MODEL_DIR" ]] && [[ -f "${MODEL_DIR}/model.onnx" ]] && [[ -f "${MODEL_DIR}/tokenizer.json" ]]; then
    ok "Model weights already downloaded"
  else
    if prompt "Download gte-small model weights (required for semantic search)?"; then
      info "Downloading model weights..."
      (cd "${ROOT}/src-rust" && cargo run --release -- setup)
      ok "Model weights downloaded"
    else
      warn "Skipped — semantic search (yaam_search) will not work without model weights"
      warn "Run 'cd src-rust && cargo run --release -- setup' later to download"
    fi
  fi
fi

# ─── Step 4: Install LSP servers ───────────────────────────────────────────
if ! $SKIP_LSP; then
  echo ""
  echo -e "${BOLD}── Step 4: Language servers (LSP) ──────────────────────────────${NC}"
  echo ""
  echo "  LSP servers are started lazily — only when a file of that"
  echo "  language is first reconciled. Without them, tree-sitter parsing"
  echo "  still works, but cross-file CALLS/IMPORTS edges won't be resolved."
  echo ""

  if prompt "Install LSP servers for enabled languages?"; then
    # Parse languages from config using python3
    LANG_COUNT=$(echo "$CONFIG_LANGUAGES" | python3 -c "import json,sys; print(len(json.load(sys.stdin)))" 2>/dev/null || echo 0)

    if [[ "$LANG_COUNT" -eq 0 ]]; then
      warn "No languages enabled in .yaam/config.json"
    else
      echo "$CONFIG_LANGUAGES" | python3 -c "
import json, sys, subprocess, shutil

langs = json.load(sys.stdin)
for name, info in langs.items():
    lsp = info.get('lsp', {})
    cmd = lsp.get('command', '')
    install = lsp.get('install', '')
    check = lsp.get('check', '')

    print(f'  {name}:')

    # Check if already installed
    if shutil.which(cmd):
        print(f'    \033[0;32m✓\033[0m Already installed ({cmd})')
    else:
        if not install:
            print(f'    \033[1;33m⚠\033[0m  No install command configured for {name}')
            continue

        print(f'    \033[0;34m►\033[0m Installing: {install}')
        try:
            result = subprocess.run(
                install, shell=True, capture_output=True, text=True, timeout=120
            )
            if result.returncode == 0:
                print(f'    \033[0;32m✓\033[0m Installed {cmd}')
            else:
                print(f'    \033[0;31m✗\033[0m Failed: {result.stderr.strip()[:200]}')
        except subprocess.TimeoutExpired:
            print(f'    \033[0;31m✗\033[0m Timed out')
        except Exception as e:
            print(f'    \033[0;31m✗\033[0m Error: {e}')
" 2>&1
    fi
  else
    warn "Skipped LSP installation"
    echo ""
    echo "  Install manually later:"
    echo "$CONFIG_LANGUAGES" | python3 -c "
import json, sys
langs = json.load(sys.stdin)
for name, info in langs.items():
    lsp = info.get('lsp', {})
    install = lsp.get('install', '')
    if install:
        print(f'    {name}: {install}')
" 2>/dev/null
  fi
fi

# ─── Step 5: Build TypeScript extension ─────────────────────────────────────
if ! $SKIP_BUILD; then
  echo ""
  echo -e "${BOLD}── Step 5: TypeScript extension ────────────────────────────────${NC}"
  if prompt "Build TypeScript extension (npm run build)?"; then
    info "Building..."
    (cd "$ROOT" && npm run build)
    ok "TypeScript extension built"
  else
    warn "Skipped — you'll need to run 'npm run build' manually"
  fi
fi

# ─── Summary ────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║  Setup Complete!                                               ║${NC}"
echo -e "${BOLD}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""

# Check final state
$SKIP_BUILD || [[ -f "${ROOT}/src-rust/target/release/yaam-engine" ]] && ok "Rust daemon: built" || warn "Rust daemon: not built"
$SKIP_BUILD || ([[ -d "${ROOT}/src-rust/model" ]] && [[ -f "${ROOT}/src-rust/model/model.onnx" ]]) && ok "Model weights: downloaded" || warn "Model weights: missing (semantic search disabled)"
$SKIP_BUILD || [[ -d "${ROOT}/node_modules" ]] && ok "Node deps: installed" || warn "Node deps: not installed"

if ! $SKIP_LSP; then
  echo "$CONFIG_LANGUAGES" | python3 -c "
import json, sys, shutil
langs = json.load(sys.stdin)
for name, info in langs.items():
    cmd = info.get('lsp', {}).get('command', '')
    if cmd and shutil.which(cmd):
        print(f'  \033[0;32m✓\033[0m {name} LSP ({cmd}): installed')
    elif cmd:
        print(f'  \033[1;33m⚠\033[0m  {name} LSP ({cmd}): not found (declarations work, no cross-file edges)')
" 2>/dev/null
fi

echo ""
echo -e "  ${BOLD}Next:${NC} YAAM auto-loads when pi starts. Use /yaam for status,"
echo -e "         /yaam viz for the graph visualizer."
echo ""