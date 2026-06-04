use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::shared::sync_identity::{
    CANOPY_AGENT_ID_ENV, CANOPY_AGENT_ID_HEADER, CANOPY_CLIENT_NAME_HEADER, CANOPY_WORKDIR_ENV,
    CANOPY_WORKDIR_HEADER,
};

pub(crate) async fn run_bridge(
    agent_id_arg: Option<String>,
    port_arg: Option<u16>,
    workdir_arg: Option<PathBuf>,
) -> Result<()> {
    let agent_id = resolve_agent_id(agent_id_arg)?;
    let workdir = resolve_workdir(workdir_arg)?;
    let port = resolve_bridge_port(port_arg);
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let client = Client::new();

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
                        return Ok(());
                    }
                    _ = sig_pipe.recv() => {
                        eprintln!("canopy bridge: received SIGPIPE, exiting");
                        return Ok(());
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
            return Ok(());
        };
        if line.trim().is_empty() {
            continue;
        }

        let response = forward_request(&client, &endpoint, &agent_id, &workdir, &line, session_id.as_deref()).await;
        match response {
            Ok((body, new_session_id)) => {
                if let Some(sid) = new_session_id {
                    session_id = Some(sid);
                }
                stdout.write_all(body.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
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
}

async fn forward_request(
    client: &Client,
    endpoint: &str,
    agent_id: &str,
    workdir: &str,
    line: &str,
    session_id: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut request = client
        .post(endpoint)
        .header(CANOPY_AGENT_ID_HEADER, agent_id)
        .header(CANOPY_WORKDIR_HEADER, workdir)
        .header(CANOPY_CLIENT_NAME_HEADER, "bridge")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .body(line.to_string());

    if let Some(sid) = session_id {
        request = request.header("Mcp-Session-Id", sid);
    }

    let response = request
        .send()
        .await
        .context("failed to reach canopy daemon")?;
    let status = response.status();

    let new_session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body = response
        .text()
        .await
        .context("failed to read canopy daemon response body")?;

    if !status.is_success() {
        anyhow::bail!("daemon returned HTTP {status}: {body}");
    }

    Ok((body, new_session_id))
}

fn resolve_agent_id(agent_id_arg: Option<String>) -> Result<String> {
    if let Some(id) = agent_id_arg
        .or_else(|| std::env::var(CANOPY_AGENT_ID_ENV).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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

fn resolve_bridge_port(port_arg: Option<u16>) -> u16 {
    if let Some(port) = port_arg {
        return port;
    }

    if let Ok(value) = std::env::var("CANOPY_PORT") {
        if let Ok(port) = value.trim().parse::<u16>() {
            return port;
        }
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
