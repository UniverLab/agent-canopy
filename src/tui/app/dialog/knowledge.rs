//! `KnowledgeDialog` — state and logic for creating/editing knowledge nodes.

use crate::tui::app::types::Focus;

/// State for the "new knowledge" dialog.
pub struct KnowledgeDialog {
    /// When `Some(id)`, the dialog is in edit mode for an existing node.
    pub edit_id: Option<String>,
    pub title: String,
    pub body: String,
    pub kind: KnowledgeKind,
    pub project_hash: Option<String>,
    /// Which field is focused: 0=title, 1=body, 2=kind
    pub field: usize,
    pub prev_focus: Option<Focus>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeKind {
    Fact,
    Pattern,
}

impl KnowledgeDialog {
    pub fn new(project_hash: Option<String>) -> Self {
        Self {
            edit_id: None,
            title: String::new(),
            body: String::new(),
            kind: KnowledgeKind::Fact,
            project_hash,
            field: 0,
            prev_focus: None,
        }
    }

    pub fn edit(
        id: String,
        title: String,
        body: String,
        kind: KnowledgeKind,
        project_hash: Option<String>,
    ) -> Self {
        Self {
            edit_id: Some(id),
            title,
            body,
            kind,
            project_hash,
            field: 0,
            prev_focus: None,
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            KnowledgeKind::Fact => "fact",
            KnowledgeKind::Pattern => "pattern",
        }
    }

    pub fn cycle_kind(&mut self) {
        self.kind = match self.kind {
            KnowledgeKind::Fact => KnowledgeKind::Pattern,
            KnowledgeKind::Pattern => KnowledgeKind::Fact,
        };
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % 3;
    }

    pub fn prev_field(&mut self) {
        self.field = self.field.checked_sub(1).unwrap_or(2);
    }
}
