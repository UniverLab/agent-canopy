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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue, Request};

    #[test]
    fn env_constants_are_nonempty() {
        assert!(!CANOPY_AGENT_ID_ENV.is_empty());
        assert!(!CANOPY_SESSION_NAME_ENV.is_empty());
        assert!(!CANOPY_WORKDIR_ENV.is_empty());
        assert!(!CANOPY_CLIENT_NAME_ENV.is_empty());
        assert!(!CANOPY_SEED_ID_ENV.is_empty());
    }

    #[test]
    fn header_constants_are_nonempty() {
        assert!(!CANOPY_AGENT_ID_HEADER.is_empty());
        assert!(!CANOPY_CLIENT_NAME_HEADER.is_empty());
        assert!(!CANOPY_SEED_ID_HEADER.is_empty());
    }

    #[test]
    fn env_constant_values_match_expected() {
        assert_eq!(CANOPY_AGENT_ID_ENV, "CANOPY_AGENT_ID");
        assert_eq!(CANOPY_SESSION_NAME_ENV, "CANOPY_SESSION_NAME");
        assert_eq!(CANOPY_WORKDIR_ENV, "CANOPY_WORKDIR");
        assert_eq!(CANOPY_CLIENT_NAME_ENV, "CANOPY_CLIENT_NAME");
        assert_eq!(CANOPY_SEED_ID_ENV, "CANOPY_SEED_ID");
    }

    #[test]
    fn header_constant_values_match_expected() {
        assert_eq!(CANOPY_AGENT_ID_HEADER, "x-canopy-agent-id");
        assert_eq!(CANOPY_CLIENT_NAME_HEADER, "x-canopy-client-name");
        assert_eq!(CANOPY_SEED_ID_HEADER, "x-canopy-seed-id");
    }

    fn make_request_with_header(name: &str, value: &str) -> axum::http::request::Parts {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::try_from(name).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        let (parts, _) = Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        let mut parts = parts;
        parts.headers = headers;
        parts
    }

    #[test]
    fn header_str_returns_value_when_present() {
        let parts = make_request_with_header("x-canopy-agent-id", "agent-123");
        assert_eq!(header_str(&parts, "x-canopy-agent-id"), Some("agent-123"));
    }

    #[test]
    fn header_str_trims_whitespace() {
        let parts = make_request_with_header("x-canopy-agent-id", "  agent-123  ");
        assert_eq!(header_str(&parts, "x-canopy-agent-id"), Some("agent-123"));
    }

    #[test]
    fn header_str_returns_none_for_missing_header() {
        let parts = make_request_with_header("other-header", "value");
        assert_eq!(header_str(&parts, "x-canopy-agent-id"), None);
    }

    #[test]
    fn header_str_returns_none_for_empty_value() {
        let parts = make_request_with_header("x-canopy-agent-id", "");
        assert_eq!(header_str(&parts, "x-canopy-agent-id"), None);
    }

    #[test]
    fn header_str_returns_none_for_whitespace_only_value() {
        let parts = make_request_with_header("x-canopy-agent-id", "   ");
        assert_eq!(header_str(&parts, "x-canopy-agent-id"), None);
    }
}
