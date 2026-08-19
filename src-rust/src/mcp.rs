use serde_json::{json, Value};
use std::io::{self, BufRead};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};

async fn get_daemon_connection() -> Result<TcpStream, String> {
    // Attempt up to 30 times (3 seconds) to connect
    for _ in 0..30 {
        if let Ok(port_str) = std::fs::read_to_string(".yaam/daemon.port") {
            if let Ok(port) = port_str.trim().parse::<u16>() {
                if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
                    return Ok(stream);
                }
            }
        }
        
        // If we get here, either port file doesn't exist, is invalid, or connection failed.
        // We fork the daemon.
        let _ = std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "yaam-engine".into()))
            .spawn();
            
        sleep(Duration::from_millis(100)).await;
    }
    
    Err("Failed to connect to or spawn YAAM daemon".to_string())
}

async fn proxy_request(method: &str, params: Value) -> Result<Value, String> {
    let mut stream = get_daemon_connection().await?;
    let req = json!({
        "jsonrpc": "2.0",
        "id": "mcp",
        "method": method,
        "params": params
    });
    
    let mut req_bytes = serde_json::to_vec(&req).unwrap();
    req_bytes.push(b'\n');
    
    stream.write_all(&req_bytes).await.map_err(|e| e.to_string())?;
    
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
    
    let resp: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    
    if let Some(err) = resp.get("error") {
        return Err(err.to_string());
    }
    
    if let Some(res) = resp.get("result") {
        return Ok(res.clone());
    }
    
    Err("Invalid response from daemon".to_string())
}

pub async fn run_mcp_bridge() {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        if handle.read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = req.get("id").cloned().unwrap_or(Value::Null);

        let mut resp = json!({
            "jsonrpc": "2.0",
            "id": id,
        });

        match method {
            "initialize" => {
                resp["result"] = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "yaam-mcp-bridge",
                        "version": "1.0.0"
                    }
                });
            }
            "notifications/initialized" => {
                continue;
            }
            "tools/list" => {
                resp["result"] = json!({
                    "tools": [
                        {
                            "name": "search",
                            "description": "Perform a semantic search on the YAAM memory graph",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "query": { "type": "string" },
                                    "limit": { "type": "number" }
                                },
                                "required": ["query"]
                            }
                        },
                        {
                            "name": "query",
                            "description": "Execute a graph query",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "dsl": { "type": "string" }
                                },
                                "required": ["dsl"]
                            }
                        }
                    ]
                });
            }
            "tools/call" => {
                let params = req.get("params").unwrap_or(&Value::Null);
                let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                
                let proxy_method = match tool_name {
                    "search" => "search",
                    "query" => "query",
                    _ => ""
                };
                
                if proxy_method.is_empty() {
                    resp["error"] = json!({
                        "code": -32601,
                        "message": format!("Tool {} not found", tool_name)
                    });
                } else {
                    match proxy_request(proxy_method, args).await {
                        Ok(res) => {
                            resp["result"] = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": serde_json::to_string_pretty(&res).unwrap_or_default()
                                    }
                                ]
                            });
                        }
                        Err(e) => {
                            resp["result"] = json!({
                                "isError": true,
                                "content": [
                                    {
                                        "type": "text",
                                        "text": e
                                    }
                                ]
                            });
                        }
                    }
                }
            }
            _ => {
                // Return method not found if an ID was provided
                if !id.is_null() {
                    resp["error"] = json!({
                        "code": -32601,
                        "message": format!("Method {} not found", method)
                    });
                } else {
                    continue; // Notification, do nothing
                }
            }
        }

        if !id.is_null() {
            println!("{}", serde_json::to_string(&resp).unwrap());
        }
    }
}
