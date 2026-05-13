pub const CANOPY_AGENT_ID_ENV: &str = "CANOPY_AGENT_ID";
pub const CANOPY_SESSION_NAME_ENV: &str = "CANOPY_SESSION_NAME";
pub const CANOPY_WORKDIR_ENV: &str = "CANOPY_WORKDIR";

pub const CANOPY_AGENT_ID_HEADER: &str = "x-canopy-agent-id";
pub const CANOPY_SESSION_NAME_HEADER: &str = "x-canopy-session-name";
pub const CANOPY_WORKDIR_HEADER: &str = "x-canopy-workdir";
/// Static header injected by the MCP client config with the harness name (e.g. "copilot", "opencode").
pub const CANOPY_CLIENT_NAME_HEADER: &str = "x-canopy-client-name";
