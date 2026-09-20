use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::error::Error;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Timeout for a single LSP definition request. If the LSP server doesn't
/// respond within this duration, the process is killed and an error is
/// returned. The LSP server will be restarted on next use.
const LSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub line: u32,
    pub col: u32,
}

pub trait LspAdapter {
    fn start(&mut self, project_root: &Path) -> Result<(), Box<dyn Error>>;
    fn notify_open(&mut self, file_uri: &str, content: &str, language_id: &str) -> Result<(), Box<dyn Error>>;
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
        // Used during initialize — generous timeout for server startup.
        self.read_message_with_timeout(Duration::from_secs(60))
    }

    /// Read a single LSP message with a timeout.
    ///
    /// Spawns a reader thread that performs the blocking I/O. If the thread
    /// doesn't produce a message within `timeout`, the LSP process is killed
    /// (causing the thread to unblock) and an error is returned. The caller
    /// should expect that the LSP client is no longer usable after a timeout —
    /// `get_or_create_lsp` will start a fresh server on next use.
    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<Value, Box<dyn Error>> {
        let process = self.process.as_mut().ok_or("LSP process not running")?;
        let stdout = process.stdout.take().ok_or("Failed to get stdout")?;

        let (tx, rx) = mpsc::channel::<Result<(Value, std::process::ChildStdout), String>>();

        let handle = std::thread::spawn(move || {
            let mut stdout = stdout;
            let result = (|| -> Result<(Value, std::process::ChildStdout), String> {
                // Read headers byte-by-byte until \r\n\r\n.
                let mut headers = String::new();
                let mut buf = [0u8; 1];
                loop {
                    stdout.read_exact(&mut buf).map_err(|e| e.to_string())?;
                    headers.push(buf[0] as char);
                    if headers.ends_with("\r\n\r\n") {
                        break;
                    }
                }

                // Parse Content-Length.
                let mut content_length = 0usize;
                for line in headers.lines() {
                    if line.starts_with("Content-Length: ") {
                        content_length = line["Content-Length: ".len()..]
                            .parse()
                            .map_err(|e: std::num::ParseIntError| e.to_string())?;
                    }
                }

                // Read body.
                let mut body = vec![0u8; content_length];
                stdout.read_exact(&mut body).map_err(|e| e.to_string())?;
                let value: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
                Ok((value, stdout))
            })();
            let _ = tx.send(result);
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok((value, stdout))) => {
                // Success — put stdout back and return the value.
                if let Some(ref mut process) = self.process {
                    process.stdout = Some(stdout);
                }
                Ok(value)
            }
            Ok(Err(e)) => {
                // Reader thread returned an error (e.g., EOF or parse failure).
                // stdout is lost — kill the process so it will be restarted.
                if let Some(mut proc) = self.process.take() {
                    let _ = proc.kill();
                    let _ = proc.wait();
                }
                let _ = handle.join();
                Err(format!("LSP read error: {}", e).into())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Timeout! Kill the LSP process to unblock the reader thread,
                // then join the thread to clean up.
                if let Some(mut proc) = self.process.take() {
                    let _ = proc.kill();
                    let _ = proc.wait();
                }
                let _ = handle.join();
                Err("LSP read timed out — process killed, will restart on next use".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed before any message — reader thread panicked.
                if let Some(mut proc) = self.process.take() {
                    let _ = proc.kill();
                    let _ = proc.wait();
                }
                let _ = handle.join();
                Err("LSP reader thread disconnected".into())
            }
        }
    }
}

