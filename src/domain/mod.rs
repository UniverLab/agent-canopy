//! Domain layer — core business entities and validation rules.
//!
//! This is the innermost layer of the architecture. It has no dependencies
//! on infrastructure, frameworks, or external crates beyond basic utilities.

pub mod blueprints;
pub mod canopy_config;
pub mod clean;
pub mod cli_config;
pub mod cli_strategy;
pub mod gamification;
pub mod loops;
pub mod models;
pub mod models_db;
pub mod notification;
pub mod nursery;
pub mod pools;
pub mod project;
pub mod prompts;
pub mod quota_reset;
pub mod seeds;
pub mod sync;
pub mod usage_stats;
pub mod validation;

#[cfg(test)]
mod domain_tests;
