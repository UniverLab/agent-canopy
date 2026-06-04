use std::path::Path;

pub const CANOPY_AGENT_ID_ENV: &str = "CANOPY_AGENT_ID";
pub const CANOPY_SESSION_NAME_ENV: &str = "CANOPY_SESSION_NAME";
pub const CANOPY_WORKDIR_ENV: &str = "CANOPY_WORKDIR";
/// Optional env var for seed-bound sessions.
pub const CANOPY_SEED_ID_ENV: &str = "CANOPY_SEED_ID";
pub const CANOPY_IDENTITY_FILE: &str = ".canopy_identity";

pub const CANOPY_AGENT_ID_HEADER: &str = "x-canopy-agent-id";
pub const CANOPY_SESSION_NAME_HEADER: &str = "x-canopy-session-name";
pub const CANOPY_WORKDIR_HEADER: &str = "x-canopy-workdir";
/// Static header injected by the MCP client config with the harness name (e.g. "copilot", "opencode").
pub const CANOPY_CLIENT_NAME_HEADER: &str = "x-canopy-client-name";
/// Optional header for seed-bound sessions.
pub const CANOPY_SEED_ID_HEADER: &str = "x-canopy-seed-id";

/// Read a header value from an HTTP request, trimmed and non-empty.
pub(crate) fn header_str<'a>(parts: &'a axum::http::request::Parts, name: &str) -> Option<&'a str> {
    parts
        .headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

pub fn write_identity_file(workdir: &str, agent_id: &str) -> std::io::Result<()> {
    let trimmed_workdir = workdir.trim();
    let trimmed_agent_id = agent_id.trim();
    if trimmed_workdir.is_empty() || trimmed_agent_id.is_empty() {
        return Ok(());
    }

    let path = Path::new(trimmed_workdir).join(CANOPY_IDENTITY_FILE);
    std::fs::write(path, format!("{trimmed_agent_id}\n"))
}

pub fn read_identity_file_agent_id(workdir: &str) -> Option<String> {
    let trimmed_workdir = workdir.trim();
    if trimmed_workdir.is_empty() {
        return None;
    }

    let path = Path::new(trimmed_workdir).join(CANOPY_IDENTITY_FILE);
    let raw = std::fs::read_to_string(path).ok()?;
    let first = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    let parsed = if let Some(value) = first.strip_prefix("agent_id=") {
        let id = value.trim();
        (!id.is_empty()).then(|| id.to_string())
    } else {
        (!first.is_empty()).then(|| first.to_string())
    }?;

    let _ = std::fs::remove_file(Path::new(trimmed_workdir).join(CANOPY_IDENTITY_FILE));
    Some(parsed)
}
