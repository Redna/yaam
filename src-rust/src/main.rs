//! YAAM Engine — JSON-RPC 2.0 server over stdio.
//!
//! This is the main entry point. It reads JSON-RPC requests from stdin (one per line),
//! dispatches them via the RPC module, and writes responses to stdout.

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
use std::io::{self, BufRead, Write};
use types::*;

fn main() {
    // Parse CLI args
    let args: Vec<String> = std::env::args().collect();
    
    if args.get(1).map(|s| s.as_str()) == Some("setup") {
        eprintln!("Downloading ONNX model and tokenizer from HuggingFace...");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            if let Err(e) = embedding::download_model_files().await {
                eprintln!("Setup failed: {}", e);
                std::process::exit(1);
            }
        });
        eprintln!("Setup complete! Model is ready.");
        std::process::exit(0);
    }

    let events_path = if args.len() > 1 {
        args[1].clone()
    } else {
        // Default: events.jsonl in current directory
        "events.jsonl".to_string()
    };

    // Initialize application state (loads events, builds graph, builds BM25 index)
    let state = match AppState::new(&events_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to initialize YAAM engine: {}", e);
            std::process::exit(1);
        }
    };

    let node_count = state.engine.read().unwrap().node_count();
    // eprintln!("YAAM engine initialized: {} nodes loaded from {}", node_count, events_path);

    // Main stdio loop: read one JSON-RPC request per line
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // EOF or read error — graceful shutdown
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON-RPC request
        let request: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let error_response = RpcResponse::error(
                    None,
                    RPC_PARSE_ERROR,
                    format!("Parse error: {}", e),
                );
                let _ = writeln!(stdout_lock, "{}", serde_json::to_string(&error_response).unwrap());
                let _ = stdout_lock.flush();
                continue;
            }
        };

        // Dispatch the request
        let response = rpc::dispatch(&state, &request);

        // Check if this is a shutdown response
        let is_shutdown = response
            .result
            .as_ref()
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str())
            == Some("shutdown");

        // Write the response as a single JSON line
        let _ = writeln!(stdout_lock, "{}", serde_json::to_string(&response).unwrap());
        let _ = stdout_lock.flush();

        if is_shutdown {
            // eprintln!("YAAM engine shutting down gracefully.");
            break;
        }
    }
}
