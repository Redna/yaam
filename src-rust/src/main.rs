//! YAAM Engine — JSON-RPC 2.0 persistent TCP daemon.
//!
//! This is the main entry point. It starts a TCP server on a random localhost port,
//! writes the port to `.yaam/daemon.port`, and serves multiple agent sessions
//! concurrently via `tokio`. The daemon stays alive until all connections close
//! and an idle timeout (10 minutes) elapses, or a `shutdown` RPC is received
//! from the last active connection.

pub mod embedding;
mod ann_index;
mod document_adapter;
mod graph;
mod language_adapter;
mod lsp_adapter;
mod mcp;
mod query_dsl;
mod reconciler;
mod rpc;
mod search;
mod storage;
mod types;

use rpc::AppState;
use std::fs::OpenOptions;
use std::io::Write;
use types::*;
use tokio::net::TcpListener;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Persist reconcile-derived events (Layer 0: code topology) to `events.jsonl`.
///
/// Default **off**: the code graph is regenerable from the checkout, and
/// re-emitting it on every reconcile is what ballooned the log (194k events /
/// 229 MB observed) and the replay cost. Layer 1 (workspaces, scratchpad notes)
/// and explicit mutations are always persisted. Set `YAAM_PERSIST_RECONCILE=true`
/// to restore the old behaviour.
pub static PERSIST_RECONCILE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Soft memory ceiling in MB (`YAAM_MAX_RSS_MB`, default 800; 0 disables).
///
/// A full reconcile of a large workspace builds the whole graph in RAM and
/// carries each node's embedding, so RSS grows with workspace size (measured:
/// >2 GB for a ~1,000-file workspace with docs). Once the ceiling is reached the
/// daemon stops accepting reconcile work instead of growing further, and says so
/// in the reply and in a diagnostic line.
pub static MAX_RSS_MB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(800);

pub fn max_rss_mb() -> u64 {
    MAX_RSS_MB.load(std::sync::atomic::Ordering::Relaxed)
}

/// Resident set size of this process in MB (Linux; 0 when unavailable).
pub fn current_rss_mb() -> u64 {
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(pages) = statm.split_whitespace().nth(1) {
            if let Ok(pages) = pages.parse::<u64>() {
                let page = 4096u64;
                return pages.saturating_mul(page) / (1024 * 1024);
            }
        }
    }
    0
}

/// `Some((rss, ceiling))` when this process is at or over the configured ceiling.
pub fn over_rss_ceiling() -> Option<(u64, u64)> {
    let ceiling = max_rss_mb();
    if ceiling == 0 {
        return None;
    }
    let rss = current_rss_mb();
    if rss >= ceiling {
        Some((rss, ceiling))
    } else {
        None
    }
}

/// Whether reconcile computes embeddings for code entities.
///
/// Default **off**. Every embedded text costs one or more 512-token ONNX passes
/// and the pass footprint dominates RSS (measured: >1 GB for a single large file
/// before `with_memory_pattern(false)` and the chunk cap). Code search still works
/// through BM25 + the graph; notes are always embedded because they are short and
/// are the part semantic search is most useful for. `YAAM_EMBED_ON_RECONCILE=true`
/// enables full semantic code search.
pub fn embed_on_reconcile() -> bool {
    std::env::var("YAAM_EMBED_ON_RECONCILE").map(|v| v == "true").unwrap_or(false)
}

/// Documents (`.md` -> Section entities) are opt-in: they are the bulk of a
/// doc-heavy workspace's graph. `YAAM_INDEX_DOCS=true` enables them.
pub fn index_docs() -> bool {
    std::env::var("YAAM_INDEX_DOCS").map(|v| v == "true").unwrap_or(false)
}

pub fn persist_reconcile() -> bool {
    PERSIST_RECONCILE.load(std::sync::atomic::Ordering::Relaxed)
}

