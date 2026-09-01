//! `canopy bridge` — stdio sidecar proxy between an MCP harness and the daemon.
//!
//! Reads JSON-RPC lines from stdin and forwards them to the daemon's
//! Streamable HTTP endpoint with identity headers (`x-canopy-agent-id`,
//! `x-canopy-seed-id`), writing responses back to stdout. This keeps a
//! single daemon owning the scheduler, watchers, RAG ingestion, and sync
//! state, while each harness session carries its own identity in
//! `argv`/env.
//!
//! Deliberately does *not* send its own `x-canopy-client-name`: every
//! platform is funneled through this sidecar (see
//! `setup_module::platform_adapter::enforce_canopy_bridge_transport`), so a
//! literal `"bridge"` client name would carry no distinguishing information
//! and would shadow the real actor name — `sync_messages.agent_name` is
//! resolved daemon-side from the session the agent_id already identifies
//! (interactive session name + cli, e.g. "cedrus · blackbox"), which already
//! carries the transport (`cli = "bridge"` for standalone sessions).
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
use crate::domain::db_paths::database_path;
use crate::shared::sync_identity::{
    CANOPY_AGENT_ID_ENV, CANOPY_AGENT_ID_HEADER, CANOPY_SEED_ID_ENV, CANOPY_SEED_ID_HEADER,
    CANOPY_WORKDIR_ENV,
};

const MCP_SESSION_HEADER: &str = "mcp-session-id";
const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(800);
/// Substring rmcp's `LocalSessionManager` puts in the body of both its 404
/// branches (`session.rs`/`tower.rs` in the `rmcp` crate) when a session id
/// was attached but the daemon has no record of it — e.g. after a restart
/// wiped its in-memory session table.
const SESSION_NOT_FOUND_MARKER: &str = "Session not found";

/// A non-2xx response from the daemon's `/mcp` endpoint, carrying the status
/// and body so callers can distinguish a dead-session 404 from other
/// failures instead of matching on a formatted string.
#[derive(Debug)]
struct DaemonHttpError {
    status: reqwest::StatusCode,
    body: String,
}

impl std::fmt::Display for DaemonHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daemon returned HTTP {}: {}", self.status, self.body)
    }
}

impl std::error::Error for DaemonHttpError {}

impl DaemonHttpError {
    fn is_session_not_found(&self) -> bool {
        self.status == reqwest::StatusCode::NOT_FOUND
            && self.body.contains(SESSION_NOT_FOUND_MARKER)
    }
}

fn is_session_not_found_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DaemonHttpError>()
        .is_some_and(DaemonHttpError::is_session_not_found)
}

fn http_status_of(err: &anyhow::Error) -> Option<reqwest::StatusCode> {
    err.downcast_ref::<DaemonHttpError>().map(|e| e.status)
}

/// Best-effort JSON-RPC `method` extraction for diagnostics; never fails the
/// request path if the line isn't valid JSON.
fn jsonrpc_method(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| "<unknown>".to_string())
}

pub(crate) async fn run_bridge(
    agent_id_arg: Option<String>,
    port_arg: Option<u16>,
    workdir_arg: Option<PathBuf>,
) -> Result<()> {
    let identity = resolve_agent_identity(agent_id_arg);
    let workdir = resolve_workdir(workdir_arg)?;
    let port = resolve_bridge_port(port_arg);

    if identity.is_standalone {
        register_standalone_session(&identity.agent_id, &workdir);
    }

    let result = if daemon_reachable(port).await {
        run_proxy_loop(port, &identity.agent_id).await
    } else {
        eprintln!(
            "canopy bridge: daemon not reachable on port {port}; \
             falling back to embedded stdio server (no daemon-side coordination)"
        );
        run_embedded_stdio(&identity.agent_id, &workdir).await
    };

    if identity.is_standalone {
        finish_standalone_session(&identity.agent_id, result.is_ok());
    }

    result
}

#[derive(Debug, PartialEq, Eq)]
struct BridgeIdentity {
    agent_id: String,
    is_standalone: bool,
}

fn resolve_agent_identity(agent_id_arg: Option<String>) -> BridgeIdentity {
    resolve_agent_identity_from_values(
        agent_id_arg,
        non_empty_env(CANOPY_AGENT_ID_ENV),
        format!("standalone-{}", uuid::Uuid::new_v4()),
    )
}

