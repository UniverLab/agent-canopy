pub mod config_manip;
pub mod daemon_service;
pub mod dir_browser;
pub mod models;
pub mod platform_adapter;
pub mod public_api;
pub mod registry_fetch;
pub mod sync_and_skills;
pub mod uninstall;
pub mod wizard;

// Re-export public types and functions for backward compatibility
pub use config_manip::strip_jsonc_comments;
pub use daemon_service::needs_setup;
pub use models::{is_platform_available, Platform, PlatformWithCli};
pub use platform_adapter::adapt_config;
pub use public_api::{
    remove_json_key_pub, remove_toml_server_pub, upsert_json_key_pub, upsert_toml_array_pub,
    upsert_toml_key_pub,
};
pub use registry_fetch::{fetch_registry_raw, maybe_refresh_registry};
pub use wizard::run_setup;
