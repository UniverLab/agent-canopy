//! Context Transfer — capture and inject conversation context between agents.
//!
//! Builds a plain-text context block from the source agent's prompt history,
//! or from recent cleaned VT lines when the agent has no recorded prompts,
//! then drives the two-step TUI modal (preview → agent picker).

use std::collections::VecDeque;

use super::agent::{InteractiveAgent, PromptEntry};

// ── Config ───────────────────────────────────────────────────────

/// Runtime defaults for context transfer (no external config file required).
pub struct ContextTransferConfig {
    pub default_prompt_history: usize,
}

impl Default for ContextTransferConfig {
    fn default() -> Self {
        Self {
            default_prompt_history: 3,
        }
    }
}

// ── Context builder ──────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSourceKind {
    Interactive,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextCaptureKind {
    Prompts,
    LinePages,
}

impl ContextSourceKind {
    fn header(self, agent: &InteractiveAgent) -> String {
        match self {
            Self::Interactive => {
                format!(
                    "--- context from: {} | workdir: {} ---\n",
                    agent.name, agent.working_dir
                )
            }
            Self::Terminal => {
                format!(
                    "--- context from terminal: {} | workdir: {} ---\n",
                    agent.name, agent.working_dir
                )
            }
        }
    }
}

pub fn build_context_payload_for(
    agent: &InteractiveAgent,
    n_units: usize,
    source_kind: ContextSourceKind,
    capture_kind: ContextCaptureKind,
) -> String {
    let mut out = source_kind.header(agent);

    match (source_kind, capture_kind) {
        (ContextSourceKind::Interactive, ContextCaptureKind::Prompts) => {
            append_interactive_prompt_context(&mut out, agent, n_units);
        }
        (ContextSourceKind::Interactive, ContextCaptureKind::LinePages)
        | (ContextSourceKind::Terminal, _) => append_line_context(&mut out, agent, n_units),
    }

    out.push_str("--- end context ---\n");
    clean_context_output(&out)
}

/// Post-process context payload: collapse blank runs, strip status-bar noise.
fn clean_context_output(raw: &str) -> String {
    let mut result = Vec::new();
    let mut blank_run = 0u8;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                result.push(String::new());
            }
            continue;
        }
        blank_run = 0;

        // Skip common TUI status-bar / sidebar artifacts
        if is_status_noise(trimmed) {
            continue;
        }
        result.push(line.to_string());
    }

    // Trim trailing blank lines before the footer
    while result.last().is_some_and(|l| l.trim().is_empty()) {
        result.pop();
    }

    let mut out = result.join("\n");
    out.push('\n');
    out
}

/// Lines that are TUI chrome / sidebar noise in CLI agents.
fn is_status_noise(line: &str) -> bool {
    let trimmed = line.trim();

    // Only filter very specific UI navigation/help text, not content
    const EXACT: &[&str] = &["for shortcuts", "Shift+Tab", "MCP issues", "MCP servers"];
    if EXACT.contains(&trimmed) {
        return true;
    }

    if trimmed.starts_with("ctrl+p commands") || trimmed.starts_with("workspace (") {
        return true;
    }

    // Very conservative: only skip lines that look like TUI status bars with no content
    if trimmed.contains("LSPs will activate") && trimmed.len() < 50 {
        return true;
    }

    false
}

fn collect_last_prompts(history: &VecDeque<PromptEntry>, n: usize) -> Vec<PromptEntry> {
    let keep = n.max(1);
    history
        .iter()
        .skip(history.len().saturating_sub(keep))
        .cloned()
        .collect()
}

pub fn interactive_capture_kind(agent: &InteractiveAgent) -> ContextCaptureKind {
    if interactive_prompt_count(agent) > 0 {
        ContextCaptureKind::Prompts
    } else {
        ContextCaptureKind::LinePages
    }
}

pub fn initial_capture_units(max_units: usize, config: &ContextTransferConfig) -> usize {
    if max_units == 0 {
        1
    } else {
        max_units.min(config.default_prompt_history.max(1))
    }
}

pub fn interactive_prompt_count(agent: &InteractiveAgent) -> usize {
    agent
        .prompt_history
        .lock()
        .ok()
        .map(|history| history.len())
        .unwrap_or(0)
}

