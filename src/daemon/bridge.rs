//! `canopy bridge` — stdio sidecar proxy between an MCP harness and the daemon.
//!
//! Reads JSON-RPC lines from stdin and forwards them to the daemon's
//! Streamable HTTP endpoint with identity headers (`x-canopy-agent-id`,
//! `x-canopy-client-name`, `x-canopy-seed-id`), writing responses back to
//! stdout. This keeps a single daemon owning the scheduler, watchers, RAG
//! ingestion, and sync state, while each harness session carries its own
//! identity in `argv`/env.
//!
//! When the daemon is unreachable the bridge degrades to spawning an
//! embedded `canopy stdio` server so the harness still gets a working MCP
//! endpoint (without daemon-side coordination).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::shared::sync_identity::{
    CANOPY_AGENT_ID_ENV, CANOPY_AGENT_ID_HEADER, CANOPY_CLIENT_NAME_ENV, CANOPY_CLIENT_NAME_HEADER,
    CANOPY_SEED_ID_ENV, CANOPY_SEED_ID_HEADER, CANOPY_WORKDIR_ENV,
};

const MCP_SESSION_HEADER: &str = "mcp-session-id";
const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(800);

pub(crate) async fn run_bridge(
    agent_id_arg: Option<String>,
    port_arg: Option<u16>,
    workdir_arg: Option<PathBuf>,
) -> Result<()> {
    let agent_id = resolve_agent_id(agent_id_arg)?;
    let workdir = resolve_workdir(workdir_arg)?;
    let port = resolve_bridge_port(port_arg);

    if daemon_reachable(port).await {
        return run_proxy_loop(port, &agent_id).await;
    }

    eprintln!(
        "canopy bridge: daemon not reachable on port {port}; \
         falling back to embedded stdio server (no daemon-side coordination)"
    );
    run_embedded_stdio(&agent_id, &workdir).await
}

// ── Proxy mode (daemon available) ────────────────────────────────────────────

async fn daemon_reachable(port: u16) -> bool {
    tokio::time::timeout(
        DAEMON_PROBE_TIMEOUT,
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map(|conn| conn.is_ok())
    .unwrap_or(false)
}

async fn run_proxy_loop(port: u16, agent_id: &str) -> Result<()> {
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let client = Client::new();
    let seed_id = non_empty_env(CANOPY_SEED_ID_ENV);

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut session_id: Option<String> = None;

    #[cfg(unix)]
    let mut sig_hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("failed to register SIGHUP handler")?;
    #[cfg(unix)]
    let mut sig_pipe = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::pipe())
        .context("failed to register SIGPIPE handler")?;

    loop {
        let line = {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = sig_hup.recv() => {
                        eprintln!("canopy bridge: received SIGHUP, exiting");
                        break;
                    }
                    _ = sig_pipe.recv() => {
                        eprintln!("canopy bridge: received SIGPIPE, exiting");
                        break;
                    }
                    next = lines.next_line() => next?,
                }
            }
            #[cfg(not(unix))]
            {
                lines.next_line().await?
            }
        };

        let Some(line) = line else {
            break; // EOF — harness closed stdin
        };
        if line.trim().is_empty() {
            continue;
        }

        let result = forward_request(
            &client,
            &endpoint,
            agent_id,
            seed_id.as_deref(),
            &line,
            session_id.as_deref(),
        )
        .await;

        match result {
            Ok(reply) => {
                if let Some(sid) = reply.session_id {
                    session_id = Some(sid);
                }
                for message in reply.messages {
                    stdout.write_all(message.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                }
                stdout.flush().await?;
            }
            Err(err) => {
                eprintln!("canopy bridge: {err}");
                let fallback = build_jsonrpc_transport_error(&line, &err.to_string());
                stdout.write_all(fallback.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
    }

    close_daemon_session(&client, &endpoint, session_id.as_deref()).await;
    Ok(())
}

struct DaemonReply {
    /// JSON-RPC messages to emit on stdout (empty for accepted notifications).
    messages: Vec<String>,
    session_id: Option<String>,
}

async fn forward_request(
    client: &Client,
    endpoint: &str,
    agent_id: &str,
    seed_id: Option<&str>,
    line: &str,
    session_id: Option<&str>,
) -> Result<DaemonReply> {
    let mut request = client
        .post(endpoint)
        .header(CANOPY_AGENT_ID_HEADER, agent_id)
        .header(CANOPY_CLIENT_NAME_HEADER, "bridge")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .body(line.to_string());

    if let Some(sid) = seed_id {
        request = request.header(CANOPY_SEED_ID_HEADER, sid);
    }
    if let Some(sid) = session_id {
        request = request.header(MCP_SESSION_HEADER, sid);
    }

    let response = request
        .send()
        .await
        .context("failed to reach canopy daemon")?;
    let status = response.status();

    let new_session_id = response
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let is_event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));

    let body = response
        .text()
        .await
        .context("failed to read canopy daemon response body")?;

    if !status.is_success() {
        anyhow::bail!("daemon returned HTTP {status}: {body}");
    }

    let messages = if is_event_stream {
        parse_sse_messages(&body)
    } else if body.trim().is_empty() {
        Vec::new() // 202 Accepted — notification with no response
    } else {
        vec![body]
    };

    Ok(DaemonReply {
        messages,
        session_id: new_session_id,
    })
}

