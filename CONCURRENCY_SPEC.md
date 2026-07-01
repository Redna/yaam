# Concurrency & Resource Sharing Specification

The YAAM engine is currently strictly one-process-per-session. With the introduction of multi-agent and multi-session workflows, we must migrate to a persistent daemon architecture to eliminate memory duplication, eliminate startup latency, and enable agents to share context seamlessly.

## 1. Current State vs. Target State

### Current State (One-Process-per-Session)
- **Model Files:** Shared (Downloaded once to `~/.yaam/models`)
- **Memory Cost:** Duplicated. Every session loads its own 127 MB ONNX model.
- **Latency:** High. Every session pays the ONNX + Tokenizer + BM25 + Event Replay boot cost.
- **Graph State:** Duplicated. Every session holds its own memory graph.
- **LSP Client:** Duplicated. Every session spins up its own `typescript-language-server`.

### Target Architecture (Persistent Local Daemon)
A single Rust daemon runs in the background per system (or per workspace) and serves multiple agent sessions via a TCP or Unix Socket JSON-RPC interface.

| Resource | Shared? | How |
| :--- | :--- | :--- |
| **Model files on disk** | ✅ Yes | Still downloaded once to `~/.yaam/models`. |
| **Model in memory (ONNX)** | ✅ Yes | Loaded **once** by the daemon. All agents query the same ONNX session. |
| **Tokenizer in memory** | ✅ Yes | Loaded **once** by the daemon. |
| **BM25 index** | ✅ Yes | Built once and updated live. All agents query the same index. |
| **Graph state (MemoryEngine)** | ✅ Yes | Centralized in the daemon. One source of truth for the entire workspace. |
| **LSP clients** | ✅ Yes | One LSP server per language, lazily started on first reconcile of that language. Managed by the daemon in a `HashMap<language_id, Arc<Mutex<StdioLspClient>>`. All agents share the same LSP instances. |

## 2. Daemon Lifecycle & Communication

To facilitate this architecture, we will update the TS Client (`src/engine-client.ts`) and the Rust Engine (`src-rust/src/main.rs`).

### Rust Daemon (`yaam-engine`)
1. **TCP Server / Unix Socket:** Switch from `stdio` JSON-RPC to a persistent `tokio` TCP server (e.g., binding to `localhost:3455`).
2. **Shared State:** `AppState` is wrapped in an `Arc<RwLock>` and shared across all incoming TCP connections. 
3. **Concurrency:** `tokio` tasks handle incoming JSON-RPC requests concurrently. Readers (queries) run in parallel, writers (upserts/links) take a write lock.
4. **Lifecycle Management:** The daemon stays alive as long as there is at least one active connection, or shuts down after an idle timeout.

### TypeScript Extension (`engine-client.ts`)
1. **Client Connection:** Instead of using `child_process.spawn()`, the client attempts to connect to `localhost:3455` via TCP.
2. **Auto-Start:** If the connection fails (connection refused), the client automatically spawns the background daemon using `spawn(binPath, [eventsPath], { detached: true, stdio: 'ignore' })`, waits a moment, and retries the connection.
3. **Multiplexing:** The client sends JSON-RPC requests over the TCP socket, managing `pendingRequests` using unique IDs exactly as it does now.

## 3. Concurrency Safety

With multiple agents potentially modifying the graph simultaneously:

- **Storage Lock:** The JSONL event store uses OS-level file locking (`fs2::FileExt::lock_exclusive()`) during appends. This ensures that even if multiple daemons are somehow spawned, the file is never corrupted.
- **In-Memory Locks:** Rust's `RwLock` around the `MemoryEngine` ensures that multiple agents can query (`yaam_graph_explore`) simultaneously without blocking each other, but mutations (`reconciler.ts`) safely queue up.

## 4. Architectural Decisions

**Single vs. Multi-Workspace Daemon**
One daemon per workspace (binding to a port derived from the workspace path hash) is recommended to ensure graphs from completely different codebases don't accidentally merge or leak context. This also resolves the limitation where `typescript-language-server` expects a single `rootUri`.