pub fn interactive_line_page_count(agent: &InteractiveAgent) -> usize {
    if !agent.visible_text().trim().is_empty() {
        return 1;
    }

    let total_depth = agent.total_depth();
    if total_depth == 0 {
        return 0;
    }

    if agent.last_lines(50).trim().is_empty() {
        return 0;
    }

    total_depth.div_ceil(50)
}

fn append_interactive_prompt_context(out: &mut String, agent: &InteractiveAgent, n_prompts: usize) {
    let prompt_history = agent
        .prompt_history
        .lock()
        .ok()
        .as_deref()
        .cloned()
        .unwrap_or_default();
    let prompts = collect_last_prompts(&prompt_history, n_prompts);
    if prompts.is_empty() {
        return;
    }

    let total_depth = agent.total_depth();
    for (idx, entry) in prompts.iter().enumerate() {
        push_prompt_block(out, &entry.input);

        let is_last_prompt = idx + 1 == prompts.len();
        let response_end = response_end_line(entry, is_last_prompt, total_depth);
        if response_end <= entry.output_range.0 {
            continue;
        }

        let response = agent.lines_at_scrollback_range(entry.output_range.0, response_end);
        if response.is_empty() {
            continue;
        }

        out.push_str(&response);
        out.push('\n');
    }
}

fn append_line_context(out: &mut String, agent: &InteractiveAgent, n_units: usize) {
    let visible = agent.visible_text();
    if n_units <= 1 {
        if visible.is_empty() {
            return;
        }
        out.push_str(&visible);
        if !visible.ends_with('\n') {
            out.push('\n');
        }
        return;
    }

    let history = agent.last_lines((n_units.max(1)) * 50);
    if history.is_empty() {
        return;
    }

    out.push_str(&history);
    if !history.ends_with('\n') {
        out.push('\n');
    }
}

/// Append a prompt with every line `> `-prefixed so multi-line prompts
/// (e.g. from the prompt builder) stay visually separated from responses.
fn push_prompt_block(out: &mut String, input: &str) {
    for line in input.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    if input.is_empty() {
        out.push_str(">\n");
    }
}

fn response_end_line(entry: &PromptEntry, is_last_prompt: bool, total_depth: usize) -> usize {
    if !is_last_prompt && entry.output_range.1 > entry.output_range.0 {
        return entry.output_range.1;
    }
    total_depth
}

// ── Persistence ──────────────────────────────────────────────────

// ── Modal state ──────────────────────────────────────────────────

/// Which step the two-step modal is on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ContextTransferStep {
    /// Step 1 — adjust the capture range and preview the payload.
    Preview,
    /// Step 2 — pick the destination agent.
    AgentPicker,
}

/// State for the context transfer modal.
pub struct ContextTransferModal {
    pub step: ContextTransferStep,
    /// Index into `App::interactive_agents` (or `terminal_agents` when `source_is_terminal`).
    pub source_agent_idx: usize,
    /// Whether the source is a terminal session (indexes `terminal_agents`).
    pub source_is_terminal: bool,
    /// How the source content is captured for preview/transfer.
    pub capture_kind: ContextCaptureKind,
    /// Number of recent prompts or line pages to include (adjustable in Step 1).
    pub n_units: usize,
    /// Currently highlighted agent in the picker (index into the picker list).
    pub picker_selected: usize,
    /// Precomputed payload shown as preview in Step 1.
    pub payload_preview: String,
}

impl ContextTransferModal {
    fn new_with_kind(
        source_agent_idx: usize,
        source_kind: ContextSourceKind,
        capture_kind: ContextCaptureKind,
        initial_units: usize,
    ) -> Self {
        Self {
            step: ContextTransferStep::Preview,
            source_agent_idx,
            source_is_terminal: source_kind == ContextSourceKind::Terminal,
            capture_kind,
            n_units: initial_units.max(1),
            picker_selected: 0,
            payload_preview: String::new(),
        }
    }

    pub fn new(
        source_agent_idx: usize,
        capture_kind: ContextCaptureKind,
        initial_units: usize,
    ) -> Self {
        Self::new_with_kind(
            source_agent_idx,
            ContextSourceKind::Interactive,
            capture_kind,
            initial_units,
        )
    }

