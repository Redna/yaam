//! YAAM Engine — JSON-RPC 2.0 persistent TCP daemon.
//!
//! This is the main entry point. It starts a TCP server on a random localhost port,
//! writes the port to `.yaam/daemon.port`, and serves multiple agent sessions
//! concurrently via `tokio`. The daemon stays alive until all connections close
//! and an idle timeout (10 minutes) elapses, or a `shutdown` RPC is received
//! from the last active connection.

pub mod embedding;
mod graph;
mod lsp_adapter;
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

#[tokio::main]
async fn main() {
    // Parse CLI args
    let args: Vec<String> = std::env::args().collect();
    
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
    let state = match AppState::new(&events_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("Failed to initialize YAAM engine: {}", e);
            std::process::exit(1);
        }
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Failed to bind to random port");
    let port = listener.local_addr().unwrap().port();
    
    // Atomically create the port file. If another daemon beat us to it,
    // read its port and exit — the client will connect to the existing daemon.
    let _ = std::fs::create_dir_all(".yaam");
    match OpenOptions::new().write(true).create_new(true).open(".yaam/daemon.port") {
        Ok(mut file) => {
            write!(file, "{}", port).expect("Failed to write port lockfile");
        }
        Err(_) => {
            // Another daemon already created the port file. Read its port and exit.
            if let Ok(existing_port_str) = std::fs::read_to_string(".yaam/daemon.port") {
                let existing_port = existing_port_str.trim();
                eprintln!("Another YAAM daemon is already starting on port {}. Exiting.", existing_port);
            }
            std::process::exit(0);
        }
    }
    
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
