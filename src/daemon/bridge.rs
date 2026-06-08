use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::shared::sync_identity::{
    CANOPY_AGENT_ID_ENV, CANOPY_CLIENT_NAME_ENV, CANOPY_WORKDIR_ENV,
};

pub(crate) async fn run_bridge(
    agent_id_arg: Option<String>,
    port_arg: Option<u16>,
    workdir_arg: Option<PathBuf>,
) -> Result<()> {
    let agent_id = resolve_agent_id(agent_id_arg)?;
    let workdir = resolve_workdir(workdir_arg)?;

    if port_arg.is_some() {
        eprintln!(
            "canopy bridge: --port is ignored in stdio mode (kept only for backward compatibility)"
        );
    }

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
