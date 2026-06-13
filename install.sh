#!/usr/bin/env bash
set -e

# Parse arguments
LOCAL_MODE=false
for arg in "$@"; do
    if [ "$arg" == "--local" ] || [ "$arg" == "-l" ]; then
        LOCAL_MODE=true
    fi
done

echo "=== YAAM Memory Engine Installer ==="

# Check if we need to fetch the codebase (e.g., if piped directly via curl/ssh)
if [ ! -f "install.py" ]; then
    echo "-> Codebase not detected locally. Fetching YAAM repository via Git SSH..."
    if ! git archive --remote=git@github.com:Redna/yaam.git main | tar -x; then
        echo "Error: Could not retrieve repository via Git SSH. Please verify your SSH credentials." >&2
        exit 1
    fi
    echo "-> Codebase fetched successfully."
fi

# Execute the python installer
if [ "$LOCAL_MODE" = true ]; then
    python3 install.py --local
else
    python3 install.py
fi