    pub fn new_terminal(source_agent_idx: usize, initial_units: usize) -> Self {
        Self::new_with_kind(
            source_agent_idx,
            ContextSourceKind::Terminal,
            ContextCaptureKind::LinePages,
            initial_units,
        )
    }

    pub fn decrement_field(&mut self) {
        self.n_units = self.n_units.saturating_sub(1).max(1);
    }

    pub fn increment_field(&mut self, max_value: usize) {
        self.n_units = (self.n_units + 1).min(max_value.max(1));
    }

    pub fn source_kind(&self) -> ContextSourceKind {
        if self.source_is_terminal {
            ContextSourceKind::Terminal
        } else {
            ContextSourceKind::Interactive
        }
    }

    pub fn unit_label(&self) -> &'static str {
        match (self.source_kind(), self.capture_kind) {
            (ContextSourceKind::Interactive, ContextCaptureKind::Prompts) => "prompts",
            (ContextSourceKind::Interactive, ContextCaptureKind::LinePages) => "line blocks (×50)",
            (ContextSourceKind::Terminal, _) => "pages (×50 lines)",
        }
    }

    pub fn unit_help(&self) -> &'static str {
        match (self.source_kind(), self.capture_kind) {
            (ContextSourceKind::Interactive, ContextCaptureKind::Prompts) => {
                "most recent prompt ranges"
            }
            (ContextSourceKind::Interactive, ContextCaptureKind::LinePages) => {
                "most recent cleaned lines"
            }
            (ContextSourceKind::Terminal, _) => "most recent terminal lines",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clean_context_output, collect_last_prompts, initial_capture_units, ContextCaptureKind,
        ContextSourceKind, ContextTransferConfig, ContextTransferModal,
    };
    use crate::tui::agent::PromptEntry;
    use chrono::Utc;
    use std::collections::VecDeque;

    #[test]
    fn collect_last_prompts_keeps_tail_in_order() {
        let history = VecDeque::from(vec![
            PromptEntry {
                input: "one".to_string(),
                output_range: (0, 1),
                timestamp: Utc::now(),
            },
            PromptEntry {
                input: "two".to_string(),
                output_range: (1, 2),
                timestamp: Utc::now(),
            },
            PromptEntry {
                input: "three".to_string(),
                output_range: (2, 3),
                timestamp: Utc::now(),
            },
        ]);

        let prompts = collect_last_prompts(&history, 2);
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].input, "two");
        assert_eq!(prompts[1].input, "three");
    }

    #[test]
    fn push_prompt_block_prefixes_every_line() {
        let mut out = String::new();
        super::push_prompt_block(&mut out, "# [INSTRUCTIONS]\nfix the bug\nrun tests");
        assert_eq!(out, "> # [INSTRUCTIONS]\n> fix the bug\n> run tests\n");
    }

    #[test]
    fn clean_context_output_drops_noise_and_collapses_blanks() {
        let raw = "--- context from: demo | workdir: /tmp ---\n\nContext\n\nhello\n\n\nremaining 12\n--- end context ---\n";
        let cleaned = clean_context_output(raw);
        assert!(cleaned.contains("hello"));
        assert!(cleaned.contains("remaining 12"));
        assert!(cleaned.contains("hello\n\nremaining 12\n--- end context ---"));
    }

    #[test]
    fn modal_source_kind_reflects_session_type() {
        let interactive = ContextTransferModal::new(1, ContextCaptureKind::Prompts, 2);
        let terminal = ContextTransferModal::new_terminal(2, 3);
        assert_eq!(interactive.source_kind(), ContextSourceKind::Interactive);
        assert_eq!(terminal.source_kind(), ContextSourceKind::Terminal);
    }

    #[test]
    fn initial_capture_units_clamps_to_available_with_default_limit() {
        let config = ContextTransferConfig::default();
        assert_eq!(initial_capture_units(1, &config), 1);
        assert_eq!(initial_capture_units(2, &config), 2);
        assert_eq!(initial_capture_units(3, &config), 3);
        assert_eq!(initial_capture_units(8, &config), 3);
    }

    #[test]
    fn line_page_mode_uses_line_labels() {
        let modal = ContextTransferModal::new(0, ContextCaptureKind::LinePages, 3);
        assert_eq!(modal.unit_label(), "line blocks (×50)");
        assert_eq!(modal.unit_help(), "most recent cleaned lines");
    }
}
