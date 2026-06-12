pub const CANOPY_AGENT_ID_ENV: &str = "CANOPY_AGENT_ID";
pub const CANOPY_SESSION_NAME_ENV: &str = "CANOPY_SESSION_NAME";
pub const CANOPY_WORKDIR_ENV: &str = "CANOPY_WORKDIR";
pub const CANOPY_CLIENT_NAME_ENV: &str = "CANOPY_CLIENT_NAME";
/// Optional env var for seed-bound sessions.
pub const CANOPY_SEED_ID_ENV: &str = "CANOPY_SEED_ID";

pub const CANOPY_AGENT_ID_HEADER: &str = "x-canopy-agent-id";
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