impl StdioLspClient {
    /// Send a request and read until its response arrives, returning `result`.
    ///
    /// Server-initiated notifications (e.g. `publishDiagnostics`) are skipped.
    fn request_result(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let req_id = self.send_request(method, params)?;
        loop {
            let msg = self.read_message_with_timeout(LSP_REQUEST_TIMEOUT)?;
            if msg.get("id").and_then(|i| i.as_u64()) == Some(req_id as u64) {
                if let Some(error) = msg.get("error") {
                    return Err(format!("LSP Error: {}", error).into());
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// `textDocument/hover` at a declaration, returning the trimmed first
    /// non-fence line of the hover text (the signature).
    pub fn get_hover(
        &mut self,
        file_uri: &str,
        line: u32,
        col: u32,
    ) -> Result<Option<String>, Box<dyn Error>> {
        let params = json!({
            "textDocument": { "uri": file_uri },
            "position": { "line": line, "character": col }
        });
        let result = self.request_result("textDocument/hover", params)?;
        Ok(parse_hover_text(&result))
    }

    /// `textDocument/diagnostic` for a file -> (errors, warnings).
    pub fn get_diagnostics(&mut self, file_uri: &str) -> Result<(u32, u32), Box<dyn Error>> {
        let params = json!({
            "textDocument": { "uri": file_uri }
        });
        let result = self.request_result("textDocument/diagnostic", params)?;
        Ok(count_diagnostics(&result))
    }

    /// `textDocument/implementation` — implementors of the declaration at
    /// `(line, col)`. Accepts both `Location` and `LocationLink` results.
    pub fn get_implementation(
        &mut self,
        file_uri: &str,
        line: u32,
        col: u32,
    ) -> Result<Vec<Location>, Box<dyn Error>> {
        let params = json!({
            "textDocument": { "uri": file_uri },
            "position": { "line": line, "character": col }
        });
        let result = self.request_result("textDocument/implementation", params)?;
        Ok(parse_locations(&result))
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
            // Advertised capabilities gate which requests the server answers.
            // Without these, `textDocument/diagnostic` and
            // `textDocument/implementation` are silently unsupported, and
            // `definition` is limited to plain `Location` results.
            "capabilities": {
                "textDocument": {
                    "diagnostic": {},
                    "implementation": {},
                    "hover": {},
                    "definition": { "linkSupport": true },
                    "publishDiagnostics": {}
                }
            }
        });

        self.send_request("initialize", params)?;

        // Wait for initialize response
        let _resp = self.read_message()?;

        self.send_notification("initialized", json!({}))?;

        Ok(())
    }

    fn notify_open(&mut self, file_uri: &str, content: &str, language_id: &str) -> Result<(), Box<dyn Error>> {
        self.send_notification("textDocument/didOpen", json!({
            "textDocument": {
                "uri": file_uri,
                "languageId": language_id,
                "version": 1,
                "text": content,
            }
        }))?;
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
        let result = self.request_result("textDocument/definition", params)?;
        Ok(parse_locations(&result))
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

/// Parse an LSP definition/implementation result into `Location`s.
///
/// Accepts all three shapes the spec allows:
/// - `Location`          (`uri` + `range.start`)
/// - `LocationLink`      (`targetUri` + `targetSelectionRange.start`)
/// - an array of either.
///
/// `targetSelectionRange` is the target's *own* name/position (not the range
/// the caller asked about), which is what a node id must be built from.
pub fn parse_locations(result: &Value) -> Vec<Location> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match result {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![result],
        _ => return out,
    };
    for item in items {
        // Location shape.
        if let (Some(uri), Some(range)) = (item.get("uri"), item.get("range")) {
            if let Some(start) = range.get("start") {
                if let Some(uri) = uri.as_str() {
                    out.push(Location {
                        uri: uri.to_string(),
                        line: start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
                        col: start
                            .get("character")
                            .and_then(|c| c.as_u64())
                            .unwrap_or(0) as u32,
                    });
                    continue;
                }
            }
        }
        // LocationLink shape.
        if let (Some(uri), Some(range)) = (
            item.get("targetUri"),
            item.get("targetSelectionRange").or_else(|| item.get("targetRange")),
        ) {
            if let Some(start) = range.get("start") {
                if let Some(uri) = uri.as_str() {
                    out.push(Location {
                        uri: uri.to_string(),
                        line: start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
                        col: start
                            .get("character")
                            .and_then(|c| c.as_u64())
                            .unwrap_or(0) as u32,
                    });
                }
            }
        }
    }
    out
}

/// Extract the first meaningful line of a hover response.
///
/// Handles `MarkupContent` (`contents.value`), plain strings, and arrays of
/// `MarkedString`. Hover text is usually a fenced code block
/// (```` ```typescript ````), so skips the fence line and blank lines and
/// returns the first content line, trimmed.
pub fn parse_hover_text(result: &Value) -> Option<String> {
    let raw = if result.is_null() {
        return None;
    } else if let Some(contents) = result.get("contents") {
        match contents {
            Value::String(s) => s.clone(),
            Value::Object(o) => o.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            Value::Array(a) => a
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        }
    } else {
        return None;
    };

    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .map(|line| line.to_string())
}

/// Count errors/warnings in a `textDocument/diagnostic` result.
///
/// Errors = severity 1 (or absent, per spec default), warnings = severity 2.
/// `{kind: "unchanged"}` (no items) counts as zero.
pub fn count_diagnostics(result: &Value) -> (u32, u32) {
    let items = result
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let mut errors = 0u32;
    let mut warnings = 0u32;
    for item in items {
        match item.get("severity").and_then(|s| s.as_u64()) {
            Some(2) => warnings += 1,
            Some(1) | None => errors += 1,
            // 3 = Information, 4 = Hint: not an error or a warning.
            _ => {}
        }
    }
    (errors, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_location() {
        let v = json!([{ "uri": "file:///a.ts", "range": { "start": { "line": 4, "character": 9 } } }]);
        let locs = parse_locations(&v);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, "file:///a.ts");
        assert_eq!((locs[0].line, locs[0].col), (4, 9));
    }

    #[test]
    fn parses_location_link_via_selection_range() {
        // `range` here is the caller's reference; `targetSelectionRange` is the
        // target's own name — the value that must win.
        let v = json!([{
            "targetUri": "file:///b.ts",
            "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 3, "character": 1 } },
            "targetSelectionRange": { "start": { "line": 0, "character": 16 }, "end": { "line": 0, "character": 22 } }
        }]);
        let locs = parse_locations(&v);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, "file:///b.ts");
        assert_eq!((locs[0].line, locs[0].col), (0, 16));
    }

    #[test]
    fn parses_single_object_and_empty() {
        let v = json!({ "targetUri": "file:///c.ts", "targetSelectionRange": { "start": { "line": 1, "character": 2 } } });
        assert_eq!(parse_locations(&v).len(), 1);
        assert!(parse_locations(&json!(null)).is_empty());
        assert!(parse_locations(&json!([])).is_empty());
    }

    #[test]
    fn hover_skips_code_fence() {
        let v = json!({ "contents": { "kind": "markdown", "value": "```typescript\nfunction helper(a: string): void\n```" } });
        assert_eq!(parse_hover_text(&v).as_deref(), Some("function helper(a: string): void"));
        let s = json!({ "contents": "  const x: number  " });
        assert_eq!(parse_hover_text(&s).as_deref(), Some("const x: number"));
        assert_eq!(parse_hover_text(&json!(null)), None);
    }

    #[test]
    fn counts_diagnostics_by_severity() {
        let v = json!({ "kind": "full", "items": [
            { "severity": 1 }, { "severity": 2 }, { "severity": 2 }, { "severity": 3 }, {}
        ]});
        assert_eq!(count_diagnostics(&v), (2, 2));
        assert_eq!(count_diagnostics(&json!({ "kind": "unchanged" })), (0, 0));
    }
}
