# Distributed Agents & Memory Synchronization

> How to run multiple YAAM-backed agents in parallel (e.g., on CI runners) while preserving a single shared memory graph across all of them.

## Problem Statement

YAAM's persistent daemon architecture (see [CONCURRENCY_SPEC.md](../CONCURRENCY_SPEC.md)) assumes all agents connect to the **same daemon** via TCP. This works when agents run on the same machine. But when agents run on **separate machines** — for example, GitHub Actions CI runners — they cannot share a daemon.

Each CI runner is ephemeral: it boots, runs an agent, and shuts down. If every runner started fresh, agents would lose all memory between runs. We need a mechanism to **persist and synchronize** the YAAM memory graph across ephemeral agent runs.

## Solution: Git-Based Event Sourcing

YAAM's event store is an append-only JSONL file (`events.jsonl`). Every graph mutation — node upserts, edge links, deletions — is recorded as one JSON line. The daemon replays this file on startup to rebuild the in-memory graph.

This format enables a simple but powerful synchronization pattern: **store the event log in a Git branch**.

```
┌─────────────────────────────────────────────────────────────┐
│  Git: memory branch                                         │
│                                                             │
│  events.jsonl              ← Base (compacted state)        │
│  events-<RUN_ID>.jsonl     ← Delta from one agent run       │
│  events-<RUN_ID>.jsonl     ← Delta from another agent run   │
│  yaam-compaction.lock      ← Lock (present only during       │
│                              compaction)                    │
└─────────────────────────────────────────────────────────────┘
```

Because the file is append-only JSONL, **concatenating multiple delta files in chronological order produces a valid event log** that the daemon can replay identically to a natively merged file. No special merge logic is needed — `cat events-*.jsonl > events.jsonl` is the merge.

## Pipeline Phases

Each agent run goes through three phases:

### Phase 1: Restore (agent start)

Before the agent starts, restore memory from the Git branch:

```bash
# scripts/restore-memory.sh

# 1. Wait if compaction is running
while git ls-tree origin/memory | grep -q "yaam-compaction.lock"; do
  echo "Remote compaction in progress. Waiting 15s..."
  sleep 15
  git fetch origin memory
done

# 2. Extract all files from the memory branch
git archive origin/memory | tar -x

# 3. Rename base for consistent sort ordering (000 prefix = first)
mv events.jsonl events-0000000000-base.jsonl

# 4. Concatenate base + all deltas → single events.jsonl
cat events-*.jsonl > events.jsonl

# 5. Clean up intermediate files
rm -f events-*.jsonl

# 6. Record line count for delta calculation later
echo "$(wc -l < events.jsonl)" > .yaam_start_lines
```

The daemon then starts, replays `events.jsonl`, and builds the graph.

### Phase 2: Agent runs

The agent works normally. The YAAM daemon appends new events to `events.jsonl` as the agent creates, links, and deletes nodes. OS-level file locking (`fs2::FileExt::lock_exclusive()`) prevents corruption even if multiple processes write simultaneously.

### Phase 3: Save (agent finish)

After the agent completes, extract only the **new** events and push them as a delta:

```bash
# scripts/save-memory.sh

# 1. Wait if compaction is running
while git ls-tree origin/memory | grep -q "yaam-compaction.lock"; do
  echo "Compaction in progress. Waiting 15s before saving..."
  sleep 15
  git fetch origin memory
done

# 2. Calculate delta (only events from THIS agent run)
START_LINES=$(cat .yaam_start_lines)
CURRENT_LINES=$(wc -l < events.jsonl)
NEW_LINES=$((CURRENT_LINES - START_LINES))

# 3. Extract delta
tail -n "$NEW_LINES" events.jsonl > "events-${RUN_ID}.jsonl"

# 4. Push to memory branch (with rebase retry for concurrent pushes)
git add "events-${RUN_ID}.jsonl"
git commit -m "Memory delta from run ${RUN_ID}"
for i in 1 2 3; do
  if git push origin memory 2>/dev/null; then
    break
  else
    git pull --rebase origin memory  # Rebase on top of other agents' deltas
  fi
done
```

