# YAAM Universal MCP Integration

## Architecture Decision Record

To provide universal compatibility with AI agents like Antigravity, Claude Desktop, and Cursor, YAAM implements the **Model Context Protocol (MCP)**. 

Rather than maintaining separate engine binaries or splitting the memory graph, YAAM uses a **Bridge Pattern**. The core Rust daemon (`yaam-engine`) continues to run as a singleton TCP server, while an `mcp` subcommand acts as a translation layer.

### The Bridge Pattern
1. **The MCP Client** (e.g., Antigravity) spawns `yaam-engine mcp`.
2. **The Bridge** (`yaam-engine mcp`) checks for `.yaam/daemon.port`. If no active daemon is found, it forks the background daemon process.
3. **Translation**: The Bridge reads MCP JSON-RPC 2.0 messages from standard input (`stdin`), translates them into YAAM's internal JSON-RPC format, sends them over TCP to the daemon, and routes the response back to `stdout`.

---

## Edge Conditions & Exception Handling

When implementing or modifying the MCP Bridge, the following edge cases must be strictly handled:

### 1. Stale Locks & Startup Race Conditions
* **Scenario:** The daemon crashes or is killed, leaving a stale `.yaam/daemon.port` file. Alternatively, two MCP clients boot simultaneously and both attempt to start the daemon.
* **Handling:** The daemon natively uses `create_new(true)` atomic file creation. If two clients fork the daemon simultaneously, one daemon will secure the lock and the other will exit gracefully. The Bridge must attempt a TCP connection to the port in the file; if the connection is refused, it must assume the lock is stale, delete the file, and fork a new daemon.

### 2. Standard Output Pollution (Protocol Breaking)
* **Scenario:** The Rust code uses `println!` or a library writes logs to `stdout`.
* **Handling:** The MCP specification dictates that `stdout` is exclusively for JSON-RPC messages. ANY non-JSON text sent to `stdout` will instantly break the client's parser. The Bridge must configure all logging, panics, and debug statements to route to `stderr`.

### 3. Daemon Disconnection / Crashes
* **Scenario:** The background daemon crashes while the MCP Bridge is actively processing a `tools/call`.
* **Handling:** The TCP read/write operation in the Bridge will return an `std::io::Error`. The Bridge must catch this, avoid crashing, and return a cleanly formatted MCP `Error` response (Code `-32000` / Internal Error) back to the client. The client can retry, at which point the Bridge will detect the dead TCP connection, fork a new daemon, and reconnect.

### 4. Client Disconnection (Graceful Shutdown)
* **Scenario:** The user closes Antigravity or Claude Desktop. The client forcefully closes the `stdin` pipe to the Bridge.
* **Handling:** The Bridge must detect `EOF` on `stdin` and immediately exit with code `0`. It should **not** send a shutdown signal to the daemon. The daemon is shared by potentially other active agents (like `pi`) and relies on an internal 10-minute idle timeout to shut itself down gracefully.

### 5. Working Directory Isolation
* **Scenario:** Multiple projects on the same machine use YAAM.
* **Handling:** The daemon and the Bridge both use the Current Working Directory (CWD) to locate `.yaam/daemon.port` and `events.jsonl`. The Bridge must not change its CWD. MCP clients inherently launch servers in the root directory of the active project, ensuring complete memory isolation between different projects.