fn resolve_agent_identity_from_values(
    agent_id_arg: Option<String>,
    env_agent_id: Option<String>,
    fallback_agent_id: String,
) -> BridgeIdentity {
    let env_agent_id = env_agent_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    if let Some(agent_id) = agent_id_arg
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or(env_agent_id)
    {
        return BridgeIdentity {
            agent_id,
            is_standalone: false,
        };
    }

    BridgeIdentity {
        agent_id: fallback_agent_id,
        is_standalone: true,
    }
}

fn register_standalone_session(agent_id: &str, workdir: &str) {
    // `agent_id` is the generated "standalone-<uuid>" fallback, so reusing it as
    // the session name keeps `sync_messages.agent_name` distinguishable from a
    // real session's codename (e.g. "boletus") instead of the bare, collidable
    // literal "standalone".
    let result = crate::ensure_data_dir()
        .and_then(|data_dir| Database::new_safe(&database_path(&data_dir), &data_dir))
        .and_then(|db| {
            db.insert_interactive_session(
                agent_id,
                agent_id,
                "bridge",
                workdir,
                Some("canopy bridge"),
                Some(std::process::id() as i64),
                "bridge",
                crate::system::boot_id().as_deref(),
            )
        });

    if let Err(err) = result {
        eprintln!("canopy bridge: could not register standalone session: {err}");
    }
}

fn finish_standalone_session(agent_id: &str, success: bool) {
    let exit_code = if success { 0 } else { 1 };
    let result = crate::ensure_data_dir()
        .and_then(|data_dir| Database::new_safe(&database_path(&data_dir), &data_dir))
        .and_then(|db| db.finish_interactive_session(agent_id, exit_code));

    if let Err(err) = result {
        eprintln!("canopy bridge: could not finish standalone session: {err}");
    }
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

        let method = jsonrpc_method(&line);
        let mut session_attached = session_id.is_some();

        let mut result = forward_request(
            &client,
            &endpoint,
            agent_id,
            seed_id.as_deref(),
            &line,
            session_id.as_deref(),
        )
        .await;

        // A cached session only ever dies this way when the daemon restarted
        // and lost its in-memory session table. Clear it and retry once
        // without a session header so the daemon can start a new one; a
        // second failure (e.g. the retried method isn't `initialize`, which
        // is the only request rmcp accepts session-less) falls through to
        // the normal error reporting below instead of looping.
        if session_attached && matches!(&result, Err(err) if is_session_not_found_error(err)) {
            eprintln!(
                "canopy bridge: daemon session not found for method {method} \
                 (daemon likely restarted); clearing cached session and retrying once"
            );
            session_id = None;
            session_attached = false;
            result = forward_request(
                &client,
                &endpoint,
                agent_id,
                seed_id.as_deref(),
                &line,
                None,
            )
            .await;
        }

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
                let status = http_status_of(&err)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "n/a".to_string());
                eprintln!(
                    "canopy bridge: {err} (status={status}, method={method}, session_attached={session_attached})"
                );
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

