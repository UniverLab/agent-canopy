//! Dialog overlays — new agent, quit confirmation, color legend, context transfer.

pub mod at_picker;
pub mod context_transfer;
pub mod launchpad;
pub mod new_agent_dialog;
pub mod pickers;
pub mod rag_transfer;
pub mod section_picker;
pub mod simple_modals;
pub mod simple_prompt;

// Re-export public drawing functions
pub use context_transfer::draw_context_transfer_modal;
pub use launchpad::draw_launchpad_dialog;
pub use new_agent_dialog::draw_new_agent_dialog;
pub use pickers::{draw_split_picker, draw_suggestion_picker};
pub use rag_transfer::draw_rag_transfer_modal;
pub use simple_modals::{draw_legend, draw_quit_confirm};
pub use simple_prompt::draw_simple_prompt_dialog;

// Common imports shared with submodules
pub(crate) use super::{centered_rect, truncate_str};
pub(crate) use super::{ACCENT, DIM};
pub(crate) use super::{BG_SELECTED, ERROR_COLOR, INTERACTIVE_COLOR};
