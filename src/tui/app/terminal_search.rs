use crate::tui::agent::InteractiveAgent;

pub struct TerminalSearch {
    /// Index of the terminal agent being searched.
    pub agent_idx: usize,
    /// Whether this is an interactive or terminal agent.
    pub is_terminal: bool,
    /// Current search query.
    pub query: String,
    /// Row indices (in the vt100 screen) where matches were found.
    pub match_rows: Vec<usize>,
    /// Current match index (cycles through match_rows).
    pub current_match: usize,
}

impl TerminalSearch {
    pub fn new(idx: usize) -> Self {
        Self {
            agent_idx: idx,
            is_terminal: true,
            query: String::new(),
            match_rows: Vec::new(),
            current_match: 0,
        }
    }

    pub fn new_interactive(idx: usize) -> Self {
        Self {
            agent_idx: idx,
            is_terminal: false,
            query: String::new(),
            match_rows: Vec::new(),
            current_match: 0,
        }
    }

    /// Search the agent's output for the query and populate match_rows.
    pub fn search(&mut self, agent: &InteractiveAgent) {
        self.match_rows.clear();
        if self.query.is_empty() {
            return;
        }
        let output = agent.output();
        let query_lower = self.query.to_lowercase();
        for (i, line) in output.lines().enumerate() {
            if line.to_lowercase().contains(&query_lower) {
                self.match_rows.push(i);
            }
        }
        if !self.match_rows.is_empty() {
            self.current_match = self.current_match.min(self.match_rows.len() - 1);
        }
    }

    /// Jump to the current match by setting the agent's scroll_offset.
    pub fn jump_to_match(&self, agent: &mut InteractiveAgent) {
        if let Some(&row) = self.match_rows.get(self.current_match) {
            let total = agent.total_depth();
            let (_, screen_rows) = agent
                .vt
                .lock()
                .map(|vt| vt.screen().size())
                .unwrap_or((40, 80));
            let screen_h = screen_rows as usize;
            // Convert absolute row to scroll offset from bottom
            if total > screen_h && row < total.saturating_sub(screen_h) {
                agent.scroll_offset = total - screen_h - row;
            } else {
                agent.scroll_offset = 0;
            }
        }
    }

    pub fn next_match(&mut self) {
        if !self.match_rows.is_empty() {
            self.current_match = (self.current_match + 1) % self.match_rows.len();
        }
    }

    pub fn prev_match(&mut self) {
        if !self.match_rows.is_empty() {
            self.current_match = self
                .current_match
                .checked_sub(1)
                .unwrap_or(self.match_rows.len() - 1);
        }
    }
}