#[derive(Debug)]
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
        return Err(DaemonHttpError { status, body }.into());
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
///
/// Shared with the state-changing `canopy loop`/`canopy spec` subcommands
/// (`daemon::cli_daemon`) so every CLI path that talks to the daemon's MCP
/// endpoint resolves the port the same way the bridge does — reading the
/// daemon's own reported port from the database rather than assuming the
/// default, which is what makes this resolution survive a stale process
/// squatting on 7755.
pub(crate) fn resolve_bridge_port(port_arg: Option<u16>) -> u16 {
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
    let db_path = database_path(data_dir);
    let db = Database::new_safe(&db_path, data_dir).ok()?;
    let port_str = db.get_state("port").ok()??;
    port_str.trim().parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── session-not-found classification ─────────────────────────

    #[test]
    fn daemon_http_error_recognizes_session_not_found_404() {
        let err = DaemonHttpError {
            status: reqwest::StatusCode::NOT_FOUND,
            body: "Not Found: Session not found".to_string(),
        };
        assert!(err.is_session_not_found());
    }

    #[test]
    fn daemon_http_error_rejects_plain_404() {
        let err = DaemonHttpError {
            status: reqwest::StatusCode::NOT_FOUND,
            body: "Not Found".to_string(),
        };
        assert!(!err.is_session_not_found());
    }

    #[test]
    fn daemon_http_error_rejects_non_404_status() {
        let err = DaemonHttpError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: "Session not found".to_string(),
        };
        assert!(!err.is_session_not_found());
    }

    #[test]
    fn is_session_not_found_error_unwraps_anyhow() {
        let err: anyhow::Error = DaemonHttpError {
            status: reqwest::StatusCode::NOT_FOUND,
            body: "Not Found: Session not found".to_string(),
        }
        .into();
        assert!(is_session_not_found_error(&err));
    }

    #[test]
    fn is_session_not_found_error_false_for_unrelated_error() {
        let err = anyhow::anyhow!("failed to reach canopy daemon");
        assert!(!is_session_not_found_error(&err));
    }

    #[test]
    fn http_status_of_extracts_status_from_daemon_http_error() {
        let err: anyhow::Error = DaemonHttpError {
            status: reqwest::StatusCode::BAD_GATEWAY,
            body: String::new(),
        }
        .into();
        assert_eq!(http_status_of(&err), Some(reqwest::StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn http_status_of_none_for_unrelated_error() {
        let err = anyhow::anyhow!("connection refused");
        assert!(http_status_of(&err).is_none());
    }

    // ── jsonrpc_method ────────────────────────────────────────────

    #[test]
    fn jsonrpc_method_extracts_method_name() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        assert_eq!(jsonrpc_method(line), "tools/list");
    }

    #[test]
    fn jsonrpc_method_unknown_for_invalid_json() {
        assert_eq!(jsonrpc_method("not json"), "<unknown>");
    }

    #[test]
    fn jsonrpc_method_unknown_when_field_missing() {
        assert_eq!(jsonrpc_method(r#"{"jsonrpc":"2.0","id":1}"#), "<unknown>");
    }

    // ── forward_request session recovery (against a real local server) ──

    /// Minimal stand-in for the daemon's `/mcp` route: rejects the session
    /// id "dead-session" the way rmcp does after a restart wipes its
    /// in-memory session table (404, body containing "Session not found"),
    /// and otherwise succeeds, minting a fresh session id — so this proves
    /// `forward_request` classifies the failure correctly and that a retry
    /// without a session header is what actually recovers.
    async fn spawn_fake_daemon() -> (u16, tokio::task::JoinHandle<()>) {
        use axum::extract::Request;
        use axum::response::IntoResponse;
        use axum::routing::post;

        async fn handle_mcp(request: Request) -> axum::response::Response {
            let has_dead_session = request
                .headers()
                .get(MCP_SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                == Some("dead-session");

            if has_dead_session {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    "Not Found: Session not found",
                )
                    .into_response();
            }

            (
                axum::http::StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                    (
                        axum::http::header::HeaderName::from_static("mcp-session-id"),
                        "fresh-session",
                    ),
                ],
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            )
                .into_response()
        }

        let router = axum::Router::new().route("/mcp", post(handle_mcp));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        (port, handle)
    }

    #[tokio::test]
    async fn forward_request_reports_session_not_found_for_dead_session() {
        let (port, server) = spawn_fake_daemon().await;
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let client = Client::new();

        let err = forward_request(
            &client,
            &endpoint,
            "agent",
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            Some("dead-session"),
        )
        .await
        .expect_err("dead session must be reported as an error");

        assert!(is_session_not_found_error(&err));
        assert_eq!(http_status_of(&err), Some(reqwest::StatusCode::NOT_FOUND));

        server.abort();
    }

    /// Regression guard for the identity-shadowing bug (T39): every platform
    /// is funneled through this sidecar, so a hardcoded `x-canopy-client-name:
    /// bridge` header would shadow a real session's name at the daemon (see
    /// `TaskTriggerHandler::resolve_sync_client_name`), turning "boletus ·
    /// claude" into "boletus · claude · bridge" — two spellings of one
    /// identity. The daemon already resolves the transport (bridge vs.
    /// direct) from the session's own `cli` column, so this header must stay
    /// absent.
    #[tokio::test]
    async fn forward_request_omits_client_name_header() {
        use axum::extract::Request;
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let saw_client_name_header = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&saw_client_name_header);

        async fn handle_mcp(
            axum::extract::State(flag): axum::extract::State<Arc<AtomicBool>>,
            request: Request,
        ) -> axum::response::Response {
            if request
                .headers()
                .contains_key(crate::shared::sync_identity::CANOPY_CLIENT_NAME_HEADER)
            {
                flag.store(true, Ordering::SeqCst);
            }
            (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            )
                .into_response()
        }

        let router = axum::Router::new()
            .route("/mcp", axum::routing::post(handle_mcp))
            .with_state(flag);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let client = Client::new();
        forward_request(
            &client,
            &endpoint,
            "sess-boletus",
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        )
        .await
        .expect("forward_request should succeed against the fake daemon");

        assert!(
            !saw_client_name_header.load(Ordering::SeqCst),
            "forward_request must not send x-canopy-client-name; the daemon \
             resolves the display name from the session itself"
        );

        server.abort();
    }

    #[tokio::test]
    async fn forward_request_recovers_once_session_header_is_dropped() {
        let (port, server) = spawn_fake_daemon().await;
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let client = Client::new();

        // Same request, retried the way run_proxy_loop does: no session header.
        let reply = forward_request(
            &client,
            &endpoint,
            "agent",
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        )
        .await
        .expect("session-less retry must succeed and mint a fresh session");

        assert_eq!(reply.session_id.as_deref(), Some("fresh-session"));
        assert_eq!(
            reply.messages,
            vec![r#"{"jsonrpc":"2.0","id":1,"result":{}}"#]
        );

        server.abort();
    }

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
    fn resolve_agent_identity_prefers_explicit_arg() {
        let identity = resolve_agent_identity_from_values(
            Some(" explicit-id ".to_string()),
            Some("env-id".to_string()),
            "fallback-id".to_string(),
        );

        assert_eq!(
            identity,
            BridgeIdentity {
                agent_id: "explicit-id".to_string(),
                is_standalone: false,
            }
        );
    }

    #[test]
    fn resolve_agent_identity_uses_env_when_arg_is_missing() {
        let identity = resolve_agent_identity_from_values(
            None,
            Some("env-id".to_string()),
            "fallback-id".to_string(),
        );

        assert_eq!(
            identity,
            BridgeIdentity {
                agent_id: "env-id".to_string(),
                is_standalone: false,
            }
        );
    }

    #[test]
    fn resolve_agent_identity_generates_standalone_when_no_identity_exists() {
        let identity = resolve_agent_identity_from_values(None, None, "fallback-id".to_string());

        assert_eq!(
            identity,
            BridgeIdentity {
                agent_id: "fallback-id".to_string(),
                is_standalone: true,
            }
        );
    }

    #[test]
    fn resolve_agent_identity_ignores_blank_env_identity() {
        let identity = resolve_agent_identity_from_values(
            None,
            Some("   ".to_string()),
            "fallback-id".to_string(),
        );

        assert_eq!(
            identity,
            BridgeIdentity {
                agent_id: "fallback-id".to_string(),
                is_standalone: true,
            }
        );
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

    /// Regression test for T22: a daemon SSE response whose JSON payload
    /// contains a cron schedule string with asterisks (e.g. from an
    /// `agent_update` success message echoing "30 * * * *") must survive
    /// `parse_sse_messages` byte-for-byte. This pins that the SSE parser
    /// does not truncate or mangle the payload at `*` characters.
    #[test]
    fn parse_sse_preserves_cron_asterisks_in_payload() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Agent 'x' updated successfully. schedule: 30 * * * *\"}]}}\n\n";
        let messages = parse_sse_messages(body);

        assert_eq!(messages.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        let text = value["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "Agent 'x' updated successfully. schedule: 30 * * * *");
    }

    // ── parse_sse_messages edge cases ────────────────────────────

    #[test]
    fn parse_sse_empty_body_returns_empty() {
        let messages = parse_sse_messages("");
        assert!(messages.is_empty());
    }

    #[test]
    fn parse_sse_only_keepalive_returns_empty() {
        let body = "retry: 3000\n\n";
        let messages = parse_sse_messages(body);
        assert!(messages.is_empty());
    }

    #[test]
    fn parse_sse_whitespace_data_lines_are_kept() {
        let body = "data: \ndata: hello\n\n";
        let messages = parse_sse_messages(body);
        // First line is empty string after "data: ", second is "hello"
        // flush_sse_event joins them: "" + "\n" + "hello" = "\nhello"
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("hello"));
    }

    #[test]
    fn parse_sse_multiple_events_with_keepalives() {
        let body = "retry: 3000\n\ndata: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["{\"id\":1}", "{\"id\":2}"]);
    }

    #[test]
    fn parse_sse_data_without_space_prefix() {
        let body = "data:{\"id\":1}\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["{\"id\":1}"]);
    }

    #[test]
    fn parse_sse_triple_multiline_data() {
        let body = "data: line1\ndata: line2\ndata: line3\n\n";
        let messages = parse_sse_messages(body);
        assert_eq!(messages, vec!["line1\nline2\nline3"]);
    }

    // ── build_jsonrpc_transport_error edge cases ─────────────────

    #[test]
    fn transport_error_preserves_string_id() {
        let error = build_jsonrpc_transport_error(r#"{"jsonrpc":"2.0","id":"abc"}"#, "boom");
        let value: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(value["id"], "abc");
        assert_eq!(value["error"]["code"], -32000);
        assert_eq!(value["error"]["data"], "boom");
    }

    #[test]
    fn transport_error_message_format() {
        let error = build_jsonrpc_transport_error("{}", "test error");
        let value: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["error"]["message"], "canopy bridge transport error");
    }

    // ── resolve_agent_identity_from_values edge cases ─────────────

    #[test]
    fn resolve_identity_empty_arg_with_env() {
        let identity = resolve_agent_identity_from_values(
            Some("".to_string()),
            Some("env-id".to_string()),
            "fallback".to_string(),
        );
        assert_eq!(identity.agent_id, "env-id");
        assert!(!identity.is_standalone);
    }

    #[test]
    fn resolve_identity_whitespace_arg_with_env() {
        let identity = resolve_agent_identity_from_values(
            Some("  ".to_string()),
            Some("env-id".to_string()),
            "fallback".to_string(),
        );
        assert_eq!(identity.agent_id, "env-id");
        assert!(!identity.is_standalone);
    }

    #[test]
    fn resolve_identity_arg_over_env() {
        let identity = resolve_agent_identity_from_values(
            Some("arg-id".to_string()),
            Some("env-id".to_string()),
            "fallback".to_string(),
        );
        assert_eq!(identity.agent_id, "arg-id");
    }

    #[test]
    fn resolve_identity_both_empty() {
        let identity = resolve_agent_identity_from_values(
            Some("".to_string()),
            Some("".to_string()),
            "fallback".to_string(),
        );
        assert_eq!(identity.agent_id, "fallback");
        assert!(identity.is_standalone);
    }

    #[test]
    fn resolve_identity_arg_trims_whitespace() {
        let identity = resolve_agent_identity_from_values(
            Some("  my-id  ".to_string()),
            None,
            "fallback".to_string(),
        );
        assert_eq!(identity.agent_id, "my-id");
    }

    // ── non_empty_env tests ─────────────────────────────────────

    #[test]
    fn non_empty_env_returns_none_for_missing_var() {
        std::env::remove_var("CANOPY_TEST_MISSING_VAR");
        assert!(non_empty_env("CANOPY_TEST_MISSING_VAR").is_none());
    }

    #[test]
    fn non_empty_env_returns_none_for_empty_var() {
        std::env::set_var("CANOPY_TEST_EMPTY_VAR", "");
        assert!(non_empty_env("CANOPY_TEST_EMPTY_VAR").is_none());
        std::env::remove_var("CANOPY_TEST_EMPTY_VAR");
    }

    #[test]
    fn non_empty_env_returns_none_for_whitespace_var() {
        std::env::set_var("CANOPY_TEST_WS_VAR", "   ");
        assert!(non_empty_env("CANOPY_TEST_WS_VAR").is_none());
        std::env::remove_var("CANOPY_TEST_WS_VAR");
    }

    #[test]
    fn non_empty_env_returns_trimmed_value() {
        std::env::set_var("CANOPY_TEST_VALUE_VAR", "  hello  ");
        let result = non_empty_env("CANOPY_TEST_VALUE_VAR");
        assert_eq!(result, Some("hello".to_string()));
        std::env::remove_var("CANOPY_TEST_VALUE_VAR");
    }

    // ── resolve_workdir tests ───────────────────────────────────

    #[test]
    fn resolve_workdir_uses_explicit_arg() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_workdir(Some(dir.path().to_path_buf())).unwrap();
        assert!(result.contains(dir.path().file_name().unwrap().to_str().unwrap()));
    }

    #[test]
    fn resolve_workdir_falls_back_to_current_dir() {
        // The cwd is only the *third* source, behind the explicit argument and
        // CANOPY_WORKDIR. That variable is set for every process the daemon
        // spawns, so a suite run from inside a canopy session inherits it and
        // would otherwise measure the env branch while claiming to test the
        // fallback. Clear it for the duration, then put it back.
        let saved = std::env::var(CANOPY_WORKDIR_ENV).ok();
        std::env::remove_var(CANOPY_WORKDIR_ENV);

        let result = resolve_workdir(None).unwrap();

        if let Some(value) = saved {
            std::env::set_var(CANOPY_WORKDIR_ENV, value);
        }

        let cwd = std::env::current_dir().unwrap();
        let canonical = std::fs::canonicalize(&cwd).unwrap();
        assert_eq!(result, canonical.to_string_lossy().to_string());
    }

    #[test]
    fn resolve_workdir_prefers_the_env_var_over_the_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        let saved = std::env::var(CANOPY_WORKDIR_ENV).ok();
        std::env::set_var(CANOPY_WORKDIR_ENV, dir.path());

        let result = resolve_workdir(None).unwrap();

        match saved {
            Some(value) => std::env::set_var(CANOPY_WORKDIR_ENV, value),
            None => std::env::remove_var(CANOPY_WORKDIR_ENV),
        }

        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(result, expected.to_string_lossy().to_string());
    }

    // ── resolve_bridge_port tests ───────────────────────────────

    #[test]
    fn resolve_bridge_port_prefers_explicit_arg() {
        assert_eq!(resolve_bridge_port(Some(9999)), 9999);
    }

    #[test]
    fn resolve_bridge_port_defaults_to_7755() {
        // Remove env var to ensure default
        std::env::remove_var("CANOPY_PORT");
        // Without a data dir or state, should default to 7755
        assert_eq!(resolve_bridge_port(None), 7755);
    }

    // ── read_port_from_state tests ──────────────────────────────

    #[test]
    fn read_port_from_state_returns_none_for_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_port_from_state(dir.path()).is_none());
    }

    #[test]
    fn read_port_from_state_returns_none_for_missing_port() {
        let dir = tempfile::tempdir().unwrap();
        let _db = Database::new(&dir.path().join("background_agents.db")).unwrap();
        // No port set in state
        assert!(read_port_from_state(dir.path()).is_none());
    }

    #[test]
    fn read_port_from_state_returns_port_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("background_agents.db")).unwrap();
        db.set_state("port", "8080").unwrap();
        assert_eq!(read_port_from_state(dir.path()), Some(8080));
    }

    #[test]
    fn read_port_from_state_returns_none_for_invalid_port() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("background_agents.db")).unwrap();
        db.set_state("port", "not-a-number").unwrap();
        assert!(read_port_from_state(dir.path()).is_none());
    }

    #[test]
    fn flush_sse_event_clears_current_and_pushes_message() {
        let mut current = vec!["line1", "line2"];
        let mut messages = Vec::new();
        flush_sse_event(&mut current, &mut messages);
        assert!(current.is_empty());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "line1\nline2");
    }

    #[test]
    fn flush_sse_event_does_nothing_when_current_is_empty() {
        let mut current = Vec::new();
        let mut messages = Vec::new();
        flush_sse_event(&mut current, &mut messages);
        assert!(current.is_empty());
        assert!(messages.is_empty());
    }

    #[test]
    fn flush_sse_event_trims_trailing_newlines() {
        let mut current = vec!["line1\n", "line2\n"];
        let mut messages = Vec::new();
        flush_sse_event(&mut current, &mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "line1\n\nline2\n");
    }

    #[test]
    fn build_jsonrpc_transport_error_with_empty_request() {
        let error = build_jsonrpc_transport_error("", "test error");
        let value: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["message"], "canopy bridge transport error");
        assert_eq!(value["error"]["data"], "test error");
    }

    #[test]
    fn build_jsonrpc_transport_error_with_null_id() {
        let error = build_jsonrpc_transport_error(r#"{"jsonrpc":"2.0","id":null}"#, "error");
        let value: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert!(value["id"].is_null());
    }

    #[test]
    fn resolve_workdir_with_env_var() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CANOPY_WORKDIR", dir.path());
        let result = resolve_workdir(None).unwrap();
        assert!(result.contains(dir.path().file_name().unwrap().to_str().unwrap()));
        std::env::remove_var("CANOPY_WORKDIR");
    }

    #[test]
    fn resolve_bridge_port_with_env_var() {
        std::env::set_var("CANOPY_PORT", "9999");
        let result = resolve_bridge_port(None);
        assert_eq!(result, 9999);
        std::env::remove_var("CANOPY_PORT");
    }

    #[test]
    fn resolve_bridge_port_with_invalid_env_var() {
        std::env::set_var("CANOPY_PORT", "not-a-number");
        let result = resolve_bridge_port(None);
        assert_eq!(result, 7755); // Should fall back to default
        std::env::remove_var("CANOPY_PORT");
    }
}
