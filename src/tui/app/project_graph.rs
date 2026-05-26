//! Project relationship dialog and graph rendering.
//!
//! Handles opening/closing the project relation picker and
//! rendering the project relationship graph in the sidebar.

use anyhow::Result;

use crate::tui::app::types::{App, Focus, ProjectRelationDialog};

impl App {
    pub fn open_project_relation_dialog(&mut self) -> Result<()> {
        let Some(project) = self.selected_project() else {
            return Ok(());
        };

        let available = self
            .db
            .list_intelligence_projects(None, 50)
            .unwrap_or_default();

        let dialog = ProjectRelationDialog {
            from_hash: project.hash.clone(),
            from_name: project.name.clone(),
            available,
            filtered: Vec::new(),
            selected_idx: 0,
            relation_idx: 0,
            relation_types: vec![
                "depends_on".to_string(),
                "complements".to_string(),
                "relates_to".to_string(),
                "independent".to_string(),
            ],
            filter_buffer: String::new(),
            error: None,
        };

        // Build initial filtered list — skip self
        let mut d = dialog;
        d.rebuild_filtered();
        self.project_relation_dialog = Some(d);
        self.focus = Focus::ProjectRelationDialog;
        Ok(())
    }

    pub fn close_project_relation_dialog(&mut self) {
        self.project_relation_dialog = None;
        self.focus = Focus::Preview;
    }

    pub fn confirm_project_relation(&mut self) -> Result<()> {
        let Some(dialog) = &self.project_relation_dialog else {
            return Ok(());
        };

        let from_hash = dialog.from_hash.clone();
        let rel_type = &dialog.relation_types[dialog.relation_idx];

        // "Independent" — skip
        if rel_type == "independent" {
            self.close_project_relation_dialog();
            return Ok(());
        }

        // Need a target project to link to
        let target_entry = dialog.filtered.get(dialog.selected_idx);
        if target_entry.is_none() {
            // No target selected but not independent — treat as independent
            self.close_project_relation_dialog();
            return Ok(());
        }
        let target_idx = *target_entry.unwrap();
        let target = &dialog.available[target_idx];

        let target_hash = target
            .project_hash
            .clone()
            .or_else(|| {
                target.metadata.as_ref().and_then(|m| {
                    serde_json::from_str::<serde_json::Value>(m)
                        .ok()
                        .and_then(|v| v.get("hash").and_then(|h| h.as_str().map(String::from)))
                })
            })
            .unwrap_or_else(|| target.id.clone());

        if let Err(e) = self
            .db
            .link_projects(&from_hash, &target_hash, rel_type, None)
        {
            let mut d = dialog.clone();
            d.error = Some(format!("{e}"));
            self.project_relation_dialog = Some(d);
            return Ok(());
        }

        self.close_project_relation_dialog();
        self.refresh_project_graph().ok();
        Ok(())
    }

    pub fn refresh_project_graph(&mut self) -> Result<()> {
        let mut edges = Vec::new();
        let mut trees: Vec<Vec<String>> = Vec::new();

        for project in &self.projects {
            let related = self
                .db
                .list_related_projects(&project.hash, 20)
                .unwrap_or_default();

            for (node, edge) in related {
                edges.push(crate::tui::app::types::ProjectGraphEdge {
                    from_name: project.name.clone(),
                    to_name: node.title.clone(),
                    from_hash: project.hash.clone(),
                    to_hash: node.project_hash.clone().unwrap_or_else(|| node.id.clone()),
                    relation: edge.relation,
                });
            }
        }

        // Build trees (connected components) from edges using Union-Find
        for edge in &edges {
            let mut root: Option<String> = None;

            // Try to find an existing tree that has either endpoint
            for tree in &trees {
                if tree.contains(&edge.from_name) || tree.contains(&edge.to_name) {
                    root = Some(tree[0].clone());
                    break;
                }
            }

            if let Some(root_name) = root {
                // Add both to that tree
                let tree = trees.iter_mut().find(|t| t.contains(&root_name)).unwrap();
                if !tree.contains(&edge.from_name) {
                    tree.push(edge.from_name.clone());
                }
                if !tree.contains(&edge.to_name) {
                    tree.push(edge.to_name.clone());
                }
            } else {
                // New tree
                trees.push(vec![edge.from_name.clone(), edge.to_name.clone()]);
            }
        }

        // Add solo projects (no relations)
        for project in &self.projects {
            let in_any_tree = trees.iter().any(|tree| tree.contains(&project.name));
            if !in_any_tree {
                trees.push(vec![project.name.clone()]);
            }
        }

        // Deduplicate within each tree
        for tree in &mut trees {
            let mut seen = std::collections::HashSet::new();
            tree.retain(|name| seen.insert(name.clone()));
        }

        self.project_graph_edges = edges;
        self.project_graph_trees = trees;
        Ok(())
    }
}

impl ProjectRelationDialog {
    pub fn rebuild_filtered(&mut self) {
        let query = self.filter_buffer.trim().to_lowercase();
        let from_hash = &self.from_hash;

        self.filtered = self
            .available
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                // Skip self
                if node.project_hash.as_deref() == Some(from_hash.as_str()) || node.id == *from_hash
                {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                node.title.to_lowercase().contains(&query)
                    || node.body.to_lowercase().contains(&query)
            })
            .map(|(idx, _)| idx)
            .collect();

        self.selected_idx = self.selected_idx.min(self.filtered.len().saturating_sub(1));
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected_idx = self
            .selected_idx
            .checked_sub(1)
            .unwrap_or(self.filtered.len() - 1);
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % self.filtered.len();
    }

    pub fn cycle_relation(&mut self, forward: bool) {
        if forward {
            self.relation_idx = (self.relation_idx + 1) % self.relation_types.len();
        } else {
            self.relation_idx = self
                .relation_idx
                .checked_sub(1)
                .unwrap_or(self.relation_types.len() - 1);
        }
    }
}