/// Extract the `data:` payloads from an SSE body, one message per event.
/// Multi-line data fields within one event are joined per the SSE spec;
/// events with empty data (keep-alive pings) are skipped.
fn parse_sse_messages(body: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in body.lines() {
        if line.is_empty() {
            flush_sse_event(&mut current, &mut messages);
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            current.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    flush_sse_event(&mut current, &mut messages);

    messages
}

fn flush_sse_event(current: &mut Vec<&str>, messages: &mut Vec<String>) {
    if current.iter().all(|part| part.is_empty()) {
        current.clear();
        return;
    }
    messages.push(current.join("\n"));
    current.clear();
}

/// Best-effort DELETE so the daemon can drop the MCP session state.
async fn close_daemon_session(client: &Client, endpoint: &str, session_id: Option<&str>) {
    let Some(sid) = session_id else {
        return;
    };
    let _ = client
        .delete(endpoint)
        .header(MCP_SESSION_HEADER, sid)
        .timeout(Duration::from_secs(2))
        .send()
        .await;
}

fn build_jsonrpc_transport_error(raw_request: &str, message: &str) -> String {
    let id = serde_json::from_str::<serde_json::Value>(raw_request)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(serde_json::Value::Null);

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32000,
            "message": "canopy bridge transport error",
            "data": message,
        }
    })
    .to_string()
}

// ── Embedded fallback (daemon unavailable) ───────────────────────────────────

async fn run_embedded_stdio(agent_id: &str, workdir: &str) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve canopy executable path")?;
    let mut child = tokio::process::Command::new(exe);
    child
        .arg("stdio")
        .env(CANOPY_AGENT_ID_ENV, agent_id)
        .env(CANOPY_WORKDIR_ENV, workdir)
        .env(CANOPY_CLIENT_NAME_ENV, "bridge")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = child
        .spawn()
        .context("failed to start canopy stdio server from bridge")?
        .wait()
        .await
        .context("bridge child process failed while waiting")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("canopy stdio exited with status {status}");
    }
}

// ── Identity & port resolution ───────────────────────────────────────────────

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn resolve_agent_id(agent_id_arg: Option<String>) -> Result<String> {
    if let Some(id) = agent_id_arg
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| non_empty_env(CANOPY_AGENT_ID_ENV))
    {
        return Ok(id);
    }

    anyhow::bail!(
        "missing agent id; pass --id <AGENT_ID> or set {CANOPY_AGENT_ID_ENV} in the environment"
    );
}

fn resolve_workdir(workdir_arg: Option<PathBuf>) -> Result<String> {
    let workdir = match workdir_arg {
        Some(path) => path,
        None => std::env::var(CANOPY_WORKDIR_ENV)
            .ok()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir().context("failed to resolve current directory")?),
    };

    let canonical = std::fs::canonicalize(&workdir).unwrap_or(workdir);
    Ok(canonical.to_string_lossy().to_string())
}

/// Daemon port discovery: `--port` → `CANOPY_PORT` → daemon state in DB → 7755.
fn resolve_bridge_port(port_arg: Option<u16>) -> u16 {
    if let Some(port) = port_arg {
        return port;
    }

    if let Some(port) = non_empty_env("CANOPY_PORT").and_then(|v| v.parse::<u16>().ok()) {
        return port;
    }

    if let Ok(data_dir) = crate::ensure_data_dir() {
        if let Some(port) = read_port_from_state(&data_dir) {
            return port;
        }
    }

    7755
}

fn read_port_from_state(data_dir: &Path) -> Option<u16> {
    let db_path = data_dir.join("background_agents.db");
    let db = Database::new(&db_path).ok()?;
    let port_str = db.get_state("port").ok()??;
    port_str.trim().parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_extracts_single_data_event() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(
            messages,
            vec!["{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"]
        );
    }

    #[test]
    fn parse_sse_skips_empty_keepalive_events() {
        // rmcp emits an initial empty event with retry metadata before the response.
        let body = "data: \nid: 0\nretry: 3000\n\ndata: {\"id\":1}\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["{\"id\":1}"]);
    }

    #[test]
    fn parse_sse_handles_multiple_events() {
        let body = "data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["{\"id\":1}", "{\"id\":2}"]);
    }

    #[test]
    fn parse_sse_joins_multiline_data() {
        let body = "data: line1\ndata: line2\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["line1\nline2"]);
    }

    #[test]
    fn parse_sse_handles_missing_trailing_blank_line() {
        let body = "data: {\"id\":7}";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["{\"id\":7}"]);
    }

    #[test]
    fn transport_error_preserves_request_id() {
        let error = build_jsonrpc_transport_error("{\"jsonrpc\":\"2.0\",\"id\":42}", "boom");
        let value: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(value["id"], 42);
        assert_eq!(value["error"]["code"], -32000);
    }

    #[test]
    fn transport_error_uses_null_id_for_invalid_request() {
        let error = build_jsonrpc_transport_error("not json", "boom");
        let value: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert!(value["id"].is_null());
    }
}