Since each agent pushes a **uniquely named** delta file (`events-<RUN_ID>.jsonl`), concurrent pushes never conflict — both delta files coexist on the branch.

### Phase 4: Compaction (periodic)

Over time, the memory branch accumulates many delta files. Compaction merges them into a single base:

```bash
# scripts/run-compaction.sh

# 1. Acquire lock (local + remote)
touch yaam-compaction.lock
git add yaam-compaction.lock && git commit && git push origin memory

# 2. Merge all deltas
cat events-*.jsonl > events.jsonl

# 3. Compact (deduplicate: keep latest UPSERT_NODE per node ID)
node scripts/compact.js events.jsonl events-compacted.jsonl

# 4. Replace base with compacted version, remove all deltas
mv events-compacted.jsonl events.jsonl
git rm -f events-*.jsonl
git add events.jsonl

# 5. Release lock
git rm -f yaam-compaction.lock
git commit -m "Compaction complete"
git push origin memory

# 6. Cleanup (also runs on failure via: trap cleanup EXIT)
rm -f yaam-compaction.lock
```

## Compaction Lock — `yaam-compaction.lock`

The lock file coordinates all three operations to prevent race conditions:

| Operation | Lock check | Behavior |
|---|---|---|
| `restore-memory.sh` (agent start) | ✅ Checks lock on memory branch | Waits until released |
| `save-memory.sh` (agent finish) | ✅ Checks lock on memory branch | Waits until released |
| `run-compaction.sh` (compaction) | ✅ Creates lock | Blocks new agents from starting |
| `run-compaction.sh` (failure) | ✅ `trap cleanup EXIT` | Releases lock even on failure |

### Race condition scenarios and how the lock prevents them

**Scenario 1: Agent saves during compaction**

```
Without lock:
  Agent A finishes → push delta ──┐
                                     │ RACE! Compaction rewrites the
  Compaction starts → rewrite base ──┘ base → agent's rebase fails

With lock:
  Compaction acquires lock
  Agent A finishes → save-memory.sh checks lock → WAITS
  Compaction completes → releases lock
  Agent A → push delta → succeeds (delta is independent of base)
```

**Scenario 2: Agent starts during compaction**

```
Without lock:
  Compaction starts → rewriting memory branch...
  Agent B starts → restore-memory.sh → gets partially written branch

With lock:
  Compaction acquires lock
  Agent B starts → restore-memory.sh checks lock → WAITS
  Compaction completes → releases lock
  Agent B → restore → gets clean compacted base
```

## Why Auto-Compaction Is Disabled

YAAM's daemon has a built-in `compact` RPC method that rewrites `events.jsonl` via `synthesize_current_state()`. This is **intentionally disabled** when using the Git-based pipeline (`YAAM_DISABLE_AUTO_COMPACT=true`):

- Auto-compaction reduces the line count in `events.jsonl`
- `save-memory.sh` calculates deltas as `CURRENT_LINES - START_LINES`
- If compaction reduces `CURRENT_LINES` below `START_LINES`, the delta is negative → **no events saved → data loss**

Instead, compaction runs **offline** via `run-compaction.sh` which:
1. Uses the JS compactor (`compact.js`) — no daemon needed
2. Acquires the lock so no agents are running
3. Merges and compacts safely
4. Pushes the clean base

## Example: Configuring a CI Pipeline

