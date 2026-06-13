#!/usr/bin/env python3
import os
import json
import sys
import shutil
import subprocess

REPO_ROOT = os.path.abspath(os.path.dirname(__file__))
VENV_PYTHON = os.path.join(REPO_ROOT, ".venv", "bin", "python3")

def run_command(args, check=True):
    try:
        subprocess.run(args, check=check, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except subprocess.CalledProcessError as e:
        print(f"Error executing command {' '.join(args)}:\n{e.stderr.decode()}", file=sys.stderr)
        if check:
            sys.exit(1)

def setup_python_env():
    print("--> Setting up Python virtual environment and dependencies...")
    if not os.path.exists(os.path.join(REPO_ROOT, ".venv")):
        print("Creating virtual environment...")
        run_command(["python3", "-m", "venv", os.path.join(REPO_ROOT, ".venv")])
    
    requirements_path = os.path.join(REPO_ROOT, "requirements.txt")
    if os.path.exists(requirements_path):
        print("Installing requirements...")
        run_command([os.path.join(REPO_ROOT, ".venv", "bin", "pip"), "install", "-r", requirements_path])
    
    print("--> Initializing LadybugDB schema...")
    run_command([VENV_PYTHON, os.path.join(REPO_ROOT, "db.py")])

def configure_claude_code():
    print("--> Configuring Claude Code (Project & Global configurations)...")
    # Project scope is handled automatically by the .mcp.json at root.
    # We will offer to register it globally in ~/.claude.json.
    claude_config_path = os.path.expanduser("~/.claude.json")
    try:
        config_data = {}
        if os.path.exists(claude_config_path) and os.path.getsize(claude_config_path) > 0:
            with open(claude_config_path, "r") as f:
                config_data = json.load(f)
        
        mcp_servers = config_data.setdefault("mcpServers", {})
        mcp_servers["yaam_memory"] = {
            "command": VENV_PYTHON,
            "args": [os.path.join(REPO_ROOT, "server.py")]
        }
        
        with open(claude_config_path, "w") as f:
            json.dump(config_data, f, indent=2)
        print(f"Successfully added yaam_memory to global Claude Code configuration: {claude_config_path}")
    except Exception as e:
        print(f"Warning: Could not configure global Claude Code: {e}", file=sys.stderr)

def configure_antigravity_cli():
    print("--> Configuring Antigravity CLI...")
    # Project scope is handled automatically by .agents/mcp_config.json
    # We will offer to register it globally in ~/.gemini/config/mcp_config.json.
    antigravity_config_dir = os.path.expanduser("~/.gemini/config")
    os.makedirs(antigravity_config_dir, exist_ok=True)
    antigravity_config_path = os.path.join(antigravity_config_dir, "mcp_config.json")
    
    try:
        config_data = {}
        if os.path.exists(antigravity_config_path) and os.path.getsize(antigravity_config_path) > 0:
            with open(antigravity_config_path, "r") as f:
                config_data = json.load(f)
        
        mcp_servers = config_data.setdefault("mcpServers", {})
        mcp_servers["yaam_memory"] = {
            "command": VENV_PYTHON,
            "args": [os.path.join(REPO_ROOT, "server.py")]
        }
        
        with open(antigravity_config_path, "w") as f:
            json.dump(config_data, f, indent=2)
        print(f"Successfully added yaam_memory to global Antigravity CLI configuration: {antigravity_config_path}")
    except Exception as e:
        print(f"Warning: Could not configure global Antigravity CLI: {e}", file=sys.stderr)
    
    # Configure Global Skills for Antigravity CLI
    skills_dest_dir = os.path.expanduser("~/.gemini/antigravity-cli/skills")
    os.makedirs(skills_dest_dir, exist_ok=True)
    symlink_path = os.path.join(skills_dest_dir, "yaam-memory-manager")
    
    if os.path.exists(symlink_path) or os.path.islink(symlink_path):
        if os.path.islink(symlink_path):
            os.unlink(symlink_path)
        else:
            shutil.rmtree(symlink_path)
            
    try:
        os.symlink(
            os.path.join(REPO_ROOT, ".agents", "skills", "yaam-memory-manager"),
            symlink_path
        )
        print(f"Linked yaam-memory-manager skill to global path: {symlink_path}")
    except Exception as e:
        print(f"Warning: Could not link global skill: {e}", file=sys.stderr)

def configure_plugin_manifests():
    print("--> Updating plugin manifests with absolute paths...")
    
    # 1. Update plugin.json
    plugin_json_path = os.path.join(REPO_ROOT, "plugin.json")
    if os.path.exists(plugin_json_path):
        try:
            with open(plugin_json_path, "r") as f:
                data = json.load(f)
            
            # Update mcpServers
            if "mcpServers" in data and "yaam_memory" in data["mcpServers"]:
                data["mcpServers"]["yaam_memory"]["command"] = VENV_PYTHON
                data["mcpServers"]["yaam_memory"]["args"] = [os.path.join(REPO_ROOT, "server.py")]
                
            # Update hooks
            if "hooks" in data and "PostToolUse" in data["hooks"]:
                for hook in data["hooks"]["PostToolUse"]:
                    if hook.get("type") == "command" and "reconciler.py" in hook.get("command", ""):
                        hook["command"] = f"{VENV_PYTHON} {os.path.join(REPO_ROOT, 'reconciler.py')}"
            
            with open(plugin_json_path, "w") as f:
                json.dump(data, f, indent=2)
            print("Successfully updated plugin.json with absolute paths.")
        except Exception as e:
            print(f"Warning: Could not update plugin.json: {e}", file=sys.stderr)
            
    # 2. Update .agents/hooks.json
    agents_hooks_path = os.path.join(REPO_ROOT, ".agents", "hooks.json")
    if os.path.exists(agents_hooks_path):
        try:
            with open(agents_hooks_path, "r") as f:
                data = json.load(f)
                
            if "yaam-reconciler" in data and "PostToolUse" in data["yaam-reconciler"]:
                for group in data["yaam-reconciler"]["PostToolUse"]:
                    if "hooks" in group:
                        for hook in group["hooks"]:
                            if hook.get("type") == "command" and "reconciler.py" in hook.get("command", ""):
                                hook["command"] = f"{VENV_PYTHON} {os.path.join(REPO_ROOT, 'reconciler.py')}"
                                
            with open(agents_hooks_path, "w") as f:
                json.dump(data, f, indent=2)
            print("Successfully updated .agents/hooks.json with absolute paths.")
        except Exception as e:
            print(f"Warning: Could not update .agents/hooks.json: {e}", file=sys.stderr)

if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description="Install and configure YAAM memory engine.")
    parser.add_argument("--local", "-l", action="store_true", help="Perform only project-local setup (skip global configurations)")
    args = parser.parse_args()

    setup_python_env()
    configure_plugin_manifests()
    if not args.local:
        configure_claude_code()
        configure_antigravity_cli()
    else:
        print("--> Skipping global configurations (project-local setup).")
    print("\n[SUCCESS] YAAM Plugin setup complete!")
