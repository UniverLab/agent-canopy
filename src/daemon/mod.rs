//! MCP Server handler implementing all canopy tools.
//!
//! Uses the `rmcp` SDK's `#[tool_router]` and `#[tool_handler]` macros
//! with `Parameters<T>` for proper MCP protocol compliance.

pub(crate) mod bridge;
pub(crate) mod clean_cli;
pub(crate) mod cli;
pub(crate) mod doctor;
pub(crate) mod handler_formatting;
pub(crate) mod handler_helpers;
pub(crate) mod helpers;
pub(crate) mod loop_cli;
pub(crate) mod models_cli;
pub(crate) mod params;
pub(crate) mod process;
pub(crate) mod project_cli;
pub(crate) mod prompts_cli;
pub(crate) mod rag_cli;
pub(crate) mod server;
pub(crate) mod service_install;
pub(crate) mod spec_cli;

pub mod handler;

#[cfg(test)]
mod test_missions;
#[cfg(test)]
mod tests;

pub use handler::TaskTriggerHandler;
