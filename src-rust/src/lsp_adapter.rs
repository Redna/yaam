use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::error::Error;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub line: u32,
    pub col: u32,
}

pub trait LspAdapter {
    fn start(&mut self, project_root: &Path) -> Result<(), Box<dyn Error>>;
    fn get_definition(
        &mut self,
        file_uri: &str,
        line: u32,
        col: u32,
    ) -> Result<Vec<Location>, Box<dyn Error>>;
    fn stop(&mut self) -> Result<(), Box<dyn Error>>;
}

pub struct StdioLspClient {
    command: String,
    args: Vec<String>,
    process: Option<Child>,
    request_id: u32,
}

impl StdioLspClient {
    pub fn new(command: &str, args: &[&str]) -> Self {
        Self {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            process: None,
            request_id: 1,
        }
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u32, Box<dyn Error>> {
        let process = self.process.as_mut().ok_or("LSP process not running")?;
        let stdin = process.stdin.as_mut().ok_or("Failed to get stdin")?;
        let id = self.request_id;
        self.request_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let payload = request.to_string();
        let message = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);
        stdin.write_all(message.as_bytes())?;
        stdin.flush()?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<(), Box<dyn Error>> {
        let process = self.process.as_mut().ok_or("LSP process not running")?;
        let stdin = process.stdin.as_mut().ok_or("Failed to get stdin")?;

        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let payload = notification.to_string();
        let message = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);
        stdin.write_all(message.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value, Box<dyn Error>> {
        let process = self.process.as_mut().ok_or("LSP process not running")?;
        let stdout = process.stdout.as_mut().ok_or("Failed to get stdout")?;

        let mut headers = String::new();
        let mut buf = [0; 1];
        loop {
            stdout.read_exact(&mut buf)?;
            headers.push(buf[0] as char);
            if headers.ends_with("\r\n\r\n") {
                break;
            }
        }

        let mut content_length = 0;
        for line in headers.lines() {
            if line.starts_with("Content-Length: ") {
                content_length = line["Content-Length: ".len()..].parse()?;
            }
        }

        let mut body = vec![0; content_length];
        stdout.read_exact(&mut body)?;

        let value: Value = serde_json::from_slice(&body)?;
        Ok(value)
    }
}

impl LspAdapter for StdioLspClient {
    fn start(&mut self, project_root: &Path) -> Result<(), Box<dyn Error>> {
        let process = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        self.process = Some(process);

        let root_uri = format!("file://{}", project_root.to_string_lossy());
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {}
        });

        self.send_request("initialize", params)?;

        // Wait for initialize response
        let _resp = self.read_message()?;

        self.send_notification("initialized", json!({}))?;

        Ok(())
    }

    fn get_definition(
        &mut self,
        file_uri: &str,
        line: u32,
        col: u32,
    ) -> Result<Vec<Location>, Box<dyn Error>> {
        let params = json!({
            "textDocument": {
                "uri": file_uri,
            },
            "position": {
                "line": line,
                "character": col,
            }
        });

        let req_id = self.send_request("textDocument/definition", params)?;

        loop {
            let msg = self.read_message()?;
            if msg.get("id").and_then(|i| i.as_u64()) == Some(req_id as u64) {
                if let Some(error) = msg.get("error") {
                    return Err(format!("LSP Error: {}", error).into());
                }

                let mut locations = Vec::new();
                if let Some(result) = msg.get("result") {
                    if result.is_array() {
                        for item in result.as_array().unwrap() {
                            if let (Some(uri), Some(range)) = (item.get("uri"), item.get("range")) {
                                if let Some(start) = range.get("start") {
                                    locations.push(Location {
                                        uri: uri.as_str().unwrap_or("").to_string(),
                                        line: start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
                                        col: start.get("character").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
                                    });
                                }
                            }
                        }
                    } else if result.is_object() {
                        if let (Some(uri), Some(range)) = (result.get("uri"), result.get("range")) {
                            if let Some(start) = range.get("start") {
                                locations.push(Location {
                                    uri: uri.as_str().unwrap_or("").to_string(),
                                    line: start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
                                    col: start.get("character").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
                                });
                            }
                        }
                    }
                }
                return Ok(locations);
            }
        }
    }

    fn stop(&mut self) -> Result<(), Box<dyn Error>> {
        if self.process.is_some() {
            self.send_request("shutdown", json!(null))?;
            let _ = self.read_message(); // Wait for shutdown response
            self.send_notification("exit", json!(null))?;

            if let Some(mut process) = self.process.take() {
                let _ = process.kill();
                let _ = process.wait();
            }
        }
        Ok(())
    }
}