#[tokio::main]
async fn main() {
    if let Ok(v) = std::env::var("YAAM_MAX_RSS_MB") {
        if let Ok(v) = v.parse::<u64>() {
            MAX_RSS_MB.store(v, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if !index_docs() {
        eprintln!("[yaam] document indexing off (set YAAM_INDEX_DOCS=true to index .md files)");
    }

    if std::env::var("YAAM_PERSIST_RECONCILE").map(|v| v == "true").unwrap_or(false) {
        PERSIST_RECONCILE.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[yaam] persisting reconcile-derived events (YAAM_PERSIST_RECONCILE=true)");
    }

    // Parse CLI args
    let args: Vec<String> = std::env::args().collect();
    
    if args.get(1).map(|s| s.as_str()) == Some("mcp") {
        mcp::run_mcp_bridge().await;
        std::process::exit(0);
    }

    if args.get(1).map(|s| s.as_str()) == Some("setup") {
        eprintln!("Downloading ONNX model and tokenizer from HuggingFace...");
        if let Err(e) = embedding::download_model_files().await {
            eprintln!("Setup failed: {}", e);
            std::process::exit(1);
        }
        eprintln!("Setup complete! Model is ready.");
        std::process::exit(0);
    }

    let events_path = if args.len() > 1 {
        args[1].clone()
    } else {
        "events.jsonl".to_string()
    };

    // Initialize application state
    let mut state = match AppState::new(&events_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("Failed to initialize YAAM engine: {}", e);
            std::process::exit(1);
        }
    };

    // ── Background LSP reference resolution worker (Spec #2) ──
    //
    // Pending references from reconcile are sent through an unbounded channel
    // to a background worker. The worker resolves each reference via LSP and
    // applies the resulting LinkNodes event to storage and the graph.
    // This ensures reconcile returns immediately without blocking on LSP.
    let (ref_tx, mut ref_rx) = tokio::sync::mpsc::unbounded_channel::<crate::reconciler::PendingReference>();
    // SAFETY: We need to set ref_queue on the Arc<AppState>. Since we just created
    // the Arc and have exclusive ownership (no other references exist yet), we can
    // safely get a mutable reference via Arc::get_mut.
    {
        let state_mut = Arc::get_mut(&mut state).expect("state should be uniquely owned at startup");
        state_mut.ref_queue = Some(ref_tx);
    }

    // Spawn the background worker task.
    //
    // The LSP servers are only useful while references are being resolved, and
    // they are expensive (a TypeScript pair holds several hundred MB in child
    // processes that YAAM_MAX_RSS_MB does not cover). When the queue has been
    // quiet for YAAM_LSP_IDLE_STOP_SECS (default 30) the worker stops every LSP
    // client and releases the memory; the next reference respawns one lazily.
    let worker_state = state.clone();
    let lsp_idle = std::time::Duration::from_secs(
        std::env::var("YAAM_LSP_IDLE_STOP_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30),
    );
    tokio::spawn(async move {
        loop {
            match tokio::time::timeout(lsp_idle, ref_rx.recv()).await {
                Ok(Some(pref)) => {
                    let s = worker_state.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::rpc::resolve_reference_sync(s.as_ref(), pref);
                    })
                    .await
                    .ok();
                }
                Ok(None) => break, // channel closed
                Err(_) => crate::rpc::stop_all_lsp(worker_state.as_ref()),
            }
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Failed to bind to random port");
    let port = listener.local_addr().unwrap().port();
    
    // Claim the port file. A file left behind by a daemon that died without a
    // graceful shutdown (SIGTERM, killed session, reboot) must not block every
    // future start in this workspace: probe the recorded port first, and take
    // the file over when nothing is listening. If a live daemon owns it, exit —
    // the client will connect to that one.
    let _ = std::fs::create_dir_all(".yaam");
    let mut claimed = false;
    for _attempt in 0..4 {
        match OpenOptions::new().write(true).create_new(true).open(".yaam/daemon.port") {
            Ok(mut file) => {
                write!(file, "{}", port).expect("Failed to write port lockfile");
                claimed = true;
                break;
            }
            Err(_) => {
                let existing_port = std::fs::read_to_string(".yaam/daemon.port")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if existing_port.is_empty() {
                    // Another daemon may be mid-write; give it a moment before deciding.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    continue;
                }
                if port_is_alive_retry(&existing_port, 3, 400) {
                    eprintln!(
                        "Another YAAM daemon is already running on port {}. Exiting.",
                        existing_port
                    );
                    std::process::exit(0);
                }
                eprintln!(
                    "Stale YAAM port file (port {}) — no daemon answered after retries; taking over.",
                    existing_port
                );
                let _ = std::fs::remove_file(".yaam/daemon.port");
            }
        }
    }
    if !claimed {
        eprintln!("Could not claim .yaam/daemon.port (another daemon is racing us). Exiting.");
        std::process::exit(1);
    }

    // Remove the port file on SIGTERM/SIGINT so a killed session does not leave
    // a stale file behind for the next start.
    spawn_shutdown_cleanup();
    
    let active_connections = Arc::new(AtomicUsize::new(0));
    let last_activity = Arc::new(AtomicU64::new(
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    ));

    // Idle timeout task
    let active_cloned = active_connections.clone();
    let last_activity_cloned = last_activity.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            // Self-heal the port file: a takeover race or manual cleanup can
            // remove it while this daemon keeps serving, which makes the
            // workspace unreachable (the client then spawns duplicate daemons).
            match std::fs::read_to_string(".yaam/daemon.port") {
                Ok(v) if v.trim() == port.to_string() => {}
                Ok(_) => { /* a different daemon owns the file — leave it alone */ }
                Err(_) => {
                    if std::fs::write(".yaam/daemon.port", port.to_string()).is_ok() {
                        eprintln!("[yaam] port file was missing — rewrote {}", port);
                    }
                }
            }
            if active_cloned.load(Ordering::SeqCst) == 0 {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                let last = last_activity_cloned.load(Ordering::SeqCst);
                // 10 minutes timeout (600 seconds)
                if now.saturating_sub(last) > 600 {
                    let _ = std::fs::remove_file(".yaam/daemon.port");
                    std::process::exit(0);
                }
            }
        }
    });

    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };

        let state = state.clone();
        let active = active_connections.clone();
        let activity = last_activity.clone();

        tokio::spawn(async move {
            active.fetch_add(1, Ordering::SeqCst);
            activity.store(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(), Ordering::SeqCst);
            
            let (reader, mut writer) = socket.split();
            let mut buf_reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                match buf_reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }

                        let request: RpcRequest = match serde_json::from_str(&trimmed) {
                            Ok(r) => r,
                            Err(e) => {
                                let err = RpcResponse::error(None, RPC_PARSE_ERROR, format!("Parse error: {}", e));
                                let _ = writer.write_all(serde_json::to_string(&err).unwrap().as_bytes()).await;
                                let _ = writer.write_all(b"\n").await;
                                continue;
                            }
                        };

                        let state_clone = state.clone();
                        let response = tokio::task::spawn_blocking(move || {
                            rpc::dispatch(state_clone, request)
                        }).await.unwrap_or_else(|_| RpcResponse::error(None, RPC_INTERNAL_ERROR, "Task panicked".to_string()));

                        let _ = writer.write_all(serde_json::to_string(&response).unwrap().as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;

                        activity.store(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(), Ordering::SeqCst);

                        // Only honor shutdown if this is the last active connection.
                        // Otherwise respond normally and let the idle timeout handle cleanup.
                        if response.result.as_ref()
                            .and_then(|v| v.get("status"))
                            .and_then(|v| v.as_str()) == Some("shutdown")
                            && active.load(Ordering::SeqCst) <= 1
                        {
                            let _ = std::fs::remove_file(".yaam/daemon.port");
                            std::process::exit(0);
                        }
                    }
                    Err(_) => break,
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}


/// Probe a recorded port, retrying a few times before concluding it is dead.
///
/// A single failed probe is not proof: a daemon busy with a large reconcile can
/// be slow to accept, and stealing its port file would orphan a live daemon.
fn port_is_alive_retry(port: &str, attempts: u32, delay_ms: u64) -> bool {
    for attempt in 0..attempts {
        if port_is_alive(port) {
            return true;
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
    }
    false
}

/// True when something is listening on `127.0.0.1:<port>` (short timeout).
///
/// Used to distinguish a live daemon from a stale `.yaam/daemon.port` file left
/// behind by a daemon that died without a graceful shutdown.
fn port_is_alive(port: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let addr = match format!("127.0.0.1:{}", port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => addr,
            None => return false,
        },
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Remove `.yaam/daemon.port` when this process receives SIGTERM or SIGINT, so a
/// killed session does not leave a stale lockfile that blocks the next start.
fn spawn_shutdown_cleanup() {
    tokio::spawn(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut intr = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = intr.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = std::fs::remove_file(".yaam/daemon.port");
        std::process::exit(0);
    });
}
