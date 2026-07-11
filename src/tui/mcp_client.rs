//! Minimal blocking MCP client for the daemon's streamable-HTTP endpoint.
//!
//! The daemon serves MCP over rmcp's stateful streamable-HTTP transport,
//! which rejects bare `tools/call` POSTs with "Unexpected message, expect
//! initialize request". Every call therefore runs a short-lived session:
//! `initialize` → `notifications/initialized` → `tools/call` → `DELETE`
//! (best-effort cleanup). Responses arrive as SSE `data:` lines inside a
//! chunked HTTP body.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of a `tools/call`: the daemon-side error flag plus the
/// concatenated text content of the result.
#[derive(Debug)]
pub(crate) struct McpToolOutcome {
    pub is_error: bool,
    pub text: String,
}

/// Call one daemon MCP tool synchronously and return its result.
///
/// Transport-level failures (daemon down, malformed response) return `Err`;
/// tool-level failures (e.g. validation errors) return `Ok` with
/// `is_error = true` so callers can surface the daemon's message as-is.
pub(crate) fn call_daemon_tool(
    port: &str,
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<McpToolOutcome> {
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "canopy-tui", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let init = http_post(port, None, &init_body.to_string())?;
    let session = init
        .session_id
        .clone()
        .ok_or_else(|| anyhow!("daemon did not return an MCP session id"))?;
    jsonrpc_response(&init.body, 1).context("MCP initialize failed")?;

    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    http_post(port, Some(&session), &initialized.to_string())?;

    let call_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    let call = http_post(port, Some(&session), &call_body.to_string());
    // Always try to close the session, even when the call failed.
    let _ = http_delete(port, &session);

    let result = jsonrpc_response(&call?.body, 2)?;
    Ok(parse_tool_outcome(&result))
}

/// Fire `agent_run` for `agent_id` via the daemon MCP endpoint.
pub(crate) fn send_mcp_task_run(port: &str, agent_id: &str) -> Result<()> {
    let outcome = call_daemon_tool(port, "agent_run", &serde_json::json!({ "id": agent_id }))?;
    if outcome.is_error {
        return Err(anyhow!(outcome.text));
    }
    Ok(())
}

// ── HTTP plumbing ────────────────────────────────────────────────

struct HttpResponse {
    status: u16,
    session_id: Option<String>,
    body: String,
}

fn http_post(port: &str, session: Option<&str>, body: &str) -> Result<HttpResponse> {
    let session_header = session
        .map(|s| format!("Mcp-Session-Id: {s}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Connection: close\r\n\
         {session_header}Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    );
    let response = http_request(port, &request)?;
    if response.status >= 400 {
        return Err(anyhow!(
            "daemon MCP endpoint returned HTTP {}: {}",
            response.status,
            response.body.trim()
        ));
    }
    Ok(response)
}

fn http_delete(port: &str, session: &str) -> Result<HttpResponse> {
    let request = format!(
        "DELETE /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Mcp-Session-Id: {session}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    http_request(port, &request)
}

fn http_request(port: &str, request: &str) -> Result<HttpResponse> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect_timeout(&addr.parse()?, CONNECT_TIMEOUT)
        .with_context(|| format!("daemon not reachable at {addr}"))?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    // `Connection: close` is sent with every request, so EOF marks the end of
    // the response. A timeout mid-read still leaves a parseable prefix.
    let _ = stream.read_to_end(&mut raw);
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse> {
    let split = find_header_end(raw).ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {status_line}"))?;

    let mut session_id = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("mcp-session-id") {
            session_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }

    let body_raw = &raw[split + 4..];
    let body_bytes = if chunked {
        decode_chunked(body_raw)
    } else {
        body_raw.to_vec()
    };
    Ok(HttpResponse {
        status,
        session_id,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    })
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Decode an HTTP/1.1 chunked body. Tolerates a truncated tail (returns what
/// was decoded so far) since responses are read until EOF or timeout.
fn decode_chunked(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = raw.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_line = String::from_utf8_lossy(&raw[..line_end]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let data_start = line_end + 2;
        let data_end = data_start + size;
        if data_end > raw.len() {
            out.extend_from_slice(&raw[data_start.min(raw.len())..]);
            break;
        }
        out.extend_from_slice(&raw[data_start..data_end]);
        // +2 skips the CRLF that terminates the chunk data.
        raw = raw.get(data_end + 2..).unwrap_or(&[]);
    }
    out
}

// ── JSON-RPC / tool result parsing ───────────────────────────────

/// Extract the JSON-RPC response with `id` from a body that is either plain
/// JSON or an SSE stream of `data: {...}` events.
fn jsonrpc_response(body: &str, id: u64) -> Result<serde_json::Value> {
    let mut candidates: Vec<serde_json::Value> = Vec::new();
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(value) = serde_json::from_str(data.trim()) {
                candidates.push(value);
            }
        }
    }
    if candidates.is_empty() {
        if let Ok(value) = serde_json::from_str(body.trim()) {
            candidates.push(value);
        }
    }

    let message = candidates
        .into_iter()
        .find(|value| value.get("id").and_then(serde_json::Value::as_u64) == Some(id))
        .ok_or_else(|| anyhow!("no JSON-RPC response for request {id} in daemon reply"))?;
    if let Some(error) = message.get("error") {
        let text = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(anyhow!("daemon MCP error: {text}"));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("JSON-RPC response has no result"))
}

/// Read `isError` and the concatenated text content from a tools/call result.
fn parse_tool_outcome(result: &serde_json::Value) -> McpToolOutcome {
    let is_error = result
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let text = result
        .get("content")
        .and_then(|content| content.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    McpToolOutcome { is_error, text }
}

// ── Test support: an in-process fake daemon MCP endpoint ─────────

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// Recorded `tools/call` params (`{"name": ..., "arguments": ...}`).
    pub(crate) type CallLog = Arc<Mutex<Vec<serde_json::Value>>>;

    pub(crate) struct FakeDaemon {
        pub port: String,
        pub calls: CallLog,
        /// Number of HTTP requests received. Bare TCP connects (the TUI's
        /// port-liveness probe) don't count — only actual MCP traffic does.
        pub requests: Arc<Mutex<usize>>,
    }

    impl FakeDaemon {
        pub fn recorded_calls(&self) -> Vec<serde_json::Value> {
            self.calls.lock().unwrap().clone()
        }

        pub fn request_count(&self) -> usize {
            *self.requests.lock().unwrap()
        }
    }

    /// Spawn a fake daemon speaking just enough of the streamable-HTTP MCP
    /// protocol for `call_daemon_tool`. Every `tools/call` gets `tool_result`
    /// as its JSON-RPC `result` value.
    pub(crate) fn spawn_fake_daemon(tool_result: serde_json::Value) -> FakeDaemon {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
        let port = listener.local_addr().unwrap().port().to_string();
        let calls: CallLog = Arc::default();
        let requests = Arc::new(Mutex::new(0));
        let calls_bg = Arc::clone(&calls);
        let requests_bg = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                handle_connection(&mut stream, &calls_bg, &requests_bg, &tool_result);
            }
        });
        FakeDaemon {
            port,
            calls,
            requests,
        }
    }

    /// A localhost port with nothing listening on it.
    pub(crate) fn unused_port() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        listener.local_addr().unwrap().port().to_string()
    }

    fn handle_connection(
        stream: &mut TcpStream,
        calls: &CallLog,
        requests: &Arc<Mutex<usize>>,
        tool_result: &serde_json::Value,
    ) {
        let Some((request_line, body)) = read_request(stream) else {
            return;
        };
        *requests.lock().unwrap() += 1;
        if request_line.starts_with("DELETE") {
            respond_accepted(stream);
            return;
        }

        let message: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        match message.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => {
                let result = serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-daemon", "version": "0"}
                });
                respond_sse(stream, &message, &result, true);
            }
            Some("tools/call") => {
                if let Some(params) = message.get("params") {
                    calls.lock().unwrap().push(params.clone());
                }
                respond_sse(stream, &message, tool_result, false);
            }
            _ => respond_accepted(stream),
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let header_end = super::find_header_end(&raw);
            if let Some(split) = header_end {
                let head = String::from_utf8_lossy(&raw[..split]).into_owned();
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if raw.len() >= split + 4 + content_length {
                    let body = String::from_utf8_lossy(&raw[split + 4..split + 4 + content_length])
                        .into_owned();
                    let request_line = head.lines().next().unwrap_or_default().to_string();
                    return Some((request_line, body));
                }
            }
            let n = stream.read(&mut buf).ok()?;
            if n == 0 {
                return None;
            }
            raw.extend_from_slice(&buf[..n]);
        }
    }

    fn respond_sse(
        stream: &mut TcpStream,
        request: &serde_json::Value,
        result: &serde_json::Value,
        with_session: bool,
    ) {
        let id = request.get("id").cloned().unwrap_or(serde_json::json!(0));
        let payload = format!(
            "data: {}\n\n",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
        );
        let session_header = if with_session {
            "mcp-session-id: fake-session\r\n"
        } else {
            ""
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             {session_header}transfer-encoding: chunked\r\n\
             \r\n\
             {:x}\r\n{payload}\r\n0\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }

    fn respond_accepted(stream: &mut TcpStream) {
        let _ = stream.write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{spawn_fake_daemon, unused_port};
    use super::*;

    #[test]
    fn decode_chunked_reassembles_multiple_chunks() {
        let raw = b"4\r\ndata\r\n5\r\n: {}\n\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw), b"data: {}\n");
    }

    #[test]
    fn decode_chunked_tolerates_truncated_tail() {
        let raw = b"a\r\ndata: {\"x\"";
        assert_eq!(decode_chunked(raw), b"data: {\"x\"");
    }

    #[test]
    fn jsonrpc_response_reads_sse_and_plain_json_bodies() {
        let sse = "data: \nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n";
        let result = jsonrpc_response(sse, 2).expect("sse body");
        assert_eq!(result["ok"], true);

        let plain = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":1}}";
        let result = jsonrpc_response(plain, 1).expect("plain body");
        assert_eq!(result["ok"], 1);
    }

    #[test]
    fn jsonrpc_response_surfaces_rpc_errors() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32602,\"message\":\"bad params\"}}\n";
        let err = jsonrpc_response(body, 2).unwrap_err();
        assert!(err.to_string().contains("bad params"));
    }

    #[test]
    fn call_daemon_tool_round_trips_with_session_handshake() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "{\"loop_id\":\"abc\"}"}],
            "isError": false
        }));

        let outcome = call_daemon_tool(
            &fake.port,
            "loop_create",
            &serde_json::json!({"name": "My Loop", "workdir": "/tmp"}),
        )
        .expect("call should succeed");

        assert!(!outcome.is_error);
        assert!(outcome.text.contains("loop_id"));
        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "loop_create");
        assert_eq!(calls[0]["arguments"]["name"], "My Loop");
    }

    #[test]
    fn call_daemon_tool_surfaces_tool_errors_without_failing_transport() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Invalid cron expression: 'nope'."}],
            "isError": true
        }));

        let outcome =
            call_daemon_tool(&fake.port, "loop_create", &serde_json::json!({})).expect("transport");

        assert!(outcome.is_error);
        assert!(outcome.text.contains("Invalid cron expression"));
    }

    #[test]
    fn call_daemon_tool_fails_when_daemon_unreachable() {
        // `unused_port()` frees the port before returning it, which leaves a
        // brief window where another test's fake daemon (also bound via
        // port 0) could claim the same port before we connect. Retry with a
        // fresh port on the rare miss instead of flaking the whole suite.
        for _ in 0..5 {
            let port = unused_port();
            match call_daemon_tool(&port, "loop_create", &serde_json::json!({})) {
                Err(err) => {
                    assert!(err.to_string().contains("not reachable"));
                    return;
                }
                Ok(_) => continue,
            }
        }
        panic!("port kept getting claimed by another test after 5 attempts");
    }

    #[test]
    fn send_mcp_task_run_maps_tool_errors_to_err() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Agent 'x' not found."}],
            "isError": true
        }));

        let err = send_mcp_task_run(&fake.port, "x").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
