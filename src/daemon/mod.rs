//! MCP Server handler implementing all canopy tools.
//!
//! Uses the `rmcp` SDK's `#[tool_router]` and `#[tool_handler]` macros
//! with `Parameters<T>` for proper MCP protocol compliance.

pub(crate) mod bridge;
pub(crate) mod cli;
pub(crate) mod doctor;
pub(crate) mod handler_formatting;
pub(crate) mod handler_helpers;
pub(crate) mod helpers;
pub(crate) mod params;
pub(crate) mod process;
pub(crate) mod rag_cli;
pub(crate) mod server;
pub(crate) mod service_install;

pub mod handler;

#[cfg(test)]
mod test_missions;
#[cfg(test)]
mod tests;

pub use handler::TaskTriggerHandler;