Here is a complete example showing how to wire YAAM memory into a GitHub Actions workflow. This is the pattern used by the [evol-hive](https://github.com/Redna/evol-hive) project.

### Directory structure

```
your-repo/
├── .github/workflows/
│   ├── agent.yml              # Your agent workflow
│   └── compaction.yml         # Scheduled compaction
├── scripts/
│   ├── restore-memory.sh      # Restore before agent runs
│   ├── save-memory.sh         # Save delta after agent finishes
│   ├── run-compaction.sh      # Compact deltas into base
│   └── compact.js             # JS compactor (deduplicate events)
└── .gitignore
    └── events.jsonl           # Don't track in main branch
```

### Agent workflow (`.github/workflows/agent.yml`)

```yaml
name: My Agent

on:
  workflow_dispatch:
    inputs:
      issue_number:
        required: true

jobs:
  run-agent:
    runs-on: ubuntu-latest
    permissions:
      contents: write    # Needed for pushing to memory branch
      issues: write
      pull-requests: write

    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0  # Full history for worktree operations

      # ── Step 1: Restore memory ──────────────────────────────
      - name: Restore YAAM memory
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: bash scripts/restore-memory.sh
        # This script:
        #   1. Waits if compaction lock is present
        #   2. Fetches memory branch
        #   3. Concatenates base + all delta files
        #   4. Records start line count in .yaam_start_lines

      # ── Step 2: Run your agent ───────────────────────────────
      - name: Run Agent
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          # Start YAAM daemon (replays events.jsonl into graph)
          yaam-engine events.jsonl &
          
          # ... your agent code here ...
          # Agent connects to daemon via JSON-RPC over TCP
          # New events are appended to events.jsonl with file locking

      # ── Step 3: Save memory delta ────────────────────────────
      - name: Save YAAM memory
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: bash scripts/save-memory.sh
        # This script:
        #   1. Waits if compaction lock is present
        #   2. Extracts new events: tail -n $((CURRENT - START)) events.jsonl
        #   3. Pushes delta file to memory branch
        #   4. Rebase retry if another agent pushed concurrently
```

### Compaction workflow (`.github/workflows/compaction.yml`)

```yaml
name: Memory Compaction

on:
  workflow_dispatch:        # Manual trigger
  schedule:
    - cron: "0 */6 * * *"   # Every 6 hours

jobs:
  compact:
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Run compaction
        env:
          GITHUB_ACTIONS: "true"
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: bash scripts/run-compaction.sh
        # This script:
        #   1. Acquires lock on memory branch
        #   2. Merges all delta files
        #   3. Compacts (deduplicates events)
        #   4. Pushes clean base
        #   5. Releases lock (with trap cleanup on failure)
```

### `.gitignore`

```
# YAAM memory — managed via memory branch, not main
events.jsonl
events-*.jsonl
.yaam/
.yaam_start_lines
yaam-compaction.lock
```

### `compact.js` — The compactor

```javascript
// Deduplicates UPSERT_NODE events (keeps latest per node ID)
// and preserves all other event types (LINK_NODES, DELETE_NODE, etc.)
const fs = require('fs');

const lines = fs.readFileSync(process.argv[2], 'utf-8').split('\n').filter(l => l.trim());
const nodes = new Map();
const other = [];

for (const line of lines) {
  const event = JSON.parse(line);
  if (event.event_type === 'UPSERT_NODE') {
    nodes.set(event.payload.id, line);  // Last write wins
  } else if (event.event_type === 'DELETE_NODE') {
    nodes.delete(event.payload.id);
  } else {
    other.push(line);
  }
}

const out = fs.createWriteStream(process.argv[3]);
for (const line of nodes.values()) out.write(line + '\n');
for (const line of other) out.write(line + '\n');
out.end();
```

## Running Multiple Agents in Parallel

When two agents run simultaneously on separate CI runners:

```
Runner A                          Runner B
────────                          ────────
restore-memory.sh                 restore-memory.sh
  → fetches memory branch           → fetches memory branch
  → gets base + existing deltas      → gets base + existing deltas
  → starts daemon                    → starts daemon

agent runs...                     agent runs...
  → appends events locally           → appends events locally

save-memory.sh                    save-memory.sh
  → pushes events-A.jsonl            → pushes events-B.jsonl
  → success!                         → push fails (Runner A pushed first)
                                     → git pull --rebase
                                     → now has both events-A.jsonl + events-B.jsonl
                                     → push succeeds!

Next agent run:
  restore-memory.sh
  → fetches memory branch
  → gets base + events-A.jsonl + events-B.jsonl
  → cat events-*.jsonl > events.jsonl
  → daemon replays all events → full shared memory
```

No data is lost. No conflicts occur. Each delta is an independent file with a unique name.

## Compaction in the Pipeline

For a full agent pipeline (Architect → Developer → QA → Merge), compaction should run **after all agents complete**:

```bash
# In your pipeline orchestrator (e.g., pipeline.sh):

# After the final PR is merged...
echo "--- Memory Compaction ---"

DELTA_COUNT=$(git ls-tree origin/memory | grep -c 'events-.*\.jsonl')
if [ "$DELTA_COUNT" -gt 0 ]; then
  bash scripts/run-compaction.sh
  # Acquires lock → agents wait → compacts → releases lock
fi
```

This ensures:
1. No agents are running during compaction (they've all finished)
2. Any agent that starts during compaction waits for the lock
3. The memory branch is clean for the next pipeline run

## YAAM Engine Internals

The Rust daemon provides the primitives that make this pattern possible:

| Feature | Implementation | Why it matters |
|---|---|---|
| **Append-only JSONL** | `storage.rs` — events are one JSON line per file | Concatenation = merge, no parse needed |
| **OS file locking** | `fs2::FileExt::lock_exclusive()` | Prevents corruption if multiple processes write |
| **Malformed line skipping** | `replay()` — skips invalid lines | Resilient to partial writes or corruption |
| **Compaction RPC** | `compact` method → `synthesize_current_state()` | Rebuilds events from graph (daemon-level compaction) |
| **Atomic rewrite** | `rewrite()` — write to tmp, then rename | No partial state if compaction crashes |
| **Concurrent connections** | `tokio::spawn` per TCP connection | Multiple agents share one daemon |
| **Idle timeout** | 10-minute shutdown after last connection | Daemon doesn't linger forever |

### Daemon-level vs. JS-level compaction

| Aspect | Daemon `compact` RPC | JS `compact.js` |
|---|---|---|
| **What it does** | `prune_old_workspaces()` + `synthesize_current_state()` → rewrites events.jsonl | Deduplicates `UPSERT_NODE` events (keeps latest per node ID) |
| **Graph awareness** | Full graph — edges, embeddings, workspace pruning | None — only looks at event types |
| **Requires daemon** | Yes (must be running) | No (standalone Node.js script) |
| **Use case** | Local development (single machine) | CI pipelines (no daemon during compaction) |
| **Edge handling** | Proper — synthesizes edges from graph | None — keeps all `LINK_NODES` events (may accumulate) |

For CI pipelines, use the JS compactor. For local development, use the daemon RPC (`/yaam compact`).

## Reference Implementation

The [evol-hive](https://github.com/Redna/evol-hive) project provides a complete reference implementation:

| File | Purpose |
|---|---|
| [`scripts/restore-memory.sh`](https://github.com/Redna/evol-hive/blob/main/scripts/restore-memory.sh) | Restore + merge before agent runs |
| [`scripts/save-memory.sh`](https://github.com/Redna/evol-hive/blob/main/scripts/save-memory.sh) | Extract delta + push after agent runs |
| [`scripts/run-compaction.sh`](https://github.com/Redna/evol-hive/blob/main/scripts/run-compaction.sh) | Lock + merge + compact + release |
| [`scripts/compact.js`](https://github.com/Redna/evol-hive/blob/main/scripts/compact.js) | JS event deduplicator |
| [`scripts/pipeline.sh`](https://github.com/Redna/evol-hive/blob/main/scripts/pipeline.sh) | Pipeline orchestrator with compaction phase |
| [`.github/workflows/compaction.yml`](https://github.com/Redna/evol-hive/blob/main/.github/workflows/compaction.yml) | Scheduled compaction workflow |
| [`docs/MEMORY_PIPELINE.md`](https://github.com/Redna/evol-hive/blob/main/docs/MEMORY_PIPELINE.md) | evol-hive-specific documentation |